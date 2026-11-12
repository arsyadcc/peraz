//! `peraz compress`.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use peraz_core::settings::{OutputFormat, Settings};
use peraz_core::units::format_bytes;
use peraz_core::{
    EncoderPreference, ExistingOutput, Job, KeepReason, Outcome, OutputTarget, Plan, Quality,
    Recipe, Report, TrimPlan, VideoCodec, VideoPlan,
};
use peraz_engine::batch::Batch;
use peraz_engine::ffmpeg::BuildInfo;
use serde::Serialize;

use crate::{exit, locate, print_json, recipes};

/// Flags that describe how files are compressed; shared by `compress` and `watch`.
#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // command-line switches
pub struct SettingsArgs {
    /// Use a recipe: a built-in or your own name (see `peraz recipe list`), or a .toml file.
    /// Flags given here override the recipe's values. Without it, a `peraz.toml` in the current
    /// folder or a parent folder is used.
    #[arg(short, long, value_name = "NAME|FILE")]
    pub recipe: Option<String>,

    /// Do not look for a `peraz.toml` project recipe.
    #[arg(long, conflicts_with = "recipe")]
    pub ignore_project_recipe: bool,

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

    /// Start this far into a video: seconds (12.5) or [h:]m:ss (0:12, 1:02:03).
    #[arg(long, value_name = "TIME", value_parser = parse_seconds)]
    pub start: Option<u64>,

    /// Use at most this much of a video: seconds or [h:]m:ss.
    #[arg(long, value_name = "TIME", value_parser = parse_seconds)]
    pub duration: Option<u64>,

    /// Only cut the clip (--start/--duration), without compressing: the streams are copied when
    /// the start is on a keyframe (instant, lossless); otherwise the clip is re-encoded.
    #[arg(long)]
    pub trim_only: bool,

    /// With --trim-only, move the start back to the previous keyframe so the clip can always be
    /// copied without re-encoding.
    #[arg(long, requires = "trim_only")]
    pub snap_to_keyframe: bool,

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
    /// Files to compress: videos (MP4/MOV/MKV/…), images (JPEG, PNG, WebP, HEIC, AVIF; BMP/TIFF with --format), GIFs, PDFs, and EPUBs.
    #[arg(required = true)]
    files: Vec<PathBuf>,

    #[command(flatten)]
    pub settings: SettingsArgs,

    /// Output file (only with a single input). Default: `<name>-1.<ext>` next to the input, or
    /// `-2`, `-3`, … when that name is taken: existing files are never replaced.
    #[arg(short, long, value_name = "FILE", conflicts_with = "out_dir")]
    output: Option<PathBuf>,

    /// Write outputs into this folder instead of next to each input.
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// With --output, replace that file if it exists.
    #[arg(short, long)]
    force: bool,

    /// Check only, for CI: write nothing, and exit with code 4 when a file could be made at
    /// least --check-threshold percent smaller.
    #[arg(long, conflicts_with_all = ["output", "out_dir", "force"])]
    check: bool,

