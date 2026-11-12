# Recipe format

A recipe is a named set of settings, written in TOML with the same names as the
[command-line flags](cli.md#settings-compress-and-watch), using `_` instead of `-`.

```toml
version = 1
description = "Pictures for our website"
quality = "balanced"
format = "webp"
max_width = 2000
max_height = 2000
```

## Keys

| Key | Type | Values | Flag |
| --- | --- | --- | --- |
| `version` | integer | `1` | |
| `description` | string | Shown by `peraz recipe list`. | |
| `quality` | string | `highest`, `high`, `balanced`, `small`, `smallest` | `--quality` |
| `format` | string | `keep`, `jpeg` (or `jpg`), `png`, `webp`, `avif`, `gif`, `mp4` | `--format` |
| `max_width` | integer | Pixels; never scales up. | `--max-width` |
| `max_height` | integer | Pixels; never scales up. | `--max-height` |
| `codec` | string | `h264`, `hevc` | `--codec` |
| `encoder` | string | `auto`, `software`, `hardware` | `--encoder` |
| `no_audio` | boolean | `true` removes all audio tracks. | `--no-audio` |
| `target_size` | string | A size such as `"25MB"` (SI units). Videos only. | `--target-size` |
| `fps` | integer | 1–50. GIFs and animated WebP/AVIF. | `--fps` |

Every key except `version` is optional, and a key that is not set takes the default. Unknown
keys and invalid values are errors, so a typo never passes silently.

## Versions

`version` is the format version. This Peraz reads and writes version 1, and refuses a recipe with
another version rather than guess what it means. New optional keys can be added within version
1; renaming or removing a key, or changing what a value means, needs version 2.

## Where recipes come from

Later places replace earlier ones with the same name:

1. built into Peraz (`peraz recipe list` shows them);
2. your recipes: `<config dir>/recipes/<name>.toml` (see
   [`PERAZ_CONFIG_DIR`](cli.md#environment-variables));
3. a file given to `--recipe` directly: anything with a path separator or ending in `.toml`.

Without `--recipe`, `peraz compress` and `peraz watch` use the nearest `peraz.toml` in the
current folder or its parents, unless `--ignore-project-recipe` is given. Flags on the command
line override the recipe's values.
