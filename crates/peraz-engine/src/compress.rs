//! Running a [`Job`]: probe, plan, encode to a temporary file, then keep the result only if it
//! is actually better than the input.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peraz_core::units::format_bytes;
use peraz_core::{
    Clip, ExistingOutput, ImageDecoder, Job, KeepReason, MediaInfo, MediaKind, Outcome, Pass, Plan,
    PlanError, RateControl, Report, SizeTarget, TrimMethod, VideoOutput, plan_animated,
    plan_document, plan_gif, plan_image, plan_trim, plan_video,
};

use crate::batch::{Batch, Lane};
use crate::ffmpeg::{Capabilities, Toolchain, command};
use crate::gif::{GifError, encode_gif};
use crate::image::{decode_native, encode_image};
use crate::probe::probe;

/// Progress callback: receives the fraction done, from 0.0 to 1.0.
pub type Progress<'a> = &'a mut dyn FnMut(f64);

/// Compresses one file. Never returns an error: every problem becomes [`Outcome::Failed`] in the
/// report, so batches keep going and every input gets an answer.
///
/// Setting `cancel` (e.g. from a Ctrl-C handler) stops FFmpeg, removes the partial file, and
/// returns [`Outcome::Cancelled`].
pub fn compress(
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
) -> Report {
    compress_in(&Batch::unlimited(), tools, caps, job, progress, cancel)
}

/// Like [`compress`], as one job of a batch: waits for a slot in the right lane after probing,
/// and reserves the output path so no other job of the batch can write it.
pub fn compress_in(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
) -> Report {
    let started = Instant::now();
    let mut report = Report {
        input: job.input.clone(),
        kind: None,
        input_bytes: None,
        outcome: Outcome::Failed {
            message: String::new(),
        },
        plan: None,
        encoder: None,
        document: None,
        elapsed_seconds: 0.0,
    };
    report.outcome = if cancel.load(Ordering::SeqCst) {
        Outcome::Cancelled
    } else {
        run(batch, tools, caps, job, progress, cancel, &mut report)
    };
    report.elapsed_seconds = started.elapsed().as_secs_f64();
    report
}

