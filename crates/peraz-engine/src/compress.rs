//! Running a [`Job`]: probe, plan, encode to a temporary file, then keep the result only if it
//! is actually better than the input.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peraz_core::plan::fit_within;
use peraz_core::units::format_bytes;
use peraz_core::{
    Clip, ExistingOutput, HdrHandling, ImageDecoder, ImageEncoding, Job, KeepReason, MediaInfo,
    MediaKind, Outcome, Pass, Plan, PlanError, RateControl, Report, SizeTarget, SkipReason,
    TrimMethod, VideoOutput, plan_animated, plan_animated_jxl, plan_animated_webp, plan_apng,
    plan_document, plan_gif, plan_image, plan_trim, plan_video, wants_animated_webp, wants_apng,
};

use crate::batch::{Batch, Lane};
use crate::ffmpeg::{Capabilities, Toolchain, command};
use crate::gif::{GifError, encode_gif};
use crate::image::{decode_native, encode_image};
use crate::probe::{probe, probe_without_ffmpeg};

/// Why a file cannot be compressed when FFmpeg is missing.
const NEEDS_FFMPEG: &str = "this file needs FFmpeg, which was not found. Install FFmpeg, or \
                            point Peraz to it with --ffmpeg or PERAZ_FFMPEG";

/// Progress callback: receives the fraction done, from 0.0 to 1.0.
pub type Progress<'a> = &'a mut dyn FnMut(f64);

/// Compresses one file. Never returns an error: every problem becomes [`Outcome::Failed`] in the
/// report, so batches keep going and every input gets an answer.
///
/// Setting `cancel` (e.g. from a Ctrl-C handler) stops FFmpeg, removes the partial file, and
/// returns [`Outcome::Cancelled`].
///
/// Without `tools` (no FFmpeg), only PDFs, EPUBs, still JPEG, PNG, WebP, BMP, TIFF, and JPEG XL
/// images, and animated PNGs can be compressed; other files fail with a message saying they need
/// FFmpeg.
pub fn compress(
    tools: Option<&Toolchain>,
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
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
) -> Report {
    compress_observed(batch, tools, caps, job, &mut OnlyProgress(progress), cancel)
}

/// Follows the compression of one file, for progress displays.
pub trait Observer {
    /// Called once per file, before any progress: with what was read about the input, or `None`
    /// when it could not be read (or the run was already cancelled).
    fn started(&mut self, info: Option<&MediaInfo>);
    /// The fraction done, from 0.0 to 1.0.
    fn progress(&mut self, fraction: f64);
}

/// An [`Observer`] for a progress callback alone.
pub struct OnlyProgress<'a>(pub Progress<'a>);

impl std::fmt::Debug for OnlyProgress<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnlyProgress")
    }
}

impl Observer for OnlyProgress<'_> {
    fn started(&mut self, _: Option<&MediaInfo>) {}
    fn progress(&mut self, fraction: f64) {
        (self.0)(fraction);
    }
}

/// Like [`compress_in`], telling `observer` when the file was read as well as the progress.
pub fn compress_observed(
    batch: &Batch,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    observer: &mut dyn Observer,
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
        score: None,
        warnings: Vec::new(),
        elapsed_seconds: 0.0,
    };
    report.outcome = if cancel.load(Ordering::SeqCst) {
        observer.started(None);
        Outcome::Cancelled
    } else if !job.recompress && crate::mark::is_current(&job.input) {
        // A result Peraz made, unchanged since: compressing it again would only cost time and
        // quality.
        observer.started(None);
        report.input_bytes = std::fs::metadata(&job.input).ok().map(|m| m.len());
        Outcome::Skipped {
            reason: SkipReason::AlreadyCompressed,
            message: "Peraz made this file and it has not changed since; use --recompress to \
                      compress it again"
                .to_owned(),
        }
    } else {
        match probe_input(tools, &job.input) {
            Ok(info) => {
                observer.started(Some(&info));
                let mut progress = |fraction: f64| observer.progress(fraction);
                run(
                    batch,
                    tools,
                    caps,
                    job,
                    &info,
                    &mut progress,
                    cancel,
                    &mut report,
                )
            }
            Err(message) => {
                observer.started(None);
                Outcome::Failed { message }
            }
        }
    };
    match &report.outcome {
        // A clip that was only cut is not a compressed result.
        Outcome::Compressed { output, .. } if !matches!(report.plan, Some(Plan::Trim(_))) => {
            crate::mark::mark(output);
        }
        Outcome::Compressed { .. } => {}
        _ => batch.release(&job.input),
    }
    report.elapsed_seconds = started.elapsed().as_secs_f64();
    report
}

#[allow(clippy::too_many_arguments)]
fn run(
    batch: &Batch,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    report.kind = Some(info.kind);
    report.input_bytes = Some(info.size_bytes);
    // Decoders fill a cut-off JPEG or GIF with gray or drop frames instead of failing.
    if matches!(info.kind, MediaKind::Image | MediaKind::Animation)
        && let Some(problem) = crate::image::incomplete(&job.input)
    {
        return failed(problem);
    }

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

    match info.kind {
        // Peraz reads and writes the frames of an animated WebP itself.
        MediaKind::Animation if info.video.as_ref().is_some_and(|v| v.codec == "webp") => {
            return make_webp_animation(batch, tools, caps, job, info, progress, cancel, report);
        }
        MediaKind::Image => {
            return make_image(batch, tools, caps, job, info, progress, cancel, report);
        }
        MediaKind::Pdf | MediaKind::Epub => {
            return make_document(batch, job, info, progress, cancel, report);
        }
        // FFmpeg builds rarely read JPEG XL: djxl turns the animation into an APNG first.
        MediaKind::Animation if info.container == "jpegxl" => {
            let workdir = match make_workdir() {
                Ok(dir) => dir,
                Err(outcome) => return outcome,
            };
            let outcome = make_jxl_animation(
                batch, tools, caps, job, info, &workdir, progress, cancel, report,
            );
            let _ = std::fs::remove_dir_all(&workdir);
            return outcome;
        }
        _ => {}
    }
    if wants_apng(info, &job.recipe) {
        return make_apng(
            batch, tools, job, info, &job.input, progress, cancel, report,
        );
    }
    // A GIF that stays a GIF is read and written in process.
    let is_gif = info.video.as_ref().is_some_and(|v| v.codec == "gif");
    if info.kind == MediaKind::Animation
        && is_gif
        && matches!(
            job.recipe.video.output,
            VideoOutput::Keep | VideoOutput::Gif
        )
    {
        return make_gif(
            batch, tools, job, info, &job.input, progress, cancel, report,
        );
    }
    // Everything below runs FFmpeg.
    let Some(tools) = tools else {
        return failed(NEEDS_FFMPEG.to_owned());
    };
    make_moving(
        batch, tools, caps, job, info, &job.input, progress, cancel, report,
    )
}

