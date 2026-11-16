# Command-line reference

The commands, flags, environment variables, and exit codes below are Peraz's interface for
people and scripts. From 0.1.0 on, renaming or removing any of them is a breaking change. The
[JSON output](json.md) and the [recipe format](recipes.md) have their own pages.

`peraz <files>` is short for `peraz compress <files>`: when the first argument is not a command,
`compress` is implied.

## Without FFmpeg

Peraz runs FFmpeg for videos (also to make a GIF from one), HEIC, and AVIF, and to convert
animations. JPEG, PNG, WebP (animated PNG and WebP too), GIF, BMP, TIFF, JPEG XL, and SVG
images, icon files (ICO, CUR, ICNS),
PDFs, and EPUBs are handled without it, so `peraz compress` works on them when FFmpeg is
not installed: the results are the same as with FFmpeg, and `--json` prints `"ffmpeg": null`. If
any input needs FFmpeg, the run stops before any work with exit code `3` and names that file.
An FFmpeg chosen with `--ffmpeg` or `PERAZ_FFMPEG` that does not exist is always exit code `3`.
`peraz probe`, `peraz doctor`, and `peraz watch` need FFmpeg.

`--format heic` needs FFmpeg with the libx265 encoder; without FFmpeg, or with one that lacks
it, the run stops before any work with exit code `3`.

`--format jxl` needs libjxl's `cjxl` instead (and `djxl` with `--target-ssimulacra2`), found
through `PERAZ_CJXL`, the bundled build, or `PATH`; without it the run stops before any work with
exit code `3`. JPEG XL inputs are read with `djxl`, likewise exit code `3` when it is missing.
An animated JPEG XL stays JPEG XL without FFmpeg, a GIF stays a GIF, an animated PNG stays an
animated PNG, an animated WebP stays an animated WebP, and `--format apng` makes an animated
PNG from any of them. Turning one into another of GIF, WebP, AVIF, or MP4
needs FFmpeg, and the run stops with exit code `3` when it is missing.

## Bundled FFmpeg