fn run(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };

    let info = match probe(tools, &job.input) {
        Ok(info) => info,
        Err(e) => return failed(e.to_string()),
    };
    report.kind = Some(info.kind);
    report.input_bytes = Some(info.size_bytes);

    // Still images and documents are in-process work; everything else runs FFmpeg encodes.
    let still_image = matches!(
        info.kind,
        MediaKind::Image | MediaKind::Pdf | MediaKind::Epub
    );
    let lane = if still_image {
        Lane::Image
    } else {
        Lane::Encode
    };
    let Some(_permit) = batch.acquire(lane, cancel) else {
        return Outcome::Cancelled;
    };

    let output_kind = job.recipe.video.output;
    let moving = matches!(info.kind, MediaKind::Video | MediaKind::Animation);
    let wants_gif = (info.kind == MediaKind::Animation
        && matches!(output_kind, VideoOutput::Keep | VideoOutput::Gif))
        || (info.kind == MediaKind::Video && output_kind == VideoOutput::Gif);
    if wants_gif {
        return make_gif(batch, tools, job, &info, progress, cancel, report);
    }
    if moving && matches!(output_kind, VideoOutput::Webp | VideoOutput::Avif) {
        return make_animated(batch, tools, caps, job, &info, progress, cancel, report);
    }
    match info.kind {
        MediaKind::Video if job.recipe.video.trim_only => {
            make_trim(batch, tools, caps, job, &info, progress, cancel, report)
        }
        MediaKind::Video | MediaKind::Animation => {
            let plan = match plan_video(&info, &job.recipe, |name| caps.has_encoder(name)) {
                Ok(plan) => plan,
                Err(e) => return failed(e.to_string()),
            };
            report.plan = Some(Plan::Video(plan.clone()));
            report.encoder = Some(plan.encoder_label());
            let output = match choose_output(batch, job, "mp4") {
                Ok(output) => output,
                Err(outcome) => return outcome,
            };
            if plan.target.is_some() {
                return encode_to_target(
                    tools, job, &info, plan, &output, progress, cancel, report,
                );
            }
            let partial = partial_path(&output);
            let duration = job
                .recipe
                .clip
                .seconds(info.duration_seconds)
                .unwrap_or(0.0);
            let seek = clip_args(job.recipe.clip);
            let sink = Sink {
                before_input: &seek,
                file: Some(&partial),
                workdir: None,
            };
            if let Err(error) = encode(
                tools, &job.input, &sink, &plan.args, duration, progress, cancel,
            ) {
                let _ = std::fs::remove_file(&partial);
                return match error {
                    EncodeError::Cancelled => Outcome::Cancelled,
                    EncodeError::Failed(message) => failed(message),
                };
            }
            finish(
                &partial,
                &output,
                info.size_bytes,
                info.kind == MediaKind::Animation,
            )
        }
        MediaKind::Image => make_image(batch, tools, job, &info, progress, cancel, report),
        MediaKind::Pdf | MediaKind::Epub => {
            make_document(batch, job, &info, progress, cancel, report)
        }
        other => Outcome::Skipped {
            message: format!("{} compression is not available yet", kind_name(other)),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn make_image(
    batch: &Batch,
    tools: &Toolchain,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    let plan = match plan_image(info, &job.recipe) {
        Ok(plan) => plan,
        Err(e @ PlanError::UnsupportedImage { .. }) => {
            return Outcome::Skipped {
                message: e.to_string(),
            };
        }
        Err(e) => return failed(e.to_string()),
    };
    report.plan = Some(Plan::Image(plan.clone()));
    let output = match choose_output(batch, job, plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let decoded = match plan.decoder {
        ImageDecoder::Native => decode_native(&job.input),
        ImageDecoder::Ffmpeg => crate::heif::decode(tools, &job.input),
    };
    let encoded = match decoded.and_then(|d| encode_image(d, &plan)) {
        Ok(encoded) => encoded,
        Err(message) => return failed(message),
    };
    report.encoder = Some(encoded.encoder);
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &encoded.bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", partial.display()));
    }
    progress(1.0);
    finish(&partial, &output, info.size_bytes, plan.converts_format)
}

fn make_document(
    batch: &Batch,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let plan = match plan_document(info, &job.recipe) {
        Ok(plan) => plan,
        Err(e) => {
            return Outcome::Failed {
                message: e.to_string(),
            };
        }
    };
    report.plan = Some(Plan::Document(plan.clone()));
    let extension = if info.kind == MediaKind::Pdf {
        "pdf"
    } else {
        "epub"
    };
    let output = match choose_output(batch, job, extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let result = if info.kind == MediaKind::Pdf {
        crate::pdf::compress_pdf(&job.input, &plan)
    } else {
        crate::epub::compress_epub(&job.input, &plan)
    };
    let (bytes, stats) = match result {
        Ok(done) => done,
        Err(crate::pdf::DocumentError::Protected(why)) => {
            return Outcome::Skipped {
                message: format!("{why}; Peraz never changes protected files"),
            };
        }
        Err(crate::pdf::DocumentError::Failed(message)) => return Outcome::Failed { message },
    };
    report.encoder = Some(format!(
        "{} of {} pictures recompressed",
        stats.recompressed, stats.images
    ));
    report.document = Some(stats);
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return Outcome::Failed {
            message: format!("cannot write {}: {e}", partial.display()),
        };
    }
    progress(1.0);
    finish(&partial, &output, info.size_bytes, false)
}

fn make_gif(
    batch: &Batch,
    tools: &Toolchain,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let plan = match plan_gif(info, &job.recipe) {
        Ok(plan) => plan,
        Err(e) => {
            return Outcome::Failed {
                message: e.to_string(),
            };
        }
    };
    report.plan = Some(Plan::Gif(plan.clone()));
    let output = match choose_output(batch, job, "gif") {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    // Playing time being converted, for progress: the selected range, else the whole source.
    let total = info.duration_seconds.unwrap_or(0.0);
    #[allow(clippy::cast_precision_loss)]
    let start = plan.start_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
    #[allow(clippy::cast_precision_loss)]
    let expected = plan
        .duration_ms
        .map_or((total - start).max(0.0), |ms| ms as f64 / 1000.0);
    let partial = partial_path(&output);
    match encode_gif(
        tools, &job.input, &partial, &plan, expected, progress, cancel,
    ) {
        Ok(encoder) => report.encoder = Some(encoder),
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            return match error {
                GifError::Cancelled => Outcome::Cancelled,
                GifError::Failed(message) => Outcome::Failed { message },
            };
        }
    }
    finish(&partial, &output, info.size_bytes, plan.converts_format)
}

#[allow(clippy::too_many_arguments)]
fn make_animated(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let plan = match plan_animated(info, &job.recipe, |name| caps.has_encoder(name)) {
        Ok(plan) => plan,
        Err(e) => {
            return Outcome::Failed {
                message: e.to_string(),
            };
        }
    };
    report.plan = Some(Plan::Animated(plan.clone()));
    report.encoder = Some(format!("{} · {} fps", plan.encoder, plan.fps));
    let output = match choose_output(batch, job, plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    // Seek before the input for speed; limit the length there too, so progress matches it.
    let before_input = clip_args(Clip {
        start_ms: plan.start_ms,
        duration_ms: plan.duration_ms,
    });
    let total = info.duration_seconds.unwrap_or(0.0);
    #[allow(clippy::cast_precision_loss)]
    let start = plan.start_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
    #[allow(clippy::cast_precision_loss)]
    let expected = plan
        .duration_ms
        .map_or((total - start).max(0.0), |ms| ms as f64 / 1000.0);
    let partial = partial_path(&output);
    let sink = Sink {
        before_input: &before_input,
        file: Some(&partial),
        workdir: None,
    };
    if let Err(error) = encode(
        tools, &job.input, &sink, &plan.args, expected, progress, cancel,
    ) {
        let _ = std::fs::remove_file(&partial);
        return match error {
            EncodeError::Cancelled => Outcome::Cancelled,
            EncodeError::Failed(message) => Outcome::Failed { message },
        };
    }
    // A different format is the point of the conversion: keep it even when larger.
    finish(&partial, &output, info.size_bytes, true)
}

/// `-ss`/`-t` input options for a clip, placed before `-i` so FFmpeg seeks quickly.
fn clip_args(clip: Clip) -> Vec<String> {
    let seconds = |ms: u64| format!("{}.{:03}", ms / 1000, ms % 1000);
    let mut args = Vec::new();
    if let Some(ms) = clip.start_ms.filter(|ms| *ms > 0) {
        args.extend(["-ss".to_owned(), seconds(ms)]);
    }
    if let Some(ms) = clip.duration_ms {
        args.extend(["-t".to_owned(), seconds(ms)]);
    }
    args
}

/// Cuts a clip: copies the streams when the start is on a keyframe, re-encodes otherwise.
#[allow(clippy::too_many_arguments)]
fn make_trim(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let start = job.recipe.clip.start_ms.unwrap_or(0);
    let keyframes = if start == 0 {
        Vec::new()
    } else {
        keyframes_before(tools, &job.input, start)
    };
    let extension = job
        .input
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let plan = match plan_trim(info, &job.recipe, &keyframes, &extension, |name| {
        caps.has_encoder(name)
    }) {
        Ok(plan) => plan,
        Err(e) => {
            return Outcome::Failed {
                message: e.to_string(),
            };
        }
    };
    report.plan = Some(Plan::Trim(plan.clone()));
    report.encoder = Some(match (&plan.method, &plan.video) {
        (TrimMethod::StreamCopy, _) => "stream copy (lossless)".to_owned(),
        (TrimMethod::Reencode, Some(video)) => {
            format!("{} (re-encoded)", video.encoder_label())
        }
        (TrimMethod::Reencode, None) => "re-encoded".to_owned(),
    });
    let output = match choose_output(batch, job, &plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let clip = Clip {
        start_ms: Some(plan.start_ms),
        duration_ms: plan.duration_ms,
    };
    let seek = clip_args(Clip {
        start_ms: Some(plan.seek_ms),
        ..clip
    });
    let args = plan.video.as_ref().map_or(&plan.args, |video| &video.args);
    let duration = clip.seconds(info.duration_seconds).unwrap_or(0.0);
    let partial = partial_path(&output);
    let sink = Sink {
        before_input: &seek,
        file: Some(&partial),
        workdir: None,
    };
    if let Err(error) = encode(tools, &job.input, &sink, args, duration, progress, cancel) {
        let _ = std::fs::remove_file(&partial);
        return match error {
            EncodeError::Cancelled => Outcome::Cancelled,
            EncodeError::Failed(message) => Outcome::Failed { message },
        };
    }
    // A clip is what was asked for, even in the rare case it is not smaller.
    finish(&partial, &output, info.size_bytes, true)
}

/// Video keyframe times (milliseconds) in the minute before `start_ms`, read from packet flags
/// without decoding.
fn keyframes_before(tools: &Toolchain, input: &Path, start_ms: u64) -> Vec<u64> {
    let Ok(url) = file_url(input) else {
        return Vec::new();
    };
    #[allow(clippy::cast_precision_loss)]
    let start = start_ms as f64 / 1000.0;
    let interval = format!("{:.3}%{:.3}", (start - 60.0).max(0.0), start + 1.0);
    let mut cmd = command(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-read_intervals",
        &interval,
    ])
    .args([
        "-show_entries",
        "packet=pts_time,flags",
        "-of",
        "csv=p=0",
        "-i",
    ])
    .arg(url);
    let Ok(output) = cmd.output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (time, flags) = line.split_once(',')?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = (time.parse::<f64>().ok()? * 1000.0).round() as u64;
            flags.contains('K').then_some(ms)
        })
        .collect()
}

/// Counter that keeps two-pass work folders unique within one process.
static WORKDIRS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Most encodes a size target may take: the first try plus corrections.
const MAX_TARGET_ATTEMPTS: u32 = 4;

/// Encodes a video to fit a file size: an analysis pass for software encoders, then final
/// passes until the measured size fits, lowering the bit rate by the measured overshoot.
#[allow(clippy::too_many_arguments)]
fn encode_to_target(
    tools: &Toolchain,
    job: &Job,
    info: &MediaInfo,
    mut plan: peraz_core::VideoPlan,
    output: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let Some(mut target) = plan.target.clone() else {
        return Outcome::Failed {
            message: "internal error: no size target".to_owned(),
        };
    };
    if info.size_bytes <= target.target_bytes {
        return Outcome::KeptOriginal(KeepReason::AlreadyWithinTarget {
            target_bytes: target.target_bytes,
        });
    }
    let duration = job
        .recipe
        .clip
        .seconds(info.duration_seconds)
        .unwrap_or(0.0);
    let workdir = std::env::temp_dir().join(format!(
        "peraz-{}-{}-{}",
        std::process::id(),
        WORKDIRS.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return Outcome::Failed {
            message: format!("cannot create a work folder: {e}"),
        };
    }
    let outcome = run_target_passes(
        tools,
        job,
        &mut plan,
        &mut target,
        output,
        &workdir,
        duration,
        progress,
        cancel,
    );
    let _ = std::fs::remove_dir_all(&workdir);
    plan.target = Some(target);
    report.plan = Some(Plan::Video(plan));
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_target_passes(
    tools: &Toolchain,
    job: &Job,
    plan: &mut peraz_core::VideoPlan,
    target: &mut SizeTarget,
    output: &Path,
    workdir: &Path,
    duration: f64,
    progress: Progress,
    cancel: &AtomicBool,
) -> Outcome {
    let seek = clip_args(job.recipe.clip);
    let as_outcome = |error: EncodeError| match error {
        EncodeError::Cancelled => Outcome::Cancelled,
        EncodeError::Failed(message) => Outcome::Failed { message },
    };
    let log = "peraz-pass".to_owned();
    let mut bit_rate = target.video_bit_rate;
    let analysis_share = if target.two_pass { 0.3 } else { 0.0 };
    if target.two_pass {
        let args = plan.ffmpeg_args(
            RateControl::Bitrate(bit_rate),
            &Pass::First { log: log.clone() },
        );
        let sink = Sink {
            before_input: &seek,
            file: None,
            workdir: Some(workdir),
        };
        let mut sub = |f: f64| progress(f * analysis_share);
        if let Err(e) = encode(tools, &job.input, &sink, &args, duration, &mut sub, cancel) {
            return as_outcome(e);
        }
    }
    let partial = partial_path(output);
    let mut smallest: Option<u64> = None;
    for attempt in 1..=MAX_TARGET_ATTEMPTS {
        let pass = if target.two_pass {
            Pass::Second { log: log.clone() }
        } else {
            Pass::Single
        };
        let args = plan.ffmpeg_args(RateControl::Bitrate(bit_rate), &pass);
        let sink = Sink {
            before_input: &seek,
            file: Some(&partial),
            workdir: Some(workdir),
        };
        let mut sub = |f: f64| progress(analysis_share + f * (1.0 - analysis_share));
        if let Err(e) = encode(tools, &job.input, &sink, &args, duration, &mut sub, cancel) {
            let _ = std::fs::remove_file(&partial);
            return as_outcome(e);
        }
        target.attempts = attempt;
        target.video_bit_rate = bit_rate;
        plan.args = args;
        let size = match std::fs::metadata(&partial) {
            Ok(m) => m.len(),
            Err(e) => {
                return Outcome::Failed {
                    message: format!("cannot read the encoded file: {e}"),
                };
            }
        };
        if size <= target.target_bytes {
            return finish(&partial, output, u64::MAX, false);
        }
        smallest = Some(smallest.map_or(size, |s| s.min(size)));
        // Lower the video bit rate by the measured overshoot, with a little extra margin.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let audio_bytes = (target.audio_bit_rate as f64 * duration / 8.0) as u64;
        let wanted_video = target.target_bytes.saturating_mul(97) / 100;
        let wanted_video = wanted_video.saturating_sub(audio_bytes);
        let actual_video = size.saturating_sub(audio_bytes).max(1);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let corrected =
            (bit_rate as f64 * (wanted_video as f64 / actual_video as f64).min(0.97)) as u64;
        bit_rate = corrected;
        if bit_rate < 20_000 {
            break;
        }
    }
    let _ = std::fs::remove_file(&partial);
    Outcome::Failed {
        message: format!(
            "could not fit under {} after {} attempts (smallest was {}); try a lower --max-width or --no-audio",
            format_bytes(target.target_bytes),
            target.attempts,
            format_bytes(smallest.unwrap_or(0))
        ),
    }
}

/// Keeps the encoded file only when it is smaller than the input ("never worse"), unless the
/// format changed on purpose (`converted`), where a larger file is still the requested result.
fn finish(partial: &Path, output: &Path, input_bytes: u64, converted: bool) -> Outcome {
    let output_bytes = match std::fs::metadata(partial) {
        Ok(m) => m.len(),
        Err(e) => {
            return Outcome::Failed {
                message: format!("cannot read the encoded file: {e}"),
            };
        }
    };
    if output_bytes >= input_bytes && !converted {
        let _ = std::fs::remove_file(partial);
        return Outcome::KeptOriginal(KeepReason::NotSmaller {
            compressed_bytes: output_bytes,
        });
    }
    if let Err(e) = std::fs::rename(partial, output) {
        let _ = std::fs::remove_file(partial);
        return Outcome::Failed {
            message: format!("cannot write {}: {e}", output.display()),
        };
    }
    Outcome::Compressed {
        output: output.to_owned(),
        output_bytes,
    }
}

fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Image => "image",
        MediaKind::Animation => "animation",
        MediaKind::Audio => "audio",
        MediaKind::Unknown => "unknown file",
        MediaKind::Pdf => "PDF",
        MediaKind::Epub => "EPUB",
    }
}

/// Highest number tried for `<name>-<number>.<ext>` before giving up.
const MAX_OUTPUT_NUMBER: u32 = 9999;

/// The output path for this job. Numbered names take the first `<name>-<number>.<ext>` that
/// neither exists nor belongs to another job of this batch, so nothing is ever replaced; an
/// explicit `--output` file goes through the usual checks.
fn choose_output(batch: &Batch, job: &Job, extension: &str) -> Result<PathBuf, Outcome> {
    let first = job.output.resolve(&job.input, extension, 1);
    if !job.output.is_numbered() {
        check_output_path(batch, job, &first)?;
        return Ok(first);
    }
    if job.existing == ExistingOutput::Skip && first.exists() {
        return Err(Outcome::Skipped {
            message: format!("already compressed: {} exists", first.display()),
        });
    }
    check_output_folder(&first)?;
    for number in 1..=MAX_OUTPUT_NUMBER {
        let candidate = job.output.resolve(&job.input, extension, number);
        if !candidate.exists() && batch.claim(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Outcome::Failed {
        message: format!(
            "every name up to {} is taken; move some results away or use --output",
            job.output
                .resolve(&job.input, extension, MAX_OUTPUT_NUMBER)
                .display()
        ),
    })
}

fn check_output_folder(output: &Path) -> Result<(), Outcome> {
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty())
        && !dir.is_dir()
    {
        return Err(Outcome::Failed {
            message: format!("output folder {} does not exist", dir.display()),
        });
    }
    Ok(())
}

fn check_output_path(batch: &Batch, job: &Job, output: &Path) -> Result<(), Outcome> {
    let fail = |message: String| Outcome::Failed { message };
    let same_file = match (
        std::fs::canonicalize(&job.input),
        std::fs::canonicalize(output),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same_file {
        return Err(fail(
            "the output path is the input file; choose a different output".to_owned(),
        ));
    }
    if !batch.claim(output) {
        return Err(fail(format!(
            "{} is already the output of another file in this batch; rename one input or use --output",
            output.display()
        )));
    }
    if output.exists() {
        match job.existing {
            ExistingOutput::Replace => {}
            ExistingOutput::Skip => {
                return Err(Outcome::Skipped {
                    message: format!("already compressed: {} exists", output.display()),
                });
            }
            ExistingOutput::Fail => {
                return Err(fail(format!(
                    "{} already exists (use --force to replace it)",
                    output.display()
                )));
            }
        }
    }
    check_output_folder(output)
}

/// A hidden sibling of the output, so a crash never leaves a half-written file under the final name.
fn partial_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    output.with_file_name(format!(".{name}.peraz-partial"))
}

fn file_url(path: &Path) -> Result<OsString, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut url = OsString::from("file:");
    url.push(absolute.as_os_str());
    Ok(url)
}

