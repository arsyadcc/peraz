---
name: peraz
description: Compress media with the `peraz` CLI. Use when asked to make videos, images, GIFs, PDFs, or EPUBs smaller; convert images to WebP, AVIF, or JPEG; turn a video into a GIF or a GIF into an MP4; fit a video under a size limit; trim a clip; check in CI that a project's images are compressed; or inspect a media file.
---

# Peraz

`peraz` makes media files smaller while they keep looking the same. Three guarantees make it safe
to run on the user's files:

- **Originals stay untouched.** Each result is a new file: `<name>-1.<ext>` next to the input
  (`-2`, `-3`, … when taken), or in `--out-dir`.
- **Never worse.** A result that is not smaller is discarded and the report says
  `kept_original`, unless the format was changed on purpose (`--format`).
- **No silent overwrite.** `--output <file>` replaces an existing file only with `--force`.

Replacing or deleting the originals is a separate step: do it only when the user asks for it,
after reading the report.

## 1. Check the install

Run `peraz doctor --json`.

- **Exit `0`**: ready. `toolchain.source` says where FFmpeg came from and `encoders` lists what it
  can encode.
- **Exit `3`**: FFmpeg was not found. JPEG, PNG, WebP, BMP, and TIFF images, PDFs, and EPUBs still
  work without it. For video, GIFs, HEIC, and AVIF, offer `peraz ffmpeg install`, which downloads
  the FFmpeg build Peraz is tested with (about 25 MB) next to `peraz`. On a Peraz without that
  command, install FFmpeg with the system's package manager.
- **`peraz` not found**: offer to install it, then check again:
  - macOS or Linux with Homebrew: `brew install arsyadcc/tap/peraz` (includes FFmpeg);
  - with Rust: `cargo install --locked peraz`;
  - otherwise the installers on https://github.com/arsyadcc/peraz/releases/latest.

Done when `peraz doctor` exits `0`, or every input is a format that works without FFmpeg.

## 2. Compress

`peraz <files>` is short for `peraz compress <files>`. Pass `--json` whenever you will read the
result, and `--out-dir <folder>` for a batch so the results land together.

| The user wants | Command |
| --- | --- |
| Smaller files, same formats | `peraz --json <files>` |
| Even smaller, visibly lossy | `peraz --json -q small <files>` (`-q`: `highest`, `high`, `balanced` (default), `small`, `smallest`) |
| Pictures for a website | `peraz --json --recipe web-images <files>` (WebP, at most 2000 px) |
| A specific image format | `peraz --json --format webp <files>` (`jpeg`, `png`, `webp`, `avif`) |
| Smaller dimensions | `--max-width 1280` and/or `--max-height 720` (never scales up) |
| A video for Discord, email, GitHub, or WhatsApp | `peraz --json --recipe discord video.mov` (or `email`, `github`, `whatsapp`) |
| A video under a size limit | `peraz --json --target-size 25MB video.mov` (SI units: 1 MB = 1,000,000 bytes) |
| Part of a video | `--start 0:12 --duration 10`; add `--trim-only` to cut without re-encoding |
| A GIF from a video | `peraz --json --format gif --start 12 --duration 4 clip.mp4` |
| A video from a GIF | `peraz --json --format mp4 anim.gif` (often 70–95% smaller) |
| The smallest file that still looks this good | `peraz --json --target-vmaf 93 video.mov`, `peraz --json --target-ssimulacra2 80 <pictures>` (the report's `score` shows the result) |
| Smaller HEVC video | `--codec hevc` |
| No audio track | `--no-audio` |
| A PDF or EPUB with lighter pictures | `peraz --json report.pdf book.epub` (text and layout stay) |
| Media information only | `peraz probe --json <files>` |

`peraz compress --help` lists every flag. HDR videos become 10-bit HEVC; `--codec h264` on one
is an error. HEIC photos become JPEG unless `--format` says otherwise.

Images take seconds; videos can take minutes each. For a batch of videos, run the command in the
background and report when it finishes.

## 3. Read the report

`compress --json` prints one document: `results[]` (one per input, in order) and `summary`
(counts per status, `input_bytes`, `saved_bytes`). For each result, `outcome.status` is one of:

- `compressed`: `outcome.output` is the new file, `outcome.output_bytes` its size;
- `kept_original`: no file written; `outcome.reason` is `not_smaller` or `already_within_target`;
- `skipped` / `failed`: `outcome.message` says why (encrypted PDFs and DRM EPUBs are skipped);
- `cancelled`: stopped by Ctrl-C.

Tell the user, per file or in total: the size before and after, the saving in percent, and the
output path. Count `kept_original` as success: the file was already as small as Peraz can make
it at that quality. Every field: [references/json.md](references/json.md).

Exit codes: `0` success (kept and skipped files included), `1` at least one input failed, `2`
invalid usage, `3` environment problem such as FFmpeg missing, `4` `--check` found files that
could be smaller, `130` cancelled.

## Recipes, project settings, and CI

For repeatable settings (a `peraz.toml` in a project, your own recipes) or a CI check that images
stay compressed (`--check`), read [references/recipes-and-ci.md](references/recipes-and-ci.md).
