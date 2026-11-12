# Peraz

A cross-platform media compressor (macOS, Windows, Linux) for creators and developers:
video, images, and GIFs, with results you can trust.

> Status: **Phase 1, core engine**. Not usable yet.

## Usage (so far)

```sh
peraz compress clip.mov                         # → clip-peraz.mp4 (H.264, balanced quality)
peraz compress -q small --codec hevc *.mp4      # smaller files
peraz compress --encoder hardware --max-width 1280 --no-audio clip.mov
peraz compress --target-size 25MB long-video.mov      # fits, verified (two-pass with x264/x265)
peraz compress --start 0:12 --duration 10 clip.mov      # compress just that part
peraz compress --trim-only --start 0:12 --duration 10 clip.mov   # cut only: lossless when possible
peraz compress photo.jpg screenshot.png          # mozjpeg / pngquant-style palette + oxipng
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

PDF and EPUB files keep their text, layout, and links: only the pictures inside are
recompressed (photos with mozjpeg, graphics as palette images), and a picture is replaced only
when the new version is smaller. Encrypted PDFs and DRM-protected EPUBs are skipped.

Batches run in parallel: still images use up to one job per logical core, while video and
animation encodes (which already use many cores each) run one per 8 cores. Two inputs that would
write the same output file are caught before either one writes. `--jobs N` sets the number of
files worked on at once.

`--start` and `--duration` (seconds or `[h:]m:ss`) pick part of a video. With `--trim-only` the
clip is cut without compressing: when the start is on a keyframe the streams are copied (instant
and lossless), otherwise the clip is re-encoded and the report says why and where the nearest
keyframe is. `--snap-to-keyframe` moves the start back to that keyframe, keeping the end, so the
clip can always be copied.

With `--target-size`, the video and audio bit rates are computed from the target, software
encoders run two passes, and the result is measured: if it is over, the final pass runs again
at a lower bit rate. A file over the target is never kept. When the bit rate is too low for the
resolution, the video is scaled down (unless you set `--max-width`/`--max-height`).

Peraz never keeps a worse result: if the compressed file is not smaller, it is discarded and the
report says why. Existing files are never overwritten without `--force`, and a half-written file
never appears under the final name.

Images: the EXIF orientation is applied and the color profile is kept; other metadata (EXIF, XMP,
including GPS location) is removed. Transparent images converted to JPEG are placed on white.
WebP keeps the color profile; AVIF output is converted to sRGB first so colors stay the same.
HEIC photos become JPEG by default (use `--format` for WebP, AVIF, or PNG). When the format
changes on purpose, the result is kept even if it is larger, and the report shows the increase.
GIFs are encoded by Peraz itself: a palette per frame (imagequant) with dithering, unchanged
pixels left transparent, frames cropped to what changed, and identical frames merged. Default
GIF size depends on the quality level (480 px wide and 12 fps for `balanced`); `--max-width` and
`--fps` override it.

`--format webp` and `--format avif` turn videos and GIFs into animated WebP or AVIF (same
`--fps`, `--start`, `--duration` controls); WebP keeps transparency, AVIF places it on white.
`--format mp4` turns a GIF into an H.264 video.

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

Peraz needs `ffmpeg` and `ffprobe`. Until bundled builds exist, it uses `--ffmpeg <path>`,
`PERAZ_FFMPEG`, or `ffmpeg` on `PATH`, in that order.

Exit codes: `0` success (including files kept or skipped), `1` at least one input failed, `2` invalid
usage, `3` environment problem (e.g. FFmpeg not found), `4` `--check` found files that could be
smaller, `130` cancelled with Ctrl-C or SIGTERM
(the current file's partial output is removed). Every `--json` document carries a
`schema_version`.

## Crates

| Crate | Contents |
|---|---|
| `peraz-core` | Domain model and planner, no I/O |
| `peraz-engine` | Engine adapters: FFmpeg, images, quality metrics |
| `peraz-cli` | The `peraz` binary |

## Development

```sh
cargo build                       # toolchain pinned in rust-toolchain.toml
cargo test --workspace            # integration tests run when ./fixtures exists
cargo clippy --workspace --all-targets -- -D warnings
./scripts/make-fixtures.sh        # test samples into ./fixtures (needs ffmpeg and zip)
```

## License

Peraz is free software: [GPL-3.0-or-later](LICENSE). Other programs can build on it by running the
`peraz` binary; its `--json` output and exit codes are a stable, versioned interface.

Outside contributions are not accepted yet; a contributor agreement will come first.
