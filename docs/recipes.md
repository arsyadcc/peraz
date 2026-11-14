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
| `format` | string | `keep`, `jpeg` (or `jpg`), `png`, `webp`, `avif`, `jxl`, `gif`, `mp4` | `--format` |
| `max_width` | integer | Pixels; never scales up. | `--max-width` |
| `max_height` | integer | Pixels; never scales up. | `--max-height` |
| `codec` | string | `h264`, `hevc` | `--codec` |
| `encoder` | string | `auto`, `software`, `hardware` | `--encoder` |
| `no_audio` | boolean | `true` removes all audio tracks. | `--no-audio` |
| `target_size` | string | A size such as `"25MB"` (SI units). Videos only. | `--target-size` |
| `target_vmaf` | integer | 1–100. Videos only; not with `target_size`. | `--target-vmaf` |
| `target_ssimulacra2` | integer | 1–100. Still images only. | `--target-ssimulacra2` |
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

## Upload limits

These built-in recipes fit each video under a service's upload limit with `target_size`, so the
video is shrunk only as far as it has to be. The limit is per file: send several files to one
email and their total counts. Pictures get the usual compression and are not fitted to the limit.
Limits change; these were checked on 2026-11-14.

| Recipe | Target | Limit it fits | Source |
| --- | --- | --- | --- |
| `discord` | 20 MB | 20 MB per file without Nitro (since 2026-08-13) | [Discord's announcement, reported by Dexerto](https://www.dexerto.com/entertainment/discord-raises-free-upload-limit-to-20mb-after-lowering-it-two-years-ago-3398251/) |
| `email` | 14 MB | 20 MB in Outlook (25 MB in Gmail) for all attachments; base64 makes a file about a third larger on the way | [Outlook](https://learn.microsoft.com/en-us/troubleshoot/outlook/message-body/attachment-size-exceeds-the-allowable-limit-error), [Gmail](https://support.google.com/mail/answer/6584) |
| `github` | 10 MB | 10 MB per video in issues and pull requests of repositories on free plans | [GitHub Docs](https://docs.github.com/en/get-started/writing-on-github/working-with-advanced-formatting/attaching-files) |
| `whatsapp` | 64 MB, 720p | 64 MB on slower connections (100 MB on faster ones), sent at up to 720p | [WhatsApp Help Center](https://faq.whatsapp.com/web/chats/how-to-send-media/?locale=en_US) |

For another limit, use `--target-size` (Discord with Nitro, or GitHub on a paid plan, for
example), or write your own recipe with the same name to replace one of these.
