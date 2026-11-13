//! Every field Peraz prints with `--json` must be documented in `docs/json.md`, with values from
//! its *One of* lists, and those lists must name every value the code can produce. A field
//! renamed or added without updating the docs fails here; the docs say when that needs a new
//! `schema_version`.
//!
//! Skipped when the docs are not next to the crate (a crate from crates.io). The commands that
//! need FFmpeg and the samples from `scripts/make-fixtures.sh` are skipped without them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Field path → description, per `## ` section of `docs/json.md`.
type Docs = BTreeMap<String, BTreeMap<String, String>>;

fn docs() -> Option<Docs> {
    let text = std::fs::read_to_string(root().join("docs/json.md")).ok()?;
    let mut docs = Docs::new();
    let mut current = String::new();
    for line in text.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            title.clone_into(&mut current);
            continue;
        }
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<&str> = row.split(" | ").collect();
        let path = cells[0].trim_end_matches('`').to_owned();
        let description = cells
            .get(2)
            .unwrap_or(&"")
            .trim_end_matches(" |")
            .to_owned();
        docs.entry(current.clone())
            .or_default()
            .insert(path, description);
    }
    Some(docs)
}

/// The values listed after "One of" in a description.
fn one_of(description: &str) -> Option<BTreeSet<String>> {
    let list = description.split("One of ").nth(1)?;
    let list = list.split(['.', ';']).next()?;
    Some(
        list.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect(),
    )
}

/// Every path in a JSON value (arrays as `[]`), with the string values found at it.
fn paths(value: &Value, prefix: &str, out: &mut BTreeMap<String, BTreeSet<String>>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                // Arrays are documented as `name[]`.
                if !child.is_array() {
                    out.entry(path.clone()).or_default();
                }
                paths(child, &path, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            out.entry(path.clone()).or_default();
            for item in items {
                paths(item, &path, out);
            }
        }
        Value::String(text) => {
            out.entry(prefix.to_owned())
                .or_default()
                .insert(text.clone());
        }
        _ => {}
    }
}

/// Checks a document against a section; `nested` maps a path prefix (`results[].`) to the
/// section that documents what is inside it.
fn check(docs: &Docs, json: &Value, section: &str, nested: &[(&str, &str)]) {
    let mut found = BTreeMap::new();
    paths(json, "", &mut found);
    for (path, values) in found {
        let (section, path) = nested
            .iter()
            .find_map(|(prefix, inner)| path.strip_prefix(prefix).map(|rest| (*inner, rest)))
            .unwrap_or((section, path.as_str()));
        // `plan` is informational, except its type.
        if path.starts_with("plan.") && path != "plan.type" {
            continue;
        }
        let description = docs
            .get(section)
            .and_then(|fields| fields.get(path))
            .unwrap_or_else(|| {
                panic!("`{path}` is printed but not documented under \"{section}\" in docs/json.md")
            });
        if let Some(allowed) = one_of(description) {
            for value in values {
                assert!(
                    allowed.contains(&value),
                    "`{path}` is `{value}`, which docs/json.md does not list"
                );
            }
        }
    }
}

fn run(args: &[&str], config: &Path) -> (Value, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_peraz"))
        .args(args)
        .env("PERAZ_CONFIG_DIR", config)
        .current_dir(root())
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let json = serde_json::from_str(&text).unwrap_or_else(|e| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("peraz {args:?} printed no JSON ({e}): {text}{stderr}")
    });
    (json, output.status.code().unwrap_or(-1))
}

const COMPRESS: &str = "`peraz compress --json`";
const RECIPES: &str = "`peraz recipe list --json` and `peraz recipe show --json`";

