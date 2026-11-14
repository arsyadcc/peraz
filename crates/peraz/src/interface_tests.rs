//! The documented command-line interface (`docs/cli.md`) must match the real one: every command
//! and flag, the exit codes, and the JSON schema version (`docs/json.md`). The tests are skipped
//! when the docs are not next to the crate (a crate downloaded from crates.io).

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::CommandFactory as _;

use crate::{Cli, SCHEMA_VERSION, exit};

fn doc(name: &str) -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(name);
    let text = std::fs::read_to_string(&path).ok();
    if text.is_none() {
        eprintln!("skipped: {} not found", path.display());
    }
    text
}

/// The lines of the section that starts with the heading `## {title}`.
fn section<'a>(text: &'a str, title: &str) -> Vec<&'a str> {
    let heading = format!("## {title}");
    text.lines()
        .skip_while(|line| *line != heading)
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .collect()
}

/// The backticked words in the first cell of each table row.
fn first_cells<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|row| row.split(" |").next())
        .flat_map(|cell| cell.split('`').skip(1).step_by(2))
        .collect()
}

/// Every flag of the command tree, as `--long` and `-s`.
fn real_flags(command: &clap::Command, flags: &mut BTreeSet<String>) {
    for arg in command.get_arguments() {
        if let Some(long) = arg.get_long() {
            flags.insert(format!("--{long}"));
        }
        if let Some(short) = arg.get_short() {
            flags.insert(format!("-{short}"));
        }
    }
    for sub in command.get_subcommands() {
        real_flags(sub, flags);
    }
}

/// `peraz a`, `peraz a b`, … for every command except help.
fn real_commands(command: &clap::Command, prefix: &str, out: &mut BTreeSet<String>) {
    for sub in command.get_subcommands().filter(|c| c.get_name() != "help") {
        let name = format!("{prefix} {}", sub.get_name());
        if sub.has_subcommands() {
            real_commands(sub, &name, out);
        } else {
            out.insert(name);
        }
    }
}

#[test]
fn every_flag_is_documented_and_every_documented_flag_exists() {
    let Some(text) = doc("cli.md") else { return };
    let mut command = Cli::command();
    command.build(); // adds the generated --help and --version
    let mut real = BTreeSet::new();
    real_flags(&command, &mut real);
    let documented: BTreeSet<String> = first_cells(&section(&text, "Flags"))
        .into_iter()
        .filter(|word| word.starts_with('-'))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        documented, real,
        "docs/cli.md and the command line differ: update the docs (renaming or removing a \
         flag is a breaking change from 0.1.0 on)"
    );
}

#[test]
fn every_command_is_documented() {
    let Some(text) = doc("cli.md") else { return };
    let mut real = BTreeSet::new();
    real_commands(&Cli::command(), "peraz", &mut real);
    let documented: BTreeSet<String> = first_cells(&section(&text, "Commands"))
        .into_iter()
        .map(|cell| {
            // `peraz recipe show <NAME>` → `peraz recipe show`
            cell.split(' ')
                .take_while(|word| !word.starts_with('<') && !word.starts_with('['))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|command| command != "peraz help")
        .collect();
    assert_eq!(documented, real);
}

#[test]
fn exit_codes_match_the_docs() {
    let Some(text) = doc("cli.md") else { return };
    let documented: BTreeSet<u8> = first_cells(&section(&text, "Exit codes"))
        .into_iter()
        .map(|code| code.parse().unwrap())
        .collect();
    let real = BTreeSet::from([
        exit::SUCCESS,
        exit::INPUT_FAILED,
        exit::USAGE,
        exit::ENVIRONMENT,
        exit::CHECK_FAILED,
        exit::CANCELLED,
    ]);
    assert_eq!(documented, real);
}

#[test]
fn the_documented_schema_version_is_the_real_one() {
    let Some(text) = doc("json.md") else { return };
    assert!(
        text.contains(&format!("currently **{SCHEMA_VERSION}**")),
        "docs/json.md names another schema version than {SCHEMA_VERSION}"
    );
}

#[test]
fn every_environment_variable_is_documented() {
    let Some(text) = doc("cli.md") else { return };
    let documented: BTreeSet<&str> = first_cells(&section(&text, "Environment variables"))
        .into_iter()
        .collect();
    let real = BTreeSet::from([
        peraz_engine::ffmpeg::FFMPEG_ENV,
        peraz_engine::jxl::CJXL_ENV,
        crate::recipes::CONFIG_ENV,
    ]);
    assert_eq!(documented, real);
}

/// The agent skill in `agents/peraz` (the Claude Code and Codex plugin): every Markdown file, or
/// `None` when the repository is not next to the crate.
fn skill_files() -> Option<Vec<(PathBuf, String)>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../agents/peraz/skills/peraz");
    let mut files = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                let text = std::fs::read_to_string(&path).ok()?;
                files.push((path, text));
            }
        }
    }
    Some(files)
}

