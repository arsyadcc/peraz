# JSON output

Every command that takes `--json` prints JSON on standard output instead of text. Errors and
warnings still go to standard error, and the exit code means the same as without `--json` (see
[the command-line reference](cli.md#exit-codes)).

## Versioning

Every document starts with `schema_version`, currently **1**. Within one schema version:

- fields may be **added**: ignore fields you do not know;
- nothing is renamed or removed, no field changes its type, and no value is added to a list
  marked *One of*. Any such change comes with a new `schema_version`.

Two parts are informational and may change in any release, without a new schema version:

- everything inside `plan` except `plan.type`: how the file was encoded (encoder settings,
  FFmpeg arguments);
- free text: `encoder`, `outcome.message`, `error`, and `errors[]`. Show it to people; do not
  parse it.

Sizes are in bytes, durations in seconds, and paths are written as Peraz received or made them.
A field shown as *or null* is always present, with `null` when there is no value.

## `peraz compress --json`

One document for the whole run.

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | integer | The JSON schema version: 1. |
| `ffmpeg` | object | The FFmpeg build used. |
| `ffmpeg.version` | string | FFmpeg's version, e.g. `7.1.1`. |
| `ffmpeg.license` | string | One of `lgpl`, `gpl`, `nonfree`. |
| `recipe` | string or null | The recipe used: its name, or the path of a project `peraz.toml`. |
| `check` | object | Only with `--check`. |
| `check.threshold_percent` | integer | The `--check-threshold`. |
| `check.could_shrink[]` | array of objects | Files that could be made at least that much smaller. |
| `check.could_shrink[].path` | string | The input file. |
| `check.could_shrink[].input_bytes` | integer | Its size. |
| `check.could_shrink[].possible_bytes` | integer | The size Peraz reached. |
| `check.could_shrink[].saving_percent` | number | The saving, in percent. |
| `results[]` | array of [reports](#report) | One per input, in the order given. |
| `summary` | object | Totals over the results. |
| `summary.compressed` | integer | Results with status `compressed`. |
| `summary.kept_original` | integer | Results with status `kept_original`. |
| `summary.skipped` | integer | Results with status `skipped`. |
| `summary.failed` | integer | Results with status `failed`. |
| `summary.cancelled` | integer | Results with status `cancelled`. |
| `summary.input_bytes` | integer | Total size of the inputs that could be read. |
| `summary.saved_bytes` | integer | Total bytes saved by the compressed results. |

With `--check`, nothing is kept: `outcome.output` names a temporary file that no longer exists.

## `peraz watch --json`

One line per file, as it is processed (JSON Lines): a [report](#report) with `schema_version`
added at the top level.

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | integer | The JSON schema version: 1. |

## Report

What happened to one input, in `compress` results and `watch` lines.

| Field | Type | Description |
| --- | --- | --- |
| `input` | string | The input file. |
| `kind` | string or null | One of `video`, `image`, `animation`, `audio`, `pdf`, `epub`, `unknown`; null when the file could not be read. |
| `input_bytes` | integer or null | The input's size; null when it could not be read. |
| `outcome` | object | The result. |
| `outcome.status` | string | One of `compressed`, `kept_original`, `skipped`, `failed`, `cancelled`. |
| `outcome.output` | string | `compressed` only: the file written. |
| `outcome.output_bytes` | integer | `compressed` only: its size. |
| `outcome.reason` | string | `kept_original` only. One of `not_smaller`, `already_within_target`. |
| `outcome.compressed_bytes` | integer | `not_smaller` only: the size of the discarded result. |
| `outcome.target_bytes` | integer | `already_within_target` only: the `--target-size`. |
| `outcome.message` | string | `skipped` and `failed` only: why, as text. |
| `plan` | object or null | How the file was compressed; null when no plan was made. Informational, except `plan.type`. |
| `plan.type` | string | One of `video`, `image`, `gif`, `animated`, `document`, `trim`. |
| `encoder` | string or null | What did the encoding, as text, e.g. `libx265, HDR kept`. |
| `document` | object or null | PDF and EPUB only: what happened to the pictures inside. |
| `document.images` | integer | Pictures found. |
| `document.recompressed` | integer | Pictures replaced by a smaller version. |
| `document.kept` | integer | Pictures whose new version was not smaller. |
| `document.skipped` | integer | Pictures left alone (formats Peraz does not rewrite). |
| `elapsed_seconds` | number | Time spent on this file. |

`outcome.status` decides the exit code: any `cancelled` makes it 130, otherwise any `failed`
makes it 1.

## `peraz probe --json`

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | integer | The JSON schema version: 1. |
| `files[]` | array of objects | One per file, in the order given. |
| `files[].path` | string | The file. |
| `files[].media` | object or null | What Peraz found; null when the file could not be read. |
| `files[].error` | string or null | Why the file could not be read, as text. |
| `files[].media.kind` | string | One of `video`, `image`, `animation`, `audio`, `pdf`, `epub`, `unknown`. |
| `files[].media.container` | string | FFmpeg's names for the container, e.g. `mov,mp4,m4a,3gp,3g2,mj2`. |
| `files[].media.size_bytes` | integer | The file's size. |
| `files[].media.duration_seconds` | number or null | Duration, for videos, audio, and animations. |
| `files[].media.bit_rate` | integer or null | Overall bit rate, in bits per second. |
| `files[].media.video` | object or null | The main picture track. |
| `files[].media.video.codec` | string | FFmpeg's codec name, e.g. `h264`, `hevc`, `mjpeg`, `png`, `gif`. |
| `files[].media.video.width` | integer | Stored width, before rotation. |
| `files[].media.video.height` | integer | Stored height, before rotation. |
| `files[].media.video.rotation` | integer | Clockwise display rotation: 0, 90, 180, or 270. |
| `files[].media.video.frame_rate` | object or null | Frames per second, as a fraction. |
| `files[].media.video.frame_rate.num` | integer | Numerator. |
| `files[].media.video.frame_rate.den` | integer | Denominator. |
| `files[].media.video.frame_count` | integer or null | Number of frames, when the file says. |
| `files[].media.video.pixel_format` | string or null | FFmpeg's pixel format, e.g. `yuv420p10le`. |
| `files[].media.video.color` | object | The color description; each field is null when the file does not say. |
| `files[].media.video.color.primaries` | string or null | FFmpeg's name, e.g. `bt709`, `bt2020`. |
| `files[].media.video.color.transfer` | string or null | FFmpeg's name, e.g. `bt709`, `smpte2084` (PQ), `arib-std-b67` (HLG). |
| `files[].media.video.color.matrix` | string or null | FFmpeg's name, e.g. `bt709`, `bt2020nc`. |
| `files[].media.video.color.range` | string or null | `tv` (limited) or `pc` (full). |
| `files[].media.video.alpha` | string | One of `present`, `absent`, `unknown`. |
| `files[].media.video.bit_rate` | integer or null | The track's bit rate, in bits per second. |
| `files[].media.audio[]` | array of objects | Audio tracks, in file order. |
| `files[].media.audio[].codec` | string | FFmpeg's codec name, e.g. `aac`. |
| `files[].media.audio[].channels` | integer | Channel count. |
| `files[].media.audio[].sample_rate` | integer or null | Samples per second. |
| `files[].media.audio[].bit_rate` | integer or null | Bits per second. |

## `peraz doctor --json`

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | integer | The JSON schema version: 1. |
| `peraz` | string | Peraz's version. |
| `toolchain` | object | The FFmpeg programs Peraz runs. |
| `toolchain.ffmpeg` | string | Path of `ffmpeg`. |
| `toolchain.ffprobe` | string | Path of `ffprobe`. |
| `toolchain.source` | string | How they were found. One of `flag`, `env`, `path`. |
| `ffmpeg` | object | The FFmpeg build. |
| `ffmpeg.version` | string | FFmpeg's version. |
| `ffmpeg.license` | string | One of `lgpl`, `gpl`, `nonfree`. |
| `hardware_video_encoders[]` | array of objects | Hardware video encoders in this FFmpeg build. |
| `hardware_video_encoders[].name` | string | FFmpeg's encoder name, e.g. `hevc_videotoolbox`. |
| `hardware_video_encoders[].api` | string | The hardware API, e.g. `videotoolbox`, `nvenc`. |
| `hwaccels[]` | array of strings | FFmpeg's hardware decoding methods. |
| `encoders[]` | array of strings | Every encoder in this FFmpeg build. |

## `peraz recipe list --json` and `peraz recipe show --json`

`recipe list` prints the document below; `recipe show` prints one [recipe](#recipe) with
`schema_version` added at the top level.

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | integer | The JSON schema version: 1. |
| `recipes_dir` | string or null | Where your recipes go; null when no config folder is known. |
| `recipes[]` | array of [recipes](#recipe) | Built-in recipes and yours, by name. |
| `errors[]` | array of strings | Recipe files that could not be read, as text. |

## Recipe

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | The recipe's name. |
| `source` | string | One of `builtin`, `user`, `file`. |
| `path` | string | `user` and `file` only: the recipe file. |
| `settings` | object | The recipe's settings, with the keys of a [recipe file](recipes.md); keys that are not set are left out. |
| `settings.version` | integer | The recipe format version. |
| `settings.description` | string | |
| `settings.quality` | string | One of `highest`, `high`, `balanced`, `small`, `smallest`. |
| `settings.format` | string | One of `keep`, `jpeg`, `jpg`, `png`, `webp`, `avif`, `gif`, `mp4`. |
| `settings.max_width` | integer | |
| `settings.max_height` | integer | |
| `settings.codec` | string | One of `h264`, `hevc`. |
| `settings.encoder` | string | One of `auto`, `software`, `hardware`. |
| `settings.no_audio` | boolean | |
| `settings.target_size` | string | As written, e.g. `25MB`. |
| `settings.fps` | integer | |
