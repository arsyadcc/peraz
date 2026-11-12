//! `peraz compress`.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use peraz_core::units::format_bytes;
use peraz_core::{
    EncoderPreference, GifRecipe, ImageFormat, ImageRecipe, Job, KeepReason, Outcome, OutputTarget,
    Plan, Quality, Recipe, Report, VideoCodec, VideoOutput, VideoPlan, VideoRecipe,
};
use peraz_engine::ffmpeg::BuildInfo;
use serde::Serialize;

use crate::{exit, locate, print_json};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Files to compress: videos (MP4/MOV/MKV/…) and images (JPEG, PNG, WebP, HEIC, AVIF; BMP/TIFF with --format).
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Quality level: highest, high, balanced, small, smallest.
    #[arg(short, long, default_value_t = Quality::Balanced, value_parser = parse::<Quality>)]
    quality: Quality,

    /// Video codec: h264 (plays everywhere) or hevc (smaller).
    #[arg(long, default_value_t = VideoCodec::H264, value_parser = parse::<VideoCodec>)]
    codec: VideoCodec,

    /// Encoder: auto (smallest files), software, or hardware (fastest).
    #[arg(long, default_value_t = EncoderPreference::Auto, value_parser = parse::<EncoderPreference>)]
    encoder: EncoderPreference,

    /// Scale down to at most this width (never scales up).
    #[arg(long, value_name = "PIXELS")]
    max_width: Option<u32>,

    /// Scale down to at most this height (never scales up).
    #[arg(long, value_name = "PIXELS")]
    max_height: Option<u32>,

    /// Output format: keep, jpeg, png, webp, avif, gif, mp4. For videos and GIFs, webp/avif make
    /// animated images, gif makes a GIF, and mp4 turns a GIF into a video.
    #[arg(long, default_value = "keep", value_parser = parse_format)]
    format: OutputFormat,

    /// Frame rate for GIF and animated WebP/AVIF (default by quality level; never above the source).
    #[arg(long, value_name = "FPS", value_parser = clap::value_parser!(u32).range(1..=50))]
    fps: Option<u32>,

    /// Start a GIF or animated WebP/AVIF this many seconds into the video (e.g. 12.5).
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    start: Option<u64>,

    /// Make a GIF or animated WebP/AVIF at most this many seconds long.
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    duration: Option<u64>,

    /// Fit each video in this size, e.g. 25MB, 8MB, 500kB (SI units: 1 MB = 1,000,000 bytes).
    /// Replaces --quality for videos; the result is verified and never larger than the target.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    target_size: Option<u64>,

    /// Remove all audio tracks.
    #[arg(long)]
    no_audio: bool,

    /// Output file (only with a single input). Default: `<name>-peraz.<ext>` next to the input.
    #[arg(short, long, value_name = "FILE", conflicts_with = "out_dir")]
    output: Option<PathBuf>,

    /// Write outputs into this folder instead of next to each input.
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Replace existing output files.
    #[arg(short, long)]
    force: bool,

    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// `--format`: one flag for every kind of input.
///
/// | value  | still images        | videos                 | GIFs                   |
/// |--------|---------------------|------------------------|------------------------|
/// | keep   | same format         | MP4                    | GIF                    |
/// | jpeg   | JPEG                | MP4                    | GIF                    |
/// | png    | PNG                 | MP4                    | GIF                    |
/// | webp   | WebP                | animated WebP          | animated WebP          |
/// | avif   | AVIF                | animated AVIF          | animated AVIF          |
/// | gif    | same format         | GIF                    | GIF                    |
/// | mp4    | same format         | MP4                    | MP4                    |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputFormat {
    image: ImageFormat,
    video: VideoOutput,
}

fn parse_format(s: &str) -> Result<OutputFormat, String> {
    let (image, video) = match s {
        "keep" => (ImageFormat::Keep, VideoOutput::Keep),
        "jpeg" | "jpg" => (ImageFormat::Jpeg, VideoOutput::Keep),
        "png" => (ImageFormat::Png, VideoOutput::Keep),
        "webp" => (ImageFormat::Webp, VideoOutput::Webp),
        "avif" => (ImageFormat::Avif, VideoOutput::Avif),
        "gif" => (ImageFormat::Keep, VideoOutput::Gif),
        "mp4" => (ImageFormat::Keep, VideoOutput::Mp4),
        other => {
            return Err(format!(
                "unknown format `{other}`; expected one of: keep, jpeg, png, webp, avif, gif, mp4"
            ));
        }
    };
    Ok(OutputFormat { image, video })
}