/// The code in Markdown: inline `code` spans and fenced blocks.
fn code_in(text: &str) -> Vec<String> {
    let mut code = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match block.take() {
                Some(finished) => code.push(finished),
                None => block = Some(String::new()),
            }
        } else if let Some(block) = &mut block {
            block.push_str(line);
            block.push('\n');
        } else {
            code.extend(line.split('`').skip(1).step_by(2).map(str::to_owned));
        }
    }
    code
}

#[test]
fn the_agent_skill_names_only_real_flags() {
    let Some(files) = skill_files() else { return };
    let mut command = Cli::command();
    command.build();
    let mut real = BTreeSet::new();
    real_flags(&command, &mut real);
    for (path, text) in files {
        // Flags after `peraz` on a line, or a snippet of flags alone (`--max-width 1280`); other
        // programs' flags (`cargo install --locked peraz`) are not Peraz's.
        let lines: Vec<String> = code_in(&text)
            .iter()
            .flat_map(|code| code.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect();
        for line in lines {
            let words: Vec<&str> = line.split_whitespace().collect();
            let start = match words.iter().position(|w| *w == "peraz") {
                Some(i) => i + 1,
                None if words.first().is_some_and(|w| w.starts_with('-')) => 0,
                None => continue,
            };
            for word in &words[start..] {
                let word = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | ':'));
                let flag = word.split('=').next().unwrap_or(word);
                let is_long = flag.starts_with("--") && flag[2..].starts_with(char::is_alphabetic);
                let is_short = flag.len() == 2
                    && flag.starts_with('-')
                    && flag[1..].starts_with(char::is_alphabetic);
                if is_long || is_short {
                    assert!(
                        real.contains(flag),
                        "{} names {flag}, which peraz does not have",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn the_agent_skill_names_only_real_commands() {
    let Some(files) = skill_files() else { return };
    let cli = Cli::command();
    for (path, text) in files {
        for code in code_in(&text) {
            let words: Vec<&str> = code.split_whitespace().collect();
            for (i, _) in words.iter().enumerate().filter(|(_, w)| **w == "peraz") {
                // `peraz recipe show <name>` → ["recipe", "show"]; files and flags end it.
                let names: Vec<&str> = words[i + 1..]
                    .iter()
                    .take_while(|w| w.chars().all(|c| c.is_ascii_lowercase()))
                    .copied()
                    .collect();
                let Some(first) = names.first() else { continue };
                let sub = cli.find_subcommand(first).unwrap_or_else(|| {
                    panic!(
                        "{} names `peraz {first}`, which does not exist",
                        path.display()
                    )
                });
                if sub.has_subcommands() {
                    let second = names.get(1).copied().unwrap_or_default();
                    assert!(
                        sub.find_subcommand(second).is_some(),
                        "{} names `peraz {first} {second}`, which does not exist",
                        path.display()
                    );
                }
            }
        }
    }
}

/// The plugin manifests for Claude Code and Codex agree with each other and with this version.
#[test]
fn the_agent_plugin_manifests_match_this_version() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |path: &str| -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(root.join(path)).ok()?;
        Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}")))
    };
    let Some(claude_plugin) = read("agents/peraz/.claude-plugin/plugin.json") else {
        return;
    };
    let codex_plugin = read("agents/peraz/plugin.json").unwrap();
    for plugin in [&claude_plugin, &codex_plugin] {
        assert_eq!(plugin["name"], "peraz");
        assert_eq!(
            plugin["version"],
            env!("CARGO_PKG_VERSION"),
            "the plugin's version follows Peraz's: update both plugin.json files"
        );
    }
    let claude_market = read(".claude-plugin/marketplace.json").unwrap();
    let codex_market = read(".agents/plugins/marketplace.json").unwrap();
    for source in [
        &claude_market["plugins"][0]["source"],
        &codex_market["plugins"][0]["source"]["path"],
    ] {
        assert_eq!(source, "./agents/peraz");
    }
}
