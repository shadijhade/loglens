//! Parsing, indexing and filtering of Serilog text logs:
//! `2026-11-18 00:00:01.311 +03:00 [ERR] message` plus continuation lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const LEVELS: [&str; 6] = ["VRB", "DBG", "INF", "WRN", "ERR", "FTL"];
pub const ALL_LEVELS: u8 = 0b11_1111;
pub const NONE: u32 = u32::MAX;
const HEADER_LEN: usize = 37; // up to and including "[LVL] "
const PATTERN_CAP: usize = 110;

#[derive(Clone, Copy)]
pub struct Entry {
    /// UTC milliseconds.
    pub ts: i64,
    pub off: u32,
    pub len: u32,
    pub pat: u32,
    /// Index into `Store::exceptions`, or `NONE`.
    pub exc: u32,
    /// Index into `Store::files`.
    pub file: u32,
    pub level: u8,
}

/// One contiguous read of a log file. A file that grows while it is watched gets a further
/// segment for each batch of appended bytes.
#[derive(Clone)]
pub struct LogFile {
    pub name: Arc<str>,
    pub data: Arc<Vec<u8>>,
    /// Index into `Store::sources`, and where this segment starts in that file.
    source: u32,
    base: u64,
}

#[derive(Clone)]
pub struct Pattern {
    pub level: u8,
    pub text: String,
}

#[derive(Clone)]
pub struct Exception {
    /// `Namespace.Type: normalized message`
    pub text: String,
    /// Throw site: `Class.Method · File.cs:123`
    pub site: String,
}

/// A file found under the opened paths.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    /// Why the file is not shown, when it is not a log.
    pub skipped: Option<String>,
    /// Bytes read so far (for a skipped file: its size when it was looked at).
    consumed: u64,
    /// The newest entry as (time, offset in the file). The next step reads it again, because
    /// the writer may not have finished it yet.
    last: Option<(i64, u64)>,
}

/// Everything loaded, as an immutable snapshot; `advance` builds the next one.
#[derive(Clone, Default)]
pub struct Store {
    /// The files and folders that were opened.
    pub roots: Vec<PathBuf>,
    pub sources: Vec<Source>,
    pub files: Vec<LogFile>,
    /// Sorted by time across all files.
    pub entries: Vec<Entry>,
    pub patterns: Vec<Pattern>,
    pub exceptions: Vec<Exception>,
    /// Offset of the first entry; used to show bucket times as the log wrote them.
    pub tz_ms: i64,
}

impl Store {
    /// The files that turned out to be logs, and the ones that did not.
    pub fn logs(&self) -> impl Iterator<Item = &Source> {
        self.sources.iter().filter(|s| s.skipped.is_none())
    }

    pub fn skipped(&self) -> impl Iterator<Item = &Source> {
        self.sources.iter().filter(|s| s.skipped.is_some())
    }

    pub fn bytes(&self) -> u64 {
        self.logs().map(|s| s.consumed).sum()
    }

    /// Where an entry sits on disk, as (source, offset in that file). Unlike its index, this
    /// stays the same from one snapshot to the next.
    pub fn origin(&self, e: &Entry) -> (u32, u64) {
        let file = &self.files[e.file as usize];
        (file.source, file.base + e.off as u64)
    }

    /// Index of the entry with this time and origin.
    pub fn locate(&self, ts: i64, origin: (u32, u64)) -> Option<usize> {
        let start = self.entries.partition_point(|e| e.ts < ts);
        let mut same_time = self.entries[start..].iter().take_while(|e| e.ts == ts);
        Some(start + same_time.position(|e| self.origin(e) == origin)?)
    }

    pub fn bytes_of(&self, e: &Entry) -> &[u8] {
        &self.files[e.file as usize].data[e.off as usize..(e.off + e.len) as usize]
    }

    /// First line of the entry, after the `[LVL] ` header.
    pub fn message(&self, e: &Entry) -> &[u8] {
        let b = self.bytes_of(e);
        let end = memchr::memchr(b'\n', b).unwrap_or(b.len());
        trim_cr(&b[HEADER_LEN.min(end)..end])
    }

    pub fn extra_lines(&self, e: &Entry) -> usize {
        memchr::memchr_iter(b'\n', self.bytes_of(e)).count()
    }
}

fn trim_cr(l: &[u8]) -> &[u8] {
    l.strip_suffix(b"\r").unwrap_or(l)
}

fn num(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |n, c| c.is_ascii_digit().then(|| n * 10 + (c - b'0') as u32))
}

/// Returns (utc millis, offset millis, level) when the line starts a new entry.
fn parse_header(l: &[u8]) -> Option<(i64, i64, u8)> {
    if l.len() < HEADER_LEN - 1 || l[4] != b'-' || l[10] != b' ' || l[19] != b'.' || l[31] != b'[' || l[35] != b']' {
        return None;
    }
    if l[7] != b'-' || l[13] != b':' || l[16] != b':' || l[23] != b' ' || l[27] != b':' {
        return None;
    }
    let level = LEVELS.iter().position(|n| n.as_bytes() == &l[32..35])? as u8;
    let date = chrono::NaiveDate::from_ymd_opt(num(&l[0..4])? as i32, num(&l[5..7])?, num(&l[8..10])?)?;
    let wall = date
        .and_hms_milli_opt(num(&l[11..13])?, num(&l[14..16])?, num(&l[17..19])?, num(&l[20..23])?)?
        .and_utc()
        .timestamp_millis();
    let sign = match l[24] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let tz = sign * (num(&l[25..27])? as i64 * 3_600_000 + num(&l[28..30])? as i64 * 60_000);
    Some((wall - tz, tz, level))
}

