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

- `--keep-metadata` (recipe key `keep_metadata`) keeps the metadata of pictures: EXIF, XMP, and
  IPTC, which includes the GPS location. The default stays the same: all of it is removed. The
  pixels are stored upright as before, and the orientation in the kept EXIF is reset, so
  nothing is turned twice; the color profile is kept either way. EXIF and XMP are kept in JPEG,
  PNG, WebP, HEIC, and JPEG XL; AVIF keeps the EXIF, TIFF the XMP and IPTC, and JPEG the IPTC
  too. A JPEG recompressed losslessly into JPEG XL keeps its metadata whole. **Interface:** new
  flag and recipe key; `settings.keep_metadata` in `peraz recipe show --json`.
- Results are marked as made by Peraz, and a marked file is skipped instead of being compressed
  a second time: by `peraz compress` when the same folder is given again, and by `peraz watch`.
  The mark does not change the file's contents: it is an extended attribute on macOS and Linux
  and an alternate data stream on Windows, and it holds the file's size and modification time,
  so a result that was edited afterwards is compressed again. `--recompress` compresses marked
  files too. Without a file system that can hold the mark, nothing changes. **Interface:** new
  flag `--recompress`; skipped results have `outcome.reason` in `--json` and `--events`:
  `unsupported`, `already_compressed`, `output_exists`, or `protected`.
- `--target-size` fits still pictures too: JPEG, PNG, WebP, AVIF, HEIC, and JPEG XL output. A
  picture that the usual compression brings under the size gets just that; otherwise the highest
  quality setting that fits is used, down to a floor, and only when the floor is too large is
  the picture scaled down, step by step. The result's real size is checked, and when nothing
  fits the file fails and the original stays. `--keep-dimensions` (recipe key
  `keep_dimensions`) never scales and fails instead. A lossless TIFF that is over the size fails
  with a message that says the size needs a lossy format. `--target-ssimulacra2` takes the size's
  place for pictures when both are given. **Interface:** new flag and recipe key; the plan of a
  picture has `target` (`target_bytes`, `reached`, `attempts`, `scaled_to`,
  `keep_dimensions`) when it was fitted to a size.
