//! The `peraz` binary.
//!
//! The JSON output and exit codes are a public, versioned interface: front ends and
//! users' scripts depend on them. Bump [`SCHEMA_VERSION`] on any breaking change.

mod compress;
mod doctor;
mod probe;
mod recipes;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
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
    /// Stopped by Ctrl-C or a termination signal (128 + SIGINT, the shell convention).
    pub const CANCELLED: u8 = 130;
}

/// Peraz: a media compressor whose results you can trust.
#[derive(Debug, Parser)]
#[command(name = "peraz", version = peraz_engine::VERSION, about)]
struct Cli {
    /// Path to the ffmpeg binary (or its directory). Overrides PERAZ_FFMPEG and PATH.
    #[arg(long, global = true, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compress videos (images and GIFs are coming).
    Compress(compress::Args),
    /// Show what Peraz knows about media files.
    Probe(probe::Args),
    /// Show which FFmpeg Peraz uses and what it can encode.
    Doctor(doctor::Args),
    /// List and show recipes (named sets of settings for --recipe).
    #[command(subcommand)]
    Recipe(recipes::Command),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let ffmpeg = cli.ffmpeg.as_deref();
    let code = match &cli.command {
        Command::Compress(args) => compress::run(ffmpeg, args),
        Command::Probe(args) => probe::run(ffmpeg, args),
        Command::Doctor(args) => doctor::run(ffmpeg, args),
        Command::Recipe(command) => recipes::run(command),
    };
    ExitCode::from(code)
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
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