/// Videos and animations, read by FFmpeg from `media` (the input, or frames decoded from it).
#[allow(clippy::too_many_arguments)]
fn make_moving(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    media: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let output_kind = job.recipe.video.output;
    let moving = matches!(info.kind, MediaKind::Video | MediaKind::Animation);
    let wants_gif = (info.kind == MediaKind::Animation
        && matches!(output_kind, VideoOutput::Keep | VideoOutput::Gif))
        || (info.kind == MediaKind::Video && output_kind == VideoOutput::Gif);
    if wants_gif {
        return make_gif(
            batch,
            Some(tools),
            job,
            info,
            media,
            progress,
            cancel,
            report,
        );
    }
    if moving && matches!(output_kind, VideoOutput::Webp | VideoOutput::Avif) {
        return make_animated(
            batch, tools, caps, job, info, media, progress, cancel, report,
        );
    }
    match info.kind {
        MediaKind::Video if job.recipe.video.trim_only => {
            make_trim(batch, tools, caps, job, info, progress, cancel, report)
        }
        MediaKind::Video | MediaKind::Animation => make_video(
            batch, tools, caps, job, info, media, progress, cancel, report,
        ),
        other => Outcome::Skipped {
            reason: SkipReason::Unsupported,
            message: format!("{} compression is not available yet", kind_name(other)),
        },
    }
}

/// An animated JPEG XL: decoded by `djxl` into an APNG in `workdir`, then encoded again as JPEG
/// XL by `cjxl` (the default), or turned into a GIF, WebP, AVIF, or MP4 by FFmpeg.
#[allow(clippy::too_many_arguments)]
fn make_jxl_animation(
    batch: &Batch,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    workdir: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    if job.recipe.video.output == VideoOutput::Keep {
        let plan = match plan_animated_jxl(info, &job.recipe) {
            Ok(plan) => plan,
            Err(e) => return failed(e.to_string()),
        };
        let ImageEncoding::Jxl { quality, .. } = plan.encoding else {
            return failed("internal error: not a JPEG XL plan".to_owned());
        };
        report.plan = Some(Plan::Image(plan));
        report.encoder = Some(format!("cjxl q{quality}"));
        let output = match choose_output(batch, job, "jxl") {
            Ok(output) => output,
            Err(outcome) => return outcome,
        };
        let frames = match crate::jxl::decode_animation(&job.input, workdir) {
            Ok(frames) => frames,
            Err(message) => return failed(message),
        };
        progress(0.3);
        if cancel.load(Ordering::Relaxed) {
            return Outcome::Cancelled;
        }
        let bytes = match crate::jxl::encode_animation(&frames, quality, workdir) {
            Ok(bytes) => bytes,
            Err(message) => return failed(message),
        };
        if cancel.load(Ordering::Relaxed) {
            return Outcome::Cancelled;
        }
        let partial = partial_path(&output);
        if let Err(e) = std::fs::write(&partial, bytes) {
            let _ = std::fs::remove_file(&partial);
            return failed(format!("cannot write {}: {e}", partial.display()));
        }
        progress(1.0);
        return finish(&partial, &output, info.size_bytes, false);
    }
    // An animated PNG is made from the frames djxl decodes, without FFmpeg.
    if job.recipe.video.output == VideoOutput::Apng {
        let frames = match crate::jxl::decode_animation(&job.input, workdir) {
            Ok(frames) => frames,
            Err(message) => return failed(message),
        };
        return make_apng(batch, tools, job, info, &frames, progress, cancel, report);
    }
    let Some(tools) = tools else {
        return failed(NEEDS_FFMPEG.to_owned());
    };
    let frames = match crate::jxl::decode_animation(&job.input, workdir) {
        Ok(frames) => frames,
        Err(message) => return failed(message),
    };
    if cancel.load(Ordering::Relaxed) {
        return Outcome::Cancelled;
    }
    let mut decoded = match probe(tools, &frames) {
        Ok(decoded) => decoded,
        Err(e) => return failed(format!("cannot read the frames djxl decoded: {e}")),
    };
    // The frames stand in for the input: an animation, compared with the input's size.
    decoded.kind = MediaKind::Animation;
    decoded.size_bytes = info.size_bytes;
    make_moving(
        batch, tools, caps, job, &decoded, &frames, progress, cancel, report,
    )
}

