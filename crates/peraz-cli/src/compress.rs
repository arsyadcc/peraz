//! `peraz compress`.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use peraz_core::settings::{OutputFormat, Settings};
use peraz_core::units::format_bytes;
use peraz_core::{
    EncoderPreference, ExistingOutput, Job, KeepReason, Outcome, OutputTarget, Plan, Quality,
    Recipe, Report, VideoCodec, VideoPlan,
};
use peraz_engine::batch::Batch;
use peraz_engine::ffmpeg::BuildInfo;
use serde::Serialize;

use crate::{exit, locate, print_json, recipes};

/// Flags that describe how files are compressed; shared by `compress` and `watch`.
#[derive(Debug, clap::Args)]
pub struct SettingsArgs {
    /// Use a recipe: a built-in or your own name (see `peraz recipe list`), or a .toml file.
    /// Flags given here override the recipe's values.
    #[arg(short, long, value_name = "NAME|FILE")]
    pub recipe: Option<String>,

    /// Quality level: highest, high, balanced (default), small, smallest.
    #[arg(short, long, value_parser = parse::<Quality>)]
    pub quality: Option<Quality>,

    /// Video codec: h264 (default, plays everywhere) or hevc (smaller).
    #[arg(long, value_parser = parse::<VideoCodec>)]
    pub codec: Option<VideoCodec>,

    /// Encoder: auto (default, smallest files), software, or hardware (fastest).
    #[arg(long, value_parser = parse::<EncoderPreference>)]
    pub encoder: Option<EncoderPreference>,

    /// Scale down to at most this width (never scales up).
    #[arg(long, value_name = "PIXELS")]
    pub max_width: Option<u32>,

    /// Scale down to at most this height (never scales up).
    #[arg(long, value_name = "PIXELS")]
    pub max_height: Option<u32>,

    /// Output format: keep (default), jpeg, png, webp, avif, gif, mp4. For videos and GIFs,
    /// webp/avif make animated images, gif makes a GIF, and mp4 turns a GIF into a video.
    #[arg(long, value_parser = parse_format)]
    pub format: Option<String>,

    /// Frame rate for GIF and animated WebP/AVIF (default by quality level; never above the source).
    #[arg(long, value_name = "FPS", value_parser = clap::value_parser!(u32).range(1..=50))]
    pub fps: Option<u32>,

    /// Start a GIF or animated WebP/AVIF this many seconds into the video (e.g. 12.5).
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    pub start: Option<u64>,

    /// Make a GIF or animated WebP/AVIF at most this many seconds long.
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    pub duration: Option<u64>,

    /// Fit each video in this size, e.g. 25MB, 8MB, 500kB (SI units: 1 MB = 1,000,000 bytes).
    /// Replaces --quality for videos; the result is verified and never larger than the target.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub target_size: Option<String>,

    /// Remove all audio tracks.
    #[arg(long)]
    pub no_audio: bool,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Files to compress: videos (MP4/MOV/MKV/…) and images (JPEG, PNG, WebP, HEIC, AVIF; BMP/TIFF with --format).
    #[arg(required = true)]
    files: Vec<PathBuf>,

    #[command(flatten)]
    pub settings: SettingsArgs,

    /// Output file (only with a single input). Default: `<name>-peraz.<ext>` next to the input.
    #[arg(short, long, value_name = "FILE", conflicts_with = "out_dir")]
    output: Option<PathBuf>,

    /// Write outputs into this folder instead of next to each input.
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Replace existing output files.
    #[arg(short, long)]
    force: bool,