- HEIC output: `--format heic` (recipe value `heic`, also spelled `heif`) writes a HEIC from
  any still picture, with its color profile, its transparency (as an alpha picture, the way
  Apple's software stores it), and any size, odd ones included. FFmpeg encodes the picture with
  libx265 at 8 bits per channel with 4:2:0 color, and Peraz writes the HEIF container. The
  quality levels use x265 CRF 17, 21, 24, 27, and 30, set by SSIMULACRA2 scores against the JPEG
  levels; `--target-ssimulacra2` and `--measure-quality` work. **Interface:** new `--format`
  and recipe `format` value `heic`; exit code `3` before any work when `--format heic` is given
  and FFmpeg is missing or has no libx265.
- TIFF output: `--format tiff` (recipe value `tiff`) writes a TIFF with lossless Deflate
  compression that keeps the bit depth (8-bit, 16-bit, floating point), transparency, and color
  profile, and a TIFF input stays a TIFF by default, kept only when smaller. Every quality level
  gives the same pixels. A TIFF with more than one page is skipped with a message.
  **Interface:** new `--format` and recipe `format` value `tiff` (also spelled `tif`).
- Animated PNG: an animated PNG stays an animated PNG by default, kept only when smaller, with
  every frame, its timing, its loop count, and its transparency. Each frame stores only the
  rectangle that changed, identical frames are merged, and below `--quality highest` the frames
  share one palette when it is good enough for the quality level; `highest` is always lossless.
  `--format apng` (recipe value `apng`) makes an animated PNG from a GIF, an animated WebP, or an
  animated JPEG XL frame for frame, from a video at `--fps`, and a PNG from a still image. An
  animated PNG that stays one needs no FFmpeg. **Interface:** new `--format` and recipe `format`
  value `apng`; the report's `plan.type` is `animated` with `plan.format` `apng`; `peraz watch`
  picks up `.apng` files.
- SVG: an SVG is minified and stays an SVG, and a gzip-compressed `.svgz` stays one, kept only
  when smaller. Comments, the XML declaration, editor data (Inkscape, Illustrator, Sketch, and
  `<metadata>`), and whitespace between elements are removed, and numbers in paths, shapes, and
  transforms are written in their shortest form. `--quality` sets the rounding: none at
  `highest`, then 4, 3, 2, and 1 decimal places, never fewer than two significant digits. IDs,
  class names, `viewBox`, style sheets, scripts, text, and `<foreignObject>` content are never
  changed. With a `--format` that names a picture format, an SVG is skipped. No FFmpeg is needed.
  **Interface:** in `peraz probe` and reports an SVG has `kind` `image`, with `codec` `svg` and
  the container `svg` or `svgz`; the report's `plan.type` is `image`; `peraz watch` picks up
  `.svg` and `.svgz` files.
- Icon files: an ICO, CUR, or ICNS file is compressed and stays what it is, with every picture
  at its size, kept only when smaller. PNG pictures inside are recompressed as 8-bit RGBA
  (losslessly at `--quality highest`, with reduced colors below it where that is smaller), a
  256-pixel bitmap in an ICO becomes a PNG, and everything else is kept byte for byte: smaller
  bitmaps, JPEG 2000 and run-length pictures and masks in an ICNS, cursor hotspots. An ICNS's
  table of contents and its dark-mode variant are rewritten to match. With a `--format` that
  names a picture format, an icon file is skipped. No FFmpeg is needed. **Interface:** in
  `peraz probe` and reports an icon file has `kind` `image`, the container `ico`, `cur`, or
  `icns`, and the size of its largest picture; the report's `plan.type` is `image`; `peraz
  watch` picks up `.ico`, `.cur`, and `.icns` files.
- A BMP is compressed without `--format`: it becomes a PNG, kept only when smaller. Before, a BMP
  or TIFF needed an explicit `--format` and was skipped without one.
- `peraz compress --cancel-on-stdin` cancels the run like the first Ctrl-C when standard input
  sends a line `cancel` or is closed: the current file stops and its partial output is removed,
  the rest is skipped, and the exit code is 130. For programs that cannot send Ctrl-C or SIGTERM,
  such as a Windows app without a console. **Interface:** new flag.

### Changed

- With `--target-size` (and the `discord`, `email`, `github`, and `whatsapp` recipes), a picture
  that would be larger than the size after the usual compression is now fitted to it. Before,
  the size applied to videos only.
- A HEIC stays a HEIC by default, kept only when smaller; before, it became a JPEG. `--format
  jpeg` still makes the JPEG. With an FFmpeg that has no libx265, a HEIC without `--format`
  fails with a message that says so. **Interface:** the default output of HEIC inputs changed
  from JPEG to HEIC.
- An animated PNG is an animation, not a video: `kind` is `animation` in `peraz probe` and in
  reports (it was `video`), and by default it stays an animated PNG instead of becoming an MP4;
  `--format mp4` still makes a video from it. An animated PNG with a single frame is a still
  image and is compressed as a PNG. **Interface:** the `kind` and the default output of animated
  PNG inputs changed.

### Fixed

- A JPEG with an HDR gain map (Ultra HDR photos from phones) left the gain map's bytes behind
  when its metadata was removed: unreachable data after the picture's end, which made a
  lossless JPEG XL recompression up to 45% larger than needed. The picture now ends at its end
  marker, and the report's new `warnings` say that the HDR gain map was removed (keeping it is
  planned). **Interface:** reports have `warnings`, an array of strings, in `--json` and
  `--events`; the text output prints them as notes.
- A grayscale JPEG stays grayscale instead of being stored as three-channel color.
- With `--keep-metadata`, an orientation stated in the XMP packet (`tiff:Orientation`) is reset
  like the one in EXIF, so no viewer turns the picture a second time; and EXIF or XMP too large
  for a JPEG segment (64 kB) is reported in `warnings` instead of being dropped silently.
- A HEIC or AVIF stored as a tile grid with an odd width or height lost its last column or row
  of pixels when read.

## [0.5.0] - 2026-11-16

### Added

- `peraz compress --events` prints what happens during a run as JSON Lines, for programs that
  show progress: a `start` event, then per file `file_start`, `progress` (percent and an estimate
  of the time left), and `file_done` with the same report as `--json`, then `done` with the
  totals. Works with `--jobs` and `--check`, and cancelled files still get their `file_done`.
  **Interface:** new flag `--events` and the events documented in docs/json.md; `--json` is
  unchanged.
- `--measure-quality` (recipe key `measure_quality`) measures every result against its source
  without changing how it is encoded: VMAF for videos, on the same samples as `--target-vmaf`,
  and SSIMULACRA2 for still images. The report's `score` carries it. Results kept as the
  original, GIFs, animations, and documents have no score, and without libvmaf videos are not
  measured (a warning says so). **Interface:** new flag and recipe key; `score.target` is `null`
  for a measured result (it stays an integer whenever a target was asked for).

## [0.4.0] - 2026-11-15

### Added

- JPEG XL inputs: still JPEG XL images are read with libjxl's `djxl` (from the bundled build or
  `PATH`, next to `cjxl`) and stay JPEG XL unless `--format` says otherwise. They need no FFmpeg.
  A JPEG XL made losslessly from a JPEG is read as that JPEG, rebuilt exactly: with `--format
  jpeg` its original data is kept (metadata removed) when that is smaller than a new encode, and
  as JPEG XL it can be recompressed losslessly again. **Interface:** exit code `3` when an input
  is JPEG XL and `djxl` is missing; `peraz watch` picks up `.jxl` files.
- Animated JPEG XL inputs: `djxl` decodes the frames, and by default `cjxl` encodes them again as
  animated JPEG XL, kept only when smaller, with no FFmpeg needed. `--format gif`, `webp`, `avif`,
  and `mp4` convert them with FFmpeg, and without FFmpeg the run stops before any work with exit
  code `3`. `--max-width` and `--max-height` fail for an animation that stays JPEG XL, because
  `cjxl` cannot scale it. **Interface:** the report's `plan.type` is `image` (with the `jxl`
  encoding) when the animation stays JPEG XL.

### Fixed

- `--format mp4` from an APNG failed: its RGB color description was passed to the YUV video
  encoder. APNG inputs also report their length now, so progress and `--target-size` work.

## [0.3.0] - 2026-11-14

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

[Unreleased]: https://github.com/arsyadcc/peraz/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/arsyadcc/peraz/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/arsyadcc/peraz/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/arsyadcc/peraz/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/arsyadcc/peraz/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.4...v0.1.0
[0.1.0-alpha.4]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.3...v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.2...v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/arsyadcc/peraz/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/arsyadcc/peraz/releases/tag/v0.1.0-alpha.1