/// Collapses the variable parts of a message so that entries of the same shape share one key.
// ponytail: heuristic template — numbers/ids become `#`, JSON bodies `{…}`. Free-text
// values still split patterns; swap in a Drain-style clusterer if that gets noisy.
fn normalize(msg: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    let start = out.len();
    while i < msg.len() && out.len() - start < PATTERN_CAP {
        let b = msg[i];
        if b.is_ascii_alphanumeric() {
            let s = i;
            while i < msg.len() && msg[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let w = &msg[s..i];
            let digits = w.iter().filter(|c| c.is_ascii_digit()).count();
            let hex_id = digits > 0 && w.len() >= 4 && w.iter().all(u8::is_ascii_hexdigit);
            if digits == w.len() || hex_id || digits >= 4 {
                out.push(b'#');
            } else {
                out.extend_from_slice(w);
            }
        } else if b == b'{' {
            out.extend_from_slice("{…}".as_bytes());
            break;
        } else {
            out.push(b);
            i += 1;
        }
    }
    if let Err(e) = std::str::from_utf8(&out[start..]) {
        out.truncate(start + e.valid_up_to());
    }
}

/// `Namespace.TypeException: message` (optionally `Type (0x80131904): message`) → index of the colon.
fn exception_colon(l: &[u8]) -> Option<usize> {
    let end = l.iter().position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'`')))?;
    if end < 3 || !l[..end].contains(&b'.') {
        return None;
    }
    let mut i = end;
    if l[i..].starts_with(b" (") {
        i += memchr::memchr(b')', &l[i..])? + 1;
    }
    (l.get(i) == Some(&b':') && l.get(i + 1) == Some(&b' ')).then_some(i)
}

/// `   at Shop.Carts.CouponChecker.Validate(String code) in C:\..\CouponChecker.cs:line 118`
/// → `CouponChecker.Validate · CouponChecker.cs:118`
fn throw_site(frames: &[u8]) -> String {
    let lines = || frames.split(|b| *b == b'\n').map(trim_cr).filter(|l| l.starts_with(b"   at "));
    let Some(frame) = lines().find(|l| memchr::memmem::find(l, b") in ").is_some()).or_else(|| lines().next()) else {
        return String::new();
    };
    let frame = String::from_utf8_lossy(&frame[6..]);
    let (call, source) = frame.split_once(") in ").unwrap_or((&frame, ""));
    let method = call.split('(').next().unwrap_or(call);
    let mut parts = method.rsplit('.');
    let name = parts.next().unwrap_or("");
    let mut site = match parts.next() {
        Some(class) => format!("{class}.{name}"),
        None => name.to_string(),
    };
    if let Some(file) = source.rsplit(['\\', '/']).next().filter(|f| !f.is_empty()) {
        site.push_str(" · ");
        site.push_str(&file.replace(":line ", ":"));
    }
    site
}

#[derive(Default)]
struct Parsed {
    entries: Vec<Entry>,
    patterns: Vec<(u8, String)>,
    exceptions: Vec<(String, String)>,
    tz_ms: i64,
}

/// Indexes one read of a file. Offsets are relative to `data`; ids are local to this call.
fn parse(data: &[u8]) -> Parsed {
    let mut out = Parsed::default();
    let mut pat_ids: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut exc_ids: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut key = Vec::with_capacity(256);

    // Pass 1: find entry starts.
    let mut starts: Vec<(usize, i64, u8)> = Vec::new();
    let mut pos = if data.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    while pos < data.len() {
        let end = memchr::memchr(b'\n', &data[pos..]).map_or(data.len(), |i| pos + i);
        if data[pos].is_ascii_digit()
            && let Some((ts, tz, level)) = parse_header(&data[pos..end])
        {
            if starts.is_empty() {
                out.tz_ms = tz;
            }
            starts.push((pos, ts, level));
        }
        pos = end + 1;
    }

    // Pass 2: classify each entry.
    out.entries.reserve(starts.len());
    for (i, &(off, ts, level)) in starts.iter().enumerate() {
        let mut end = starts.get(i + 1).map_or(data.len(), |n| n.0);
        while end > off && matches!(data[end - 1], b'\n' | b'\r' | b' ') {
            end -= 1;
        }
        let body = &data[off..end];
        let first_end = memchr::memchr(b'\n', body).unwrap_or(body.len());
        let msg = trim_cr(&body[HEADER_LEN.min(first_end)..first_end]);

        key.clear();
        key.push(level);
        normalize(msg, &mut key);
        let next = pat_ids.len() as u32;
        let pat = *pat_ids.entry(key.clone()).or_insert_with(|| {
            out.patterns.push((level, String::from_utf8_lossy(&key[1..]).into_owned()));
            next
        });

        // The exception is either the message itself or the first continuation line.
        let rest = body.get(first_end + 1..).unwrap_or(&[]);
        let second = trim_cr(&rest[..memchr::memchr(b'\n', rest).unwrap_or(rest.len())]);
        let exc_line = [msg, second].into_iter().find_map(|l| exception_colon(l).map(|c| (l, c)));
        let exc = match exc_line {
            Some((line, colon)) => {
                let site = throw_site(rest);
                key.clear();
                key.extend_from_slice(&line[..colon + 2]);
                normalize(&line[colon + 2..], &mut key);
                let text_len = key.len();
                key.push(0);
                key.extend_from_slice(site.as_bytes());
                let next = exc_ids.len() as u32;
                *exc_ids.entry(key.clone()).or_insert_with(|| {
                    out.exceptions.push((String::from_utf8_lossy(&key[..text_len]).into_owned(), site));
                    next
                })
            }
            None => NONE,
        };

        out.entries.push(Entry { ts, off: off as u32, len: (end - off) as u32, pat, exc, file: 0, level });
    }
    out
}