    /// How many files to work on at once (default: one per logical core; encodes are further
    /// limited to one per 8 cores). 1 processes files one by one with live progress.
    #[arg(short, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,

    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// `--format`: checked here so mistakes are usage errors; the meaning lives in `peraz-core`.
fn parse_format(s: &str) -> Result<String, String> {
    s.parse::<OutputFormat>()
        .map(|_| s.to_owned())
        .map_err(|e| e.to_string())
}

/// `--target-size`: an SI size such as `25MB`.
fn parse_size(s: &str) -> Result<String, String> {
    peraz_core::units::parse_size(s).map(|_| s.to_owned())
}

/// Parses seconds (`12`, `12.5`) into milliseconds.
fn parse_seconds(s: &str) -> Result<u64, String> {
    let seconds: f64 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a number of seconds"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("`{s}` must be zero or more seconds"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // checked non-negative
    Ok((seconds * 1000.0).round() as u64)
}

fn parse<T: std::str::FromStr<Err = peraz_core::recipe::UnknownValue>>(
    s: &str,
) -> Result<T, String> {
    s.parse()
        .map_err(|e: peraz_core::recipe::UnknownValue| e.to_string())
}

#[derive(Serialize)]
struct Output<'a> {
    ffmpeg: Option<&'a BuildInfo>,
    /// The recipe used, when one was given with `--recipe`.
    recipe: Option<&'a str>,
    results: &'a [Report],
    summary: Summary,
}

#[derive(Serialize, Default, Clone, Copy)]
pub(crate) struct Summary {
    compressed: usize,
    kept_original: usize,
    skipped: usize,
    failed: usize,
    cancelled: usize,
    input_bytes: u64,
    saved_bytes: u64,
}

pub fn run(ffmpeg: Option<&Path>, args: &Args) -> u8 {
    if args.output.is_some() && args.files.len() > 1 {
        eprintln!("error: --output works with a single input; use --out-dir for several files");
        return exit::USAGE;
    }
    if let Some(dir) = &args.out_dir
        && !dir.is_dir()
    {
        eprintln!("error: --out-dir {} is not a folder", dir.display());
        return exit::USAGE;
    }
    let (recipe, recipe_name) = match recipe(&args.settings) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::USAGE;
        }
    };
    let tools = match locate(ffmpeg) {
        Ok(tools) => tools,
        Err(code) => return code,
    };
    let (build, caps) = match tools
        .build_info()
        .and_then(|b| Ok((b, tools.capabilities()?)))
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::ENVIRONMENT;
        }
    };

    let target = output_target(args);
    let cancel = install_cancel_handler();
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let workers = args
        .jobs
        .map_or(cores, |n| n as usize)
        .min(args.files.len())
        .max(1);
    let batch = if workers == 1 {
        Batch::new(1, 1)
    } else {
        Batch::for_this_machine()
    };
    let job_for = |input: &PathBuf| Job {
        input: input.clone(),
        output: target.clone(),
        recipe: recipe.clone(),
        existing: if args.force {
            ExistingOutput::Replace
        } else {
            ExistingOutput::Fail
        },
    };
    // After cancellation every remaining file still gets a report (status `cancelled`), so
    // callers learn the fate of each input; the engine returns immediately for these.
    let reports = if workers == 1 {
        run_sequential(&args.files, args.json, |input, progress| {
            peraz_engine::compress::compress_in(
                &batch,
                &tools,
                &caps,
                &job_for(input),
                progress,
                cancel,
            )
        })
    } else {
        run_parallel(&args.files, workers, args.json, |input| {
            peraz_engine::compress::compress_in(
                &batch,
                &tools,
                &caps,
                &job_for(input),
                &mut |_| {},
                cancel,
            )
        })
    };

    let summary = summarize(&reports);
    let (failed, cancelled) = (summary.failed, summary.cancelled > 0);
    if args.json {
        print_json(Output {
            ffmpeg: Some(&build),
            recipe: recipe_name.as_deref(),
            results: &reports,
            summary,
        });
    } else if reports.len() > 1 || cancelled {
        print_summary(&summary, args.files.len());
    }
    if cancelled {
        exit::CANCELLED
    } else if failed > 0 {
        exit::INPUT_FAILED
    } else {
        exit::SUCCESS
    }
}

