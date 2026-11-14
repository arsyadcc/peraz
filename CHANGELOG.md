# Changelog

All notable changes to Peraz are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Changes to the interface that scripts depend on (command-line flags, the `--json` output, exit
codes, and the recipe format) are marked **Interface**. From 0.1.0 on, a breaking change to them
needs a new major version (or a new `schema_version` for `--json`); before 0.1.0 they changed
between alpha versions.

## [Unreleased]

### Added

- Built-in recipes for upload limits: `discord` (20 MB), `email` (14 MB), `github` (10 MB), and
  `whatsapp` (64 MB, 720p) fit each video under that service's limit. The limits and their
  sources are in docs/recipes.md.

- `--target-vmaf <score>` makes each video as small as it can be while it still reaches that
  VMAF score: the CRF is searched on samples of the video (the whole video when it is at most
  30 seconds), then the video is encoded once. Needs FFmpeg with libvmaf, which the bundled build
  (`ffmpeg-9.0.2-2`, from `peraz ffmpeg install`) now includes. **Interface:** new flag and
  recipe key `target_vmaf`.
- `--target-ssimulacra2 <score>` does the same for still images, with SSIMULACRA2.
  **Interface:** new flag and recipe key `target_ssimulacra2`.
- `--format jxl` writes JPEG XL with libjxl's `cjxl`, run as a separate program like FFmpeg (from
  the bundled build, `PATH`, or `PERAZ_CJXL`). A JPEG is also recompressed losslessly when it is
  not resized, and the smaller of the two files is kept. The bundled build `ffmpeg-9.0.2-3`
  includes `cjxl` and `djxl` from libjxl 0.12.0. **Interface:** new format value `jxl`, exit code
  `3` when `cjxl` is missing, and `peraz doctor --json` a `jxl` field.
- **Interface:** reports have a `score` field (`metric`, `target`, `score`, `sampled`), and
  `peraz doctor --json` a `vmaf` field saying whether FFmpeg has libvmaf.

### Changed

- GIFs are denoised over time before frames are compared, so camera noise no longer counts as
  change. At the default quality, GIFs from noisy footage were 1.6-2.1 times smaller in our
  tests, and from clean footage 12-20% smaller. The strength follows `--quality` (none at
  `highest`, strongest at `smallest`) and is lighter when the source is a GIF.

## [0.2.0] - 2026-11-14

### Added

- Peraz uses a bundled FFmpeg build in a `peraz-ffmpeg` folder next to the `peraz` executable,
  after `--ffmpeg` and `PERAZ_FFMPEG` and before `PATH`. **Interface:** `toolchain.source` in
  `--json` output can be `bundled`.
- `peraz ffmpeg install` downloads the FFmpeg build Peraz is tested with into that folder, and
  `peraz ffmpeg uninstall` removes it. The builds (GPL, with x264, x265, SVT-AV1, dav1d, and
  libwebp) are published with their sources as `ffmpeg-*` releases. **Interface:** new commands.
- Each release has `peraz-full-<platform>` archives with Peraz and the bundled FFmpeg together.
- A plugin for Claude Code and Codex (`/plugin marketplace add arsyadcc/peraz`): a skill that
  teaches coding agents to compress media with Peraz and read its reports.

## [0.1.0] - 2026-11-13

