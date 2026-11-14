//! `peraz completions <shell>`: shell completion scripts.
//!
//! The scripts are generated from the command-line definition. Options that take a fixed word
//! (`--quality`, `--codec`, …) offer their values, and in zsh:
//! - `peraz <FILES>` is short for `peraz compress <FILES>`, so a first word that is not a
//!   command completes like `compress` (files and its options);
//! - recipe names are completed by running `peraz recipe list --names` at the time of
//!   completion, so your own recipes appear too. Fish gets the recipe names as well.

use std::io::Write as _;

use clap::CommandFactory as _;
use clap::builder::PossibleValuesParser;
use clap_complete::Shell;
use peraz_core::{EncoderPreference, Quality, VideoCodec};

use crate::{Cli, exit};

#[derive(Debug, clap::Args)]
#[command(after_help = "Examples:\n  \
    peraz completions zsh > \"${fpath[1]}/_peraz\"\n  \
    peraz completions bash > ~/.local/share/bash-completion/completions/peraz\n  \
    peraz completions fish > ~/.config/fish/completions/peraz.fish")]
pub struct Args {
    /// The shell to complete for.
    shell: Shell,
}

pub fn run(args: &Args) -> u8 {
    let script = script(args.shell);
    let mut out = std::io::stdout().lock();
    match out.write_all(script.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => exit::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => exit::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            exit::ENVIRONMENT
        }
    }
}

/// Values of `--format`, in the order help lists them.
const FORMATS: &[&str] = &["keep", "jpeg", "png", "webp", "avif", "jxl", "gif", "mp4"];

/// The command line as completion sees it.
fn completion_command() -> clap::Command {
    let cli = with_value_lists(Cli::command());
    let names: Vec<String> = cli
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect();
    names
        .iter()
        .fold(cli, |cli, name| cli.mut_subcommand(name, with_value_lists))
}

/// Lists the accepted values of the options that take a fixed word, so shells can offer them
/// (the real parsers accept the same words; this only changes what completion shows).
fn with_value_lists(command: clap::Command) -> clap::Command {
    [
        ("quality", Quality::NAMES),
        ("codec", VideoCodec::NAMES),
        ("encoder", EncoderPreference::NAMES),
        ("format", FORMATS),
    ]
    .into_iter()
    .fold(command, |command, (id, values)| {
        if command.get_arguments().any(|a| a.get_id() == id) {
            command.mut_arg(id, |arg| {
                arg.value_parser(PossibleValuesParser::new(values.iter().copied()))
            })
        } else {
            command
        }
    })
}

/// The completion script for `shell`.
pub fn script(shell: Shell) -> String {
    let mut command = completion_command();
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, "peraz", &mut buffer);
    let script = String::from_utf8_lossy(&buffer).into_owned();
    match shell {
        Shell::Zsh => dynamic_zsh(&script),
        Shell::Fish => dynamic_fish(&script),
        _ => script,
    }
}

/// How the generated zsh script completes a recipe argument: as any file.
const ZSH_RECIPE_FLAG: &str = ":NAME|FILE:_default'";
const ZSH_RECIPE_SHOW: &str = ":name -- Recipe name or path to a .toml file:_default'";

/// Completes recipe names, and `.toml` files, for `--recipe` and `peraz recipe show`.
fn dynamic_zsh(script: &str) -> String {
    let commands: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .chain(["help".to_owned()])
        .collect();
    let helper = format!(
        r#"(( $+functions[_peraz_recipes] )) ||
_peraz_recipes() {{
    local -a recipes
    recipes=(${{(f)"$(peraz recipe list --names 2>/dev/null)"}})
    _alternative 'recipes:recipe:compadd -a recipes' 'files:recipe file:_files -g "*.toml"'
}}

# `peraz <FILES>` means `peraz compress <FILES>`: complete it that way.
_peraz() {{
    local -a commands=({commands})
    if (( CURRENT == 2 )) && [[ $PREFIX != -* ]]; then
        _alternative 'commands:command:_peraz_commands' 'files:file:_files'
        return
    fi
    if (( ! ${{commands[(Ie)${{words[2]}}]}} )); then
        words=($words[1] compress "${{(@)words[2,-1]}}")
        (( CURRENT += 1 ))
    fi
    _peraz_generated "$@"
}}

"#,
        commands = commands.join(" ")
    );
    let script = script
        .replacen("\n_peraz() {\n", "\n_peraz_generated() {\n", 1)
        .replace(ZSH_RECIPE_FLAG, ":NAME|FILE:_peraz_recipes'")
        .replace(
            ZSH_RECIPE_SHOW,
            ":name -- Recipe name or path to a .toml file:_peraz_recipes'",
        );
    match script.find("if [ \"$funcstack[1]\" = \"_peraz\" ]") {
        Some(at) => format!("{}{helper}{}", &script[..at], &script[at..]),
        None => script,
    }
}

/// Completes recipe names for `--recipe` (files still complete too) and `peraz recipe show`.
fn dynamic_fish(script: &str) -> String {
    const NAMES: &str = "-a \"(peraz recipe list --names 2>/dev/null)\"";
    let mut out: String = script
        .lines()
        .map(|line| {
            if line.contains(" -l recipe ") && line.ends_with(" -r") {
                format!("{line} {NAMES}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    out.push_str("complete -c peraz -n \"__fish_peraz_using_subcommand recipe; and __fish_seen_subcommand_from show\" -f ");
    out.push_str(NAMES);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_gets_a_script() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let script = script(shell);
            assert!(script.contains("peraz"), "{shell}");
            // Fixed words are offered as values.
            assert!(script.contains("smallest"), "{shell}: quality values");
            assert!(script.contains("hevc"), "{shell}: codec values");
        }
    }

    #[test]
    fn listed_formats_are_accepted() {
        for format in FORMATS {
            assert!(
                format.parse::<peraz_core::settings::OutputFormat>().is_ok(),
                "{format}"
            );
        }
    }

    /// Guards the text replacements against changes in the generated scripts.
    #[test]
    fn zsh_and_fish_complete_recipe_names() {
        let mut command = completion_command();
        let mut raw = Vec::new();
        clap_complete::generate(Shell::Zsh, &mut command, "peraz", &mut raw);
        let raw = String::from_utf8(raw).unwrap();
        // -r and --recipe under compress and under watch; and `recipe show`.
        assert_eq!(raw.matches(ZSH_RECIPE_FLAG).count(), 4, "zsh --recipe");
        assert_eq!(raw.matches(ZSH_RECIPE_SHOW).count(), 1, "zsh recipe show");
        let zsh = script(Shell::Zsh);
        assert_eq!(zsh.matches(":_peraz_recipes'").count(), 5);
        assert!(zsh.contains("_peraz_recipes() {"));
        // The generated function is wrapped by one that knows the `compress` shortcut.
        assert_eq!(raw.matches("\n_peraz() {\n").count(), 1);
        assert_eq!(zsh.matches("\n_peraz_generated() {\n").count(), 1);
        assert!(zsh.contains(
            "local -a commands=(compress probe doctor watch ffmpeg recipe completions help)"
        ));

        let fish = script(Shell::Fish);
        assert_eq!(fish.matches("recipe list --names").count(), 3, "{fish}");
    }
}
