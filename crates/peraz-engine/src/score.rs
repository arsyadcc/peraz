//! Score targets: the smallest file that still reaches a measured visual quality.
//!
//! Videos: the CRF is searched on samples of the video, each encode scored with VMAF against
//! the source (at the output's size), and the highest CRF that reaches the score encodes the
//! whole video. Short videos are scored whole; long ones on a few evenly spread samples, so
//! the search costs about the same whatever the length.
//!
//! Still images: the encoder's quality setting is searched, each result decoded and scored
//! with SSIMULACRA2 against the picture the encoder was given (upright, at the output's size).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use peraz_core::plan::{AudioPlan, fit_within};
use peraz_core::{
    ImageEncoding, ImagePlan, Job, MediaInfo, MediaKind, Metric, Outcome, Pass, Plan, RateControl,
    Report, Score, VideoPlan,
};

use crate::compress::{EncodeError, Progress, Sink, encode, finish, make_workdir, partial_path};
use crate::ffmpeg::{Toolchain, command};
use crate::image::{Decoded, EncodedImage, encode_prepared, prepare};

/// Videos up to this long (seconds) are scored whole.
const WHOLE_SECONDS: f64 = 30.0;
/// Longer videos are scored on this many samples of `SAMPLE_SECONDS` each.
const SAMPLES: u32 = 4;
const SAMPLE_SECONDS: f64 = 5.0;
/// CRF values searched (x264 and x265 use the same scale).
const CRF_MIN: u8 = 10;
const CRF_MAX: u8 = 40;
/// Most sample encodes per video: a binary search over `CRF_MIN..=CRF_MAX` needs about five.
const MAX_PROBES: u32 = 6;
/// VMAF's model is made for pictures up to 1080p: larger ones are scaled into this box (or its
/// portrait twin) for scoring, which also keeps scoring fast.
const SCORE_BOX: (u32, u32) = (1920, 1080);

/// A part of the source to score: start and length in seconds (`None`: to the end).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    start: f64,
    length: Option<f64>,
}

/// Where to sample a clip that starts at `start` and lasts `duration` seconds (when known).
fn windows(start: f64, duration: Option<f64>) -> Vec<Window> {
    match duration {
        Some(d) if d > WHOLE_SECONDS => (0..SAMPLES)
            .map(|i| {
                let center = start + d * (f64::from(i) + 0.5) / f64::from(SAMPLES);
                Window {
                    start: center - SAMPLE_SECONDS / 2.0,
                    length: Some(SAMPLE_SECONDS),
                }
            })
            .collect(),
        _ => vec![Window {
            start,
            length: duration,
        }],
    }
}

/// The highest setting in `range` whose score reaches `target` (for a CRF: the smallest file),
/// found by bisection from `start`, assuming the score falls as the setting rises. Returns the
/// setting to use and every score measured. When none reaches the target, the lowest one
/// measured is used: the closest the search got.
fn search<E>(
    target: f64,
    start: u8,
    range: std::ops::RangeInclusive<u8>,
    max_probes: usize,
    mut score: impl FnMut(u8) -> Result<f64, E>,
) -> Result<(u8, BTreeMap<u8, f64>), E> {
    let mut scores = BTreeMap::new();
    let (mut low, mut high) = (*range.start(), *range.end());
    let mut value = start.clamp(low, high);
    let mut best = None;
    while scores.len() < max_probes {
        let s = score(value)?;
        scores.insert(value, s);
        if s >= target {
            best = Some(value);
            low = value + 1;
        } else if value == *range.start() {
            break;
        } else {
            high = value - 1;
        }
        if low > high {
            break;
        }
        value = low + (high - low) / 2;
    }
    let chosen = best.unwrap_or_else(|| *scores.keys().next().unwrap_or(range.start()));
    Ok((chosen, scores))
}