fn collect_files(paths: &[PathBuf], depth: usize, out: &mut Vec<PathBuf>) {
    for p in paths {
        if p.is_dir() {
            // Depth cap: a junction loop or a whole drive dropped by mistake must not hang the walk.
            if depth == 6 {
                continue;
            }
            let mut children: Vec<PathBuf> =
                std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect();
            children.sort();
            collect_files(&children, depth + 1, out);
        } else if p.is_file() {
            out.push(p.clone());
        }
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Reads every file under `paths` (folders are walked recursively) and builds the index.
// ponytail: whole files are held in memory (RAM ≈ log size). Switch to memory-mapped
// files if multi-GB folders show up.
pub fn load(paths: &[PathBuf]) -> Store {
    let empty = Store { roots: paths.to_vec(), ..Store::default() };
    advance(&empty).unwrap_or(empty)
}

/// What one read of a file gave: its bytes and their index, or why it is not a log.
type Read = Result<(Vec<u8>, Parsed), String>;

struct Job {
    path: PathBuf,
    source: Option<usize>,
    /// Offset to read from, and the file's size when the job was planned.
    from: u64,
    len: u64,
}

/// Picks up what changed on disk since `old` was built — appended bytes and new files — and
/// returns the next snapshot, or `None` when nothing changed. Loading is the same step taken
/// from an empty store.
// ponytail: polls by listing the roots and comparing sizes, and every step that finds changes
// copies the entry index (32 bytes per entry) and adds one segment per grown file. Fine for
// folders of rolling logs; move to change notifications and in-place appends if it gets heavy.
pub fn advance(old: &Store) -> Option<Store> {
    let mut paths = Vec::new();
    collect_files(&old.roots, 0, &mut paths);
    paths.sort();
    paths.dedup();
    let known: HashMap<&Path, usize> = old.sources.iter().enumerate().map(|(i, s)| (s.path.as_path(), i)).collect();
    let mut jobs = Vec::new();
    for path in paths {
        let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else { continue };
        let source = known.get(path.as_path()).copied();
        let from = match source.map(|i| &old.sources[i]) {
            Some(s) if s.consumed == len => continue,
            // A log that shrank was truncated or replaced: start over.
            Some(s) if s.skipped.is_none() && len < s.consumed => return Some(load(&old.roots)),
            Some(s) if s.skipped.is_none() => s.last.map_or(0, |last| last.1),
            // New, or not a log last time (still empty, say): look at it from the start.
            _ => 0,
        };
        jobs.push(Job { path, source, from, len });
    }
    if jobs.is_empty() {
        return None;
    }

    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, Read)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads().min(jobs.len()))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break done };
                        let read = read_from(&job.path, job.from).map(|data| {
                            let parsed = parse(&data);
                            (data, parsed)
                        });
                        done.push((i, read));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    results.sort_by_key(|r| r.0);

    let mut store = old.clone();
    let mut pat_ids: HashMap<(u8, String), u32> =
        store.patterns.iter().enumerate().map(|(i, p)| ((p.level, p.text.clone()), i as u32)).collect();
    let mut exc_ids: HashMap<(String, String), u32> =
        store.exceptions.iter().enumerate().map(|(i, x)| ((x.text.clone(), x.site.clone()), i as u32)).collect();
    let mut fresh: Vec<Entry> = Vec::new();
    for (job, (_, result)) in jobs.into_iter().zip(results) {
        let source = job.source.unwrap_or_else(|| {
            store.sources.push(Source { path: job.path.clone(), skipped: None, consumed: 0, last: None });
            store.sources.len() - 1
        });
        let (data, parsed) = match result {
            Ok(read) => read,
            Err(reason) => {
                store.sources[source].skipped = Some(reason);
                store.sources[source].consumed = job.len;
                continue;
            }
        };
        let Some(newest) = parsed.entries.last() else { continue };
        if let Some((ts, offset)) = store.sources[source].last {
            // The read restarted at this file's newest entry. If that entry is no longer what
            // sits there, the file was replaced under the same name: start over.
            if !parsed.entries.first().is_some_and(|e| e.off == 0 && e.ts == ts) {
                return Some(load(&old.roots));
            }
            // Otherwise the copy indexed last time goes; the fresh one may be longer.
            if let Some(i) = store.locate(ts, (source as u32, offset)) {
                store.entries.remove(i);
            }
        }
        if store.entries.is_empty() && fresh.is_empty() {
            store.tz_ms = parsed.tz_ms;
        }
        store.sources[source].skipped = None;
        store.sources[source].consumed = job.from + data.len() as u64;
        store.sources[source].last = Some((newest.ts, job.from + newest.off as u64));

        // Per-read ids → global ids.
        let pats: Vec<u32> = parsed
            .patterns
            .into_iter()
            .map(|(level, text)| {
                let next = store.patterns.len() as u32;
                *pat_ids.entry((level, text.clone())).or_insert_with(|| {
                    store.patterns.push(Pattern { level, text });
                    next
                })
            })
            .collect();
        let excs: Vec<u32> = parsed
            .exceptions
            .into_iter()
            .map(|(text, site)| {
                let next = store.exceptions.len() as u32;
                *exc_ids.entry((text.clone(), site.clone())).or_insert_with(|| {
                    store.exceptions.push(Exception { text, site });
                    next
                })
            })
            .collect();
        let file = store.files.len() as u32;
        fresh.extend(parsed.entries.into_iter().map(|mut e| {
            e.file = file;
            e.pat = pats[e.pat as usize];
            if e.exc != NONE {
                e.exc = excs[e.exc as usize];
            }
            e
        }));
        let name = job.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        store.files.push(LogFile { name: name.into(), data: Arc::new(data), source: source as u32, base: job.from });
    }

    let key = |e: &Entry| (e.ts, e.file, e.off);
    fresh.sort_unstable_by_key(key);
    if let Some(first) = fresh.first() {
        // New entries are nearly always the newest, so only the end of the index is re-sorted.
        let at = store.entries.partition_point(|e| key(e) < key(first));
        let mut tail = store.entries.split_off(at);
        tail.append(&mut fresh);
        tail.sort_unstable_by_key(key);
        store.entries.append(&mut tail);
    }
    Some(store)
}

