//! The window: toolbar, timeline, pattern and exception lists, entry list and detail pane.
//! Loading, filtering, tailing and exporting run on background threads; the UI only ever swaps
//! in a finished `Store` or `View`.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, Theme, ThemeMode, TitleBar, h_flex, h_resizable,
    resizable_panel, v_flex, v_resizable,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::logs::{self, Filter, LEVELS, NONE, Store, View, commas, format_time};

const ROW_H: f32 = 24.;
const SIDE_ROW_H: f32 = 28.;
const EXC_ROW_H: f32 = 46.;
const TAIL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Patterns,
    Exceptions,
}

pub struct LogLens {
    store: Arc<Store>,
    view: Arc<View>,
    filter: Filter,
    /// Levels that occur in the loaded logs; only those get a toggle.
    present: u8,
    regex_mode: bool,
    /// Show JSON bodies in the detail pane indented, with `\uXXXX` escapes decoded.
    pretty: bool,
    /// Shown in the status bar: an invalid search pattern or a failed export.
    error: Option<SharedString>,
    /// Index into `store.entries`.
    selected: Option<u32>,
    /// What was opened, for the title bar, and the last thing that happened, for the status bar.
    source: SharedString,
    status: SharedString,
    loading: bool,
    tab: Tab,
    /// Timeline bucket under the mouse, and the one a drag started on.
    hover: Option<usize>,
    drag: Option<usize>,
    search: Entity<InputState>,
    detail: Entity<TextareaState>,
    rows_scroll: UniformListScrollHandle,
    side_scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
    /// Bumped whenever the filter changes, so results computed for an older one are dropped.
    generation: u64,
    load_task: Option<Task<()>>,
    filter_task: Option<Task<()>>,
    /// Running while live tail is on.
    tail_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn level_color(level: u8, cx: &App) -> Hsla {
    let t = cx.theme();
    // Indexes into `LEVELS`: ERR and FTL, WRN, INF, then DBG and VRB.
    match level {
        4 | 5 => t.danger,
        3 => t.warning,
        2 => t.info,
        _ => t.muted_foreground,
    }
}

/// Lossy text of at most `max` bytes, cut on a character boundary.
fn clip(bytes: &[u8], max: usize) -> String {
    let mut end = bytes.len().min(max);
    while end > 0 && end < bytes.len() && bytes[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// GPUI's Windows text layout places right-to-left runs left-to-right, which mirrors Arabic.
/// Lines with RTL text are therefore shaped and reordered here, then pinned with a
/// left-to-right override so DirectWrite draws them exactly as given.
// ponytail: display-only workaround (text selected in the detail pane copies in visual
// order; the Copy button copies it in reading order). Drop it once GPUI handles bidi on Windows.
fn visual(text: &str) -> String {
    let rtl = |c: char| matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFC}');
    let lines = text.split('\n').map(|line| {
        if !line.chars().any(rtl) {
            return line.to_string();
        }
        let shaped = ar_reshaper::reshape_line(line);
        let bidi = unicode_bidi::ParagraphBidiInfo::new(&shaped, None);
        format!("\u{202D}{}", bidi.reorder_line(0..shaped.len()))
    });
    lines.collect::<Vec<_>>().join("\n")
}

fn step_label(step_ms: i64) -> String {
    match step_ms / 1000 {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86400),
    }
}

fn chip(
    id: &'static str,
    label: String,
    cx: &App,
    on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = cx.theme();
    h_flex()
        .id(id)
        .flex_none()
        .max_w(px(560.))
        .h(px(24.))
        .pl_2()
        .gap_1()
        .rounded(t.radius)
        .bg(t.secondary)
        .text_xs()
        .child(div().min_w_0().truncate().child(visual(&label)))
        .child(Button::new("remove").xsmall().ghost().icon(IconName::Close).on_click(on_remove))
}

impl LogLens {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search every entry, stack traces included…  (Ctrl+F)"));
        let detail = cx.new(|cx| TextareaState::new(window, cx));
        let subscriptions = vec![cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(true, cx);
            }
        })];
        let mut this = Self {
            store: Arc::default(),
            view: Arc::default(),
            filter: Filter::default(),
            present: 0,
            regex_mode: false,
            pretty: true,
            error: None,
            selected: None,
            source: SharedString::default(),
            status: SharedString::default(),
            loading: false,
            tab: Tab::Patterns,
            hover: None,
            drag: None,
            search,
            detail,
            rows_scroll: UniformListScrollHandle::new(),
            side_scroll: UniformListScrollHandle::new(),
            list_focus: cx.focus_handle(),
            generation: 0,
            load_task: None,
            filter_task: None,
            tail_task: None,
            _subscriptions: subscriptions,
        };
        if !paths.is_empty() {
            this.open(paths, cx);
        }
        this
    }

    fn open(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.loading = true;
        self.status = "Loading…".into();
        self.source = match paths.as_slice() {
            [one] => one.to_string_lossy().into_owned().into(),
            many => format!("{} items", many.len()).into(),
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let (store, present) = cx
                .background_spawn(async move {
                    let store = logs::load(&paths);
                    let present = store.entries.iter().fold(0u8, |mask, e| mask | 1 << e.level);
                    (store, present)
                })
                .await;
            this.update(cx, |this, cx| {
                this.status = format!("Loaded in {:.2} s", started.elapsed().as_secs_f32()).into();
                this.loading = false;
                this.store = Arc::new(store);
                this.present = present;
                this.view = Arc::default();
                this.selected = None;
                this.filter = Filter { levels: this.filter.levels, ..Filter::default() };
                this.rows_scroll.scroll_to_item(0, ScrollStrategy::Top);
                this.refilter(false, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_folder(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open log folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| this.open(paths, cx)).ok();
            }
        })
        .detach();
    }

    /// Live tail: every second, pick up what was appended or created under the opened paths.
    fn toggle_tail(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if self.tail_task.take().is_some() {
            return;
        }
        self.rows_scroll.scroll_to_bottom();
        self.tail_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TAIL_INTERVAL).await;
                let snapshot = this.update(cx, |this, _| {
                    (!this.loading).then(|| (this.store.clone(), this.filter.clone(), this.generation, this.selected))
                });
                let Ok(snapshot) = snapshot else { break };
                let Some((base, filter, generation, selected)) = snapshot else { continue };
                let step = cx
                    .background_spawn(async move {
                        let next = logs::advance(&base)?;
                        // The selection follows its entry to wherever it sits in the new snapshot.
                        let selected = selected.and_then(|ix| {
                            let entry = base.entries.get(ix as usize)?;
                            Some(next.locate(entry.ts, base.origin(entry))? as u32)
                        });
                        let present = next.entries.iter().fold(0u8, |mask, e| mask | 1 << e.level);
                        let view = logs::compute(&next, &filter);
                        Some((base, next, view, selected, present))
                    })
                    .await;
                let Some((base, next, view, selected, present)) = step else { continue };
                let applied = this.update(cx, |this, cx| {
                    // Built from data or a filter that has changed since: the next round redoes it.
                    if !Arc::ptr_eq(&this.store, &base) || this.generation != generation {
                        return;
                    }
                    // Keep showing the newest entries unless the user has scrolled away from them.
                    let follow = this.rows_scroll.is_scrolled_to_end() != Some(false);
                    this.store = Arc::new(next);
                    this.view = Arc::new(view);
                    this.selected = selected;
                    this.present = present;
                    if follow {
                        this.rows_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        }));
    }

    /// Saves the entries currently shown: log text as it was read, or a table for a `.csv` name.
    fn export(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.view.rows.is_empty() {
            return;
        }
        // Offer Documents rather than the log folder, where live tail would pick the file up.
        let home = std::env::home_dir().unwrap_or_default();
        let documents = home.join("Documents");
        let folder = if documents.is_dir() { documents } else { home };
        let picked = cx.prompt_for_new_path(&folder, Some("loglens-export.log"));
        let (store, view) = (self.store.clone(), self.view.clone());
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else { return };
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let count = commas(view.rows.len() as u64);
            let result = cx.background_spawn(async move { logs::export(&store, &view.rows, &path) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.status = format!("Exported {count} entries to {name}").into(),
                    Err(error) => this.error = Some(format!("Export failed: {error}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Recomputes the view for the current filter off the UI thread.
    fn refilter(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let text = self.search.read(cx).value();
        self.generation += 1;
        self.error = None;
        self.filter.query = None;
        if !text.is_empty() {
            let pattern = if self.regex_mode { text.to_string() } else { logs::literal_query(&text) };
            match regex::bytes::RegexBuilder::new(&pattern).case_insensitive(true).build() {
                Ok(regex) => self.filter.query = Some(regex),
                Err(_) => {
                    self.error = Some("Invalid regular expression".into());
                    cx.notify();
                    return;
                }
            }
        }
        let (store, filter) = (self.store.clone(), self.filter.clone());
        // Replacing the task drops a computation that is still waiting on its debounce.
        self.filter_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(Duration::from_millis(140)).await;
            }
            let (view, store) = cx.background_spawn(async move { (logs::compute(&store, &filter), store) }).await;
            this.update(cx, |this, cx| {
                // Live tail swapped in newer data meanwhile, along with a view that already fits it.
                if !Arc::ptr_eq(&this.store, &store) {
                    return;
                }
                this.view = Arc::new(view);
                this.hover = None;
                this.drag = None;
                // Keep the selected entry in sight when it survived the filter, drop it otherwise.
                match this.selected.and_then(|ix| this.view.rows.binary_search(&ix).ok()) {
                    Some(row) => this.rows_scroll.scroll_to_item(row, ScrollStrategy::Center),
                    None => {
                        this.selected = None;
                        this.rows_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&ix) = self.view.rows.get(row) else { return };
        self.selected = Some(ix);
        self.show_detail(window, cx);
        self.rows_scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// The selected entry as the detail pane presents it (and as Copy hands it over).
    fn detail_text(&self) -> Option<String> {
        let entry = self.store.entries.get(self.selected? as usize)?;
        let text = String::from_utf8_lossy(self.store.bytes_of(entry)).replace("\r\n", "\n");
        Some(if self.pretty { logs::pretty_json(&text) } else { text })
    }

    fn show_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.detail_text() {
            self.detail.update(cx, |detail, cx| detail.set_value(visual(&text), window, cx));
        }
    }

    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.view.rows.len();
        if count == 0 {
            return;
        }
        let current = self.selected.and_then(|ix| self.view.rows.binary_search(&ix).ok());
        let page = 20;
        let row = match (event.keystroke.key.as_str(), current) {
            ("home", _) => 0,
            ("end", _) => count - 1,
            ("down", Some(row)) => (row + 1).min(count - 1),
            ("up", Some(row)) => row.saturating_sub(1),
            ("pagedown", Some(row)) => (row + page).min(count - 1),
            ("pageup", Some(row)) => row.saturating_sub(page),
            ("down" | "up" | "pagedown" | "pageup", None) => 0,
            _ => return,
        };
        self.select(row, window, cx);
        cx.stop_propagation();
    }

    fn on_root_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.control && event.keystroke.key == "f" {
            self.search.update(cx, |search, cx| search.focus(window, cx));
            cx.stop_propagation();
        }
    }

    fn bucket_range(&self, a: usize, b: usize) -> (i64, i64) {
        let hist = &self.view.hist;
        (hist.start + a.min(b) as i64 * hist.step, hist.start + (a.max(b) as i64 + 1) * hist.step)
    }

    fn copy_selected(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.detail_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    // ---- rendering -------------------------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let dark = t.mode.is_dark();
        let live = self.tail_task.is_some();
        TitleBar::new()
            .child(
                h_flex().gap_3().min_w_0().child(div().font_weight(FontWeight::SEMIBOLD).child("LogLens")).child(
                    div().min_w_0().truncate().text_xs().text_color(t.muted_foreground).child(self.source.clone()),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .pr_2()
                    // Keep clicks on the buttons from starting a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when(!self.store.roots.is_empty(), |buttons| {
                        buttons
                            .child(
                                Button::new("tail")
                                    .small()
                                    .ghost()
                                    .icon(if live { IconName::Pause } else { IconName::Play })
                                    .label(if live { "Live" } else { "Live tail" })
                                    .selected(live)
                                    .tooltip("Keep reading the opened files and folders for new entries")
                                    .on_click(cx.listener(Self::toggle_tail)),
                            )
                            .child(
                                Button::new("export")
                                    .small()
                                    .ghost()
                                    .icon(IconName::ExternalLink)
                                    .label("Export")
                                    .tooltip("Save the entries shown: log text, or a table if the name ends in .csv")
                                    .on_click(cx.listener(Self::export)),
                            )
                    })
                    .child(
                        Button::new("open")
                            .small()
                            .ghost()
                            .icon(IconName::FolderOpen)
                            .label("Open folder")
                            .on_click(cx.listener(Self::pick_folder)),
                    )
                    .child(
                        Button::new("theme")
                            .small()
                            .ghost()
                            .icon(if dark { IconName::Sun } else { IconName::Moon })
                            .on_click(move |_, _, cx| {
                                Theme::change(if dark { ThemeMode::Light } else { ThemeMode::Dark }, None, cx)
                            }),
                    ),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let chips = (0..LEVELS.len()).rev().filter(|l| self.present & (1 << l) != 0).map(|level| {
            let on = self.filter.levels & (1 << level) != 0;
            let color = level_color(level as u8, cx);
            h_flex()
                .id(("level", level))
                .flex_none()
                .h(px(30.))
                .px_2p5()
                .gap_1p5()
                .rounded(t.radius)
                .border_1()
                .border_color(if on { color.opacity(0.55) } else { t.border })
                .when(on, |chip| chip.bg(color.opacity(0.12)))
                .when(!on, |chip| chip.opacity(0.5))
                .cursor_pointer()
                .text_xs()
                .child(div().size(px(8.)).rounded_full().bg(color))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(LEVELS[level]))
                .child(div().text_color(t.muted_foreground).child(commas(self.view.level_counts[level] as u64)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter.levels ^= 1 << level;
                    this.refilter(false, cx);
                }))
        });
        h_flex()
            .flex_none()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small().text_color(t.muted_foreground))
                        .suffix(
                            Button::new("regex")
                                .xsmall()
                                .ghost()
                                .label(".*")
                                .selected(self.regex_mode)
                                .tooltip("Treat the search as a regular expression")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.regex_mode = !this.regex_mode;
                                    this.refilter(false, cx);
                                })),
                        ),
                ),
            )
            .children(chips)
    }

    /// Removable chips for the time range, pattern and exception filters.
    fn render_active_filters(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let f = &self.filter;
        if f.range.is_none() && f.pat.is_none() && f.exc.is_none() {
            return None;
        }
        let tz = self.store.tz_ms;
        let range = f.range.map(|(from, to)| {
            let label = format!(
                "Time  {} → {}",
                format_time(from, tz, "%m-%d %H:%M:%S"),
                format_time(to, tz, "%m-%d %H:%M:%S")
            );
            chip(
                "range",
                label,
                cx,
                cx.listener(|this, _, _, cx| {
                    this.filter.range = None;
                    this.refilter(false, cx);
                }),
            )
        });
        let pattern = f.pat.and_then(|id| self.store.patterns.get(id as usize)).map(|p| {
            chip(
                "pattern",
                format!("Pattern  {}", p.text),
                cx,
                cx.listener(|this, _, _, cx| {
                    this.filter.pat = None;
                    this.refilter(false, cx);
                }),
            )
        });
        let exception = f.exc.and_then(|id| self.store.exceptions.get(id as usize)).map(|x| {
            chip(
                "exception",
                format!("Exception  {}", x.text),
                cx,
                cx.listener(|this, _, _, cx| {
                    this.filter.exc = None;
                    this.refilter(false, cx);
                }),
            )
        });
        Some(h_flex().flex_none().gap_2().px_3().pb_2().children(range).children(pattern).children(exception).child(
            Button::new("reset").xsmall().ghost().label("Clear filters").on_click(cx.listener(|this, _, _, cx| {
                this.filter = Filter { levels: this.filter.levels, ..Filter::default() };
                this.refilter(false, cx);
            })),
        ))
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let hist = &self.view.hist;
        let tz = self.store.tz_ms;
        let colors = [t.danger, t.warning, t.info.opacity(0.6)];
        let (accent, muted) = (t.accent, t.muted);
        let fmt = match hist.step {
            s if s >= 86_400_000 => "%m-%d",
            s if s >= 60_000 => "%m-%d %H:%M",
            _ => "%m-%d %H:%M:%S",
        };
        let selection = self.drag.zip(self.hover).map(|(a, b)| a.min(b)..=a.max(b));
        let info = match self.hover.and_then(|i| hist.buckets.get(i).map(|b| (i, b))) {
            Some((i, b)) => format!(
                "{} → {}   ·   {} entries   ·   {} errors   ·   {} warnings",
                format_time(hist.start + i as i64 * hist.step, tz, fmt),
                format_time(hist.start + (i as i64 + 1) * hist.step, tz, fmt),
                commas(b.iter().sum::<u32>() as u64),
                commas(b[0] as u64),
                commas(b[1] as u64),
            ),
            None => format!("{} per bar   ·   drag across bars to zoom in", step_label(hist.step)),
        };
        let bars = hist.buckets.iter().enumerate().map(|(i, bucket)| {
            // Errors sit on the baseline so their heights compare across bars.
            let segments = [2, 1, 0].into_iter().filter(|s| bucket[*s] > 0).map(|s| {
                div()
                    .w_full()
                    .flex_none()
                    .min_h(px(1.5))
                    .h(relative(bucket[s] as f32 / hist.max.max(1) as f32))
                    .bg(colors[s])
            });
            v_flex()
                .id(i)
                .flex_1()
                .h_full()
                // The spacing between bars is padding, so presses and releases never fall in a gap.
                .pr(px(1.))
                .justify_end()
                .when(selection.as_ref().is_some_and(|sel| sel.contains(&i)), |bar| bar.bg(accent))
                .hover(|bar| bar.bg(muted))
                .children(segments)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.drag = Some(i);
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if this.hover != Some(i) {
                        this.hover = Some(i);
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(start) = this.drag.take() {
                            this.filter.range = Some(this.bucket_range(start, i));
                            this.refilter(false, cx);
                        }
                    }),
                )
        });
        let end = hist.start + hist.buckets.len() as i64 * hist.step;
        let ticks = (0..=4).map(|i| format_time(hist.start + (end - hist.start) * i / 4, tz, fmt));
        v_flex()
            .flex_none()
            .px_3()
            .pb_1()
            .gap_1()
            .border_b_1()
            .border_color(t.border)
            .text_xs()
            .text_color(t.muted_foreground)
            .child(h_flex().justify_between().child("Timeline").child(info))
            .child(
                h_flex()
                    .id("timeline")
                    .h(px(76.))
                    .border_b_1()
                    .border_color(t.border)
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.hover = None;
                            cx.notify();
                        }
                    }))
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.drag = None;
                            cx.notify();
                        }),
                    )
                    .children(bars),
            )
            .child(h_flex().justify_between().children(ticks))
    }

    fn render_side_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let tab_button = |id: &'static str, tab: Tab, label: String| {
            Button::new(id).small().ghost().selected(self.tab == tab).label(label).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.tab = tab;
                    this.side_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                },
            ))
        };
        let list = match self.tab {
            Tab::Patterns => uniform_list(
                "patterns",
                self.view.patterns.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.pattern_rows(range, cx)),
            ),
            Tab::Exceptions => uniform_list(
                "exceptions",
                self.view.exceptions.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.exception_rows(range, cx)),
            ),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .p_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(tab_button(
                        "tab-patterns",
                        Tab::Patterns,
                        format!("Patterns  {}", commas(self.view.patterns.len() as u64)),
                    ))
                    .child(tab_button(
                        "tab-exceptions",
                        Tab::Exceptions,
                        format!("Exceptions  {}", commas(self.view.exceptions.len() as u64)),
                    )),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(list.track_scroll(&self.side_scroll).size_full())
                    .vertical_scrollbar(&self.side_scroll),
            )
    }

    fn pattern_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let (store, view) = (self.store.clone(), self.view.clone());
        let max = view.patterns.first().map_or(1, |p| p.1) as f32;
        let (active, hover, mono) = {
            let t = cx.theme();
            (t.list_active, t.list_hover, t.mono_font_family.clone())
        };
        range
            .filter_map(|i| {
                let (id, count) = *view.patterns.get(i)?;
                let pattern = &store.patterns[id as usize];
                let color = level_color(pattern.level, cx);
                Some(
                    div()
                        .id(i)
                        .relative()
                        .h(px(SIDE_ROW_H))
                        .w_full()
                        .cursor_pointer()
                        .when(self.filter.pat == Some(id), |row| row.bg(active))
                        .hover(|row| row.bg(hover))
                        // Share of the biggest pattern, drawn behind the text.
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(4.))
                                .bottom(px(4.))
                                .w(relative(count as f32 / max))
                                .bg(color.opacity(0.16)),
                        )
                        .child(
                            h_flex()
                                .size_full()
                                .px_2()
                                .gap_2()
                                .text_xs()
                                .font_family(mono.clone())
                                .child(div().flex_none().w(px(64.)).text_right().child(commas(count as u64)))
                                .child(div().flex_none().size(px(7.)).rounded_full().bg(color))
                                .child(div().flex_1().min_w_0().truncate().child(visual(&pattern.text))),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.filter.pat = (this.filter.pat != Some(id)).then_some(id);
                            this.refilter(false, cx);
                        })),
                )
            })
            .collect()
    }

    fn exception_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let (store, view) = (self.store.clone(), self.view.clone());
        let max = view.exceptions.first().map_or(1, |x| x.1) as f32;
        let (active, hover, muted, danger, mono) = {
            let t = cx.theme();
            (t.list_active, t.list_hover, t.muted_foreground, t.danger, t.mono_font_family.clone())
        };
        range
            .filter_map(|i| {
                let (id, count) = *view.exceptions.get(i)?;
                let exception = &store.exceptions[id as usize];
                Some(
                    div()
                        .id(i)
                        .relative()
                        .h(px(EXC_ROW_H))
                        .w_full()
                        .cursor_pointer()
                        .when(self.filter.exc == Some(id), |row| row.bg(active))
                        .hover(|row| row.bg(hover))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(4.))
                                .bottom(px(4.))
                                .w(relative(count as f32 / max))
                                .bg(danger.opacity(0.14)),
                        )
                        .child(
                            h_flex()
                                .size_full()
                                .px_2()
                                .gap_2()
                                .text_xs()
                                .font_family(mono.clone())
                                .child(div().flex_none().w(px(64.)).text_right().child(commas(count as u64)))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().truncate().child(visual(&exception.text)))
                                        .child(div().truncate().text_color(muted).child(exception.site.clone())),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.filter.exc = (this.filter.exc != Some(id)).then_some(id);
                            this.refilter(false, cx);
                        })),
                )
            })
            .collect()
    }

    fn render_entries(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .id("entries-pane")
            .relative()
            .size_full()
            .track_focus(&self.list_focus)
            .on_key_down(cx.listener(Self::on_list_key))
            .font_family(t.mono_font_family.clone())
            .text_size(px(12.))
            .child(
                uniform_list(
                    "entries",
                    self.view.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| this.entry_rows(range, cx)),
                )
                .track_scroll(&self.rows_scroll)
                .size_full(),
            )
            .when(self.view.rows.is_empty() && !self.loading, |pane| {
                pane.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(t.muted_foreground)
                        .child("No entries match the current filters"),
                )
            })
            .vertical_scrollbar(&self.rows_scroll)
    }

    fn entry_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let (store, view) = (self.store.clone(), self.view.clone());
        let (active, hover, muted) = {
            let t = cx.theme();
            (t.list_active, t.list_hover, t.muted_foreground)
        };
        range
            .filter_map(|row| {
                let ix = *view.rows.get(row)?;
                let entry = store.entries.get(ix as usize)?;
                let color = level_color(entry.level, cx);
                let extra = store.extra_lines(entry);
                Some(
                    h_flex()
                        .id(row)
                        .h(px(ROW_H))
                        .w_full()
                        .pr_4()
                        .gap_3()
                        .when(self.selected == Some(ix), |r| r.bg(active))
                        .hover(|r| r.bg(hover))
                        // Stripe only where it matters, so warnings and errors stand out.
                        .child(div().flex_none().w(px(3.)).h_full().when(entry.level >= 3, |s| s.bg(color)))
                        .child(div().flex_none().text_color(muted).child(clip(&store.bytes_of(entry)[5..], 18)))
                        .child(div().flex_none().w(px(26.)).text_color(color).child(LEVELS[entry.level as usize]))
                        .child(div().flex_1().min_w_0().truncate().child(visual(&clip(store.message(entry), 400))))
                        .when(extra > 0, |r| r.child(div().flex_none().text_color(muted).child(format!("+{extra}"))))
                        .on_click(cx.listener(move |this, _, window, cx| this.select(row, window, cx))),
                )
            })
            .collect()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let Some(entry) = self.selected.and_then(|ix| self.store.entries.get(ix as usize).copied()) else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(t.muted_foreground)
                .child("Select an entry to read it in full")
                .into_any_element();
        };
        let color = level_color(entry.level, cx);
        let file = self.store.files[entry.file as usize].name.clone();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .h(px(38.))
                    .px_3()
                    .gap_3()
                    .border_b_1()
                    .border_color(t.border)
                    .text_xs()
                    .child(
                        div().font_weight(FontWeight::SEMIBOLD).text_color(color).child(LEVELS[entry.level as usize]),
                    )
                    .child(clip(self.store.bytes_of(&entry), 30))
                    .child(div().text_color(t.muted_foreground).child(match self.store.extra_lines(&entry) + 1 {
                        1 => format!("{file}   ·   1 line"),
                        lines => format!("{file}   ·   {lines} lines"),
                    }))
                    .child(div().flex_1())
                    .child(
                        Button::new("pretty")
                            .xsmall()
                            .ghost()
                            .label("{ }")
                            .selected(self.pretty)
                            .tooltip("Pretty-print JSON and decode \\uXXXX escapes")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pretty = !this.pretty;
                                this.show_detail(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(Button::new("same-pattern").xsmall().ghost().label("Filter this pattern").on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.filter.pat = Some(entry.pat);
                            this.tab = Tab::Patterns;
                            this.refilter(false, cx);
                        }),
                    ))
                    .when(entry.exc != NONE, |bar| {
                        bar.child(
                            Button::new("same-exception").xsmall().ghost().label("Filter this exception").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.filter.exc = Some(entry.exc);
                                    this.tab = Tab::Exceptions;
                                    this.refilter(false, cx);
                                }),
                            ),
                        )
                    })
                    .child(
                        Button::new("copy")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Copy)
                            .label("Copy")
                            .on_click(cx.listener(Self::copy_selected)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family(t.mono_font_family.clone())
                    .text_size(px(12.))
                    .child(Textarea::new(&self.detail).readonly(true).appearance(false).h_full()),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let store = &self.store;
        let skipped = store.skipped().count();
        h_flex()
            .flex_none()
            .h(px(26.))
            .px_3()
            .gap_4()
            .border_t_1()
            .border_color(t.border)
            .text_xs()
            .text_color(t.muted_foreground)
            .child(format!("{} of {} entries", commas(self.view.rows.len() as u64), commas(store.entries.len() as u64)))
            .child(format!("{} files   ·   {:.1} MB", store.logs().count(), store.bytes() as f64 / 1_048_576.))
            .child(self.status.clone())
            .when(self.tail_task.is_some(), |bar| {
                bar.child(h_flex().gap_1p5().child(div().size(px(7.)).rounded_full().bg(t.success)).child("Live"))
            })
            .when(skipped > 0, |bar| bar.child(div().text_color(t.warning).child(format!("{skipped} files skipped"))))
            .child(div().flex_1())
            .children(self.error.clone().map(|error| div().text_color(t.danger).child(error)))
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let message = if self.loading {
            "Reading and indexing the logs…".to_string()
        } else if let Some(source) = self.store.skipped().next() {
            let name = source.path.file_name().unwrap_or_default().to_string_lossy();
            format!("No log entries found ({name}: {})", source.skipped.as_deref().unwrap_or_default())
        } else {
            "Drop a log folder or files here".to_string()
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_4()
            .child(Icon::new(IconName::FolderOpen).size_12().text_color(t.muted_foreground))
            .child(div().text_color(t.muted_foreground).child(message))
            .when(!self.loading, |welcome| {
                welcome.child(
                    Button::new("open-welcome")
                        .primary()
                        .label("Open log folder")
                        .on_click(cx.listener(Self::pick_folder)),
                )
            })
    }
}

impl Render for LogLens {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground) = (cx.theme().background, cx.theme().foreground);
        let root = v_flex()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .on_key_down(cx.listener(Self::on_root_key))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.open(paths.paths().to_vec(), cx)))
            .child(self.render_title_bar(cx));
        if self.store.entries.is_empty() {
            return root.child(self.render_welcome(cx)).child(self.render_status_bar(cx));
        }
        root.child(self.render_toolbar(cx))
            .children(self.render_active_filters(cx))
            .child(self.render_timeline(cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("main")
                        .child(
                            resizable_panel()
                                .size(px(420.))
                                .size_range(px(240.)..px(900.))
                                .child(self.render_side_panel(cx)),
                        )
                        .child(
                            v_resizable("entries-and-detail")
                                .child(resizable_panel().child(self.render_entries(cx)))
                                .child(resizable_panel().size(px(300.)).child(self.render_detail(cx))),
                        ),
                ),
            )
            .child(self.render_status_bar(cx))
    }
}
