# Peraz `--json` output

The fields an agent needs most. The complete, versioned description is
https://github.com/arsyadcc/peraz/blob/main/docs/json.md.

Every document starts with `schema_version` (currently `1`). Within a schema version fields are
only added, never renamed or removed: ignore fields you do not know. Sizes are bytes, durations
seconds. Free text (`encoder`, `outcome.message`, `error`, `errors[]`) is for people; show it, do
not parse it. Errors and warnings go to standard error, and exit codes mean the same with or
without `--json`.

## `peraz compress --json`

| Field | Meaning |
| --- | --- |
| `ffmpeg` | `{version, license}` of the FFmpeg used, or null when none was needed. |
| `recipe` | The recipe used (name, or path of a project `peraz.toml`), or null. |
| `results[]` | One report per input, in the order given (below). |
| `summary` | `compressed`, `kept_original`, `skipped`, `failed`, `cancelled` (counts), `input_bytes`, `saved_bytes`. |
| `check` | Only with `--check`: `threshold_percent`, and `could_shrink[]` with `path`, `input_bytes`, `possible_bytes`, `saving_percent`. |

A report (also one JSON line per file from `peraz watch --json`):

| Field | Meaning |
| --- | --- |
| `input` | The input file. |
| `kind` | `video`, `image`, `animation`, `audio`, `pdf`, `epub`, `unknown`, or null when unreadable. |
| `input_bytes` | Input size, or null when unreadable. |
| `outcome.status` | `compressed`, `kept_original`, `skipped`, `failed`, `cancelled`. |
| `outcome.output`, `outcome.output_bytes` | `compressed` only: the file written and its size. |
| `outcome.reason` | `kept_original` only: `not_smaller` (with `outcome.compressed_bytes`) or `already_within_target` (with `outcome.target_bytes`). |
| `outcome.message` | `skipped` and `failed` only: why. |
| `encoder` | What encoded it, as text, e.g. `libx265, HDR kept`. |
| `document` | PDF and EPUB only: `images`, `recompressed`, `kept`, `skipped` (pictures inside). |
| `elapsed_seconds` | Time spent on this file. |

With `--check` nothing is kept: `outcome.output` names a temporary file that no longer exists.

## `peraz probe --json`

`files[]`, one per file: `path`, `error` (text or null), and `media` (null when unreadable) with
`kind`, `size_bytes`, `duration_seconds`, `bit_rate`, `video` (`codec`, `width`, `height` before
`rotation`, `frame_rate {num, den}`, `pixel_format`, `color {primaries, transfer, matrix, range}`,
`alpha`), and `audio[]` (`codec`, `channels`, `sample_rate`, `bit_rate`). HDR shows as
`color.transfer` `smpte2084` (HDR10) or `arib-std-b67` (HLG).

## `peraz doctor --json`

`peraz` (version), `toolchain` (`ffmpeg`, `ffprobe` paths and `source`: `flag`, `env`, `bundled`,
`path`), `ffmpeg` (`version`, `license`), `hardware_video_encoders[]` (`name`, `api`),
`hwaccels[]`, `encoders[]`. Without FFmpeg it prints no JSON and exits `3`.
