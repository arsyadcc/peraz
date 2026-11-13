# Recipes, project settings, and CI

## Recipes

A recipe is a named set of settings in TOML, with the same keys as the flags:

```toml
version = 1                      # required: the recipe format version
description = "Pictures for our website"
quality = "balanced"             # highest, high, balanced, small, smallest
format = "webp"                  # keep, jpeg, png, webp, avif, gif, mp4
max_width = 2000                 # pixels; never scales up
max_height = 2000
codec = "h264"                   # h264, hevc
encoder = "auto"                 # auto, software, hardware
no_audio = false
target_size = "25MB"             # SI units; videos only
fps = 12                         # 1-50; GIFs and animated WebP/AVIF
```

Every key except `version` is optional; unknown keys and invalid values are errors.

- `peraz recipe list` shows the built-in recipes and the user's own; `peraz recipe show <name>`
  prints one as TOML.
- Use one with `peraz --recipe <name or file.toml>`. Flags on the command line override its
  values.
- The user's recipes live in `<config dir>/recipes/<name>.toml`; `peraz recipe list` prints that
  folder. A user recipe with a built-in's name replaces it.

## Project settings: `peraz.toml`

A recipe saved as `peraz.toml` at a project's root applies to every `peraz` and `peraz watch` run
in that folder or below it, without `--recipe`. Flags still override it, and
`--ignore-project-recipe` skips it. The report's `recipe` field names the file used. Write one
when the user wants the same settings every time in a project.

## CI: check that files are compressed

`--check` writes nothing. It compresses into a temporary folder and exits `4` when any file could
be made at least `--check-threshold` percent smaller (default `10`, so files that are already
compressed pass). `--json` lists those files under `check.could_shrink[]`.

A GitHub Actions step, after installing Peraz:

```yaml
- name: Check that images are compressed
  run: peraz --check assets/**/*.{jpg,png,webp}
```

To fix a failing check, compress the listed files and replace them (the user's decision), then
commit the result.

## Watch folders

`peraz watch <folders>` compresses new files as they appear, with the same flags as `compress`.
`--out-dir` keeps the results apart, `--recursive` includes subfolders, `--include-existing` also
handles files that were already there, and `--json` prints one report per line. It runs until
stopped, so start it in the background or leave it to the user.
