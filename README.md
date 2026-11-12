<h1 align="center">Peraz</h1>

<p align="center">
  <img src=".github/assets/peraz-icon.png" width="160" alt="Peraz logo: an orange citrus juicer" />
</p>

<p align="center">
  Squeeze videos, images, GIFs, and documents from the terminal, with results you can trust.
</p>

<p align="center"><img alt="Platform" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-blue" />&nbsp;<img alt="Rust" src="https://img.shields.io/badge/rust-1.99-orange" />&nbsp;<img alt="License" src="https://img.shields.io/badge/license-GPL--3.0--or--later-green" />&nbsp;<img alt="Latest release" src="https://img.shields.io/github/v/release/arsyadcc/peraz?include_prereleases" />&nbsp;<a href="https://github.com/arsyadcc/peraz/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/arsyadcc/peraz/actions/workflows/ci.yml/badge.svg" /></a></p>

---

> Point `peraz` at a file and get a smaller one. It never keeps a worse result, never overwrites
> without asking, and tells you exactly what it did, in words or in JSON. The name comes from the
> Indonesian *peras*, "to squeeze".

```console
$ peraz compress peraz-icon.png
peraz-icon.png → peraz-icon-peraz.png
  199 kB → 47.1 kB (−76%) · imagequant q88 + oxipng · 0.2 s
```

> [!NOTE]
> Peraz is in **alpha**. Expect rough edges. The `--json` output and exit codes are already
> versioned, so scripts built on them keep working.

## Installation

**Homebrew** (macOS and Linux) installs Peraz and FFmpeg together:

```sh
brew tap arsyadcc/tap
brew install peraz
```

(or in one step: `brew install arsyadcc/tap/peraz`)