The first stable version. It works like 0.1.0-alpha.4; what changes is the promise: the
command-line flags, the `--json` output (`schema_version` 1), exit codes, and the recipe format
described in [docs/](https://github.com/arsyadcc/peraz/tree/main/docs) stay compatible until a
new major version. The [README](https://github.com/arsyadcc/peraz#readme) shows what Peraz does,
and the alpha versions below list every change on the way here.

### Changed

- The Homebrew tap builds stable versions from source (`brew install arsyadcc/tap/peraz`), with
  the same formula that is tested in CI on macOS and Linux. It links Homebrew's libdeflate.
- `cargo install --locked peraz` installs 0.1.0 without `--version`.
- Later alpha and beta versions are marked as pre-releases on GitHub and are not published to the
  Homebrew tap, so the latest release and `brew upgrade` stay on stable versions.

## [0.1.0-alpha.4] - 2026-11-13

### Added

- Without FFmpeg, `peraz compress` still compresses JPEG, PNG, WebP, BMP, and TIFF images, PDFs,
  and EPUBs, with the same results. If any input needs FFmpeg (videos, GIFs, HEIC, AVIF), the
  run stops before any work with exit code 3 and names that file. `peraz doctor` says what works
  without FFmpeg.

### Changed

- **Interface:** in `peraz compress --json`, `ffmpeg` is `null` when FFmpeg was not found and
  every input could be compressed without it.

## [0.1.0-alpha.3] - 2026-11-12

### Added

- `peraz completions <shell>` prints a completion script for bash, zsh, fish, PowerShell, or
  elvish. In zsh and fish, `--recipe` completes recipe names. The Homebrew formula installs the
  completions.
- **Interface:** `peraz recipe list --names` prints only the recipe names, one per line.
- Reference documentation in `docs/`: every command, flag, environment variable, and exit code
  ([cli.md](docs/cli.md)), every `--json` field ([json.md](docs/json.md)), and the recipe format
  ([recipes.md](docs/recipes.md)). Tests fail when the interface and the docs disagree.
- A Homebrew formula that builds from source (`packaging/homebrew/peraz.rb`), tested on macOS
  and Linux in CI. From 0.1.0 on, the tap installs stable versions with it.
- Build feature `system-libs`: links the system's libdeflate (found with `pkg-config`) instead of
  the bundled copy.
- **Interface:** `--json` probe and report output include each video's color description
  (`color`: `primaries`, `transfer`, `matrix`, `range`), and `peraz probe` shows `HDR (PQ)` or
  `HDR (HLG)`.

### Changed

- Videos keep their color description (primaries, transfer, matrix, range), and 10-bit HEVC
  stays 10-bit.
- HDR videos become 10-bit HEVC when no codec is given, because 8-bit H.264 cannot keep HDR;
  the report shows `HDR kept`. **Interface:** `--codec h264` on an HDR video now fails with a
  message instead of producing washed-out colors.
- AVIF output keeps the color profile instead of converting the colors to sRGB, so wide-gamut
  pictures (Display P3) stay wide-gamut.
- **Interface:** `peraz doctor --json` names the FFmpeg version and license `ffmpeg`, like
  `peraz compress --json`, instead of `build`.
- **Interface:** `fps` in a recipe accepts 1–50, like `--fps` (it accepted up to 60).
- `--out-dir` makes the folder when it does not exist.
- Error messages are clearer: a file FFprobe cannot read is "not a media file Peraz can read",
  FFmpeg's own errors are shown without its internal prefixes, and write errors name the output
  file instead of a temporary one.

### Fixed

- Transparent HEIC and AVIF pictures keep their transparency (it was dropped).
- HEIC files whose picture is not the first tile grid in the file decode correctly.
- Still (single-frame) GIFs are compressed instead of skipped: to PNG by default, with their
  transparency, kept only when smaller.
- JPEGs and GIFs that were cut off (while copying or downloading) fail with a clear message
  instead of being "compressed" with the missing part filled with gray.
- File names near the 255-byte limit work: the result's name is shortened to fit.
- In a batch, a file that fails no longer takes an output number, so the next file with the same
  name gets `-1` instead of `-2`.

## [0.1.0-alpha.2] - 2026-11-12

### Added

- `peraz <files>` is short for `peraz compress <files>`.
- Peraz is on crates.io: `peraz` (the command), `peraz-core`, and `peraz-engine`.

### Changed

- **Interface:** results are named `<name>-1.<ext>`, then `<name>-2.<ext>` when that is taken,
  instead of `<name>-peraz.<ext>`. Compressing a file again never fails or replaces an earlier
  result, and inputs of a batch that would share a name get different numbers. `--output` still
  writes exactly the file given, replaced only with `--force`.
- Watch folders treat `<name>-<number>.<ext>` next to a `<name>.*` file as an earlier result and
  do not compress it again.
- The release archives are named `peraz-*` (they were named after the internal crate).

## [0.1.0-alpha.1] - 2026-11-12

The first public alpha.

### Added

- Video compression to H.264 or HEVC with software or hardware (VideoToolbox) encoders, five
  quality levels calibrated with VMAF, and a never-worse rule: a result that is not smaller is
  not kept.
- Images: JPEG (mozjpeg), PNG (imagequant and oxipng), WebP, and AVIF output; HEIC and AVIF
  input. The EXIF orientation is applied, the color profile is kept, and other metadata is
  removed.
- GIFs, animated WebP and AVIF from videos and GIFs, and GIF to MP4.
- `--target-size` fits a video into a file size.
- Trimming with `--start` and `--duration`, losslessly with `--trim-only` when the cut lands on
  a keyframe.
- PDF and EPUB compression by recompressing their pictures.
- Parallel batches with per-resource limits.
- Recipes: named settings in TOML files, built-in or your own, and a `peraz.toml` project
  recipe used automatically.
- `--check` mode for CI.
- `peraz watch` compresses new files as they appear in folders.
- `peraz probe` and `peraz doctor`.
- **Interface:** `--json` output with `schema_version` 1, and exit codes 0, 1, 2, 3, 4, and 130.
- Release builds for macOS, Linux, and Windows, shell and PowerShell installers, and a Homebrew
  tap that installs FFmpeg too.

[Unreleased]: https://github.com/arsyadcc/peraz/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/arsyadcc/peraz/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.4...v0.1.0
[0.1.0-alpha.4]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.3...v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.2...v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/arsyadcc/peraz/releases/tag/v0.1.0-alpha.1