/// Encodes a video at the highest CRF that reaches its VMAF target.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_to_vmaf(
    tools: &Toolchain,
    job: &Job,
    info: &MediaInfo,
    mut plan: VideoPlan,
    output: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let Some(mut target) = plan.score_target.clone() else {
        return Outcome::Failed {
            message: "internal error: no score target".to_owned(),
        };
    };
    let workdir = match make_workdir() {
        Ok(dir) => dir,
        Err(outcome) => return outcome,
    };
    let clip = job.recipe.clip;
    #[allow(clippy::cast_precision_loss)] // milliseconds in f64 are exact far beyond any video
    let start = clip.start_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
    let duration = clip.seconds(info.duration_seconds);
    let windows = windows(start, duration);
    let picture = plan.scale_to.or_else(|| {
        info.video
            .as_ref()
            .map(peraz_core::VideoTrack::display_size)
    });

    let searched = search_crf(
        tools,
        job,
        &plan,
        &windows,
        picture,
        target.vmaf,
        target.crf,
        &workdir,
        progress,
        cancel,
    );
    let _ = std::fs::remove_dir_all(&workdir);
    let (crf, scores, probes) = match searched {
        Ok(found) => found,
        Err(EncodeError::Cancelled) => return Outcome::Cancelled,
        Err(EncodeError::Failed(message)) => return Outcome::Failed { message },
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sampled_ms = windows
        .iter()
        .map(|w| (w.length.or(duration).unwrap_or(0.0) * 1000.0).round() as u64)
        .sum();
    target.crf = crf;
    target.probes = probes;
    target.sampled_ms = sampled_ms;
    plan.args = plan.ffmpeg_args(RateControl::Crf(crf), &Pass::Single);
    plan.score_target = Some(target.clone());
    report.score = Some(Score {
        metric: Metric::Vmaf,
        target: target.vmaf,
        score: (scores.get(&crf).copied().unwrap_or(0.0) * 100.0).round() / 100.0,
        sampled: windows.len() > 1,
    });
    report.plan = Some(Plan::Video(plan.clone()));

    // The whole video at the chosen CRF.
    let partial = partial_path(output);
    let seek = crate::compress::clip_args(clip);
    let sink = Sink {
        before_input: &seek,
        file: Some(&partial),
        workdir: None,
    };
    let mut sub = |f: f64| progress(SEARCH_SHARE + f * (1.0 - SEARCH_SHARE));
    if let Err(error) = encode(
        tools,
        &job.input,
        &sink,
        &plan.args,
        duration.unwrap_or(0.0),
        &mut sub,
        cancel,
    ) {
        let _ = std::fs::remove_file(&partial);
        return match error {
            EncodeError::Cancelled => Outcome::Cancelled,
            EncodeError::Failed(message) => Outcome::Failed { message },
        };
    }
    finish(
        &partial,
        output,
        info.size_bytes,
        info.kind == MediaKind::Animation,
    )
}

/// Share of the progress bar for the search; the final encode takes the rest.
const SEARCH_SHARE: f64 = 0.6;