`peraz ffmpeg install` downloads the FFmpeg build that this version of Peraz is tested with, for
the platform it runs on, from the Peraz repository's
[`ffmpeg-*` releases](https://github.com/arsyadcc/peraz/releases?q=ffmpeg-). The download is
checked against a SHA-256 built into `peraz` and unpacked into a `peraz-ffmpeg` folder next to the
`peraz` executable (after resolving symbolic links), which Peraz uses unless `--ffmpeg` or
`PERAZ_FFMPEG` names another FFmpeg. Run it again after updating Peraz: it does nothing when the
build is already there. It needs permission to write to the executable's folder; when it cannot,
or the download fails, it exits with code `3` and changes nothing. The `peraz-full` release
archives come with the same folder. The build also holds libjxl's `cjxl` and `djxl`, for JPEG XL.

## Commands

| Command | What it does |
| --- | --- |
| `peraz compress <FILES>...` | Compress videos, images, GIFs, PDFs, and EPUBs. |
| `peraz probe <FILES>...` | Show what Peraz knows about media files. |
| `peraz doctor` | Show which FFmpeg Peraz uses and what it can encode. |
| `peraz watch <FOLDERS>...` | Compress new files as they appear in folders. |
| `peraz ffmpeg install` | Download the FFmpeg build Peraz is tested with into a `peraz-ffmpeg` folder next to `peraz` ([details](#bundled-ffmpeg)). |
| `peraz ffmpeg uninstall` | Remove the `peraz-ffmpeg` folder next to `peraz`. |
| `peraz recipe list` | List the built-in recipes and yours. |
| `peraz recipe show <NAME>` | Print a recipe as TOML. |
| `peraz completions <SHELL>` | Print a shell completion script: `bash`, `zsh`, `fish`, `powershell`, or `elvish`. |
| `peraz help [COMMAND]` | Print help. |

## Flags

### Every command

| Flag | Value | Description |
| --- | --- | --- |
| `--ffmpeg` | path | The `ffmpeg` binary, or the folder holding `ffmpeg` and `ffprobe`. |
| `-h`, `--help` | | Print help. |
| `-V`, `--version` | | Print the version (`peraz --version` only). |

### Settings: `compress` and `watch`

The same settings can come from a [recipe](recipes.md); a flag overrides the recipe's value.

| Flag | Value | Description |
| --- | --- | --- |
| `-r`, `--recipe` | name or file | Use a recipe: a built-in or your own name, or a `.toml` file. |
| `--ignore-project-recipe` | | Do not use a `peraz.toml` from the current folder or its parents. |
| `-q`, `--quality` | `highest`, `high`, `balanced`, `small`, `smallest` | Quality level. Default `balanced`. Lossless for TIFF at every level, and for animated PNG, SVG, and icon files at `highest`. |
| `--codec` | `h264`, `hevc` | Video codec. Default `h264`, or `hevc` for HDR videos (`h264` on an HDR video is an error). |
| `--encoder` | `auto`, `software`, `hardware` | Default `auto`: software if available, else hardware. |
| `--max-width` | pixels | Scale down to at most this width; never scales up. |
| `--max-height` | pixels | Scale down to at most this height; never scales up. |
| `--format` | `keep`, `jpeg`, `png`, `webp`, `avif`, `jxl`, `heic`, `tiff`, `gif`, `apng`, `mp4` | Output format. Default `keep`: every file stays in its format, except that a BMP or a still GIF becomes a PNG when that is smaller. `heic` applies to still images. `jxl` (JPEG XL) applies to still images and animated JPEG XL. `tiff` applies to still images and is always lossless: Deflate compression at the picture's own bit depth, so `--quality` and `--target-ssimulacra2` do not change it. For videos and animations (GIF, animated PNG, animated JPEG XL), `webp` and `avif` make animated images, `gif` makes a GIF, `apng` makes an animated PNG, and `mp4` turns an animation into a video. `apng` keeps every frame of a GIF, an animated WebP, or an animated JPEG XL with its timing, takes frames from a video at `--fps`, and makes a PNG from a still image. An animated PNG stays an animated PNG with `keep`, and an animated WebP an animated WebP (also with `webp`): every frame, with its timing and loop count. |
| `--fps` | 1–50 | Frame rate of GIFs, animated WebP/AVIF, and animated PNGs made from videos; never above the source. A GIF that stays a GIF keeps every frame with its own delay unless `--fps` is given: then frames are dropped down to that rate, and each kept frame takes the time of the frames dropped after it. An animated PNG made from an animation always keeps every frame. |
| `--start` | time | Start this far into a video: seconds (`12.5`) or `[h:]m:ss`. |
| `--duration` | time | Use at most this much of a video. |
| `--trim-only` | | Only cut the clip, without compressing: the streams are copied when the start is on a keyframe. |
| `--snap-to-keyframe` | | With `--trim-only`: move the start back to the keyframe so the clip can always be copied. |
| `--target-size` | size | Fit each video and each still picture in this size, e.g. `25MB` or `500kB` (SI units: 1 MB = 1,000,000 bytes). Replaces `--quality` for videos. A picture gets the usual compression when that fits; otherwise its quality is lowered, down to a floor below which pictures look broken, and only then is it scaled down, step by step, until it fits. The result's real size is checked, and when nothing fits the file fails and the original stays. Applies to JPEG, PNG, WebP, AVIF, HEIC, and JPEG XL output. A TIFF is lossless, so it either fits as it is or fails with a message that says the size needs a lossy format. Animations, SVGs, icon files, PDFs, and EPUBs are not fitted to a size, and neither are pictures when `--target-ssimulacra2` is given. |
| `--keep-dimensions` | | With `--target-size`: never scale a picture down. When lower quality alone cannot fit the size, the file fails with a message that says how small it got. |
| `--target-vmaf` | 1–100 | Make each video as small as it can be while it still scores this VMAF. Replaces `--quality` for videos. Needs FFmpeg with libvmaf and a software encoder; cannot be combined with `--target-size`. |
| `--target-ssimulacra2` | 1–100 | Make each still image as small as it can be while it still scores this SSIMULACRA2. Replaces `--quality` for still images. |
| `--no-audio` | | Remove all audio tracks. |
| `--measure-quality` | | Measure every result against its source, also without a score target, and report it in `score`: VMAF for videos (on samples of videos over 30 seconds; needs FFmpeg with libvmaf, otherwise videos are not measured and a warning says so), SSIMULACRA2 for still images. Encoding does not change. Measuring takes extra time: about as long as compressing for videos, a fraction of a second per picture. |
| `--keep-metadata` | | Keep the metadata of pictures: EXIF, XMP, and IPTC. That includes the GPS location, camera, and dates, so leave it out for pictures you share. Without it, all of this is removed. The color profile is kept either way, and so is the right way up: the pixels are stored upright and the orientation in the kept EXIF is reset. EXIF and XMP are kept in JPEG, PNG, WebP, HEIC, and JPEG XL; AVIF keeps the EXIF only, TIFF the XMP and IPTC but not the EXIF, and JPEG the IPTC too. BMP and GIF pictures hold no metadata. Videos, GIFs, animations, SVGs, icon files, and documents are not affected. |
| `--recompress` | | Also compress files that Peraz made itself. Without it, a result of an earlier run is skipped (unless it was changed since): see [Output files](#output-files). |
| `-j`, `--jobs` | number | How many files to work on at once. Default: one per logical core. |
| `--json` | | Print [JSON](json.md) instead of text. |

### `compress` only

| Flag | Value | Description |
| --- | --- | --- |
| `-o`, `--output` | file | The output file (a single input only). |
| `--out-dir` | folder | Write outputs into this folder instead of next to each input; it is made if it does not exist. |
| `-f`, `--force` | | With `--output`: replace that file if it exists. |
| `--check` | | Write nothing; exit with code 4 when a file could be made at least `--check-threshold` percent smaller. |
| `--check-threshold` | 1–99 | With `--check`: the smallest saving, in percent, that counts. Default 10. |
| `--events` | | Print [events](json.md#peraz-compress---events) as they happen, one JSON object per line, instead of text: each file's start, progress, and report, then the totals. For programs that show progress. Cannot be combined with `--json`. |
| `--cancel-on-stdin` | | Cancel the run like the first Ctrl-C when standard input sends a line `cancel` or is closed; other lines are ignored. For programs that cannot send Ctrl-C or SIGTERM, such as a Windows app without a console: start `peraz` with a pipe as its standard input, and write `cancel` or close the pipe. Without this flag, standard input is not read. |

### `watch` only

| Flag | Value | Description |
| --- | --- | --- |
| `--out-dir` | folder | Write outputs into this folder; subfolders are mirrored with `--recursive`. |
| `--recursive` | | Also watch subfolders. |
| `--include-existing` | | Also compress files that were there before watching started. |

### `probe` and `doctor`

| Flag | Value | Description |
| --- | --- | --- |
| `--json` | | Print [JSON](json.md) instead of text. |

### `ffmpeg install` only

| Flag | Value | Description |
| --- | --- | --- |
| `--force` | | Download and install the build again, even if it is already installed. |

### `recipe list` and `recipe show`

| Flag | Value | Description |
| --- | --- | --- |
| `--json` | | Print [JSON](json.md) instead of text. |
| `--names` | | `recipe list` only: print only the names, one per line. |

## Output files

Without `--output`, a result is written as `<name>-1.<ext>` next to the input (or in `--out-dir`),
and as `<name>-2.<ext>`, `<name>-3.<ext>`, … when that name is taken: existing files are never
replaced. `--output` writes exactly the file given and replaces it only with `--force`. A result
that is not smaller than the input is not kept, unless the format was changed on purpose.

Every result is marked as made by Peraz, without changing its contents: in an extended attribute
(`user.peraz.compressed`) on macOS and Linux, in an alternate data stream (`peraz.compressed`)
on Windows. A marked file is skipped when it is given to `peraz compress` or found by `peraz
watch` again, with `outcome.reason` `already_compressed` in the JSON output, so dropping the same
folder twice does not compress results a second time. The mark holds the file's size and
modification time: a result that was edited afterwards is compressed like any other file.
`--recompress` compresses marked files too. Clips cut with `--trim-only` are not marked. On a
file system without extended attributes, and after a copy that does not carry them (some sync
and archive tools), nothing is marked and nothing is skipped.

A TIFF with more than one page is skipped, whatever the format asked for: Peraz compresses
single pictures, and would drop the other pages.

An SVG (or a gzip-compressed `.svgz`) is minified and stays what it is. `--quality` sets how far
its numbers are rounded (`highest` rounds nothing); `--max-width`, `--max-height`,
`--target-ssimulacra2`, and `--measure-quality` do not apply to it. With a `--format` that names
a picture format it is skipped, because an SVG is not converted. An SVG in a text encoding other
than UTF-8 is skipped too.

An icon file (ICO, CUR, ICNS) keeps its format and every picture inside at its size; the same
flags do not apply to it, and with a `--format` that names a picture format it is skipped.
`--quality highest` keeps every pixel of its pictures.

## PDF files

Peraz changes only the pictures inside a PDF; text, fonts, layout, links, and forms stay as they
are. Streams that are Flate-compressed are deflated again at the best level, which keeps what
they hold byte for byte. Some files and pictures are left alone on purpose:

| What | What Peraz does | Why |
| --- | --- | --- |
| A PDF that needs a password to open | Skips it (`reason` `protected`). | It cannot be read, and Peraz never removes protection. |
| A signed PDF: its form says so (`SigFlags`), or it holds a signature with a `ByteRange` | Skips it (`reason` `protected`). | A signature covers exact bytes of the file, so any rewrite breaks it. |
| A PDF that opens without a password but restricts printing, copying, or changing | Compresses it, and encrypts the result the same way: the same restrictions, the same owner password, still no password to open. | The restrictions are the author's; they are never removed. |
| A PDF/A (its metadata has `pdfaid:part`) | Compresses it, and writes it without object streams, with the kind of cross-reference data and the version it had, and with uncompressed metadata. | PDF/A-1 is PDF 1.4 and allows none of those. |
| A linearized PDF (fast web view) | Compresses it; a warning says that the result is not linearized. | Peraz does not write linearized files yet. |
| A picture with a color key (`Mask` with color values) | Keeps it exact. | Exactly those values are see-through; a palette or a JPEG would move them. |
| A soft mask or stencil mask | Keeps it exact. | Its edges are the picture's outline. |
| A picture blended with a matte color (its soft mask has `Matte`) | Recompresses it at its size, never scaled down. | The picture and its mask belong together pixel by pixel. |
| A picture that takes more than 512 MB once decoded, or whose data expands beyond what its size holds | Leaves it as it is, with a warning. | A small file must not be able to ask for gigabytes of memory. |
| CMYK, Lab, indexed, and spot-color pictures; JPEG 2000, JBIG2, and fax pictures; depths other than 8 bits | Leaves them as they are (counted as `skipped` in `document`). | Peraz does not rewrite these yet. |

Before a new PDF is written, Peraz reads it back and compares the number of pages with the
original. If it does not read back the same, the file fails and nothing is written.

## Environment variables

| Variable | Description |
| --- | --- |
| `PERAZ_FFMPEG` | The `ffmpeg` binary or its folder, used when `--ffmpeg` is not given. Otherwise Peraz uses the bundled build in a `peraz-ffmpeg` folder next to the `peraz` executable, if there is one, and then looks on `PATH`. |
| `PERAZ_CJXL` | The `cjxl` binary for JPEG XL output. Otherwise Peraz uses the one in the bundled build, then looks on `PATH`. `djxl` is looked up next to it, then on `PATH`. |
| `PERAZ_CONFIG_DIR` | The configuration folder, which holds `recipes/`. Default: `~/Library/Application Support/peraz` (macOS), `$XDG_CONFIG_HOME/peraz` or `~/.config/peraz` (Linux), `%APPDATA%\peraz` (Windows). |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, including files kept or skipped. |
| `1` | At least one input failed. |
| `2` | Invalid usage. |
| `3` | Environment problem, for example FFmpeg not found when a file needs it ([Without FFmpeg](#without-ffmpeg)), `cjxl` not found with `--format jxl` (`djxl` with a JPEG XL input), or an FFmpeg without libx265 with `--format heic`. |
| `4` | `--check` found files that could be smaller. |
| `130` | Cancelled with Ctrl-C, SIGTERM, or `--cancel-on-stdin`; the current file's partial output is removed. |

When several apply, the first in this order wins: 130, 1, 4.