    /// With --check, the smallest saving (percent) that counts as "could be smaller".
    #[arg(long, value_name = "PERCENT", default_value_t = 10, requires = "check", value_parser = clap::value_parser!(u8).range(1..=99))]
    check_threshold: u8,

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
/// Parses `12`, `12.5`, `0:12`, `1:02:03.5` into milliseconds.
fn parse_seconds(s: &str) -> Result<u64, String> {
    let bad = || format!("`{s}` is not a time like 12.5, 0:12, or 1:02:03");
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() > 3 {
        return Err(bad());
    }
    let mut seconds = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value: f64 = part.parse().map_err(|_| bad())?;
        let last = i == parts.len() - 1;
        if i > 0 && (value >= 60.0 || (!last && value.fract() != 0.0)) {
            return Err(bad());
        }
        seconds = seconds * 60.0 + value;
    }
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
    /// The recipe used: a name, or the path of a project `peraz.toml`.
    recipe: Option<&'a str>,
    /// Present in --check mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<&'a Check>,
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
    let check_dir = check_folder(args, recipe_name.as_deref());
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
    let job_for = |input: &PathBuf| {
        let output = match &check_dir {
            Some(dir) => {
                let folder = dir.join(input_key(input));
                let _ = std::fs::create_dir_all(&folder);
                OutputTarget::Folder(folder)
            }
            None => target.clone(),
        };
        let existing = if args.force || args.check {
            ExistingOutput::Replace
        } else {
            ExistingOutput::Fail
        };
        Job {
            input: input.clone(),
            output,
            recipe: recipe.clone(),
            existing,
        }
    };
    // After cancellation every remaining file still gets a report (status `cancelled`), so
    // callers learn the fate of each input; the engine returns immediately for these.
    let reports = if workers == 1 {
        run_sequential(&args.files, args.json || args.check, |input, progress| {
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
        run_parallel(&args.files, workers, args.json || args.check, |input| {
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

    finish_run(
        args,
        &build,
        recipe_name.as_deref(),
        &reports,
        check_dir.as_deref(),
    )
}

/// Prints the results and picks the exit code.
fn finish_run(
    args: &Args,
    build: &BuildInfo,
    recipe_name: Option<&str>,
    reports: &[Report],
    check_dir: Option<&Path>,
) -> u8 {
    let summary = summarize(reports);
    let (failed, cancelled) = (summary.failed, summary.cancelled > 0);
    let check = check_dir.map(|dir| {
        let result = check_results(reports, args.check_threshold);
        // Only the folder this run created; it holds nothing but this run's temporary outputs.
        let _ = std::fs::remove_dir_all(dir);
        result
    });
    if args.json {
        print_json(Output {
            ffmpeg: Some(build),
            recipe: recipe_name,
            results: reports,
            summary,
            check: check.as_ref(),
        });
    } else if let Some(check) = &check {
        print_check(check, reports);
    } else if reports.len() > 1 || cancelled {
        print_summary(&summary, args.files.len());
    }
    if cancelled {
        exit::CANCELLED
    } else if failed > 0 {
        exit::INPUT_FAILED
    } else if check.as_ref().is_some_and(|c| !c.could_shrink.is_empty()) {
        exit::CHECK_FAILED
    } else {
        exit::SUCCESS
    }
}

/// For --check: a private temporary folder (one subfolder per input, removed at the end).
/// Also tells the user when a project `peraz.toml` is in use.
fn check_folder(args: &Args, recipe_name: Option<&str>) -> Option<PathBuf> {
    if let Some(project) = recipe_name.filter(|n| n.ends_with(recipes::PROJECT_RECIPE))
        && !args.json
    {
        eprintln!("using recipe {project}");
    }
    args.check
        .then(|| std::env::temp_dir().join(format!("peraz-check-{}", std::process::id())))
}

/// A short, stable folder name for an input inside the --check folder.
fn input_key(input: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::path::absolute(input)
        .unwrap_or_else(|_| input.to_path_buf())
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// What --check found.
#[derive(Serialize)]
pub(crate) struct Check {
    threshold_percent: u8,
    /// Files that could be made at least the threshold smaller.
    could_shrink: Vec<CheckItem>,
}

#[derive(Serialize)]
struct CheckItem {
    path: PathBuf,
    input_bytes: u64,
    possible_bytes: u64,
    saving_percent: f64,
}

fn check_results(reports: &[Report], threshold: u8) -> Check {
    let could_shrink = reports
        .iter()
        .filter_map(|report| {
            let (Outcome::Compressed { output_bytes, .. }, Some(input)) =
                (&report.outcome, report.input_bytes)
            else {
                return None;
            };
            #[allow(clippy::cast_precision_loss)]
            let saving = 100.0 * (1.0 - *output_bytes as f64 / input.max(1) as f64);
            (saving >= f64::from(threshold)).then(|| CheckItem {
                path: report.input.clone(),
                input_bytes: input,
                possible_bytes: *output_bytes,
                saving_percent: (saving * 10.0).round() / 10.0,
            })
        })
        .collect();
    Check {
        threshold_percent: threshold,
        could_shrink,
    }
}

fn print_check(check: &Check, reports: &[Report]) {
    for item in &check.could_shrink {
        println!(
            "{}: could be {:.0}% smaller ({} → {})",
            item.path.display(),
            item.saving_percent,
            format_bytes(item.input_bytes),
            format_bytes(item.possible_bytes)
        );
    }
    for report in reports {
        if let Outcome::Failed { message } = &report.outcome {
            eprintln!("{}: error: {message}", report.input.display());
        }
    }
    let ok = reports.len() - check.could_shrink.len();
    println!(
        "\n{} of {} files could be at least {}% smaller; {ok} are fine",
        check.could_shrink.len(),
        reports.len(),
        check.threshold_percent
    );
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
    let project = if args.recipe.is_none() && !args.ignore_project_recipe {
        std::env::current_dir()
            .ok()
            .and_then(|dir| recipes::project_recipe(&dir))
    } else {
        None
    };
    let found = match (&args.recipe, &project) {
        (Some(name), _) => Some(recipes::find(name)?),
        (None, Some(path)) => Some(recipes::load_project(path)?),
        (None, None) => None,
    };
    let (merged, name) = match found {
        Some(found) => (flags.over(&found.settings), Some(found.name)),
        None => (flags, None),
    };
    let mut recipe = merged
        .to_recipe(args.start, args.duration)
        .map_err(|e| e.to_string())?;
    recipe.video.trim_only = args.trim_only;
    recipe.video.snap_to_keyframe = args.snap_to_keyframe;
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
            if let Some(Plan::Trim(TrimPlan {
                reason: Some(reason),
                ..
            })) = &report.plan
            {
                println!("  note: {reason}");
            }
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
        assert_eq!(parse_seconds("0:12"), Ok(12_000));
        assert_eq!(parse_seconds("1:02:03.5"), Ok(3_723_500));
        assert!(parse_seconds("1:75").is_err());
        assert!(parse_seconds("1:2:3:4").is_err());
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