/// Probes with FFmpeg, or without it for the files Peraz reads itself.
fn probe_input(tools: Option<&Toolchain>, input: &Path) -> Result<MediaInfo, String> {
    let probed = match tools {
        Some(tools) => probe(tools, input).map(Some),
        None => probe_without_ffmpeg(input),
    };
    match probed {
        Ok(Some(info)) => Ok(info),
        Ok(None) => Err(NEEDS_FFMPEG.to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_image(
    batch: &Batch,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    let mut plan = match plan_image(info, &job.recipe) {
        Ok(plan) => plan,
        Err(
            e @ (PlanError::UnsupportedImage { .. }
            | PlanError::SvgConversion { .. }
            | PlanError::IconConversion { .. }),
        ) => {
            return Outcome::Skipped {
                reason: SkipReason::Unsupported,
                message: e.to_string(),
            };
        }
        Err(e) => return failed(e.to_string()),
    };
    report.plan = Some(Plan::Image(plan.clone()));
    // SVGs and icon files are rewritten whole; a TIFF with several pages is skipped, and a
    // HEIC without its encoder fails.
    let early = make_without_pixels(batch, job, info, &plan, progress, report)
        .or_else(|| missing_heic_encoder(&plan, tools, caps).map(failed));
    if let Some(outcome) = early {
        return outcome;
    }
    let output = match choose_output(batch, job, plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let (decoded, rebuilt_jpeg) = decode_image(tools, &job.input, plan.decoder);
    let codec = info
        .video
        .as_ref()
        .map_or("", |picture| picture.codec.as_str());
    let decoded = decoded.map(|decoded| settle_hdr(decoded, &mut plan, codec));
    report.plan = Some(Plan::Image(plan.clone()));
    if rebuilt_jpeg.is_some()
        && let ImageEncoding::Jxl { from_jpeg, .. } = &mut plan.encoding
    {
        *from_jpeg = true;
        report.plan = Some(Plan::Image(plan.clone()));
    }
    let lossless = decoded
        .as_ref()
        .ok()
        .and_then(|d| lossless_route(&plan, d, rebuilt_jpeg.is_some()));
    // A copy of the picture to measure the result against, when only measuring.
    let mut reference = match &decoded {
        Ok(d) if job.recipe.measure_quality && plan.target_ssimulacra2.is_none() => Some(d.clone()),
        _ => None,
    };
    let gain_map = decoded.as_ref().is_ok_and(|d| d.gain_map);
    let encoded =
        decoded.and_then(|d| encode_to_plan(tools, d, &mut plan, cancel, report, &mut reference));
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let mut encoded = match encoded {
        Ok(encoded) => encoded,
        Err(message) => return failed(message),
    };
    // The JPEG's own data holds the very same pixels.
    let mut identical = false;
    if let Some(kind) = lossless
        && let Some(jpeg) = rebuilt_jpeg.or_else(|| std::fs::read(&job.input).ok())
        && let Some((bytes, encoder)) = keep_jpeg_data(&jpeg, kind, plan.keep_metadata)
        && bytes.len() < encoded.bytes.len()
    {
        encoded.bytes = bytes;
        encoder.clone_into(&mut encoded.encoder);
        identical = true;
        if let Some(score) = &mut report.score {
            score.score = 100.0;
        }
    }
    // The gain map the encoder attached is gone when the JPEG's own data took its place.
    let kept_gain_map = encoded.kept_gain_map && !identical;
    report.encoder = Some(encoded.encoder);
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &encoded.bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", output.display()));
    }
    let outcome = finish(&partial, &output, info.size_bytes, plan.converts_format);
    if matches!(outcome, Outcome::Compressed { .. }) {
        report.warnings = encoded.warnings;
        // A JPEG kept whole with its metadata keeps its gain map; no other result has one.
        if gain_map && !kept_gain_map && !(identical && plan.keep_metadata) {
            report.warnings.push(GAIN_MAP_REMOVED.to_owned());
        }
        if plan.hdr == Some(HdrHandling::Converted) {
            report.warnings.push(HDR_CONVERTED.to_owned());
        }
    }
    // Only results that are kept are measured.
    if let (Outcome::Compressed { .. }, Some(reference)) = (&outcome, reference) {
        report.score = measured_image(tools, reference, &plan, &encoded.bytes, identical);
    }
    progress(1.0);
    outcome
}

/// What the report says about a photo whose HDR gain map is not in the result.
const GAIN_MAP_REMOVED: &str = "this photo's HDR gain map was removed: the result is its \
                                standard picture, without the extra brightness on HDR screens";

/// Settles what becomes of a picture that is HDR by its transfer function: what its pixels
/// say counts, whatever the probe saw. Where the output cannot hold it, it becomes a
/// standard picture here, before anything else is done with it.
fn settle_hdr(
    decoded: crate::image::Decoded,
    plan: &mut peraz_core::ImagePlan,
    codec: &str,
) -> crate::image::Decoded {
    plan.hdr = decoded.hdr.map(|_| HdrHandling::of(&plan.encoding, codec));
    match (plan.hdr, decoded.hdr) {
        (Some(HdrHandling::Converted), Some(color)) => crate::image::Decoded {
            image: crate::hdr::to_standard(&decoded.image, color),
            icc: None,
            hdr: None,
            png_color: crate::image::PngColor::default(),
            ..decoded
        },
        _ => decoded,
    }
}

/// What the report says about a picture that was HDR by its transfer function and is not
/// any more.
const HDR_CONVERTED: &str = "this picture was HDR (PQ or HLG) and was converted to standard \
                             range, which is all the output format holds: its highlights \
                             were rolled off";

/// Encodes a picture at the plan's setting, or to its score target, or, with a size target, at
/// the best setting that fits; the plan in the report then says what a search found.
fn encode_to_plan(
    tools: Option<&Toolchain>,
    decoded: crate::image::Decoded,
    plan: &mut peraz_core::ImagePlan,
    cancel: &AtomicBool,
    report: &mut Report,
    reference: &mut Option<crate::image::Decoded>,
) -> Result<crate::image::EncodedImage, String> {
    if let Some(target) = plan.target_ssimulacra2 {
        let (encoded, score) =
            crate::score::encode_image_to_ssimulacra2(tools, decoded, plan, target, cancel)?;
        report.score = score.map(|score| peraz_core::Score {
            metric: peraz_core::Metric::Ssimulacra2,
            target: Some(target),
            score: (score * 100.0).round() / 100.0,
            sampled: false,
        });
        report.plan = Some(Plan::Image(plan.clone()));
        return Ok(encoded);
    }
    let Some(wanted) = plan.target else {
        return encode_image(tools, decoded, plan);
    };
    let fitted = crate::fit::encode_image_to_size(tools, decoded, plan, cancel)?;
    report.plan = Some(Plan::Image(plan.clone()));
    // A measured result is scored against the picture at the size it ended up with.
    if reference.is_some() {
        *reference = Some(fitted.prepared);
    }
    fitted.encoded.ok_or_else(|| {
        let (target, smallest) = (
            format_bytes(wanted.target_bytes),
            format_bytes(fitted.smallest),
        );
        match plan.target.and_then(|found| found.scaled_to) {
            _ if fitted.lossless => format!(
                "cannot fit under {target}: this format is lossless and the file is \
                 {smallest}, so the size is not reachable without lossy encoding; choose \
                 another --format"
            ),
            _ if wanted.keep_dimensions => format!(
                "cannot fit under {target} at this size: the smallest was {smallest}; leave \
                 out --keep-dimensions to let the picture be scaled down, or raise the target"
            ),
            Some((width, height)) => format!(
                "could not fit under {target}: the smallest was {smallest}, scaled down to \
                 {width}×{height}; raise the target"
            ),
            None => format!("could not fit under {target}: the smallest was {smallest}"),
        }
    })
}

/// When the pixels stay as they are, a JPEG's own data can be kept losslessly: recompressed
/// into JPEG XL, or, for a JPEG rebuilt from JPEG XL that becomes JPEG again, as it is.
fn lossless_route(
    plan: &peraz_core::ImagePlan,
    decoded: &crate::image::Decoded,
    rebuilt_jpeg: bool,
) -> Option<Lossless> {
    let size = (decoded.image.width(), decoded.image.height());
    if fit_within(size, plan.max_width, plan.max_height).is_some() {
        return None;
    }
    match plan.encoding {
        ImageEncoding::Jxl {
            from_jpeg: true, ..
        } => Some(Lossless::JpegXl),
        // The rebuilt JPEG is what it was before it became JPEG XL, which may be progressive:
        // when a baseline JPEG is asked for, the picture is encoded anew.
        ImageEncoding::Jpeg {
            progressive: true, ..
        } if rebuilt_jpeg => Some(Lossless::Jpeg),
        _ => None,
    }
}

/// Why a HEIC cannot be written here, when the plan asks for one: FFmpeg encodes it, with
/// libx265.
fn missing_heic_encoder(
    plan: &peraz_core::ImagePlan,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
) -> Option<String> {
    if !matches!(plan.encoding, ImageEncoding::Heic { .. }) {
        return None;
    }
    let Some(tools) = tools else {
        return Some(format!("HEIC output needs FFmpeg: {NEEDS_FFMPEG}"));
    };
    (!caps.has_encoder(crate::heic::ENCODER)).then(|| {
        format!(
            "HEIC output needs FFmpeg with libx265, and {} has none; run `peraz ffmpeg \
             install` for a build that has it, or choose another --format",
            tools.ffmpeg.display()
        )
    })
}

/// The outcome for the files that are not encoded from pixels: an SVG and an icon file are
/// rewritten in their own format, and a TIFF with several pages is skipped. `None` for a
/// picture that goes through the pixel pipeline.
fn make_without_pixels(
    batch: &Batch,
    job: &Job,
    info: &MediaInfo,
    plan: &peraz_core::ImagePlan,
    progress: Progress,
    report: &mut Report,
) -> Option<Outcome> {
    match plan.encoding {
        ImageEncoding::Svg { decimals } => Some(make_svg(
            batch,
            job,
            info,
            plan.extension,
            decimals,
            progress,
            report,
        )),
        ImageEncoding::Icon { palette_quality } => Some(make_icon(
            batch,
            job,
            info,
            plan.extension,
            palette_quality,
            progress,
            report,
        )),
        _ => multi_page_tiff(info, &job.input),
    }
}

/// Minifies an SVG, which stays an SVG: gzip-compressed again when it was.
fn make_svg(
    batch: &Batch,
    job: &Job,
    info: &MediaInfo,
    extension: &str,
    decimals: Option<u8>,
    progress: Progress,
    report: &mut Report,
) -> Outcome {
    use crate::svg::SvgError;
    let failed = |message: String| Outcome::Failed { message };
    let output = match choose_output(batch, job, extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let minified = crate::svg::read(&job.input)
        .and_then(|(text, gzip)| Ok((crate::svg::minify(&text, decimals)?, gzip)));
    let (text, gzip) = match minified {
        Ok(minified) => minified,
        Err(SvgError::Unsupported(message)) => {
            return Outcome::Skipped {
                reason: SkipReason::Unsupported,
                message,
            };
        }
        Err(SvgError::Invalid(message)) => {
            return failed(format!("{}: {message}", job.input.display()));
        }
    };
    let bytes = if gzip {
        match crate::svg::gzip(&text) {
            Ok(bytes) => bytes,
            Err(message) => return failed(message),
        }
    } else {
        text.into_bytes()
    };
    report.encoder = Some(
        if gzip {
            "SVG minifier + gzip"
        } else {
            "SVG minifier"
        }
        .to_owned(),
    );
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", output.display()));
    }
    progress(1.0);
    finish(&partial, &output, info.size_bytes, false)
}

/// Compresses the pictures inside an icon file, which keeps its format and sizes.
fn make_icon(
    batch: &Batch,
    job: &Job,
    info: &MediaInfo,
    extension: &str,
    palette_quality: Option<(u8, u8)>,
    progress: Progress,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    let output = match choose_output(batch, job, extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let bytes = match std::fs::read(&job.input) {
        Ok(bytes) => bytes,
        Err(e) => return failed(format!("cannot read {}: {e}", job.input.display())),
    };
    let (bytes, stats) = match crate::icon::compress(&bytes, palette_quality) {
        Ok(done) => done,
        Err(message) => return failed(format!("{}: {message}", job.input.display())),
    };
    report.encoder = Some(format!(
        "{} · {} of {} pictures recompressed",
        if palette_quality.is_some() {
            "imagequant + oxipng"
        } else {
            "oxipng (lossless)"
        },
        stats.recompressed,
        stats.pictures
    ));
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", output.display()));
    }
    progress(1.0);
    finish(&partial, &output, info.size_bytes, false)
}

/// Only the first page of a TIFF would be read, so a document with more is skipped.
fn multi_page_tiff(info: &MediaInfo, input: &Path) -> Option<Outcome> {
    if info.video.as_ref().is_none_or(|v| v.codec != "tiff") {
        return None;
    }
    let pages = crate::tiff::page_count(input).filter(|pages| *pages > 1)?;
    Some(Outcome::Skipped {
        reason: SkipReason::Unsupported,
        message: format!(
            "this TIFF holds {pages} pages, and Peraz compresses single pictures only; split \
             it into one file per page first"
        ),
    })
}

/// The SSIMULACRA2 score of an encoded picture against `reference`; 100 when it holds the very
/// same pixels, and none when it cannot be read back (the result stands without its score).
fn measured_image(
    tools: Option<&Toolchain>,
    reference: crate::image::Decoded,
    plan: &peraz_core::ImagePlan,
    bytes: &[u8],
    identical: bool,
) -> Option<peraz_core::Score> {
    if identical {
        return Some(peraz_core::Score {
            metric: peraz_core::Metric::Ssimulacra2,
            target: None,
            score: 100.0,
            sampled: false,
        });
    }
    crate::score::measure_image(tools, reference, plan, bytes)
        .ok()
        .flatten()
}

/// Decodes a still image input, and for a JPEG XL made losslessly from a JPEG, also returns that
/// JPEG, rebuilt.
fn decode_image(
    tools: Option<&Toolchain>,
    input: &Path,
    decoder: ImageDecoder,
) -> (Result<crate::image::Decoded, String>, Option<Vec<u8>>) {
    match decoder {
        ImageDecoder::Native => (decode_native(input), None),
        ImageDecoder::Text => (
            Err("internal error: an SVG has no pixels to decode".to_owned()),
            None,
        ),
        ImageDecoder::Ffmpeg => match tools {
            Some(tools) => (crate::heif::decode(tools, input), None),
            None => (Err(NEEDS_FFMPEG.to_owned()), None),
        },
        ImageDecoder::Djxl => match crate::jxl::reconstruct_jpeg(input) {
            Ok(Some(jpeg)) => (
                crate::image::decode_native_bytes(&jpeg, &input.display().to_string()),
                Some(jpeg),
            ),
            Ok(None) => (crate::jxl::decode_file(input), None),
            Err(message) => (Err(message), None),
        },
    }
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
        crate::epub::compress_epub(&job.input, &plan).map(|(bytes, stats)| (bytes, stats, vec![]))
    };
    let (bytes, stats, warnings) = match result {
        Ok(done) => done,
        Err(crate::pdf::DocumentError::Protected(why)) => {
            return Outcome::Skipped {
                reason: SkipReason::Protected,
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
    report.warnings.extend(warnings);
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return Outcome::Failed {
            message: format!("cannot write {}: {e}", output.display()),
        };
    }
    progress(1.0);
    finish(&partial, &output, info.size_bytes, false)
}

#[allow(clippy::too_many_arguments)]
fn make_gif(
    batch: &Batch,
    tools: Option<&Toolchain>,
    job: &Job,
    info: &MediaInfo,
    media: &Path,
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
    // A GIF is read in process, frame by frame with its own timing; anything else by FFmpeg.
    let from_gif = matches!(
        crate::apng::sniff(media),
        Ok(Some(crate::apng::Source::Gif))
    );
    let made = match (from_gif, tools) {
        (true, _) => crate::gif::encode_gif_from_gif(
            media,
            &partial,
            &plan,
            job.recipe.gif.fps,
            progress,
            cancel,
        ),
        (false, Some(tools)) => {
            encode_gif(tools, media, &partial, &plan, expected, progress, cancel)
        }
        (false, None) => Err(GifError::Failed(NEEDS_FFMPEG.to_owned())),
    };
    match made {
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
    media: &Path,
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
    if let Err(error) = encode(tools, media, &sink, &plan.args, expected, progress, cancel) {
        let _ = std::fs::remove_file(&partial);
        return match error {
            EncodeError::Cancelled => Outcome::Cancelled,
            EncodeError::Failed(message) => Outcome::Failed { message },
        };
    }
    // A different format is the point of the conversion: keep it even when larger.
    finish(&partial, &output, info.size_bytes, true)
}

/// An animated PNG from `media` (the input, or frames decoded from it): every frame of an
/// animation Peraz decodes itself, with its timing, or frames FFmpeg takes from a video.
#[allow(clippy::too_many_arguments)]
fn make_apng(
    batch: &Batch,
    tools: Option<&Toolchain>,
    job: &Job,
    info: &MediaInfo,
    media: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    let mut plan = match plan_apng(info, &job.recipe) {
        Ok(plan) => plan,
        Err(e) => return failed(e.to_string()),
    };
    let is_apng = info.video.as_ref().is_some_and(|v| v.codec == "apng");
    // An animated PNG named `.apng` keeps that name.
    if is_apng
        && job
            .input
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("apng"))
    {
        plan.extension = "apng";
    }
    report.plan = Some(Plan::Animated(plan.clone()));
    let output = match choose_output(batch, job, plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let encoded = match crate::apng::sniff(media) {
        Ok(Some(source)) => {
            encode_apng_from_animation(media, source, &mut plan, job, progress, cancel)
        }
        Ok(None) => match tools {
            Some(tools) => encode_apng_from_video(tools, media, &plan, info, job, progress, cancel),
            None => Err(crate::apng::ApngError::Failed(NEEDS_FFMPEG.to_owned())),
        },
        Err(e) => Err(crate::apng::ApngError::Failed(format!(
            "cannot read {}: {e}",
            media.display()
        ))),
    };
    report.plan = Some(Plan::Animated(plan));
    let encoded = match encoded {
        Ok(encoded) => encoded,
        Err(crate::apng::ApngError::Cancelled) => return Outcome::Cancelled,
        Err(crate::apng::ApngError::Failed(message)) => return failed(message),
    };
    report.encoder = Some(encoded.encoder);
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &encoded.bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", output.display()));
    }
    progress(1.0);
    // An animated PNG stays one only when smaller; from another format it was asked for.
    finish(&partial, &output, info.size_bytes, !is_apng)
}

/// Share of the progress for reading the frames of an animated PNG; writing takes the rest.
const APNG_READ_SHARE: f64 = 0.5;

/// Hands every frame of an animation Peraz decodes itself to `on_frame`, at the plan's size
/// and within the part the plan selects, with the time it is shown. Returns the play count
/// (0: forever) and the color profile.
fn selected_frames(
    media: &Path,
    source: crate::apng::Source,
    plan: &peraz_core::AnimatedPlan,
    cancel: &AtomicBool,
    on_frame: &mut dyn FnMut(image::RgbaImage, crate::apng::Delay) -> Result<(), String>,
) -> Result<(u32, Option<Vec<u8>>), crate::apng::ApngError> {
    let frames = crate::apng::decode(media, source)?;
    let (plays, icc) = (frames.plays, frames.icc.clone());
    let resize = (frames.width, frames.height) != (plan.width, plan.height);
    #[allow(clippy::cast_precision_loss)]
    let (start, end) = (
        plan.start_ms.map_or(0.0, |ms| ms as f64),
        plan.duration_ms
            .map_or(f64::INFINITY, |ms| (plan.start_ms.unwrap_or(0) + ms) as f64),
    );
    let mut at = 0.0;
    for frame in frames {
        if cancel.load(Ordering::SeqCst) {
            return Err(crate::apng::ApngError::Cancelled);
        }
        let (pixels, delay) = frame?;
        let shown_at = at;
        at += delay.milliseconds();
        // The selected part holds the frames that start inside it.
        if shown_at < start {
            continue;
        }
        if shown_at >= end {
            break;
        }
        let pixels = if resize {
            image::imageops::resize(
                &pixels,
                plan.width,
                plan.height,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            pixels
        };
        on_frame(pixels, delay)?;
    }
    Ok((plays, icc))
}

/// The frames of an animated WebP as an animated PNG file in `workdir`, written quickly and
/// without loss: FFmpeg reads that whether or not it can read animated WebP.
fn webp_frames_for_ffmpeg(
    input: &Path,
    info: &MediaInfo,
    workdir: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, crate::apng::ApngError> {
    let picture = info
        .video
        .as_ref()
        .ok_or_else(|| "the file contains no picture".to_owned())?;
    // Every frame at its own size: the conversion that follows clips and scales.
    let whole = peraz_core::AnimatedPlan {
        format: VideoOutput::Apng,
        encoder: "apng",
        fps: 1,
        width: picture.width,
        height: picture.height,
        start_ms: None,
        duration_ms: None,
        flatten_alpha: false,
        background: None,
        extension: "apng",
        args: Vec::new(),
    };
    let mut frames = crate::apng::Encoder::new(whole.width, whole.height);
    let source = crate::apng::Source::Webp;
    let (plays, _) = selected_frames(input, source, &whole, cancel, &mut |pixels, delay| {
        frames.push(pixels.as_raw(), delay)
    })?;
    let file = workdir.join("frames.apng");
    let png = frames.finish_plain(plays)?;
    std::fs::write(&file, png).map_err(|e| format!("cannot write the decoded frames: {e}"))?;
    Ok(file)
}

/// An animated WebP: re-encoded as an animated WebP in process (the default), made into an
/// animated PNG in process, or decoded into frames that FFmpeg turns into another format.
#[allow(clippy::too_many_arguments)]
fn make_webp_animation(
    batch: &Batch,
    tools: Option<&Toolchain>,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    use crate::apng::ApngError;
    let failed = |message: String| Outcome::Failed { message };
    let as_outcome = |error: ApngError| match error {
        ApngError::Cancelled => Outcome::Cancelled,
        ApngError::Failed(message) => Outcome::Failed { message },
    };
    if wants_apng(info, &job.recipe) {
        return make_apng(
            batch, tools, job, info, &job.input, progress, cancel, report,
        );
    }
    if !wants_animated_webp(info, &job.recipe) {
        let Some(tools) = tools else {
            return failed(NEEDS_FFMPEG.to_owned());
        };
        let workdir = match make_workdir() {
            Ok(dir) => dir,
            Err(outcome) => return outcome,
        };
        let outcome = match webp_frames_for_ffmpeg(&job.input, info, &workdir, cancel) {
            Err(error) => as_outcome(error),
            Ok(frames) => match probe(tools, &frames) {
                Err(e) => failed(format!("cannot read the decoded frames: {e}")),
                Ok(mut decoded) => {
                    // The frames stand in for the input: an animation, compared with the
                    // input's size.
                    decoded.kind = MediaKind::Animation;
                    decoded.size_bytes = info.size_bytes;
                    make_moving(
                        batch, tools, caps, job, &decoded, &frames, progress, cancel, report,
                    )
                }
            },
        };
        let _ = std::fs::remove_dir_all(&workdir);
        return outcome;
    }
    let mut plan = match plan_animated_webp(info, &job.recipe) {
        Ok(plan) => plan,
        Err(e) => return failed(e.to_string()),
    };
    report.plan = Some(Plan::Animated(plan.clone()));
    let output = match choose_output(batch, job, plan.extension) {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    let mut animation = crate::webp_animation::Animation::default();
    let source = crate::apng::Source::Webp;
    let read = selected_frames(&job.input, source, &plan, cancel, &mut |pixels, delay| {
        animation.push(pixels, delay)?;
        #[allow(clippy::cast_precision_loss)]
        let seen = animation.len() as f64;
        progress(APNG_READ_SHARE * seen / (seen + 10.0));
        Ok(())
    });
    let plays = match read {
        Ok((plays, _)) => plays,
        Err(error) => return as_outcome(error),
    };
    plan.fps = animation.average_fps();
    let quality = peraz_core::plan::animated_webp_quality(job.recipe.quality);
    report.plan = Some(Plan::Animated(plan));
    report.encoder = Some(format!("libwebp q{quality} · {} frames", animation.len()));
    let bytes = match animation.encode(quality, plays) {
        Ok(bytes) => bytes,
        Err(message) => return failed(message),
    };
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    let partial = partial_path(&output);
    if let Err(e) = std::fs::write(&partial, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return failed(format!("cannot write {}: {e}", output.display()));
    }
    progress(1.0);
    // An animated WebP stays one only when smaller.
    finish(&partial, &output, info.size_bytes, false)
}

/// Every frame of an animation, at the plan's size, within the selected part.
fn encode_apng_from_animation(
    media: &Path,
    source: crate::apng::Source,
    plan: &mut peraz_core::AnimatedPlan,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<crate::apng::Encoded, crate::apng::ApngError> {
    let mut encoder = crate::apng::Encoder::new(plan.width, plan.height);
    let (plays, icc) = selected_frames(media, source, plan, cancel, &mut |pixels, delay| {
        encoder.push(pixels.as_raw(), delay)?;
        // The frame count is not known ahead: approach the share without reaching it.
        #[allow(clippy::cast_precision_loss)]
        let seen = encoder.frames_seen as f64;
        progress(APNG_READ_SHARE * seen / (seen + 10.0));
        Ok(())
    })?;
    plan.fps = encoder.average_fps();
    progress(APNG_READ_SHARE);
    encoder.finish(
        plays,
        peraz_core::plan::apng_palette_quality(job.recipe.quality),
        icc.as_deref(),
        cancel,
    )
}

/// Frames FFmpeg takes from a video at the plan's frame rate and size.
fn encode_apng_from_video(
    tools: &Toolchain,
    media: &Path,
    plan: &peraz_core::AnimatedPlan,
    info: &MediaInfo,
    job: &Job,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<crate::apng::Encoded, crate::apng::ApngError> {
    let stream = crate::gif::FrameStream {
        start_ms: plan.start_ms,
        duration_ms: plan.duration_ms,
        fps: plan.fps,
        width: plan.width,
        height: plan.height,
    };
    let expected = job
        .recipe
        .clip
        .seconds(info.duration_seconds)
        .unwrap_or(0.0)
        * f64::from(plan.fps);
    let delay = crate::apng::Delay::of_frame_rate(plan.fps);
    let mut encoder = crate::apng::Encoder::new(plan.width, plan.height);
    let streamed = crate::gif::stream_frames(tools, media, &stream, cancel, &mut |frame| {
        encoder.push(frame, delay)?;
        #[allow(clippy::cast_precision_loss)]
        progress((encoder.frames_seen as f64 / expected.max(1.0)).min(1.0) * APNG_READ_SHARE);
        Ok(())
    });
    match streamed {
        Ok(()) => {}
        Err(GifError::Cancelled) => return Err(crate::apng::ApngError::Cancelled),
        Err(GifError::Failed(message)) => return Err(message.into()),
    }
    // Loops forever, like the GIFs and animated WebPs made from videos.
    encoder.finish(
        0,
        peraz_core::plan::apng_palette_quality(job.recipe.quality),
        None,
        cancel,
    )
}

/// Share of the progress for encoding a video whose quality is measured afterwards.
const ENCODE_SHARE: f64 = 0.75;

/// Compresses a video (or a GIF into MP4), then measures it when the recipe asks to.
#[allow(clippy::too_many_arguments)]
fn make_video(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    media: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    // A GIF's palette and dithering make a poor VMAF reference.
    let measure = job.recipe.measure_quality && caps.vmaf && info.kind == MediaKind::Video;
    if !measure {
        return encode_video(
            batch, tools, caps, job, info, media, progress, cancel, report,
        );
    }
    let mut encoding = |f: f64| progress(f * ENCODE_SHARE);
    let outcome = encode_video(
        batch,
        tools,
        caps,
        job,
        info,
        media,
        &mut encoding,
        cancel,
        report,
    );
    // A score target already measured the result.
    if let (Outcome::Compressed { output, .. }, None, Some(Plan::Video(plan))) =
        (&outcome, &report.score, &report.plan)
    {
        let reading = Job {
            input: media.to_owned(),
            ..job.clone()
        };
        let mut measuring = |f: f64| progress(ENCODE_SHARE + f * (1.0 - ENCODE_SHARE));
        // The result stands without its score when measuring fails or is cancelled.
        report.score = crate::score::measure_video(
            tools,
            &reading,
            info,
            &plan.clone(),
            output,
            &mut measuring,
            cancel,
        )
        .ok();
    }
    outcome
}

/// Compresses a video (or a GIF into MP4): at a quality level, a file size, or a VMAF score.
#[allow(clippy::too_many_arguments)]
fn encode_video(
    batch: &Batch,
    tools: &Toolchain,
    caps: &Capabilities,
    job: &Job,
    info: &MediaInfo,
    media: &Path,
    progress: Progress,
    cancel: &AtomicBool,
    report: &mut Report,
) -> Outcome {
    let failed = |message: String| Outcome::Failed { message };
    let plan = match plan_video(info, &job.recipe, |name| caps.has_encoder(name)) {
        Ok(plan) => plan,
        Err(e) => return failed(e.to_string()),
    };
    report.plan = Some(Plan::Video(plan.clone()));
    report.encoder = Some(plan.encoder_label());
    let output = match choose_output(batch, job, "mp4") {
        Ok(output) => output,
        Err(outcome) => return outcome,
    };
    // The size and score searches read the frames in place of the input.
    let reading = Job {
        input: media.to_owned(),
        ..job.clone()
    };
    if plan.target.is_some() {
        return encode_to_target(
            tools, &reading, info, plan, &output, progress, cancel, report,
        );
    }
    if plan.score_target.is_some() {
        return crate::score::encode_to_vmaf(
            tools, &reading, info, plan, &output, progress, cancel, report,
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
    if let Err(error) = encode(tools, media, &sink, &plan.args, duration, progress, cancel) {
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

/// `-ss`/`-t` input options for a clip, placed before `-i` so FFmpeg seeks quickly.
pub(crate) fn clip_args(clip: Clip) -> Vec<String> {
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

/// A new, empty work folder in the system's temporary folder; the caller removes it.
pub(crate) fn make_workdir() -> Result<PathBuf, Outcome> {
    let workdir = std::env::temp_dir().join(format!(
        "peraz-{}-{}-{}",
        std::process::id(),
        WORKDIRS.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&workdir).map_err(|e| Outcome::Failed {
        message: format!("cannot create a work folder: {e}"),
    })?;
    Ok(workdir)
}

/// How a JPEG's own data is kept without loss.
#[derive(Debug, Clone, Copy)]
enum Lossless {
    /// Recompressed into JPEG XL.
    JpegXl,
    /// As it is, with its metadata removed.
    Jpeg,
}

/// The JPEG's data kept losslessly, with the encoder name for the report; `None` when that is not
/// possible (cjxl does not support some JPEGs, such as arithmetic-coded ones): the lossy encode is
/// kept then.
fn keep_jpeg_data(
    jpeg: &[u8],
    kind: Lossless,
    keep_metadata: bool,
) -> Option<(Vec<u8>, &'static str)> {
    use image::ImageDecoder as _;
    // With its metadata, the JPEG is kept whole: its own EXIF says how it is turned.
    if keep_metadata {
        return match kind {
            Lossless::JpegXl => crate::jxl::recompress_whole_jpeg(jpeg)
                .ok()
                .map(|bytes| (bytes, "cjxl lossless JPEG recompression")),
            Lossless::Jpeg => Some((jpeg.to_vec(), "JPEG rebuilt from JPEG XL")),
        };
    }
    let orientation = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg))
        .ok()?
        .orientation()
        .ok()?;
    let exif =
        (1..=8).find(|&n| image::metadata::Orientation::from_exif(n) == Some(orientation))?;
    match kind {
        Lossless::JpegXl => crate::jxl::recompress_jpeg(jpeg, exif)
            .ok()
            .map(|bytes| (bytes, "cjxl lossless JPEG recompression")),
        Lossless::Jpeg => crate::jxl::without_metadata(jpeg, exif)
            .map(|bytes| (bytes, "JPEG rebuilt from JPEG XL")),
    }
}

/// [`make_workdir`] for code that reports errors as text.
pub(crate) fn work_folder() -> Result<PathBuf, String> {
    make_workdir().map_err(|outcome| match outcome {
        Outcome::Failed { message } => message,
        _ => "cannot create a work folder".to_owned(),
    })
}

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
    let workdir = match make_workdir() {
        Ok(dir) => dir,
        Err(outcome) => return outcome,
    };
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
pub(crate) fn finish(partial: &Path, output: &Path, input_bytes: u64, converted: bool) -> Outcome {
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
            reason: SkipReason::OutputExists,
            message: format!("already compressed: {} exists", first.display()),
        });
    }
    check_output_folder(&first)?;
    for number in 1..=MAX_OUTPUT_NUMBER {
        let candidate = job.output.resolve(&job.input, extension, number);
        if !candidate.exists() && batch.claim(&candidate, &job.input) {
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
    if !batch.claim(output, &job.input) {
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
                    reason: SkipReason::OutputExists,
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
pub(crate) fn partial_path(output: &Path) -> PathBuf {
    const SUFFIX: &str = ".peraz-partial";
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let partial = format!(".{name}{SUFFIX}");
    if partial.len() <= peraz_core::report::MAX_NAME_BYTES {
        return output.with_file_name(partial);
    }
    // Too long for the file system: a shortened name, kept unique by a hash of the full one.
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&name, &mut hasher);
    let hash = format!("~{:016x}", std::hash::Hasher::finish(&hasher));
    let room = peraz_core::report::MAX_NAME_BYTES - 1 - hash.len() - SUFFIX.len();
    let short = peraz_core::report::truncate_bytes(&name, room);
    output.with_file_name(format!(".{short}{hash}{SUFFIX}"))
}

pub(crate) fn file_url(path: &Path) -> Result<OsString, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut url = OsString::from("file:");
    url.push(absolute.as_os_str());
    Ok(url)
}

pub(crate) enum EncodeError {
    Cancelled,
    Failed(String),
}

impl From<String> for EncodeError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Where an encode writes.
pub(crate) struct Sink<'a> {
    /// Arguments placed before `-i` (input seeking: `-ss`, `-t`).
    pub before_input: &'a [String],
    /// The output file, or `None` for an analysis pass that writes nothing.
    pub file: Option<&'a Path>,
    /// FFmpeg's working directory (holds two-pass statistics), or `None` for the current one.
    pub workdir: Option<&'a Path>,
}

pub(crate) fn encode(
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
        // A name near the 255-byte limit gets a shorter partial name, still unique.
        let long = |n: u32| PathBuf::from(format!("/a/{}-{n}.jpg", "x".repeat(240)));
        let (one, two) = (partial_path(&long(1)), partial_path(&long(2)));
        for partial in [&one, &two] {
            let name = partial.file_name().unwrap().to_str().unwrap();
            assert!(name.len() <= 255, "{}", name.len());
            assert!(name.starts_with('.') && name.ends_with(".peraz-partial"));
        }
        assert_ne!(one, two);
    }
}