**Installers:** the [latest release](https://github.com/arsyadcc/peraz/releases/latest) has a shell
installer for macOS and Linux and a PowerShell installer for Windows. These install `peraz` only:
install FFmpeg with your package manager, then check with `peraz doctor`.

**From source** (Rust 1.99 or newer):

```sh
cargo install --locked --path crates/peraz-cli
```

## Quick start

```sh
peraz compress clip.mov                          # → clip-peraz.mp4 (H.264, balanced quality)
peraz compress --target-size 25MB long-video.mov # fits under 25 MB, verified
peraz compress --format webp photos/*            # smaller pictures for the web
peraz compress --format gif --start 12 --duration 4 clip.mp4   # video → GIF
peraz compress report.pdf book.epub              # recompress the pictures inside
peraz doctor                                     # which FFmpeg is used and what it can encode
```

## Features

- **Never worse** — if the compressed file is not smaller, it is thrown away and the report says
  why. A file over `--target-size` is never kept either.
- **Safe by default** — existing files are never overwritten without `--force`, a half-written
  file never appears under the final name, and Ctrl-C stops cleanly and removes partial output.
- **Video** — H.264 and HEVC with five quality levels calibrated with VMAF, scaling without ever
  upscaling, optional audio removal, and hardware encoding with VideoToolbox on macOS.
- **Target size** — `--target-size 25MB` computes the bit rates, runs two passes, measures the
  result, and tries again if it is over. Sizes use SI units (1 MB = 1,000,000 bytes).
- **Trim** — cut a clip by time. When the cut lands on a keyframe the streams are copied:
  instant and lossless.
- **Images** — JPEG (mozjpeg), PNG (palette + oxipng, with a lossless fallback), WebP, and AVIF.
  EXIF orientation is applied, the color profile is kept, and other metadata, including GPS
  location, is removed.
- **iPhone photos** — HEIC and AVIF input, upright and with their colors, converted to JPEG by
  default.
- **GIFs done properly** — Peraz encodes GIFs itself: a palette per frame, dithering, only the
  pixels that changed, and identical frames merged.
- **Modern animation** — turn videos and GIFs into animated WebP or AVIF, or a GIF into an MP4
  that is often 70–95% smaller.
- **PDF and EPUB** — the pictures inside are recompressed; text, layout, and links stay as they
  are.
- **Fast batches** — files run in parallel, with separate limits for still images and for video
  encodes, so the machine stays responsive.
- **Recipes** — save settings as TOML and reuse them with `--recipe`. A `peraz.toml` in a project
  is picked up automatically.
- **CI check** — `--check` writes nothing and fails when files in a repository could be smaller.
- **Watch folders** — `peraz watch ~/Screenshots` compresses new files as they appear.
- **Scriptable** — every command has `--json` output with a `schema_version`, and exit codes are
  documented and stable.

## Supported formats

| Kind | Input | Output |
| --- | --- | --- |
| Video | Anything your FFmpeg can read (MOV, MP4, MKV, WebM, …) | MP4 (H.264, HEVC) |
| Images | JPEG, PNG, WebP, AVIF, HEIC/HEIF | JPEG, PNG, WebP, AVIF |
| Animation | GIF, video | GIF, animated WebP, animated AVIF, MP4 |
| Documents | PDF, EPUB | Same format, smaller pictures inside |

Encrypted PDFs and DRM-protected EPUBs are skipped.

## Usage

```sh
peraz compress clip.mov                         # → clip-peraz.mp4 (H.264, balanced quality)
peraz compress -q small --codec hevc *.mp4      # smaller files
peraz compress --encoder hardware --max-width 1280 --no-audio clip.mov
peraz compress --target-size 25MB long-video.mov      # fits, verified (two-pass with x264/x265)
peraz compress --start 0:12 --duration 10 clip.mov      # compress just that part
peraz compress --trim-only --start 0:12 --duration 10 clip.mov   # cut only: lossless when possible
peraz compress photo.jpg screenshot.png          # mozjpeg / palette PNG + oxipng
peraz compress --format jpeg --max-width 2000 *.png
peraz compress --format webp *.png *.jpg           # or --format avif (smallest, slower)
peraz compress IMG_0042.HEIC                       # → IMG_0042-peraz.jpg, upright, colors kept
peraz compress --format gif --start 12 --duration 4 clip.mp4   # video → GIF (480 px, 12 fps)
peraz compress -q small reaction.gif               # smaller GIF, same timing
peraz compress --format webp clip.mp4              # animated WebP (or avif: smallest)
peraz compress --format mp4 reaction.gif           # GIF → MP4, often 70-95% smaller
peraz compress report.pdf book.epub                # recompress the pictures inside
peraz compress --json --out-dir out/ media/*     # machine-readable report
peraz compress --jobs 1 *.mov                      # one at a time, with live progress
peraz compress --recipe web-images photos/*        # named settings (see Recipes)
peraz recipe list
peraz watch ~/Screenshots --recipe web-images --out-dir ~/Screenshots/small
peraz probe video.mp4 photo.heic                # inspect media
peraz doctor                                    # which FFmpeg is used and what it can encode
```

Quality levels, from largest to smallest file: `highest`, `high`, `balanced` (default), `small`,
`smallest`.

### Video

With `--target-size`, the video and audio bit rates are computed from the target, software
encoders run two passes, and the result is measured: if it is over, the final pass runs again
at a lower bit rate. A file over the target is never kept. When the bit rate is too low for the
resolution, the video is scaled down (unless you set `--max-width`/`--max-height`).

`--start` and `--duration` (seconds or `[h:]m:ss`) pick part of a video. With `--trim-only` the
clip is cut without compressing: when the start is on a keyframe the streams are copied (instant
and lossless), otherwise the clip is re-encoded and the report says why and where the nearest
keyframe is. `--snap-to-keyframe` moves the start back to that keyframe, keeping the end, so the
clip can always be copied.

### Images

The EXIF orientation is applied and the color profile is kept; other metadata (EXIF, XMP,
including GPS location) is removed. Transparent images converted to JPEG are placed on white.
WebP keeps the color profile; AVIF output is converted to sRGB first so colors stay the same.
HEIC photos become JPEG by default (use `--format` for WebP, AVIF, or PNG). When the format
changes on purpose, the result is kept even if it is larger, and the report shows the increase.

### GIFs and animation

GIFs are encoded by Peraz itself: a palette per frame (imagequant) with dithering, unchanged
pixels left transparent, frames cropped to what changed, and identical frames merged. Default
GIF size depends on the quality level (480 px wide and 12 fps for `balanced`); `--max-width` and
`--fps` override it.

`--format webp` and `--format avif` turn videos and GIFs into animated WebP or AVIF (same
`--fps`, `--start`, `--duration` controls); WebP keeps transparency, AVIF places it on white.
`--format mp4` turns a GIF into an H.264 video.

### PDF and EPUB

PDF and EPUB files keep their text, layout, and links: only the pictures inside are
recompressed (photos with mozjpeg, graphics as palette images), and a picture is replaced only
when the new version is smaller. Encrypted PDFs and DRM-protected EPUBs are skipped.

### Batches

Batches run in parallel: still images use up to one job per logical core, while video and
animation encodes (which already use many cores each) run one per 8 cores. Two inputs that would
write the same output file are caught before either one writes. `--jobs N` sets the number of
files worked on at once.

### Recipes

A recipe is a named set of settings, written in TOML with the same keys as the flags:

```toml
version = 1                      # recipe format version
description = "Pictures for our website"
quality = "balanced"             # highest, high, balanced, small, smallest
format = "webp"                  # keep, jpeg, png, webp, avif, gif, mp4
max_width = 2000                 # pixels; never scales up
max_height = 2000
codec = "h264"                   # h264, hevc
encoder = "auto"                 # auto, software, hardware
no_audio = false
target_size = "25MB"             # SI units; videos only
fps = 12                         # GIF and animated WebP/AVIF
```

Every key except `version` is optional, and unknown keys are errors. Use one with
`peraz compress --recipe <name or file.toml>`; flags on the command line override the recipe's
values. `peraz recipe list` shows the built-in recipes and yours; `peraz recipe show <name>`
prints one as TOML. Your recipes live in `<config dir>/recipes/<name>.toml`, where the config
dir is `PERAZ_CONFIG_DIR`, or `~/Library/Application Support/peraz` (macOS),
`$XDG_CONFIG_HOME/peraz` or `~/.config/peraz` (Linux), `%APPDATA%\peraz` (Windows). A recipe
with the same name as a built-in one replaces it.

### Project recipes and CI

Put a `peraz.toml` recipe at the root of a project: `peraz compress` and `peraz watch` use the
nearest one found in the current folder or its parents (flags still override it; skip it with
`--ignore-project-recipe`).

`--check` writes nothing: it compresses into a temporary folder and exits with code `4` when any
file could be made at least `--check-threshold` percent smaller (default 10, so files that were
already compressed pass). A GitHub Actions step:

```yaml
- name: Check that images are compressed
  run: peraz compress --check assets/**/*.{jpg,png,webp}
```

### Watch folders

`peraz watch <folders>` compresses new files as they appear, with the same flags as `compress`
(including `--recipe`). A file is picked up once it has stopped changing for two seconds, so files
still being copied or recorded are never read half-written. Files that were there before watching
started are left alone unless you pass `--include-existing`, and a file whose output already exists
is skipped, so restarting a watch never compresses anything twice. `--recursive` includes
subfolders (mirrored inside `--out-dir`), and `--json` prints one JSON report per line. Stop with
Ctrl-C.

### Scripting: JSON and exit codes

Every `--json` document carries a `schema_version`, which changes only on breaking changes.

| Exit code | Meaning |
| --- | --- |
| `0` | Success, including files kept or skipped |
| `1` | At least one input failed |
| `2` | Invalid usage |
| `3` | Environment problem, for example FFmpeg not found |
| `4` | `--check` found files that could be smaller |
| `130` | Cancelled with Ctrl-C or SIGTERM; the current file's partial output is removed |

## Requirements

- macOS, Linux, or Windows
- `ffmpeg` and `ffprobe` (Homebrew installs them with Peraz). Peraz uses `--ffmpeg <path>`,
  `PERAZ_FFMPEG`, or `ffmpeg` on `PATH`, in that order. `peraz doctor` shows which one it found
  and what it can encode.

## Building from source

```sh
git clone https://github.com/arsyadcc/peraz.git
cd peraz
cargo build --release             # toolchain pinned in rust-toolchain.toml
```

Release builds use SIMD for JPEG and AVIF encoding (about 1.6x and 2x faster than a plain build);
build with `--features simd` to get the same, which needs `nasm` on x86 machines.

```sh
cargo test --workspace            # integration tests run when ./fixtures exists
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                  # dependency licenses, advisories, sources (deny.toml)
./scripts/make-fixtures.sh        # test samples into ./fixtures (needs ffmpeg and zip)
```

## Project layout

```
crates/peraz-core/      Domain model and planner, no I/O
crates/peraz-engine/    Engine: FFmpeg, probing, images, HEIF, GIF, PDF, EPUB, batches
crates/peraz-cli/       The `peraz` binary
scripts/                Test fixtures and video preset calibration
dist-workspace.toml     Release builds and installers (cargo-dist)
deny.toml               Allowed dependency licenses
```

## License

Peraz is free software: [GPL-3.0-or-later](LICENSE). Other programs can build on it by running the
`peraz` binary; its `--json` output and exit codes are a stable, versioned interface.

Outside contributions are not accepted yet; a contributor agreement will come first.