/// Parses an SI size (`25MB`, `8 MB`, `500kB`, `1.5GB`, `900000B`) into bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let text = s.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("`{s}` is not a size like 25MB"))?;
    let scale = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "k" => 1e3,
        "mb" | "m" => 1e6,
        "gb" | "g" => 1e9,
        "kib" | "mib" | "gib" => {
            return Err(format!(
                "`{s}` uses binary units; Peraz uses SI units (kB, MB, GB)"
            ));
        }
        "" => return Err(format!("`{s}` needs a unit: B, kB, MB, or GB")),
        other => return Err(format!("unknown size unit `{other}`; use B, kB, MB, or GB")),
    };
    let bytes = value * scale;
    if !bytes.is_finite() || bytes < 1.0 {
        return Err(format!("`{s}` is not a usable size"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // checked positive and finite
    Ok(bytes.floor() as u64)
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
    results: &'a [Report],
    summary: Summary,
}

#[derive(Serialize, Default, Clone, Copy)]
struct Summary {
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

    let recipe = recipe(args);
    let target = output_target(args);
    let show_progress = !args.json && std::io::stderr().is_terminal();
    let cancel = install_cancel_handler();

    let mut reports = Vec::with_capacity(args.files.len());
    for input in &args.files {
        // After cancellation every remaining file still gets a report (status `cancelled`), so
        // callers learn the fate of each input; the engine returns immediately for these.
        let job = Job {
            input: input.clone(),
            output: target.clone(),
            recipe: recipe.clone(),
            overwrite: args.force,
        };
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
        let report = peraz_engine::compress::compress(&tools, &caps, &job, &mut progress, cancel);
        if show_progress && last_percent.is_some() {
            eprint!("\r\x1b[2K");
        }
        if !args.json {
            print_human(&report);
        }
        reports.push(report);
    }

    let summary = summarize(&reports);
    let (failed, cancelled) = (summary.failed, summary.cancelled > 0);
    if args.json {
        print_json(Output {
            ffmpeg: Some(&build),
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
fn recipe(args: &Args) -> Recipe {
    Recipe {
        quality: args.quality,
        video: VideoRecipe {
            codec: args.codec,
            encoder: args.encoder,
            max_width: args.max_width,
            max_height: args.max_height,
            remove_audio: args.no_audio,
            target_bytes: args.target_size,
            output: args.format.video,
        },
        gif: GifRecipe {
            fps: args.fps,
            start_ms: args.start,
            duration_ms: args.duration,
        },
        image: ImageRecipe {
            format: args.format.image,
            max_width: args.max_width,
            max_height: args.max_height,
        },
    }
}

fn output_target(args: &Args) -> OutputTarget {
    match (&args.output, &args.out_dir) {
        (Some(file), _) => OutputTarget::File(file.clone()),
        (None, Some(dir)) => OutputTarget::Folder(dir.clone()),
        (None, None) => OutputTarget::NextToInput,
    }
}

/// First Ctrl-C (or SIGTERM): stop the current file cleanly and skip the rest.
/// Second Ctrl-C: exit immediately.
fn install_cancel_handler() -> &'static AtomicBool {
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

fn summarize(reports: &[Report]) -> Summary {
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

fn print_human(report: &Report) {
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

fn print_summary(summary: &Summary, total: usize) {
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
    fn format_and_seconds_parsing() {
        assert_eq!(parse_format("gif").unwrap().video, VideoOutput::Gif);
        let webp = parse_format("webp").unwrap();
        assert_eq!(
            (webp.image, webp.video),
            (ImageFormat::Webp, VideoOutput::Webp)
        );
        assert_eq!(parse_format("mp4").unwrap().image, ImageFormat::Keep);
        assert!(parse_format("mov").unwrap_err().contains("mp4"));
        assert_eq!(parse_seconds("12.5"), Ok(12_500));
        assert_eq!(parse_size("25MB"), Ok(25_000_000));
        assert_eq!(parse_size("8 mb"), Ok(8_000_000));
        assert_eq!(parse_size("1.5GB"), Ok(1_500_000_000));
        assert_eq!(parse_size("500kB"), Ok(500_000));
        assert!(parse_size("25MiB").unwrap_err().contains("SI"));
        assert!(parse_size("25").unwrap_err().contains("unit"));
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
