//! Running a [`Job`]: probe, plan, encode to a temporary file, then keep the result only if it
//! is actually better than the input.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peraz_core::{
    ImageDecoder, Job, KeepReason, MediaInfo, MediaKind, Outcome, Plan, PlanError, Report,
    VideoOutput, plan_gif, plan_image, plan_video,
};

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
        elapsed_seconds: 0.0,
    };
    report.outcome = if cancel.load(Ordering::SeqCst) {
        Outcome::Cancelled
    } else {
        run(tools, caps, job, progress, cancel, &mut report)
    };
    report.elapsed_seconds = started.elapsed().as_secs_f64();
    report
}

fn run(
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

    let wants_gif = info.kind == MediaKind::Animation
        || (info.kind == MediaKind::Video && job.recipe.video.output == VideoOutput::Gif);
    if wants_gif {
        return make_gif(tools, job, &info, progress, cancel, report);
    }
    match info.kind {
        MediaKind::Video => {
            let plan = match plan_video(&info, &job.recipe, |name| caps.has_encoder(name)) {
                Ok(plan) => plan,
                Err(e) => return failed(e.to_string()),
            };
            report.plan = Some(Plan::Video(plan.clone()));
            report.encoder = Some(plan.encoder.ffmpeg_name().to_owned());
            let output = job.output.resolve(&job.input, "mp4");
            if let Err(message) = check_output_path(job, &output) {
                return failed(message);
            }
            let partial = partial_path(&output);
            let duration = info.duration_seconds.unwrap_or(0.0);
            if let Err(error) = encode(
                tools, &job.input, &partial, &plan.args, duration, progress, cancel,
            ) {
                let _ = std::fs::remove_file(&partial);
                return match error {
                    EncodeError::Cancelled => Outcome::Cancelled,
                    EncodeError::Failed(message) => failed(message),
                };
            }
            finish(&partial, &output, info.size_bytes, false)
        }
        MediaKind::Image => {
            let plan = match plan_image(&info, &job.recipe) {
                Ok(plan) => plan,
                Err(e @ PlanError::UnsupportedImage { .. }) => {
                    return Outcome::Skipped {
                        message: e.to_string(),
                    };
                }
                Err(e) => return failed(e.to_string()),
            };
            report.plan = Some(Plan::Image(plan.clone()));
            let output = job.output.resolve(&job.input, plan.extension);
            if let Err(message) = check_output_path(job, &output) {
                return failed(message);
            }
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
        other => Outcome::Skipped {
            message: format!("{} compression is not available yet", kind_name(other)),
        },
    }
}

fn make_gif(
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
    let output = job.output.resolve(&job.input, "gif");
    if let Err(message) = check_output_path(job, &output) {
        return Outcome::Failed { message };
    }
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
    }
}

fn check_output_path(job: &Job, output: &Path) -> Result<(), String> {
    let same_file = match (
        std::fs::canonicalize(&job.input),
        std::fs::canonicalize(output),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same_file {
        return Err("the output path is the input file; choose a different output".to_owned());
    }
    if output.exists() && !job.overwrite {
        return Err(format!(
            "{} already exists (use --force to replace it)",
            output.display()
        ));
    }
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty())
        && !dir.is_dir()
    {
        return Err(format!("output folder {} does not exist", dir.display()));
    }
    Ok(())
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

fn encode(
    tools: &Toolchain,
    input: &Path,
    output: &Path,
    plan_args: &[String],
    duration_seconds: f64,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<(), EncodeError> {
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-y", "-i"])
        .arg(file_url(input)?)
        .args(plan_args)
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(file_url(output)?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
            partial_path(Path::new("/a/b/clip-peraz.mp4")),
            PathBuf::from("/a/b/.clip-peraz.mp4.peraz-partial")
        );
    }
}
