# Builds the exe that goes on the releases page and prints its SHA-256.
#
#   powershell -File tools\release.ps1
#
# Rust bakes source paths into a binary for its panic messages. This build remaps the three that
# belong to the machine (the cargo home, the toolchain and this checkout) so they stay out of a
# public file, then checks that none slipped through. It uses its own target folder, because
# changing compiler flags would otherwise rebuild everything in the everyday one.
$root = Split-Path $PSScriptRoot -Parent
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$toolchain = (rustc --print sysroot).Trim()
$flags = '-C', 'target-feature=+crt-static', "--remap-path-prefix=$cargoHome=cargo",
    "--remap-path-prefix=$toolchain=rust", "--remap-path-prefix=$root=."
# The encoded form separates flags with 0x1F, so paths with spaces survive.
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
Push-Location $root
try {
    cargo build --release --locked --target-dir target\dist
    if ($LASTEXITCODE -ne 0) { throw 'the build failed' }
    $exe = Join-Path $root 'target\dist\release\loglens.exe'
    $text = [System.Text.Encoding]::GetEncoding(28591).GetString([System.IO.File]::ReadAllBytes($exe))
    foreach ($private in $env:USERPROFILE, $root) {
        if ($text.Contains($private)) { throw "the exe still contains the path $private" }
    }
    '{0}  ({1:N1} MB)' -f $exe, ((Get-Item $exe).Length / 1MB)
    'SHA256 ' + (Get-FileHash $exe -Algorithm SHA256).Hash
}
finally {
    Pop-Location
    Remove-Item Env:CARGO_ENCODED_RUSTFLAGS
}