/// Makes lossless references of the samples, then searches the CRF. Returns the CRF, the scores
/// measured, and the number of probes.
#[allow(clippy::too_many_arguments)]
fn search_crf(
    tools: &Toolchain,
    job: &Job,
    plan: &VideoPlan,
    windows: &[Window],
    picture: Option<(u32, u32)>,
    vmaf_target: u8,
    start_crf: u8,
    workdir: &Path,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<(u8, BTreeMap<u8, f64>, u32), EncodeError> {
    // References: the samples at the output's size and pixel format, losslessly (FFV1).
    let mut reference_args: Vec<String> = ["-map", "0:v:0", "-an", "-sn", "-dn"]
        .map(String::from)
        .to_vec();
    if let Some((w, h)) = plan.scale_to {
        reference_args.extend(["-vf".into(), format!("scale={w}:{h}:flags=lanczos")]);
    }
    reference_args.extend(
        [
            "-pix_fmt",
            plan.pixel_format,
            "-c:v",
            "ffv1",
            "-f",
            "matroska",
        ]
        .map(String::from),
    );
    let mut references = Vec::new();
    for (i, window) in windows.iter().enumerate() {
        let file = workdir.join(format!("reference-{i}.mkv"));
        let mut seek = Vec::new();
        if window.start > 0.0 {
            seek.extend(["-ss".to_owned(), format!("{:.3}", window.start)]);
        }
        if let Some(length) = window.length {
            seek.extend(["-t".to_owned(), format!("{length:.3}")]);
        }
        let sink = Sink {
            before_input: &seek,
            file: Some(&file),
            workdir: None,
        };
        let done = i;
        #[allow(clippy::cast_precision_loss)]
        let mut sub = |f: f64| progress((done as f64 + f) / windows.len() as f64 * 0.1);
        encode(
            tools,
            &job.input,
            &sink,
            &reference_args,
            window.length.unwrap_or(0.0),
            &mut sub,
            cancel,
        )?;
        references.push(file);
    }

    // Probes: the references encoded as the final video would be, minus scaling and audio.
    let mut probe_plan = plan.clone();
    probe_plan.scale_to = None;
    probe_plan.audio = AudioPlan::None;
    let mut probes = 0;
    let (crf, scores) = search(
        f64::from(vmaf_target),
        start_crf,
        CRF_MIN..=CRF_MAX,
        MAX_PROBES as usize,
        |crf| {
            probes += 1;
            let args = probe_plan.ffmpeg_args(RateControl::Crf(crf), &Pass::Single);
            let (mut weighted, mut frames) = (0.0, 0u64);
            for (i, reference) in references.iter().enumerate() {
                let encoded = workdir.join(format!("probe-{i}.mp4"));
                let sink = Sink {
                    before_input: &[],
                    file: Some(&encoded),
                    workdir: None,
                };
                encode(tools, reference, &sink, &args, 0.0, &mut |_| {}, cancel)?;
                let (mean, count) = vmaf(tools, &encoded, reference, picture, workdir, i, cancel)?;
                #[allow(clippy::cast_precision_loss)]
                {
                    weighted += mean * count as f64;
                }
                frames += count;
                let _ = std::fs::remove_file(&encoded);
            }
            #[allow(clippy::cast_precision_loss)]
            progress(0.1 + f64::from(probes) / f64::from(MAX_PROBES) * (SEARCH_SHARE - 0.1));
            if frames == 0 {
                return Err(EncodeError::Failed(
                    "VMAF scored no frames; the video may be too short".to_owned(),
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            Ok(weighted / frames as f64)
        },
    )?;
    Ok((crf, scores, probes))
}

/// Mean VMAF of `distorted` against `reference` (same size and frame count), and the number of
/// frames scored. Frames are paired by position, not time: containers round frame times
/// differently, and pairing by time can compare neighbouring frames.
fn vmaf(
    tools: &Toolchain,
    distorted: &Path,
    reference: &Path,
    picture: Option<(u32, u32)>,
    workdir: &Path,
    index: usize,
    cancel: &AtomicBool,
) -> Result<(f64, u64), EncodeError> {
    let scale = picture
        .and_then(|(w, h)| {
            let (box_w, box_h) = if h > w {
                (SCORE_BOX.1, SCORE_BOX.0)
            } else {
                SCORE_BOX
            };
            fit_within((w, h), Some(box_w), Some(box_h))
        })
        .map_or(String::new(), |(w, h)| {
            format!(",scale={w}:{h}:flags=bicubic")
        });
    let log = format!("vmaf-{index}.json");
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let graph = format!(
        "[0:v]settb=1/1000,setpts=N{scale}[d];[1:v]settb=1/1000,setpts=N{scale}[r];\
         [d][r]libvmaf=model=version=vmaf_v0.6.1:log_fmt=json:log_path={log}:n_threads={threads}"
    );
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(crate::compress::file_url(distorted)?)
        .arg("-i")
        .arg(crate::compress::file_url(reference)?)
        .args(["-lavfi", &graph, "-f", "null", "-"])
        .current_dir(workdir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", tools.ffmpeg.display()))?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(EncodeError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("ffmpeg did not finish: {e}").into()),
        }
    };
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error message");
        return Err(format!("VMAF scoring failed ({status}): {}", last.trim()).into());
    }
    let path = workdir.join(&log);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read the VMAF log: {e}"))?;
    let _ = std::fs::remove_file(&path);
    parse_vmaf_log(&text).ok_or_else(|| EncodeError::Failed("unexpected VMAF log".to_owned()))
}

/// The pooled mean and the frame count from a libvmaf JSON log.
fn parse_vmaf_log(text: &str) -> Option<(f64, u64)> {
    let log: serde_json::Value = serde_json::from_str(text).ok()?;
    let mean = log["pooled_metrics"]["vmaf"]["mean"].as_f64()?;
    let frames = log["frames"].as_array()?.len() as u64;
    Some((mean, frames))
}

/// Image quality settings searched (0–100 for every image encoder); below 10 nothing is
/// worth keeping.
const IMAGE_QUALITY_MIN: u8 = 10;
/// Most image encodes per picture: a bisection over 10–100 needs about seven.
const IMAGE_PROBES: usize = 8;

/// The plan's encoding with its quality setting replaced by `quality`. PNG searches the palette
/// quality, and at 100 stays true-color (lossless) when a palette cannot reach that.
fn with_quality(encoding: ImageEncoding, quality: u8) -> ImageEncoding {
    match encoding {
        ImageEncoding::Jpeg { .. } => ImageEncoding::Jpeg { quality },
        ImageEncoding::Webp { .. } => ImageEncoding::Webp { quality },
        ImageEncoding::Avif { .. } => ImageEncoding::Avif { quality },
        ImageEncoding::Png { .. } if quality >= 100 => ImageEncoding::Png {
            min_quality: 100,
            max_quality: 100,
        },
        ImageEncoding::Png { .. } => ImageEncoding::Png {
            min_quality: 0,
            max_quality: quality,
        },
    }
}

/// The quality setting an encoding starts from.
fn quality_of(encoding: ImageEncoding) -> u8 {
    match encoding {
        ImageEncoding::Jpeg { quality }
        | ImageEncoding::Webp { quality }
        | ImageEncoding::Avif { quality } => quality,
        ImageEncoding::Png { max_quality, .. } => max_quality,
    }
}

/// Encodes a still image at the lowest quality setting that reaches its SSIMULACRA2 target.
/// `plan.encoding` is updated to the setting used. Returns the encoded image and its score, or
/// no score for pictures too small to score (under 8 pixels a side), encoded as planned.
pub(crate) fn encode_image_to_ssimulacra2(
    tools: Option<&Toolchain>,
    decoded: Decoded,
    plan: &mut ImagePlan,
    target: u8,
    cancel: &AtomicBool,
) -> Result<(EncodedImage, Option<f64>), String> {
    let prepared = prepare(decoded, plan);
    if prepared.image.width() < 8 || prepared.image.height() < 8 {
        return Ok((encode_prepared(&prepared, plan)?, None));
    }
    let reference = linear_input(&prepared.image)?;
    let mut results: BTreeMap<u8, EncodedImage> = BTreeMap::new();
    // Search the loss (100 - quality), so a higher setting means a smaller file, as for CRF.
    let start = 100 - quality_of(plan.encoding);
    let (loss, scores) = search(
        f64::from(target),
        start,
        0..=100 - IMAGE_QUALITY_MIN,
        IMAGE_PROBES,
        |loss| {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_owned());
            }
            let mut probe = plan.clone();
            probe.encoding = with_quality(plan.encoding, 100 - loss);
            let encoded = encode_prepared(&prepared, &probe)?;
            let image = decode_encoded(tools, &encoded.bytes, plan.extension)?;
            let score =
                ssimulacra2::compute_frame_ssimulacra2(reference.clone(), linear_input(&image)?)
                    .map_err(|e| format!("SSIMULACRA2: {e}"))?;
            results.insert(loss, encoded);
            Ok(score)
        },
    )?;
    plan.encoding = with_quality(plan.encoding, 100 - loss);
    let encoded = results
        .remove(&loss)
        .ok_or("internal error: the chosen image was not kept")?;
    Ok((encoded, scores.get(&loss).copied()))
}