#[test]
fn printed_json_matches_the_docs() {
    let Some(docs) = docs() else {
        eprintln!("skipped: docs/json.md not found");
        return;
    };
    let temp = std::env::temp_dir().join(format!("peraz-json-{}", std::process::id()));
    let config = temp.join("config");
    std::fs::create_dir_all(config.join("recipes")).unwrap();
    std::fs::write(
        config.join("recipes/mine.toml"),
        "version = 1\ndescription = \"Mine\"\nquality = \"small\"\nformat = \"webp\"\n\
         max_width = 800\nmax_height = 600\ncodec = \"hevc\"\nencoder = \"software\"\n\
         no_audio = true\ntarget_size = \"5MB\"\nfps = 10\n",
    )
    .unwrap();
    let recipe_file = temp.join("file.toml");
    std::fs::write(&recipe_file, "version = 1\nquality = \"high\"\n").unwrap();

    // Recipes need nothing installed.
    let (list, _) = run(&["recipe", "list", "--json"], &config);
    check(&docs, &list, RECIPES, &[("recipes[].", "Recipe")]);
    for name in ["mine", recipe_file.to_str().unwrap(), "web-images"] {
        let (mut show, _) = run(&["recipe", "show", name, "--json"], &config);
        // A recipe, with `schema_version` added at the top level.
        let version = show.as_object_mut().unwrap().remove("schema_version");
        assert_eq!(version, Some(Value::from(1)));
        check(&docs, &show, "Recipe", &[]);
    }

    // Without FFmpeg, doctor exits with code 3 and prints no JSON.
    let has_ffmpeg = Command::new(env!("CARGO_BIN_EXE_peraz"))
        .arg("doctor")
        .output()
        .is_ok_and(|o| o.status.success());
    let fixtures = root().join("fixtures");
    if !has_ffmpeg || !fixtures.join("image-photo.jpg").is_file() {
        eprintln!("skipped: compress, probe, and doctor need FFmpeg and the fixtures");
        let _ = std::fs::remove_dir_all(&temp);
        return;
    }
    let (doctor, _) = run(&["doctor", "--json"], &config);
    check(&docs, &doctor, "`peraz doctor --json`", &[]);

    let (probe, _) = run(
        &[
            "probe",
            "--json",
            "fixtures/video-1080p-audio.mp4",
            "fixtures/image-alpha.png",
            "fixtures/missing.mp4",
        ],
        &config,
    );
    check(&docs, &probe, "`peraz probe --json`", &[]);

    let out = temp.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out = out.to_str().unwrap();
    let runs: [&[&str]; 3] = [
        // compressed, failed, and a document
        &[
            "--out-dir",
            out,
            "fixtures/image-photo.jpg",
            "fixtures/book.epub",
            "fixtures/missing.png",
        ],
        // kept_original: already_within_target
        &[
            "--out-dir",
            out,
            "--target-size",
            "1GB",
            "fixtures/video-720p-noaudio.mp4",
        ],
        // --check writes nothing, so it takes no --out-dir
        &["--check", "fixtures/image-opaque.png"],
    ];
    for args in runs {
        let mut all = vec!["compress", "--json"];
        all.extend_from_slice(args);
        let (json, _) = run(&all, &config);
        check(&docs, &json, COMPRESS, &[("results[].", "Report")]);
    }
    let _ = std::fs::remove_dir_all(&temp);
}

/// The *One of* lists name every value the code can produce, no more.
#[test]
fn documented_values_are_complete() {
    use peraz_core::{Alpha, KeepReason, MediaKind, Outcome};
    let Some(docs) = docs() else { return };
    let listed = |section: &str, path: &str| {
        one_of(&docs[section][path]).unwrap_or_else(|| panic!("{path} has no One of list"))
    };
    let names = |values: &[Value], key: &str| -> BTreeSet<String> {
        values
            .iter()
            .map(|v| match v.get(key).unwrap_or(v) {
                Value::String(s) => s.clone(),
                other => panic!("{other}"),
            })
            .collect()
    };

    let outcomes = [
        Outcome::Compressed {
            output: PathBuf::new(),
            output_bytes: 0,
        },
        Outcome::KeptOriginal(KeepReason::NotSmaller {
            compressed_bytes: 0,
        }),
        Outcome::Skipped {
            message: String::new(),
        },
        Outcome::Failed {
            message: String::new(),
        },
        Outcome::Cancelled,
    ]
    .map(to_json);
    assert_eq!(
        names(&outcomes, "status"),
        listed("Report", "outcome.status")
    );

    let reasons = [
        KeepReason::NotSmaller {
            compressed_bytes: 0,
        },
        KeepReason::AlreadyWithinTarget { target_bytes: 0 },
    ]
    .map(to_json);
    assert_eq!(
        names(&reasons, "reason"),
        listed("Report", "outcome.reason")
    );

    let kinds = [
        MediaKind::Video,
        MediaKind::Image,
        MediaKind::Animation,
        MediaKind::Audio,
        MediaKind::Pdf,
        MediaKind::Epub,
        MediaKind::Unknown,
    ]
    .map(to_json);
    assert_eq!(names(&kinds, ""), listed("Report", "kind"));
    assert_eq!(
        names(&kinds, ""),
        listed("`peraz probe --json`", "files[].media.kind")
    );

    let alpha = [Alpha::Present, Alpha::Absent, Alpha::Unknown].map(to_json);
    assert_eq!(
        names(&alpha, ""),
        listed("`peraz probe --json`", "files[].media.video.alpha")
    );

    let words = |list: &[&str]| {
        list.iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        words(peraz_core::Quality::NAMES),
        listed("Recipe", "settings.quality")
    );
    assert_eq!(
        words(peraz_core::VideoCodec::NAMES),
        listed("Recipe", "settings.codec")
    );
    assert_eq!(
        words(peraz_core::EncoderPreference::NAMES),
        listed("Recipe", "settings.encoder")
    );
    for format in listed("Recipe", "settings.format") {
        assert!(
            format.parse::<peraz_core::settings::OutputFormat>().is_ok(),
            "{format}"
        );
    }
}

/// `toolchain.source` names every place Peraz finds FFmpeg.
#[test]
fn documented_tool_sources_are_complete() {
    use peraz_engine::ffmpeg::ToolSource;
    let Some(docs) = docs() else { return };
    let sources: BTreeSet<String> = [
        ToolSource::Flag,
        ToolSource::Env,
        ToolSource::Bundled,
        ToolSource::Path,
    ]
    .map(|source| to_json(source).as_str().unwrap().to_owned())
    .into_iter()
    .collect();
    assert_eq!(
        Some(sources),
        one_of(&docs["`peraz doctor --json`"]["toolchain.source"])
    );
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap()
}