enum EncodeError {
    Cancelled,
    Failed(String),
}

impl From<String> for EncodeError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Where an encode writes.
struct Sink<'a> {
    /// Arguments placed before `-i` (input seeking: `-ss`, `-t`).
    before_input: &'a [String],
    /// The output file, or `None` for an analysis pass that writes nothing.
    file: Option<&'a Path>,
    /// FFmpeg's working directory (holds two-pass statistics), or `None` for the current one.
    workdir: Option<&'a Path>,
}

fn encode(
    tools: &Toolchain,
    input: &Path,
    sink: &Sink,
    plan_args: &[String],
    duration_seconds: f64,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<(), EncodeError> {
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-y"])
        .args(sink.before_input)
        .arg("-i")
        .arg(file_url(input)?)
        .args(plan_args)
        .args(["-progress", "pipe:1", "-nostats"]);
    match sink.file {
        Some(file) => cmd.arg(file_url(file)?),
        None => cmd.arg("-"),
    };
    if let Some(dir) = sink.workdir {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", tools.ffmpeg.display()))?;

    // Drain stderr on its own thread so a chatty FFmpeg can never block on a full pipe.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let stdout = child.stdout.take().expect("stdout is piped");

    let child = Mutex::new(child);
    let finished = AtomicBool::new(false);
    let status = std::thread::scope(|scope| {
        // A watcher kills FFmpeg when cancellation is requested. A terminal Ctrl-C reaches FFmpeg
        // directly too, but a signal sent only to Peraz (e.g. by the app) would not.
        scope.spawn(|| {
            while !finished.load(Ordering::SeqCst) {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.lock().map(|mut c| c.kill());
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(fraction) = parse_progress(&line, duration_seconds) {
                progress(fraction);
            }
        }
        // stdout closed: FFmpeg has exited or is about to, so waiting here is brief.
        let status = child
            .lock()
            .map_err(|_| "ffmpeg watcher panicked".to_owned())
            .and_then(|mut c| c.wait().map_err(|e| format!("ffmpeg did not finish: {e}")));
        finished.store(true, Ordering::SeqCst);
        status
    })?;
    let stderr = stderr_reader.join().unwrap_or_default();

    if cancel.load(Ordering::SeqCst) {
        return Err(EncodeError::Cancelled);
    }
    if status.success() {
        progress(1.0);
        Ok(())
    } else {
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error message");
        Err(format!("ffmpeg failed ({status}): {}", last.trim()).into())
    }
}

/// Reads one `-progress` line. FFmpeg reports `out_time_us` in microseconds.
fn parse_progress(line: &str, duration_seconds: f64) -> Option<f64> {
    let micros: f64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    (duration_seconds > 0.0 && micros >= 0.0)
        .then(|| (micros / 1e6 / duration_seconds).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress("out_time_us=5000000", 10.0), Some(0.5));
        assert_eq!(parse_progress("out_time_us=20000000", 10.0), Some(1.0));
        assert_eq!(parse_progress("out_time_us=N/A", 10.0), None);
        assert_eq!(parse_progress("out_time_us=-1000", 10.0), None);
        assert_eq!(parse_progress("frame=10", 10.0), None);
        assert_eq!(parse_progress("out_time_us=1", 0.0), None);
    }

    #[test]
    fn partial_file_is_hidden_next_to_output() {
        assert_eq!(
            partial_path(Path::new("/a/b/clip-1.mp4")),
            PathBuf::from("/a/b/.clip-1.mp4.peraz-partial")
        );
    }
}