/// A picture as SSIMULACRA2 reads it: sRGB, transparency composited onto white.
fn linear_input(image: &image::DynamicImage) -> Result<ssimulacra2::Rgb, String> {
    let rgba = image.to_rgba32f();
    let data = rgba
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            [r * a + (1.0 - a), g * a + (1.0 - a), b * a + (1.0 - a)]
        })
        .collect();
    ssimulacra2::Rgb::new(
        data,
        rgba.width() as usize,
        rgba.height() as usize,
        ssimulacra2::TransferCharacteristic::SRGB,
        ssimulacra2::ColorPrimaries::BT709,
    )
    .map_err(|e| format!("SSIMULACRA2: {e}"))
}

/// Reads back an image Peraz just encoded. AVIF goes through FFmpeg, like AVIF inputs.
fn decode_encoded(
    tools: Option<&Toolchain>,
    bytes: &[u8],
    extension: &str,
) -> Result<image::DynamicImage, String> {
    if extension != "avif" {
        return image::load_from_memory(bytes).map_err(|e| format!("cannot read back: {e}"));
    }
    let tools = tools.ok_or(
        "--target-ssimulacra2 with AVIF output needs FFmpeg, to read each try back".to_owned(),
    )?;
    let workdir = make_workdir().map_err(|outcome| match outcome {
        Outcome::Failed { message } => message,
        _ => "cannot create a work folder".to_owned(),
    })?;
    let file = workdir.join("probe.avif");
    let decoded = std::fs::write(&file, bytes)
        .map_err(|e| format!("cannot write a work file: {e}"))
        .and_then(|()| crate::heif::decode(tools, &file));
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(decoded?.image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_clips_are_scored_whole_and_long_ones_sampled() {
        assert_eq!(
            windows(2.0, Some(20.0)),
            [Window {
                start: 2.0,
                length: Some(20.0)
            }]
        );
        assert_eq!(windows(0.0, None).len(), 1, "unknown length: whole");
        let samples = windows(10.0, Some(100.0));
        assert_eq!(samples.len(), SAMPLES as usize);
        assert!((samples[0].start - (10.0 + 12.5 - 2.5)).abs() < 1e-9);
        assert!(
            samples
                .iter()
                .all(|w| w.start >= 10.0 && w.start + SAMPLE_SECONDS <= 110.0)
        );
    }

    /// A made-up encoder whose score falls by one point per CRF step.
    #[allow(clippy::unnecessary_wraps)] // `search` takes a fallible scorer
    fn linear(crf: u8) -> Result<f64, ()> {
        Ok(120.0 - f64::from(crf))
    }

    #[test]
    fn search_finds_the_highest_crf_that_reaches_the_target() {
        // Reaches 93 up to CRF 27.
        let (crf, scores) =
            search(93.0, 24, CRF_MIN..=CRF_MAX, MAX_PROBES as usize, linear).unwrap();
        assert_eq!(crf, 27);
        assert!(scores.len() <= MAX_PROBES as usize);
        assert!(scores[&27] >= 93.0);
        // Start above the answer.
        let (crf, _) = search(100.0, 24, CRF_MIN..=CRF_MAX, MAX_PROBES as usize, linear).unwrap();
        assert_eq!(crf, 20);
    }

    #[test]
    fn search_uses_the_best_it_found_when_the_target_is_out_of_reach() {
        let (crf, _) = search(200.0, 24, CRF_MIN..=CRF_MAX, MAX_PROBES as usize, linear).unwrap();
        assert_eq!(crf, CRF_MIN, "nothing reaches 200: the lowest CRF");
        let (crf, scores) =
            search(1.0, 24, CRF_MIN..=CRF_MAX, MAX_PROBES as usize, linear).unwrap();
        assert_eq!(crf, CRF_MAX, "everything reaches 1: the highest CRF");
        assert!(scores.len() <= MAX_PROBES as usize);
    }

    fn photo_like(width: u32, height: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            // Smooth gradients with some fine texture, so quality settings make a difference.
            #[allow(clippy::cast_possible_truncation)]
            let t = ((x * 7 + y * 13) % 23) as u8;
            #[allow(clippy::cast_possible_truncation)]
            image::Rgb([
                (x * 255 / width) as u8 ^ t,
                (y * 255 / height) as u8,
                ((x + y) * 127 / (width + height)) as u8 + t,
            ])
        }))
    }

    fn jpeg_plan() -> ImagePlan {
        ImagePlan {
            encoding: ImageEncoding::Jpeg { quality: 80 },
            max_width: None,
            max_height: None,
            flatten_alpha: false,
            extension: "jpg",
            decoder: peraz_core::ImageDecoder::Native,
            converts_format: false,
            target_ssimulacra2: Some(80),
        }
    }

    #[test]
    fn images_reach_their_target_with_the_lowest_setting() {
        let cancel = AtomicBool::new(false);
        let mut scores = Vec::new();
        for target in [40, 65] {
            let mut plan = jpeg_plan();
            let decoded = Decoded {
                image: photo_like(96, 64),
                icc: None,
            };
            let (encoded, score) =
                encode_image_to_ssimulacra2(None, decoded, &mut plan, target, &cancel).unwrap();
            let score = score.unwrap();
            assert!(score >= f64::from(target), "{target}: {score}");
            let ImageEncoding::Jpeg { quality } = plan.encoding else {
                panic!("still JPEG")
            };
            scores.push((quality, encoded.bytes.len()));
        }
        assert!(
            scores[0].0 < scores[1].0 && scores[0].1 < scores[1].1,
            "a lower target gives a lower setting and a smaller file: {scores:?}"
        );
    }

    #[test]
    fn tiny_images_are_encoded_as_planned_without_a_score() {
        let mut plan = jpeg_plan();
        let decoded = Decoded {
            image: photo_like(6, 6),
            icc: None,
        };
        let (_, score) =
            encode_image_to_ssimulacra2(None, decoded, &mut plan, 80, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(score, None);
        assert_eq!(plan.encoding, ImageEncoding::Jpeg { quality: 80 });
    }

    #[test]
    fn png_searches_the_palette_and_ends_lossless() {
        assert_eq!(
            with_quality(
                ImageEncoding::Png {
                    min_quality: 60,
                    max_quality: 95
                },
                70
            ),
            ImageEncoding::Png {
                min_quality: 0,
                max_quality: 70
            }
        );
        assert_eq!(
            with_quality(
                ImageEncoding::Png {
                    min_quality: 60,
                    max_quality: 95
                },
                100
            ),
            ImageEncoding::Png {
                min_quality: 100,
                max_quality: 100
            }
        );
    }

    #[test]
    fn reads_the_vmaf_log() {
        let log = r#"{"version":"3.0.0","frames":[{"frameNum":0,"metrics":{"vmaf":91.2}},{"frameNum":1,"metrics":{"vmaf":94.8}}],"pooled_metrics":{"vmaf":{"min":91.2,"max":94.8,"mean":93.0,"harmonic_mean":92.9}}}"#;
        assert_eq!(parse_vmaf_log(log), Some((93.0, 2)));
        assert_eq!(parse_vmaf_log("{}"), None);
    }
}
