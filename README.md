# Peraz

A cross-platform media compressor (macOS, Windows, Linux) for creators and developers:
video, images, and GIFs, with results you can trust.

> Status: **Phase 1, core engine**. Not usable yet.

## Usage (so far)

```sh
peraz compress clip.mov                         # → clip-peraz.mp4 (H.264, balanced quality)
peraz compress -q small --codec hevc *.mp4      # smaller files
peraz compress --encoder hardware --max-width 1280 --no-audio clip.mov
peraz compress photo.jpg screenshot.png          # mozjpeg / pngquant-style palette + oxipng
peraz compress --format jpeg --max-width 2000 *.png
peraz compress --format webp *.png *.jpg           # or --format avif (smallest, slower)
peraz compress IMG_0042.HEIC                       # → IMG_0042-peraz.jpg, upright, colors kept
peraz compress --format gif --start 12 --duration 4 clip.mp4   # video → GIF (480 px, 12 fps)
peraz compress -q small reaction.gif               # smaller GIF, same timing
peraz compress --json --out-dir out/ media/*     # machine-readable report
peraz probe video.mp4 photo.heic                # inspect media
peraz doctor                                    # which FFmpeg is used and what it can encode
```

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

Peraz needs `ffmpeg` and `ffprobe`. Until bundled builds exist, it uses `--ffmpeg <path>`,
`PERAZ_FFMPEG`, or `ffmpeg` on `PATH`, in that order.

Exit codes: `0` success (including files kept or skipped), `1` at least one input failed, `2` invalid
usage, `3` environment problem (e.g. FFmpeg not found), `130` cancelled with Ctrl-C or SIGTERM
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