/// The recipe the command-line flags describe.
/// The settings the command line describes: flags first, then the recipe, then defaults.
pub fn recipe(args: &SettingsArgs) -> Result<(Recipe, Option<String>), String> {
    let flags = Settings {
        quality: args.quality,
        format: args.format.clone(),
        max_width: args.max_width,
        max_height: args.max_height,
        codec: args.codec,
        encoder: args.encoder,
        no_audio: args.no_audio.then_some(true),
        target_size: args.target_size.clone(),
        fps: args.fps,
        ..Settings::default()
    };
    let (merged, name) = match &args.recipe {
        Some(name) => {
            let found = recipes::find(name)?;
            (flags.over(&found.settings), Some(found.name))
        }
        None => (flags, None),
    };
    let recipe = merged
        .to_recipe(args.start, args.duration)
        .map_err(|e| e.to_string())?;
    Ok((recipe, name))
}

fn output_target(args: &Args) -> OutputTarget {
    match (&args.output, &args.out_dir) {
        (Some(file), _) => OutputTarget::File(file.clone()),
        (None, Some(dir)) => OutputTarget::Folder(dir.clone()),
        (None, None) => OutputTarget::NextToInput,
    }
}

/// One file at a time, with a live percentage on the terminal.
fn run_sequential(
    files: &[PathBuf],
    json: bool,
    compress: impl Fn(&PathBuf, &mut dyn FnMut(f64)) -> Report,
) -> Vec<Report> {
    let show_progress = !json && std::io::stderr().is_terminal();
    let mut reports = Vec::with_capacity(files.len());
    for input in files {
        let mut last_percent = None;
        let mut progress = |fraction: f64| {
            if show_progress {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..=100
                let percent = (fraction * 100.0).floor() as u8;
                if last_percent != Some(percent) {
                    last_percent = Some(percent);
                    eprint!("\r{} {percent:>3}%", input.display());
                }
            }
        };
        let report = compress(input, &mut progress);
        if show_progress && last_percent.is_some() {
            eprint!("\r\x1b[2K");
        }
        if !json {
            print_human(&report);
        }
        reports.push(report);
    }
    reports
}

/// Several files at once on `workers` threads. Text results print as they finish, prefixed
/// with a counter; the returned reports keep the input order.
pub(crate) fn run_parallel(
    files: &[PathBuf],
    workers: usize,
    json: bool,
    compress: impl Fn(&PathBuf) -> Report + Sync,
) -> Vec<Report> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let print = Mutex::new(());
    let slots: Vec<Mutex<Option<Report>>> = files.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(input) = files.get(index) else { break };
                    let report = compress(input);
                    if !json {
                        let _guard = print.lock().unwrap_or_else(PoisonError::into_inner);
                        let finished = done.fetch_add(1, Ordering::SeqCst) + 1;
                        print!("[{finished}/{}] ", files.len());
                        print_human(&report);
                    }
                    *slots[index].lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .expect("every file was processed")
        })
        .collect()
}

/// First Ctrl-C (or SIGTERM): stop the current file cleanly and skip the rest.
/// Second Ctrl-C: exit immediately.
pub(crate) fn install_cancel_handler() -> &'static AtomicBool {
    static CANCEL: AtomicBool = AtomicBool::new(false);
    let result = ctrlc::set_handler(|| {
        if CANCEL.swap(true, Ordering::SeqCst) {
            std::process::exit(i32::from(exit::CANCELLED));
        }
        eprintln!("\ncancelling… (press Ctrl-C again to quit immediately)");
    });
    if let Err(e) = result {
        eprintln!("warning: Ctrl-C handling unavailable: {e}");
    }
    &CANCEL
}