/// Reads a file from `from` to its end. Read from the start, it is first checked for being a log.
fn read_from(path: &Path, from: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len().saturating_sub(from) > u32::MAX as u64 {
        return Err("larger than 4 GB".into());
    }
    let mut data = Vec::new();
    if from == 0 {
        // Sniff the head first, so a folder of big unrelated files is never pulled into memory.
        file.by_ref().take(8192).read_to_end(&mut data).map_err(|e| e.to_string())?;
        let is_log = data
            .split(|b| *b == b'\n')
            .any(|l| parse_header(l.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(l)).is_some());
        if !is_log {
            return Err("not a Serilog text log".into());
        }
    } else {
        file.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
    }
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;
    Ok(data)
}

/// Writes the given entries to `path`: as a table when the name ends in `.csv`, otherwise as
/// log text exactly as it was read, so the result opens in this viewer again.
pub fn export(store: &Store, rows: &[u32], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let csv = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("csv"));
    if csv {
        // The byte-order mark makes Excel read the file as UTF-8.
        out.write_all("\u{FEFF}Time,Level,File,Exception,Thrown at,Message\r\n".as_bytes())?;
    }
    for &row in rows {
        let entry = &store.entries[row as usize];
        let bytes = store.bytes_of(entry);
        if !csv {
            out.write_all(bytes)?;
            out.write_all(b"\r\n")?;
            continue;
        }
        let exception = store.exceptions.get(entry.exc as usize);
        let fields = [
            String::from_utf8_lossy(&bytes[..23]),
            LEVELS[entry.level as usize].into(),
            (&*store.files[entry.file as usize].name).into(),
            exception.map_or("", |x| x.text.as_str()).into(),
            exception.map_or("", |x| x.site.as_str()).into(),
            String::from_utf8_lossy(store.message(entry)),
        ];
        for (i, field) in fields.iter().enumerate() {
            // A leading apostrophe keeps spreadsheets from running a cell that looks like a formula.
            let guard = if field.starts_with(['=', '+', '-', '@', '\t', '\r']) { "'" } else { "" };
            write!(out, "{}\"{guard}{}\"", if i > 0 { "," } else { "" }, field.replace('"', "\"\""))?;
        }
        out.write_all(b"\r\n")?;
    }
    out.flush()
}

#[derive(Clone)]
pub struct Filter {
    /// Bit `i` enables `LEVELS[i]`.
    pub levels: u8,
    pub query: Option<regex::bytes::Regex>,
    /// `[from, to)` in UTC milliseconds.
    pub range: Option<(i64, i64)>,
    pub pat: Option<u32>,
    pub exc: Option<u32>,
}

impl Default for Filter {
    fn default() -> Self {
        Self { levels: ALL_LEVELS, query: None, range: None, pat: None, exc: None }
    }
}

#[derive(Default)]
pub struct View {
    /// Indices into `Store::entries` that pass every filter, in time order.
    pub rows: Vec<u32>,
    /// Counts per level, ignoring the level filter itself (so toggles keep their numbers).
    pub level_counts: [u32; 6],
    /// (pattern id, count) by count, ignoring the pattern filter itself.
    pub patterns: Vec<(u32, u32)>,
    /// (exception id, count) by count, ignoring the exception filter itself.
    pub exceptions: Vec<(u32, u32)>,
    pub hist: Histogram,
}

#[derive(Default)]
pub struct Histogram {
    /// UTC millis of the first bucket's start.
    pub start: i64,
    pub step: i64,
    /// [errors (ERR+FTL), warnings, everything else]
    pub buckets: Vec<[u32; 3]>,
    pub max: u32,
}

