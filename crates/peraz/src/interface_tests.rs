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
    let real = BTreeSet::from([peraz_engine::ffmpeg::FFMPEG_ENV, crate::recipes::CONFIG_ENV]);
    assert_eq!(documented, real);
}
