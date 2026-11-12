# Command-line reference

The commands, flags, environment variables, and exit codes below are Peraz's interface for
people and scripts. From 0.1.0 on, renaming or removing any of them is a breaking change. The
[JSON output](json.md) and the [recipe format](recipes.md) have their own pages.

`peraz <files>` is short for `peraz compress <files>`: when the first argument is not a command,
`compress` is implied.

## Commands

| Command | What it does |
| --- | --- |
| `peraz compress <FILES>...` | Compress videos, images, GIFs, PDFs, and EPUBs. |
| `peraz probe <FILES>...` | Show what Peraz knows about media files. |
| `peraz doctor` | Show which FFmpeg Peraz uses and what it can encode. |
| `peraz watch <FOLDERS>...` | Compress new files as they appear in folders. |
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
| `-q`, `--quality` | `highest`, `high`, `balanced`, `small`, `smallest` | Quality level. Default `balanced`. |
| `--codec` | `h264`, `hevc` | Video codec. Default `h264`, or `hevc` for HDR videos (`h264` on an HDR video is an error). |
| `--encoder` | `auto`, `software`, `hardware` | Default `auto`: software if available, else hardware. |
| `--max-width` | pixels | Scale down to at most this width; never scales up. |
| `--max-height` | pixels | Scale down to at most this height; never scales up. |
| `--format` | `keep`, `jpeg`, `png`, `webp`, `avif`, `gif`, `mp4` | Output format. Default `keep`. For videos and GIFs, `webp` and `avif` make animated images, `gif` makes a GIF, and `mp4` turns a GIF into a video. |
| `--fps` | 1–50 | Frame rate of GIFs and animated WebP/AVIF; never above the source. |
| `--start` | time | Start this far into a video: seconds (`12.5`) or `[h:]m:ss`. |
| `--duration` | time | Use at most this much of a video. |
| `--trim-only` | | Only cut the clip, without compressing: the streams are copied when the start is on a keyframe. |
| `--snap-to-keyframe` | | With `--trim-only`: move the start back to the keyframe so the clip can always be copied. |
| `--target-size` | size | Fit each video in this size, e.g. `25MB` (SI units: 1 MB = 1,000,000 bytes). Replaces `--quality` for videos. |
| `--no-audio` | | Remove all audio tracks. |
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

## Environment variables

| Variable | Description |
| --- | --- |
| `PERAZ_FFMPEG` | The `ffmpeg` binary or its folder, used when `--ffmpeg` is not given. Otherwise Peraz looks on `PATH`. |
| `PERAZ_CONFIG_DIR` | The configuration folder, which holds `recipes/`. Default: `~/Library/Application Support/peraz` (macOS), `$XDG_CONFIG_HOME/peraz` or `~/.config/peraz` (Linux), `%APPDATA%\peraz` (Windows). |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, including files kept or skipped. |
| `1` | At least one input failed. |
| `2` | Invalid usage. |
| `3` | Environment problem, for example FFmpeg not found. |
| `4` | `--check` found files that could be smaller. |
| `130` | Cancelled with Ctrl-C or SIGTERM; the current file's partial output is removed. |

When several apply, the first in this order wins: 130, 1, 4.