const STEPS: [i64; 20] =
    [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800];
const MAX_BUCKETS: i64 = 120;

struct Part {
    rows: Vec<u32>,
    levels: [u32; 6],
    pats: Vec<u32>,
    excs: Vec<u32>,
}

/// Applies the filter and computes every statistic the UI shows.
pub fn compute(store: &Store, f: &Filter) -> View {
    let all = &store.entries;
    // Entries are time-sorted, so the range filter is two binary searches.
    let (lo, hi) = match f.range {
        Some((from, to)) => (all.partition_point(|e| e.ts < from), all.partition_point(|e| e.ts < to)),
        None => (0, all.len()),
    };
    let slice = &all[lo..hi];
    let chunk = slice.len().div_ceil(threads()).max(1);

    let parts: Vec<Part> = std::thread::scope(|s| {
        let workers: Vec<_> = slice
            .chunks(chunk)
            .enumerate()
            .map(|(c, entries)| {
                s.spawn(move || {
                    let mut part = Part {
                        rows: Vec::new(),
                        levels: [0; 6],
                        pats: vec![0; store.patterns.len()],
                        excs: vec![0; store.exceptions.len()],
                    };
                    let base = lo + c * chunk;
                    for (i, e) in entries.iter().enumerate() {
                        let level = f.levels & (1 << e.level) != 0;
                        let pat = f.pat.is_none_or(|p| p == e.pat);
                        let exc = f.exc.is_none_or(|x| x == e.exc);
                        // Each facet needs every *other* filter to pass.
                        if (level as u8 + pat as u8 + exc as u8) < 2 {
                            continue;
                        }
                        if f.query.as_ref().is_some_and(|q| !q.is_match(store.bytes_of(e))) {
                            continue;
                        }
                        if pat && exc {
                            part.levels[e.level as usize] += 1;
                        }
                        if level && exc {
                            part.pats[e.pat as usize] += 1;
                        }
                        if level && pat && e.exc != NONE {
                            part.excs[e.exc as usize] += 1;
                        }
                        if level && pat && exc {
                            part.rows.push((base + i) as u32);
                        }
                    }
                    part
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    let mut view = View::default();
    let mut pats = vec![0u32; store.patterns.len()];
    let mut excs = vec![0u32; store.exceptions.len()];
    for part in parts {
        view.rows.extend(part.rows);
        for i in 0..6 {
            view.level_counts[i] += part.levels[i];
        }
        pats.iter_mut().zip(part.pats).for_each(|(a, b)| *a += b);
        excs.iter_mut().zip(part.excs).for_each(|(a, b)| *a += b);
    }
    view.patterns = ranked(pats);
    view.exceptions = ranked(excs);
    view.hist = histogram(store, &view.rows, f.range);
    view
}

fn ranked(counts: Vec<u32>) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> =
        counts.into_iter().enumerate().filter(|c| c.1 > 0).map(|(i, c)| (i as u32, c)).collect();
    v.sort_unstable_by_key(|&(id, count)| (std::cmp::Reverse(count), id));
    v
}

fn histogram(store: &Store, rows: &[u32], range: Option<(i64, i64)>) -> Histogram {
    let (Some(first), Some(last)) = (store.entries.first(), store.entries.last()) else {
        return Histogram::default();
    };
    let (from, to) = range.unwrap_or((first.ts, last.ts + 1));
    let span = (to - from).max(1);
    let step = STEPS.iter().map(|s| s * 1000).find(|s| span / s < MAX_BUCKETS).unwrap_or(STEPS[19] * 1000);
    // Align buckets to the log's own clock so hourly bars start on the hour.
    let start = (from + store.tz_ms).div_euclid(step) * step - store.tz_ms;
    let count = ((to - 1 - start) / step + 1).max(1) as usize;
    let mut buckets = vec![[0u32; 3]; count];
    for &r in rows {
        let e = &store.entries[r as usize];
        let series = match e.level {
            4 | 5 => 0,
            3 => 1,
            _ => 2,
        };
        buckets[(((e.ts - start) / step) as usize).min(count - 1)][series] += 1;
    }
    let max = buckets.iter().map(|b| b.iter().sum()).max().unwrap_or(0);
    Histogram { start, step, buckets, max }
}

/// Regex source for a plain-text search. Non-ASCII characters also match their JSON-escaped
/// spelling, because Arabic in a logged request body is written as backslash-u sequences while
/// the same words in an exception message are raw UTF-8.
pub fn literal_query(text: &str) -> String {
    use std::fmt::Write as _;
    let mut pattern = String::new();
    for c in text.chars() {
        let raw = regex::escape(c.encode_utf8(&mut [0; 4]));
        if c.is_ascii() {
            pattern.push_str(&raw);
        } else {
            let escaped: String = c.encode_utf16(&mut [0; 2]).iter().map(|unit| format!(r"\\u{unit:04x}")).collect();
            _ = write!(pattern, "(?:{raw}|{escaped})");
        }
    }
    pattern
}

/// Pretty-prints every JSON object (or array of objects) embedded in an entry, such as a logged
/// request body. Keys keep their order and numbers their exact text; `\uXXXX`
/// escapes become the characters they stand for.
pub fn pretty_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut rest = text;
    while let Some(start) = rest.find(['{', '[']) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // serde_json only validates and finds where the value ends; the text is re-indented as is.
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
        let worth_it = match values.next() {
            Some(Ok(serde_json::Value::Object(map))) => !map.is_empty(),
            Some(Ok(serde_json::Value::Array(items))) => items.first().is_some_and(|v| v.is_object() || v.is_array()),
            _ => false,
        };
        let len = if worth_it { values.byte_offset() } else { 1 };
        if worth_it {
            reindent(&rest[..len], &mut out);
        } else {
            out.push_str(&rest[..len]);
        }
        rest = &rest[len..];
    }
    out.push_str(rest);
    out
}

/// Re-indents JSON that is already known to be valid.
fn reindent(json: &str, out: &mut String) {
    use std::fmt::Write as _;
    type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;
    fn newline(out: &mut String, depth: usize) {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    }
    fn hex4(chars: &mut Chars) -> u32 {
        (0..4).filter_map(|_| chars.next()?.to_digit(16)).fold(0, |n, digit| n * 16 + digit)
    }
    let mut depth = 0usize;
    let mut chars: Chars = json.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('u') => {
                                let mut code = hex4(&mut chars);
                                if (0xD800..0xDC00).contains(&code) {
                                    // Valid JSON always follows a high surrogate with `\uDC00..DFFF`.
                                    chars.next();
                                    chars.next();
                                    code = 0x10000 + ((code - 0xD800) << 10) + hex4(&mut chars).wrapping_sub(0xDC00);
                                }
                                match char::from_u32(code) {
                                    Some('"') => out.push_str("\\\""),
                                    Some('\\') => out.push_str("\\\\"),
                                    Some(decoded) if !decoded.is_control() => out.push(decoded),
                                    _ => _ = write!(out, "\\u{code:04x}"),
                                }
                            }
                            Some(escaped) => out.extend(['\\', escaped]),
                            None => {}
                        },
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
                match chars.next_if(|c| matches!(c, '}' | ']')) {
                    Some(close) => out.push(close),
                    None => {
                        depth += 1;
                        newline(out, depth);
                    }
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
}

/// `1234567` → `1,234,567`
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats UTC millis on the log's own clock.
pub fn format_time(utc_ms: i64, tz_ms: i64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp_millis(utc_ms + tz_ms).map_or_else(String::new, |t| t.format(fmt).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder under the system temp directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("loglens-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn append(&self, file: &str, text: &str) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.0.join(file)).unwrap();
            f.write_all(text.as_bytes()).unwrap();
        }

        fn size(&self, file: &str) -> u64 {
            std::fs::metadata(self.0.join(file)).unwrap().len()
        }

        fn load(&self) -> Store {
            load(std::slice::from_ref(&self.0))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn messages(store: &Store) -> Vec<String> {
        store.entries.iter().map(|e| String::from_utf8_lossy(store.message(e)).into_owned()).collect()
    }

    const SAMPLE: &str = "\
2026-03-14 09:00:01.311 +02:00 [ERR] Unhandled exception for POST /v1/coupons/validate\r
Shop.RateLimitedException: Too many attempts, try again in 42 seconds.\r
   at Shop.Carts.CouponChecker.Validate(String code, Nullable`1 storeId) in C:\\build\\src\\Shop.Carts\\CouponChecker.cs:line 118\r
   at lambda_method4821(Closure, Object)\r
2026-03-14 09:00:05.099 +02:00 [INF] Order placed | correlation=0f3c9a7e5b2d4c18a6e1d2f3b4c5a697 | customer=1001 | order=700021\r
2026-03-14 09:00:06.442 +02:00 [INF] Loaded the basket list for courier 7\r
2026-03-14 10:30:00.000 +02:00 [INF] Order placed | correlation=aaaa9a7e5b2d4c18a6e1d2f3b4c5a697 | customer=7 | order=1\r
2026-03-14 10:30:00.500 +02:00 [WRN] Cart request body: {\"id\":\"1\",\"productId\":2562022}\r
2026-03-14 10:31:00.000 +02:00 [ERR] Unhandled exception for POST /v1/coupons/validate\r
Shop.RateLimitedException: Too many attempts, try again in 7 seconds.\r
   at Shop.Carts.CouponChecker.Validate(String code, Nullable`1 storeId) in C:\\build\\src\\Shop.Carts\\CouponChecker.cs:line 118\r
";

    #[test]
    fn parses_groups_and_filters() {
        let dir = Scratch::new("parse");
        dir.append("api20260314.log", SAMPLE);
        dir.append("notes.txt", "not a log");
        let s = dir.load();
        assert_eq!(s.entries.len(), 6);
        assert_eq!((s.logs().count(), s.skipped().count(), s.bytes()), (1, 1, SAMPLE.len() as u64));
        assert_eq!(s.tz_ms, 2 * 3_600_000);
        // 09:00:01.311 +02:00 is 07:00:01.311 UTC.
        assert_eq!(format_time(s.entries[0].ts, 0, "%Y-%m-%d %H:%M:%S%.3f"), "2026-03-14 07:00:01.311");
        assert_eq!(s.message(&s.entries[0]), b"Unhandled exception for POST /v1/coupons/validate");
        assert_eq!(s.extra_lines(&s.entries[0]), 3);

        // Same shape → same pattern, ids and numbers collapsed, JSON body cut.
        assert_eq!(s.entries[1].pat, s.entries[3].pat);
        assert_eq!(s.patterns[s.entries[1].pat as usize].text, "Order placed | correlation=# | customer=# | order=#");
        assert_eq!(s.patterns[s.entries[2].pat as usize].text, "Loaded the basket list for courier #");
        assert_eq!(s.patterns[s.entries[4].pat as usize].text, "Cart request body: {…}");

        // Both errors are one exception group with its throw site.
        assert_eq!(s.exceptions.len(), 1);
        assert_eq!(s.entries[0].exc, s.entries[5].exc);
        assert_eq!(s.exceptions[0].text, "Shop.RateLimitedException: Too many attempts, try again in # seconds.");
        assert_eq!(s.exceptions[0].site, "CouponChecker.Validate · CouponChecker.cs:118");
        assert_eq!(s.entries[1].exc, NONE);

        let all = compute(&s, &Filter::default());
        assert_eq!(all.rows.len(), 6);
        assert_eq!(all.level_counts, [0, 0, 3, 1, 2, 0]);
        assert_eq!(all.patterns[0].1, 2);
        assert_eq!(all.exceptions, vec![(0, 2)]);
        assert_eq!(all.hist.buckets.iter().flatten().sum::<u32>(), 6);

        // Level filter narrows the rows but level counts keep the other levels visible.
        let errors = compute(&s, &Filter { levels: 1 << 4, ..Default::default() });
        assert_eq!(errors.rows, vec![0, 5]);
        assert_eq!(errors.level_counts, [0, 0, 3, 1, 2, 0]);

        // Search looks at the whole entry, including the stack trace.
        let q = regex::bytes::RegexBuilder::new("couponchecker").case_insensitive(true).build().unwrap();
        assert_eq!(compute(&s, &Filter { query: Some(q), ..Default::default() }).rows, vec![0, 5]);

        // Time range is [from, to).
        let from = s.entries[3].ts;
        let late = compute(&s, &Filter { range: Some((from, from + 60_000)), ..Default::default() });
        assert_eq!(late.rows, vec![3, 4]);
        assert_eq!(late.hist.buckets.iter().flatten().sum::<u32>(), 2);

        assert_eq!(commas(1234567), "1,234,567");
        assert_eq!(commas(999), "999");
    }

    #[test]
    fn follows_growing_and_new_files() {
        let dir = Scratch::new("tail");
        // The writer is caught halfway through a line.
        dir.append(
            "api.log",
            "2026-03-14 09:00:00.000 +02:00 [INF] started\r\n2026-03-14 09:00:02.000 +02:00 [ERR] request fai",
        );
        dir.append("empty.log", "");
        let first = dir.load();
        assert_eq!(messages(&first), ["started", "request fai"]);
        assert_eq!((first.logs().count(), first.skipped().count()), (1, 1));
        assert!(advance(&first).is_none(), "nothing changed on disk");

        // It finishes the line, adds the stack trace and one more entry. A second instance starts
        // its own file with an entry that belongs between the first two, and the empty file fills.
        dir.append("api.log", "led\r\nShop.ValidationException: nope\r\n   at Shop.Api.Handle() in C:\\src\\Api.cs:line 7\r\n2026-03-14 09:00:03.000 +02:00 [INF] done\r\n");
        dir.append("api_001.log", "2026-03-14 09:00:01.000 +02:00 [WRN] slow\r\n");
        dir.append("empty.log", "2026-03-14 09:00:04.000 +02:00 [INF] late\r\n");
        let second = advance(&first).expect("new data");
        assert_eq!(messages(&second), ["started", "slow", "request failed", "done", "late"]);
        assert_eq!(second.extra_lines(&second.entries[2]), 2, "the grown entry is whole, and there once");
        assert_eq!(second.exceptions[second.entries[2].exc as usize].site, "Api.Handle · Api.cs:7");
        assert_eq!((second.logs().count(), second.skipped().count()), (3, 0));
        assert_eq!(second.bytes(), dir.size("api.log") + dir.size("api_001.log") + dir.size("empty.log"));
        // An entry keeps its origin while its index moves.
        assert_eq!(second.locate(first.entries[1].ts, first.origin(&first.entries[1])), Some(2));
        assert!(advance(&second).is_none());

        // A file that shrinks was replaced: everything is read again.
        std::fs::write(dir.0.join("api_001.log"), "2026-03-14 09:00:09.000 +02:00 [WRN] x\r\n").unwrap();
        let third = advance(&second).expect("reload");
        assert_eq!(messages(&third), ["started", "request failed", "done", "late", "x"]);
    }

    #[test]
    fn exports_log_text_and_csv() {
        let dir = Scratch::new("export-source");
        dir.append("api.log", SAMPLE);
        dir.append("api.log", "2026-03-14 11:00:00.000 +02:00 [INF] =1+1 \"quoted\"\r\n");
        let s = dir.load();
        let all: Vec<u32> = (0..s.entries.len() as u32).collect();

        // Log text opens in the viewer again, entry for entry.
        let out = Scratch::new("export-target");
        export(&s, &all, &out.0.join("out.log")).unwrap();
        let again = out.load();
        assert_eq!(messages(&again), messages(&s));
        assert_eq!(again.extra_lines(&again.entries[0]), 3);

        // CSV: one row per entry, quotes doubled, formula-looking cells defused.
        export(&s, &[0, 6], &out.0.join("out.csv")).unwrap();
        let csv = std::fs::read_to_string(out.0.join("out.csv")).unwrap();
        let rows: Vec<&str> = csv.lines().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].ends_with("Time,Level,File,Exception,Thrown at,Message"));
        assert_eq!(
            rows[1],
            "\"2026-03-14 09:00:01.311\",\"ERR\",\"api.log\",\"Shop.RateLimitedException: Too many attempts, try again in # seconds.\",\"CouponChecker.Validate · CouponChecker.cs:118\",\"Unhandled exception for POST /v1/coupons/validate\""
        );
        assert_eq!(rows[2], "\"2026-03-14 11:00:00.000\",\"INF\",\"api.log\",\"\",\"\",\"'=1+1 \"\"quoted\"\"\"");
    }

    #[test]
    fn pretty_prints_embedded_json() {
        // The note is escaped the way System.Text.Json writes it: Arabic letters, quotes, a
        // surrogate pair and a line feed, each as a backslash-u sequence (built here in code).
        let u = |code: u32| format!("{}u{code:04X}", '\\');
        let note = [0x627, 0x647, 0x644, 0x627, 0x20, 0x22, 0x78, 0x22, 0x20, 0xD83D, 0xDE00, 0xA].map(u).concat();
        let line = format!(
            r#"Cart request body: {{"id":"1","qty":2,"lat":31.89500,"note":"{note}","members":{{"values":[12951,17999]}},"empty":{{}},"list":[ ]}} [ERR] tail Object[] x"#
        );
        let expected = "Cart request body: {\n  \"id\": \"1\",\n  \"qty\": 2,\n  \"lat\": 31.89500,\n  \"note\": \"اهلا \\\"x\\\" 😀\\u000a\",\n  \"members\": {\n    \"values\": [\n      12951,\n      17999\n    ]\n  },\n  \"empty\": {},\n  \"list\": []\n} [ERR] tail Object[] x";
        assert_eq!(pretty_json(&line), expected);
        // Already formatted JSON comes out the same way.
        assert_eq!(pretty_json(expected), expected);
        // Not JSON, or not worth expanding: untouched.
        let plain = "   at Foo.Bar(Object[] arguments) {PII hidden} [1, 2] {} {\"cut\":";
        assert_eq!(pretty_json(plain), plain);

        // A plain-text search finds the words whether the log holds them raw or escaped.
        let query = regex::RegexBuilder::new(&literal_query("بدون (x)")).case_insensitive(true).build().unwrap();
        let escaped = format!("\"note\":\"{} (X)\"", [0x628, 0x62F, 0x648, 0x646].map(u).concat());
        assert!(query.is_match("Shop.ValidationException: بدون (x)"));
        assert!(query.is_match(&escaped));
        assert!(!query.is_match("بدون x"));
    }

    /// Runs the whole pipeline over a real folder and prints what it finds:
    /// `LOGLENS_DIR=<folder> cargo test --release real_logs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_logs() {
        let dir = std::env::var("LOGLENS_DIR").expect("set LOGLENS_DIR");
        let t = std::time::Instant::now();
        let s = load(&[PathBuf::from(dir)]);
        println!(
            "load: {:?} — {} entries, {} files, {} MB, {} patterns, {} exceptions, {} skipped",
            t.elapsed(),
            commas(s.entries.len() as u64),
            s.logs().count(),
            s.bytes() / 1_048_576,
            s.patterns.len(),
            s.exceptions.len(),
            s.skipped().count()
        );
        let t = std::time::Instant::now();
        assert!(advance(&s).is_none());
        println!("advance (nothing new): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let v = compute(&s, &Filter::default());
        println!(
            "compute (no query): {:?} — levels {:?}, {} buckets of {} s",
            t.elapsed(),
            v.level_counts,
            v.hist.buckets.len(),
            v.hist.step / 1000
        );
        for (id, n) in v.patterns.iter().take(25) {
            let p = &s.patterns[*id as usize];
            println!("{n:>8}  {} {}", LEVELS[p.level as usize], p.text);
        }
        println!("…singletons: {}", v.patterns.iter().filter(|p| p.1 == 1).count());
        for (id, n) in v.exceptions.iter().take(15) {
            let x = &s.exceptions[*id as usize];
            println!("{n:>8}  {}  @ {}", x.text, x.site);
        }
        let q = regex::bytes::RegexBuilder::new("not found").case_insensitive(true).build().unwrap();
        let t = std::time::Instant::now();
        let v = compute(&s, &Filter { query: Some(q), ..Default::default() });
        println!("compute (query): {:?} — {} rows", t.elapsed(), v.rows.len());
        // The formatter must survive whatever real entries contain.
        let t = std::time::Instant::now();
        let sample = s.entries.iter().step_by(25).map(|e| String::from_utf8_lossy(s.bytes_of(e)));
        let expanded = sample.filter(|text| pretty_json(text) != **text).count();
        println!("pretty_json over every 25th entry: {:?} — {expanded} expanded", t.elapsed());
        // The cost of one live-tail step that finds new data: copy the index, then re-filter.
        let t = std::time::Instant::now();
        let copy = s.clone();
        println!("snapshot copy: {:?} ({} segments)", t.elapsed(), copy.files.len());
    }
}