pub(crate) fn summarize(reports: &[Report]) -> Summary {
    let mut summary = Summary::default();
    for report in reports {
        match report.outcome {
            Outcome::Compressed { .. } => summary.compressed += 1,
            Outcome::KeptOriginal(_) => summary.kept_original += 1,
            Outcome::Skipped { .. } => summary.skipped += 1,
            Outcome::Failed { .. } => summary.failed += 1,
            Outcome::Cancelled => summary.cancelled += 1,
        }
        summary.input_bytes += report.input_bytes.unwrap_or(0);
        summary.saved_bytes += report.saved_bytes().unwrap_or(0);
    }
    summary
}

fn percent_saved(before: u64, after: u64) -> String {
    if before == 0 {
        return "0%".to_owned();
    }
    #[allow(clippy::cast_precision_loss)] // display only
    let change = 100.0 * (after as f64 - before as f64) / before as f64;
    if change > 0.0 {
        format!("+{change:.0}%")
    } else {
        format!("−{:.0}%", -change)
    }
}

pub(crate) fn print_human(report: &Report) {
    let input = report.input.display();
    match &report.outcome {
        Outcome::Compressed {
            output,
            output_bytes,
        } => {
            let before = report.input_bytes.unwrap_or(0);
            let encoder = report.encoder.as_deref().unwrap_or("");
            println!("{input} → {}", output.display());
            let target = match &report.plan {
                Some(Plan::Video(VideoPlan {
                    target: Some(t), ..
                })) => format!(
                    " · target {} met in {} {}",
                    format_bytes(t.target_bytes),
                    t.attempts,
                    if t.attempts == 1 { "try" } else { "tries" }
                ),
                _ => String::new(),
            };
            println!(
                "  {} → {} ({}) · {encoder}{target} · {:.1} s",
                format_bytes(before),
                format_bytes(*output_bytes),
                percent_saved(before, *output_bytes),
                report.elapsed_seconds
            );
        }
        Outcome::KeptOriginal(KeepReason::NotSmaller { compressed_bytes }) => {
            println!("{input}");
            println!(
                "  kept the original: the compressed version was not smaller ({} vs {}); try a smaller quality level",
                format_bytes(*compressed_bytes),
                format_bytes(report.input_bytes.unwrap_or(0))
            );
        }
        Outcome::KeptOriginal(KeepReason::AlreadyWithinTarget { target_bytes }) => {
            println!("{input}");
            println!(
                "  kept the original: it is already {} (target {})",
                format_bytes(report.input_bytes.unwrap_or(0)),
                format_bytes(*target_bytes)
            );
        }
        Outcome::Skipped { message } => println!("{input}\n  skipped: {message}"),
        Outcome::Failed { message } => eprintln!("{input}: error: {message}"),
        Outcome::Cancelled => eprintln!("{input}: cancelled"),
    }
}

pub(crate) fn print_summary(summary: &Summary, total: usize) {
    let mut parts = vec![format!("{} compressed", summary.compressed)];
    for (count, label) in [
        (summary.kept_original, "kept original"),
        (summary.skipped, "skipped"),
        (summary.failed, "failed"),
        (summary.cancelled, "cancelled"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    println!(
        "\n{total} files: {} · saved {}",
        parts.join(", "),
        format_bytes(summary.saved_bytes)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_values_are_checked() {
        assert_eq!(parse_format("webp").as_deref(), Ok("webp"));
        assert!(parse_format("mov").unwrap_err().contains("mp4"));
        assert_eq!(parse_size("25MB").as_deref(), Ok("25MB"));
        assert!(parse_size("25MiB").unwrap_err().contains("SI"));
        assert_eq!(parse_seconds("12.5"), Ok(12_500));
        assert_eq!(parse_seconds("0"), Ok(0));
        assert!(parse_seconds("-1").is_err());
        assert!(parse_seconds("soon").is_err());
    }

    #[test]
    fn percent_formatting() {
        assert_eq!(percent_saved(1000, 250), "−75%");
        assert_eq!(percent_saved(0, 0), "0%");
        assert_eq!(percent_saved(1000, 1240), "+24%");
    }
}
