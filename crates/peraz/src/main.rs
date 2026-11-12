//! The `peraz` binary.
//!
//! The JSON output and exit codes are a public, versioned interface: front ends and
//! users' scripts depend on them. Bump [`SCHEMA_VERSION`] on any breaking change.

mod completions;
mod compress;
mod doctor;
#[cfg(test)]
mod interface_tests;
mod probe;
mod recipes;
mod watch;

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory as _, Parser, Subcommand};
use peraz_engine::ffmpeg::Toolchain;
use serde::Serialize;

/// Version of the `--json` output format.
pub const SCHEMA_VERSION: u32 = 1;

/// Exit codes, documented in the README.
pub mod exit {
    pub const SUCCESS: u8 = 0;
    /// At least one input failed.
    pub const INPUT_FAILED: u8 = 1;
    /// Invalid usage (also clap's code for argument errors).
    pub const USAGE: u8 = 2;
    /// The environment is broken, e.g. FFmpeg is missing.
    pub const ENVIRONMENT: u8 = 3;
    /// `--check`: at least one file could be made noticeably smaller.
    pub const CHECK_FAILED: u8 = 4;
    /// Stopped by Ctrl-C or a termination signal (128 + SIGINT, the shell convention).
    pub const CANCELLED: u8 = 130;
}

/// Peraz: a media compressor whose results you can trust.
#[derive(Debug, Parser)]
#[command(
    name = "peraz",
    version = peraz_engine::VERSION,
    about,
    override_usage = "peraz [OPTIONS] <FILES>...\n       peraz <COMMAND>",
    after_help = "`peraz <FILES>...` is short for `peraz compress <FILES>...`: run `peraz compress --help` for its options.",
    arg_required_else_help = true
)]
struct Cli {
    /// Path to the ffmpeg binary (or its directory). Overrides PERAZ_FFMPEG and PATH.
    #[arg(long, global = true, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compress videos, images, GIFs, PDFs, and EPUBs.
    Compress(compress::Args),
    /// Show what Peraz knows about media files.
    Probe(probe::Args),
    /// Show which FFmpeg Peraz uses and what it can encode.
    Doctor(doctor::Args),
    /// Compress new files as they appear in folders.
    Watch(watch::Args),
    /// List and show recipes (named sets of settings for --recipe).
    #[command(subcommand)]
    Recipe(recipes::Command),
    /// Print a shell completion script: bash, zsh, fish, powershell, or elvish.
    Completions(completions::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse_from(with_default_command(std::env::args_os().collect()));
    let ffmpeg = cli.ffmpeg.as_deref();
    let code = match &cli.command {
        Command::Compress(args) => compress::run(ffmpeg, args),
        Command::Probe(args) => probe::run(ffmpeg, args),
        Command::Doctor(args) => doctor::run(ffmpeg, args),
        Command::Recipe(command) => recipes::run(command),
        Command::Watch(args) => watch::run(ffmpeg, args),
        Command::Completions(args) => completions::run(args),
    };
    ExitCode::from(code)
}

/// `peraz photo.jpg` means `peraz compress photo.jpg`: inserts `compress` unless the first
/// argument names a command or asks for help or the version.
fn with_default_command(mut args: Vec<OsString>) -> Vec<OsString> {
    let Some(first) = args.get(1).and_then(|a| a.to_str()) else {
        return args;
    };
    let cli = Cli::command();
    let is_command = first == "help"
        || cli
            .get_subcommands()
            .any(|c| c.get_name() == first || c.get_all_aliases().any(|a| a == first));
    if !is_command && !matches!(first, "-h" | "--help" | "-V" | "--version") {
        args.insert(1, OsString::from("compress"));
    }
    args
}

/// Finds FFmpeg or prints why not.
fn locate(ffmpeg: Option<&Path>) -> Result<Toolchain, u8> {
    Toolchain::locate(ffmpeg).map_err(|e| {
        eprintln!("error: {e}");
        exit::ENVIRONMENT
    })
}

/// Wraps every JSON document with the schema version.
#[derive(Serialize)]
struct Envelope<T: Serialize> {
    schema_version: u32,
    #[serde(flatten)]
    body: T,
}

fn print_json<T: Serialize>(body: T) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer_pretty(
        &mut out,
        &Envelope {
            schema_version: SCHEMA_VERSION,
            body,
        },
    );
    let _ = writeln!(out);
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser as _};

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }

    fn rewrite(args: &[&str]) -> Vec<String> {
        let args = std::iter::once("peraz").chain(args.iter().copied());
        super::with_default_command(args.map(Into::into).collect())
            .into_iter()
            .skip(1)
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn files_and_options_mean_compress() {
        assert_eq!(rewrite(&["image.png"]), ["compress", "image.png"]);
        assert_eq!(
            rewrite(&["-q", "small", "a.mov"]),
            ["compress", "-q", "small", "a.mov"]
        );
        assert_eq!(
            rewrite(&["--ffmpeg", "/x", "a.mov"]),
            ["compress", "--ffmpeg", "/x", "a.mov"]
        );
        // Commands, help, and the version stay as they are.
        assert_eq!(rewrite(&["compress", "a.mov"]), ["compress", "a.mov"]);
        assert_eq!(rewrite(&["doctor"]), ["doctor"]);
        assert_eq!(rewrite(&["watch", "dir"]), ["watch", "dir"]);
        assert_eq!(rewrite(&["recipe", "list"]), ["recipe", "list"]);
        assert_eq!(rewrite(&["help"]), ["help"]);
        assert_eq!(rewrite(&["--help"]), ["--help"]);
        assert_eq!(rewrite(&["-V"]), ["-V"]);
        assert_eq!(rewrite(&[]), Vec::<String>::new());
    }

    #[test]
    fn short_form_parses_like_compress() {
        let cli = super::Cli::try_parse_from(super::with_default_command(
            ["peraz", "photo.jpg", "-q", "small"]
                .map(Into::into)
                .to_vec(),
        ))
        .unwrap();
        assert!(matches!(cli.command, super::Command::Compress(_)));
    }
}
