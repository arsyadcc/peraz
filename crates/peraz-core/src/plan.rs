//! The planner: turns a file's [`MediaInfo`] and a [`Recipe`] into concrete encoder settings.
//!
//! Pure functions only, so every decision is unit-testable and the resulting FFmpeg arguments can
//! be golden-tested.

use serde::Serialize;

use crate::media::Alpha;
use crate::media::{AudioTrack, ColorInfo, MediaInfo, MediaKind, Rational, VideoTrack};
use crate::recipe::{EncoderPreference, ImageFormat, Quality, Recipe, VideoCodec, VideoOutput};

/// A video encoder Peraz knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoder {
    Libx264,
    Libx265,
    H264VideoToolbox,
    HevcVideoToolbox,
}

impl VideoEncoder {
    /// The FFmpeg encoder name.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Libx264 => "libx264",
            Self::Libx265 => "libx265",
            Self::H264VideoToolbox => "h264_videotoolbox",
            Self::HevcVideoToolbox => "hevc_videotoolbox",
        }
    }

    pub fn is_hardware(self) -> bool {
        matches!(self, Self::H264VideoToolbox | Self::HevcVideoToolbox)
    }

    fn codec(self) -> VideoCodec {
        match self {
            Self::Libx264 | Self::H264VideoToolbox => VideoCodec::H264,
            Self::Libx265 | Self::HevcVideoToolbox => VideoCodec::Hevc,
        }
    }

    /// Candidates in preference order.
    fn candidates(codec: VideoCodec, preference: EncoderPreference) -> Vec<Self> {
        let (software, hardware) = match codec {
            VideoCodec::H264 => (Self::Libx264, Self::H264VideoToolbox),
            VideoCodec::Hevc => (Self::Libx265, Self::HevcVideoToolbox),
        };
        match preference {
            EncoderPreference::Auto => vec![software, hardware],
            EncoderPreference::Software => vec![software],
            EncoderPreference::Hardware => vec![hardware],
        }
    }

    /// The rate-control arguments for a quality level.
    ///
    /// Calibrated with `scripts/calibrate-video.py`: each preset aims at a mean VMAF (model
    /// `vmaf_v0.6.1`) over a sample set of natural footage, screen recordings, and motion graphics,
    /// with no clip more than 3 points below it. Targets: highest 97, high 95, balanced 93,
    /// small 89, smallest 84. Each value is the most economical one that met its target.
    /// The CRF a quality level uses (software encoders), where a score search starts.
    pub fn crf(self, quality: Quality) -> u8 {
        [16, 22, 24, 28, 30][quality as usize]
    }

    fn quality_args(self, quality: Quality) -> Vec<String> {
        let level = quality as usize; // Highest = 0 … Smallest = 4
        let (flag, value) = match self {
            Self::Libx264 | Self::Libx265 => ("-crf", self.crf(quality)),
            Self::H264VideoToolbox => ("-q:v", [85, 75, 70, 60, 55][level]),
            Self::HevcVideoToolbox => ("-q:v", [85, 75, 70, 65, 60][level]),
        };
        vec![flag.to_owned(), value.to_string()]
    }
}

/// What happens to one audio track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum AudioAction {
    /// Already compact and MP4-compatible: copied bit for bit.
    Copy,
    /// Re-encoded to AAC at this bit rate (bits per second).
    Aac { bit_rate: u64 },
}

/// The audio part of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum AudioPlan {
    /// No audio in the output (none in the input, or removal requested).
    None,
    /// One action per input audio track, in order.
    Tracks { tracks: Vec<AudioAction> },
}

/// A complete plan for compressing one video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoPlan {
    pub encoder: VideoEncoder,
    /// Output size when scaling down, `None` to keep the source size.
    pub scale_to: Option<(u32, u32)>,
    pub pixel_format: &'static str,
    /// The source's color description, written into the output unchanged.
    pub color: ColorInfo,
    pub audio: AudioPlan,
    /// Present when the video must fit a file size.
    pub target: Option<SizeTarget>,
    /// Present when the video must reach a VMAF score.
    pub score_target: Option<ScoreTarget>,
    /// FFmpeg arguments between the input and the output (single pass, or the final pass of a
    /// size-targeted encode), for transparency in reports.
    pub args: Vec<String>,
}

/// Why a plan could not be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum PlanError {
    /// The file is not a video.
    NotVideo { kind: MediaKind },
    /// H.264 was asked for, but the video is HDR: 8-bit H.264 would lose it.
    HdrNeedsHevc,
    /// No usable encoder for the requested codec and preference.
    NoEncoder {
        codec: VideoCodec,
        tried: Vec<&'static str>,
    },
    /// The file is not a still image.
    NotImage { kind: MediaKind },
    /// A size target needs the duration, which this file does not report.
    UnknownDuration,
    /// The target is too small for this video's length.
    TargetTooSmall {
        target_bytes: u64,
        minimum_bytes: u64,
    },
    /// The FFmpeg build has no encoder for this animated format.
    NoAnimationEncoder {
        format: VideoOutput,
        tried: Vec<&'static str>,
    },
    /// The file is not a PDF or EPUB.
    NotDocument { kind: MediaKind },
    /// The file cannot become an animated GIF.
    NotAnimation { kind: MediaKind },
    /// Peraz cannot read, or cannot write, this image format yet.
    UnsupportedImage { codec: String, hint: &'static str },
    /// A score target was combined with hardware encoding, which has no setting to search.
    ScoreNeedsSoftware,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotVideo { kind } => write!(f, "not a video (detected: {kind:?})"),
            Self::HdrNeedsHevc => write!(
                f,
                "this is an HDR video, which H.264 cannot keep; use --codec hevc, or leave --codec out"
            ),
            Self::NoEncoder { codec, tried } => write!(
                f,
                "no {codec} encoder available (needs one of: {}); run `peraz doctor`",
                tried.join(", ")
            ),
            Self::NotImage { kind } => write!(f, "not a still image (detected: {kind:?})"),
            Self::UnknownDuration => {
                write!(f, "cannot aim for a file size: the duration is unknown")
            }
            Self::TargetTooSmall {
                target_bytes,
                minimum_bytes,
            } => write!(
                f,
                "{} is too small for this video; it needs at least {} (or use --no-audio, or trim it)",
                crate::units::format_bytes(*target_bytes),
                crate::units::format_bytes(*minimum_bytes)
            ),
            Self::NotDocument { kind } => write!(f, "not a PDF or EPUB (detected: {kind:?})"),
            Self::NoAnimationEncoder { format, tried } => write!(
                f,
                "no encoder for animated {format} in this FFmpeg (needs one of: {}); run `peraz doctor`",
                tried.join(", ")
            ),
            Self::NotAnimation { kind } => {
                write!(f, "cannot make a GIF from this file (detected: {kind:?})")
            }
            Self::UnsupportedImage { codec, hint } => {
                write!(f, "{codec} images are not supported yet{hint}")
            }
            Self::ScoreNeedsSoftware => write!(
                f,
                "--target-vmaf needs a software encoder; leave out --encoder hardware"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Audio codecs that can be copied into MP4 unchanged.
const MP4_AUDIO: &[&str] = &["aac", "mp3", "ac3", "eac3", "alac"];

/// Plans the compression of a video.
///
/// `has_encoder` reports whether the FFmpeg build contains an encoder (by FFmpeg name).
pub fn plan_video(
    info: &MediaInfo,
    recipe: &Recipe,
    has_encoder: impl Fn(&str) -> bool,
) -> Result<VideoPlan, PlanError> {
    let gif_to_mp4 = info.kind == MediaKind::Animation && recipe.video.output == VideoOutput::Mp4;
    let (Some(video), MediaKind::Video | MediaKind::Animation) = (&info.video, info.kind) else {
        return Err(PlanError::NotVideo { kind: info.kind });
    };
    if info.kind == MediaKind::Animation && !gif_to_mp4 {
        return Err(PlanError::NotVideo { kind: info.kind });
    }
    let codec = video_codec(recipe.video.codec, video)?;
    let mut candidates = VideoEncoder::candidates(codec, recipe.video.encoder);
    if recipe.video.target_vmaf.is_some() && recipe.video.target_bytes.is_none() {
        // The search tunes CRF, which only the software encoders have.
        if recipe.video.encoder == EncoderPreference::Hardware {
            return Err(PlanError::ScoreNeedsSoftware);
        }
        candidates.retain(|e| !e.is_hardware());
    }
    let encoder = candidates
        .iter()
        .copied()
        .find(|e| has_encoder(e.ffmpeg_name()))
        .ok_or_else(|| PlanError::NoEncoder {
            codec,
            tried: candidates.iter().map(|e| e.ffmpeg_name()).collect(),
        })?;

    let pixel_format = pixel_format(encoder, video);
    let (scale_to, audio, rate, target) = match recipe.video.target_bytes {
        None => {
            let scale_to = fit_within(
                video.display_size(),
                recipe.video.max_width,
                recipe.video.max_height,
            )
            .or_else(|| even_size(video.display_size()));
            let audio = if recipe.video.remove_audio || info.audio.is_empty() {
                AudioPlan::None
            } else {
                AudioPlan::Tracks {
                    tracks: info
                        .audio
                        .iter()
                        .map(|t| audio_action(t, recipe.quality))
                        .collect(),
                }
            };
            let rate = match recipe.video.target_vmaf {
                // The search starts from the quality level's own setting.
                Some(_) => RateControl::Crf(encoder.crf(recipe.quality)),
                None => RateControl::Quality(recipe.quality),
            };
            (scale_to, audio, rate, None)
        }
        Some(target_bytes) => {
            let budget = size_budget(info, recipe, encoder, target_bytes)?;
            let target = SizeTarget {
                target_bytes,
                video_bit_rate: budget.video_bit_rate,
                audio_bit_rate: budget.audio_bit_rate,
                two_pass: !encoder.is_hardware(),
                attempts: 0,
            };
            (
                budget.scale_to,
                budget.audio,
                RateControl::Bitrate(budget.video_bit_rate),
                Some(target),
            )
        }
    };
    let mut plan = VideoPlan {
        encoder,
        scale_to,
        pixel_format,
        color: video.color.clone(),
        audio,
        target,
        score_target: match (recipe.video.target_vmaf, &rate) {
            (Some(vmaf), RateControl::Crf(crf)) => Some(ScoreTarget {
                vmaf,
                crf: *crf,
                probes: 0,
                sampled_ms: 0,
            }),
            _ => None,
        },
        args: Vec::new(),
    };
    plan.args = plan.ffmpeg_args(rate, &Pass::Single);
    Ok(plan)
}

/// How the encoder decides how many bits to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant quality (CRF / `-q:v`) for a quality level.
    Quality(Quality),
    /// This CRF value (software encoders), found by a score search.
    Crf(u8),
    /// Average video bit rate in bits per second, for a target file size.
    Bitrate(u64),
}

/// Which pass of an encode the arguments are for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pass {
    /// A normal, single-pass encode.
    Single,
    /// The analysis pass of a two-pass encode: video only, no output file. `log` is the stats
    /// file name, relative to FFmpeg's working directory.
    First { log: String },
    /// The final pass of a two-pass encode, reading the stats from `log`.
    Second { log: String },
}

/// The VMAF score a video must reach, and what the search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScoreTarget {
    /// The VMAF score to reach (model `vmaf_v0.6.1`, 0–100).
    pub vmaf: u8,
    /// The CRF used: the highest (smallest file) that reached the score, once searched.
    pub crf: u8,
    /// Encodes of the samples it took (filled in by the engine).
    pub probes: u32,
    /// Length of the samples scored, in milliseconds (filled in by the engine).
    pub sampled_ms: u64,
}

/// The size a video must fit in, and how the bits are split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizeTarget {
    pub target_bytes: u64,
    /// Video bit rate of the final attempt, in bits per second.
    pub video_bit_rate: u64,
    /// Total audio bit rate, in bits per second.
    pub audio_bit_rate: u64,
    /// Software encoders analyze the whole video first for the best quality at this size.
    pub two_pass: bool,
    /// Encodes it took to fit under the target (filled in by the engine).
    pub attempts: u32,
}

impl VideoPlan {
    /// The encoder, as reports show it: with a note when the HDR is carried over.
    pub fn encoder_label(&self) -> String {
        let name = self.encoder.ffmpeg_name();
        if self.color.is_hdr() {
            format!("{name}, HDR kept")
        } else {
            name.to_owned()
        }
    }

    /// FFmpeg arguments between the input and the output for one pass.
    pub fn ffmpeg_args(&self, rate: RateControl, pass: &Pass) -> Vec<String> {
        let analysis = matches!(pass, Pass::First { .. });
        let mut args: Vec<String> = vec!["-map".into(), "0:v:0".into()];
        if let (AudioPlan::Tracks { tracks }, false) = (&self.audio, analysis) {
            for i in 0..tracks.len() {
                args.extend(["-map".into(), format!("0:a:{i}")]);
            }
        }
        args.extend(["-map_metadata".into(), "0".into()]);
        if let Some((w, h)) = self.scale_to {
            args.extend(["-vf".into(), format!("scale={w}:{h}:flags=lanczos")]);
        }
        args.extend(["-c:v".into(), self.encoder.ffmpeg_name().into()]);
        match rate {
            RateControl::Quality(quality) => args.extend(self.encoder.quality_args(quality)),
            RateControl::Crf(crf) => args.extend(["-crf".into(), crf.to_string()]),
            RateControl::Bitrate(bps) => {
                args.extend(["-b:v".into(), format!("{}k", bps.div_ceil(1000))]);
            }
        }
        match (pass, self.encoder) {
            (Pass::Single, _) => {}
            (Pass::First { log }, VideoEncoder::Libx265) => {
                args.extend(["-x265-params".into(), format!("pass=1:stats={log}")]);
            }
            (Pass::Second { log }, VideoEncoder::Libx265) => {
                args.extend(["-x265-params".into(), format!("pass=2:stats={log}")]);
            }
            (Pass::First { log }, _) => args.extend([
                "-pass".into(),
                "1".into(),
                "-passlogfile".into(),
                log.clone(),
            ]),
            (Pass::Second { log }, _) => args.extend([
                "-pass".into(),
                "2".into(),
                "-passlogfile".into(),
                log.clone(),
            ]),
        }
        if self.encoder.codec() == VideoCodec::Hevc {
            // Apple players only accept HEVC in MP4 with the `hvc1` tag.
            args.extend(["-tag:v".into(), "hvc1".into()]);
        }
        args.extend(["-pix_fmt".into(), self.pixel_format.into()]);
        // Carry the color description over explicitly: without it, older FFmpeg versions tag
        // nothing, and HDR looks washed out.
        let color = &self.color;
        for (flag, value) in [
            ("-color_primaries", &color.primaries),
            ("-color_trc", &color.transfer),
            ("-colorspace", &color.matrix),
            ("-color_range", &color.range),
        ] {
            if let Some(value) = value {
                args.extend([flag.into(), value.clone()]);
            }
        }
        if analysis {
            args.extend(["-an".into(), "-f".into(), "null".into()]);
            return args;
        }
        match &self.audio {
            AudioPlan::None => args.push("-an".into()),
            AudioPlan::Tracks { tracks } => {
                for (i, action) in tracks.iter().enumerate() {
                    match action {
                        AudioAction::Copy => args.extend([format!("-c:a:{i}"), "copy".into()]),
                        AudioAction::Aac { bit_rate } => args.extend([
                            format!("-c:a:{i}"),
                            "aac".into(),
                            format!("-b:a:{i}"),
                            format!("{}k", bit_rate / 1000),
                        ]),
                    }
                }
            }
        }
        // Put the index first so playback can start before the whole file is downloaded.
        args.extend([
            "-movflags".into(),
            "+faststart".into(),
            "-f".into(),
            "mp4".into(),
        ]);
        args
    }
}

/// How a size target is spent.
struct Budget {
    video_bit_rate: u64,
    audio_bit_rate: u64,
    audio: AudioPlan,
    scale_to: Option<(u32, u32)>,
}

/// Lowest video bit rate Peraz will plan, in bits per second: below this nothing is watchable.
const MIN_VIDEO_BIT_RATE: u64 = 40_000;

/// Splits a size target into audio and video bit rates and picks a resolution that the video
/// bit rate can carry.
fn size_budget(
    info: &MediaInfo,
    recipe: &Recipe,
    encoder: VideoEncoder,
    target_bytes: u64,
) -> Result<Budget, PlanError> {
    // A clip only needs its own length in the budget.
    let duration = recipe
        .clip
        .seconds(info.duration_seconds)
        .filter(|d| *d > 0.0)
        .ok_or(PlanError::UnknownDuration)?;
    // Keep 3% for the container (MP4 index, headers) and encoder rate-control variance.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let total = ((target_bytes as f64 * 8.0 * 0.97) / duration) as u64;

    let mut audio_bit_rate = 0;
    let audio = if recipe.video.remove_audio || info.audio.is_empty() {
        AudioPlan::None
    } else {
        // About a tenth of the budget, between 48 and 128 kb/s per stereo track.
        let per_track = (total / 10).clamp(48_000, 128_000);
        let tracks: Vec<AudioAction> = info
            .audio
            .iter()
            .map(|t| {
                let wanted = match t.channels {
                    0..=2 => per_track,
                    channels => per_track * u64::from(channels) / 2,
                };
                match t.bit_rate {
                    Some(rate) if MP4_AUDIO.contains(&t.codec.as_str()) && rate <= wanted => {
                        audio_bit_rate += rate;
                        AudioAction::Copy
                    }
                    _ => {
                        audio_bit_rate += wanted;
                        AudioAction::Aac { bit_rate: wanted }
                    }
                }
            })
            .collect();
        AudioPlan::Tracks { tracks }
    };
    let video_bit_rate = total.saturating_sub(audio_bit_rate);
    if video_bit_rate < MIN_VIDEO_BIT_RATE {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let minimum_bytes =
            ((MIN_VIDEO_BIT_RATE + audio_bit_rate) as f64 * duration / 8.0 / 0.97).ceil() as u64;
        return Err(PlanError::TargetTooSmall {
            target_bytes,
            minimum_bytes,
        });
    }

    let video = info
        .video
        .as_ref()
        .ok_or(PlanError::NotVideo { kind: info.kind })?;
    let user_box = recipe.video.max_width.is_some() || recipe.video.max_height.is_some();
    let scale_to = if user_box {
        fit_within(
            video.display_size(),
            recipe.video.max_width,
            recipe.video.max_height,
        )
    } else {
        let fps = video.frame_rate.map_or(30.0, Rational::as_f64);
        resolution_for_bit_rate(video.display_size(), fps, video_bit_rate, encoder.codec())
    };
    Ok(Budget {
        video_bit_rate,
        audio_bit_rate,
        audio,
        scale_to,
    })
}

/// Picks the largest standard resolution (by the shorter side) whose bits per pixel stay above a
/// watchable floor at `bit_rate`. Returns `None` when the source size already works.
///
/// Floors are starting points from common encoding guidance: about 0.04 bits per pixel per frame
/// for H.264 and 0.03 for HEVC keep typical footage free of heavy blocking.
pub fn resolution_for_bit_rate(
    (width, height): (u32, u32),
    fps: f64,
    bit_rate: u64,
    codec: VideoCodec,
) -> Option<(u32, u32)> {
    const RUNGS: [u32; 8] = [2160, 1440, 1080, 720, 540, 480, 360, 240];
    let floor = match codec {
        VideoCodec::H264 => 0.04,
        VideoCodec::Hevc => 0.03,
    };
    #[allow(clippy::cast_precision_loss)]
    let bpp = |w: u32, h: u32| bit_rate as f64 / (f64::from(w) * f64::from(h) * fps.max(1.0));
    if bpp(width, height) >= floor {
        return None;
    }
    let short = width.min(height);
    let mut chosen = None;
    for rung in RUNGS.into_iter().filter(|r| *r < short) {
        let factor = f64::from(rung) / f64::from(short);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = |v: u32| (((f64::from(v) * factor).round() as u32) & !1).max(2);
        let (w, h) = (size(width), size(height));
        chosen = Some((w, h));
        if bpp(w, h) >= floor {
            break;
        }
    }
    chosen
}

/// How a still image is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "format")]
pub enum ImageEncoding {
    /// mozjpeg at this quality (0–100).
    Jpeg { quality: u8 },
    /// Palette quantization (pngquant-style) with this quality range (0–100), then lossless
    /// optimization. When the minimum cannot be met, the image stays true-color (lossless).
    Png { min_quality: u8, max_quality: u8 },
    /// libwebp lossy at this quality (0–100), with transparency when present.
    Webp { quality: u8 },
    /// AV1 still image (rav1e) at this quality (0–100), with transparency when present.
    Avif { quality: u8 },
}

/// A complete plan for compressing one still image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImagePlan {
    pub encoding: ImageEncoding,
    /// Bounding box, applied after the EXIF orientation (which ffprobe does not report), so the
    /// engine computes the final size with [`fit_within`] once the pixels are upright.
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    /// Transparent pixels are composited onto white because the output format has no alpha.
    pub flatten_alpha: bool,
    /// File extension for the output, without the dot.
    pub extension: &'static str,
    /// How the input is decoded.
    pub decoder: ImageDecoder,
    /// The output format differs from the input's. A converted file is kept even when it is
    /// larger: the format change was the point (e.g. HEIC to JPEG for compatibility).
    pub converts_format: bool,
    /// Search the encoder's quality setting for the smallest file with this SSIMULACRA2 score;
    /// `encoding` then holds the setting the search starts from (and, once done, the one used).
    pub target_ssimulacra2: Option<u8>,
}

/// How a still image is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDecoder {
    /// In-process decoders (JPEG, PNG, WebP, BMP, TIFF).
    Native,
    /// FFmpeg, for HEIC/HEIF and AVIF (tile grids, HEVC/AV1 pictures).
    Ffmpeg,
}

/// Image codecs (FFmpeg names) decoded in process.
const NATIVE_IMAGES: &[&str] = &["mjpeg", "png", "webp", "bmp", "tiff"];
/// Image codecs decoded through FFmpeg: HEIC/HEIF (HEVC) and AVIF (AV1) stills, and GIFs with
/// a single frame (animated GIFs take the GIF pipeline).
const FFMPEG_IMAGES: &[&str] = &["hevc", "av1", "gif"];

/// The format an input image is stored in, when Peraz can also write it.
fn source_format(codec: &str) -> Option<ImageFormat> {
    match codec {
        "mjpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::Webp),
        "av1" => Some(ImageFormat::Avif),
        _ => None,
    }
}

/// Plans the compression of a still image.
pub fn plan_image(info: &MediaInfo, recipe: &Recipe) -> Result<ImagePlan, PlanError> {
    let (Some(picture), MediaKind::Image) = (&info.video, info.kind) else {
        return Err(PlanError::NotImage { kind: info.kind });
    };
    let codec = picture.codec.as_str();
    let decoder = if NATIVE_IMAGES.contains(&codec) {
        ImageDecoder::Native
    } else if FFMPEG_IMAGES.contains(&codec) {
        ImageDecoder::Ffmpeg
    } else {
        return Err(PlanError::UnsupportedImage {
            codec: picture.codec.clone(),
            hint: "",
        });
    };
    let source = source_format(codec);
    let format = match (recipe.image.format, codec) {
        // Peraz cannot write HEIC; JPEG opens everywhere, which is why people convert HEIC.
        (ImageFormat::Keep, "hevc") => ImageFormat::Jpeg,
        // A still GIF is a graphic: PNG keeps it lossless, with its transparency.
        (ImageFormat::Keep, "gif") => ImageFormat::Png,
        (ImageFormat::Keep, _) if source.is_some() => source.unwrap_or_default(),
        (ImageFormat::Keep, other) => {
            return Err(PlanError::UnsupportedImage {
                codec: other.to_owned(),
                hint: " as an output format; choose --format jpeg, png, webp, or avif",
            });
        }
        (chosen, _) => chosen,
    };
    let level = recipe.quality as usize;
    let (encoding, extension) = match format {
        // mozjpeg quality: about 75–85 is the usual range for photos on the web.
        ImageFormat::Jpeg => (
            ImageEncoding::Jpeg {
                quality: [90, 85, 78, 70, 60][level],
            },
            "jpg",
        ),
        // pngquant-style ranges: aim for the maximum, give up below the minimum.
        ImageFormat::Png => {
            let (min_quality, max_quality) =
                [(90, 100), (75, 95), (65, 85), (50, 75), (30, 60)][level];
            (
                ImageEncoding::Png {
                    min_quality,
                    max_quality,
                },
                "png",
            )
        }
        // libwebp quality: default 75; WebP reaches JPEG-like quality at lower settings.
        ImageFormat::Webp => (
            ImageEncoding::Webp {
                quality: [90, 84, 76, 68, 58][level],
            },
            "webp",
        ),
        // ravif quality: about 60–80 suits the web; AV1 holds detail at lower settings than JPEG.
        ImageFormat::Avif => (
            ImageEncoding::Avif {
                quality: [85, 78, 68, 58, 48][level],
            },
            "avif",
        ),
        ImageFormat::Keep => unreachable!("resolved above"),
    };
    Ok(ImagePlan {
        encoding,
        max_width: recipe.image.max_width,
        max_height: recipe.image.max_height,
        flatten_alpha: format == ImageFormat::Jpeg && picture.alpha != Alpha::Absent,
        extension,
        decoder,
        // A still GIF becomes PNG only when that is smaller (it is not a format asked for).
        converts_format: source != Some(format)
            && !(codec == "gif" && recipe.image.format == ImageFormat::Keep),
        target_ssimulacra2: recipe.image.target_ssimulacra2,
    })
}

/// A complete plan for making an animated GIF from a video or a GIF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GifPlan {
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// Palette quality range per frame (0–100), pngquant-style.
    pub min_quality: u8,
    pub max_quality: u8,
    /// Dithering strength in percent: smoother gradients, but noisier frames compress worse.
    pub dithering_percent: u8,
    /// A pixel whose color moved less than this (0–255 per channel) since the previous frame is
    /// left unchanged, which lets unchanged areas cost almost nothing.
    pub change_threshold: u8,
    /// Temporal denoising strength (0 = off): small changes of a pixel from one frame to the next
    /// are averaged away before diffing, so camera noise does not count as change.
    pub denoise: u8,
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// The output is a GIF made from something else (a video): kept even when larger.
    pub converts_format: bool,
}

/// Plans an animated GIF from a video (when the recipe asks for GIF) or from a GIF.
pub fn plan_gif(info: &MediaInfo, recipe: &Recipe) -> Result<GifPlan, PlanError> {
    let wants_gif = (info.kind == MediaKind::Animation
        && matches!(recipe.video.output, VideoOutput::Keep | VideoOutput::Gif))
        || (info.kind == MediaKind::Video && recipe.video.output == VideoOutput::Gif);
    let (Some(picture), true) = (&info.video, wants_gif) else {
        return Err(PlanError::NotAnimation { kind: info.kind });
    };
    let level = recipe.quality as usize;
    // GIFs grow fast with size and frame rate: default to a width typical for chat and docs,
    // unless the user set a bounding box.
    let default_width = [800, 640, 480, 400, 320][level];
    let max_width = recipe
        .video
        .max_width
        .or(if recipe.video.max_height.is_none() {
            Some(default_width)
        } else {
            None
        });
    let (width, height) = fit_within(picture.display_size(), max_width, recipe.video.max_height)
        .unwrap_or_else(|| picture.display_size());
    let source_fps = picture
        .frame_rate
        .map_or(f64::from(u32::MAX), Rational::as_f64);
    let wanted = recipe
        .gif
        .fps
        .unwrap_or([20, 15, 12, 10, 8][level])
        .clamp(1, 50);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 1..=50
    let fps = if f64::from(wanted) > source_fps {
        source_fps.round().clamp(1.0, 50.0) as u32
    } else {
        wanted
    };
    // Tuned on real footage (an aerial shot and a screen recording) against FFmpeg's
    // palettegen/paletteuse: `balanced` matches its quality (SSIM) at well under half the size.
    let (min_quality, max_quality) = [(75, 100), (65, 100), (60, 95), (45, 85), (30, 75)][level];
    Ok(GifPlan {
        fps,
        width,
        height,
        min_quality,
        max_quality,
        dithering_percent: [100, 100, 90, 80, 70][level],
        change_threshold: [1, 2, 3, 5, 8][level],
        // Measured on an aerial shot and a screen recording, each also with noise added (scored
        // against the clean original). At `balanced`, GIFs from the noisy clips got 1.6-2.1 times
        // smaller and closer to the clean picture, and GIFs from the clean ones 12-20% smaller
        // for an SSIM at most 0.003 lower; heavy noise needs the stronger levels. A GIF source's
        // dithering also counts as noise, so GIFs get a lighter touch.
        denoise: if info.kind == MediaKind::Animation {
            [0, 2, 3, 5, 8][level]
        } else {
            [0, 3, 5, 8, 12][level]
        },
        start_ms: recipe.clip.start_ms,
        duration_ms: recipe.clip.duration_ms,
        converts_format: info.kind != MediaKind::Animation,
    })
}

/// An animated WebP or AVIF made by FFmpeg from a video or a GIF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnimatedPlan {
    /// `webp` or `avif`.
    pub format: VideoOutput,
    /// FFmpeg encoder name.
    pub encoder: &'static str,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Transparent areas are placed on white because the output format here has no alpha.
    pub flatten_alpha: bool,
    pub extension: &'static str,
    /// FFmpeg arguments between the input and the output.
    pub args: Vec<String>,
}

/// Plans an animated WebP or AVIF from a video or a GIF.
///
/// `has_encoder` reports whether the FFmpeg build contains an encoder (by FFmpeg name).
pub fn plan_animated(
    info: &MediaInfo,
    recipe: &Recipe,
    has_encoder: impl Fn(&str) -> bool,
) -> Result<AnimatedPlan, PlanError> {
    let format = recipe.video.output;
    let (
        Some(picture),
        MediaKind::Video | MediaKind::Animation,
        VideoOutput::Webp | VideoOutput::Avif,
    ) = (&info.video, info.kind, format)
    else {
        return Err(PlanError::NotAnimation { kind: info.kind });
    };
    let level = recipe.quality as usize;
    let (widths, rates) = if format == VideoOutput::Webp {
        ([1080, 960, 720, 640, 480], [30, 24, 20, 15, 12])
    } else {
        ([1280, 1080, 960, 720, 540], [30, 30, 24, 20, 15])
    };
    let max_width = recipe
        .video
        .max_width
        .or(if recipe.video.max_height.is_none() {
            Some(widths[level])
        } else {
            None
        });
    let (width, height) = fit_within(picture.display_size(), max_width, recipe.video.max_height)
        .or_else(|| even_size(picture.display_size()))
        .unwrap_or_else(|| picture.display_size());
    let source_fps = picture
        .frame_rate
        .map_or(f64::from(u32::MAX), Rational::as_f64);
    let wanted = recipe.gif.fps.unwrap_or(rates[level]).clamp(1, 60);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 1..=60
    let fps = if f64::from(wanted) > source_fps {
        source_fps.round().clamp(1.0, 60.0) as u32
    } else {
        wanted
    };

    // GIFs may be transparent. WebP keeps it; AVIF output here does not, so it gets a white matte.
    let maybe_alpha = info.kind == MediaKind::Animation || picture.alpha != Alpha::Absent;
    let flatten_alpha = format == VideoOutput::Avif && maybe_alpha;
    let scale = format!("fps={fps},scale={width}:{height}:flags=lanczos");
    let frames = AnimationFrames {
        scale,
        width,
        height,
        fps,
        level,
        maybe_alpha,
        flatten_alpha,
    };
    let (encoder, extension, args) = if format == VideoOutput::Webp {
        animated_webp_args(&frames, &has_encoder)?
    } else {
        animated_avif_args(&frames, &has_encoder)?
    };
    Ok(AnimatedPlan {
        format,
        encoder,
        fps,
        width,
        height,
        start_ms: recipe.clip.start_ms,
        duration_ms: recipe.clip.duration_ms,
        flatten_alpha,
        extension,
        args,
    })
}

/// What the animated encoders need to know about the frames.
struct AnimationFrames {
    scale: String,
    width: u32,
    height: u32,
    fps: u32,
    level: usize,
    maybe_alpha: bool,
    flatten_alpha: bool,
}

type EncoderArgs = (&'static str, &'static str, Vec<String>);

fn animated_webp_args(
    frames: &AnimationFrames,
    has_encoder: &impl Fn(&str) -> bool,
) -> Result<EncoderArgs, PlanError> {
    let encoder = "libwebp_anim";
    if !has_encoder(encoder) {
        return Err(PlanError::NoAnimationEncoder {
            format: VideoOutput::Webp,
            tried: vec![encoder],
        });
    }
    let pix = if frames.maybe_alpha {
        "yuva420p"
    } else {
        "yuv420p"
    };
    let mut args: Vec<String> = vec![
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        format!("{},format={pix}", frames.scale),
    ];
    args.extend(["-c:v".into(), encoder.into()]);
    // libwebp quality: the same scale as still WebP images.
    args.extend([
        "-quality".into(),
        [90, 84, 76, 68, 58][frames.level].to_string(),
    ]);
    args.extend([
        "-compression_level".into(),
        "5".into(),
        "-loop".into(),
        "0".into(),
    ]);
    args.extend(["-an".into(), "-f".into(), "webp".into()]);
    Ok((encoder, "webp", args))
}

fn animated_avif_args(
    frames: &AnimationFrames,
    has_encoder: &impl Fn(&str) -> bool,
) -> Result<EncoderArgs, PlanError> {
    let tried = vec!["libsvtav1", "libaom-av1"];
    let encoder = tried
        .iter()
        .copied()
        .find(|e| has_encoder(e))
        .ok_or_else(|| PlanError::NoAnimationEncoder {
            format: VideoOutput::Avif,
            tried: tried.clone(),
        })?;
    let AnimationFrames {
        scale,
        width,
        height,
        fps,
        level,
        ..
    } = frames;
    let mut args: Vec<String> = if frames.flatten_alpha {
        vec![
            "-filter_complex".into(),
            format!(
                "[0:v:0]{scale},format=rgba[v];color=c=white:s={width}x{height}:r={fps}[bg];\
                 [bg][v]overlay=shortest=1,format=yuv420p[out]"
            ),
            "-map".into(),
            "[out]".into(),
        ]
    } else {
        vec![
            "-map".into(),
            "0:v:0".into(),
            "-vf".into(),
            format!("{scale},format=yuv420p"),
        ]
    };
    args.extend(["-c:v".into(), encoder.into()]);
    // AV1 CRF 0–63; around 30 suits the web. SVT-AV1 preset 8 and libaom cpu-used 6 are quick.
    args.extend(["-crf".into(), [24, 28, 32, 36, 42][*level].to_string()]);
    if encoder == "libsvtav1" {
        args.extend(["-preset".into(), "8".into()]);
    } else {
        args.extend([
            "-cpu-used".into(),
            "6".into(),
            "-row-mt".into(),
            "1".into(),
            "-b:v".into(),
            "0".into(),
        ]);
    }
    args.extend(["-an".into(), "-f".into(), "avif".into()]);
    Ok((encoder, "avif", args))
}

/// Rounds odd dimensions down to even ones (4:2:0 video needs even sizes); `None` when already even.
fn even_size((width, height): (u32, u32)) -> Option<(u32, u32)> {
    let even = ((width & !1).max(2), (height & !1).max(2));
    (even != (width, height)).then_some(even)
}

/// How the pictures inside a PDF or EPUB are recompressed. Text, layout, and links are untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentPlan {
    /// mozjpeg quality for photos.
    pub jpeg_quality: u8,
    /// Palette quality range for graphics (pngquant-style).
    pub png_min_quality: u8,
    pub png_max_quality: u8,
    /// libwebp quality for WebP pictures (EPUB).
    pub webp_quality: u8,
    /// Pictures larger than this on their long side are scaled down; `None` keeps every size.
    pub max_image_side: Option<u32>,
}

impl DocumentPlan {
    /// The plan for one picture inside the document, kept in `format`.
    pub fn image_plan(&self, format: ImageFormat) -> ImagePlan {
        let (encoding, extension) = match format {
            ImageFormat::Png => (
                ImageEncoding::Png {
                    min_quality: self.png_min_quality,
                    max_quality: self.png_max_quality,
                },
                "png",
            ),
            ImageFormat::Webp => (
                ImageEncoding::Webp {
                    quality: self.webp_quality,
                },
                "webp",
            ),
            ImageFormat::Avif => (
                ImageEncoding::Avif {
                    quality: self.webp_quality,
                },
                "avif",
            ),
            ImageFormat::Jpeg | ImageFormat::Keep => (
                ImageEncoding::Jpeg {
                    quality: self.jpeg_quality,
                },
                "jpg",
            ),
        };
        ImagePlan {
            encoding,
            max_width: self.max_image_side,
            max_height: self.max_image_side,
            flatten_alpha: false,
            extension,
            decoder: ImageDecoder::Native,
            converts_format: false,
            target_ssimulacra2: None,
        }
    }
}

/// Plans the recompression of a PDF or EPUB.
pub fn plan_document(info: &MediaInfo, recipe: &Recipe) -> Result<DocumentPlan, PlanError> {
    if !matches!(info.kind, MediaKind::Pdf | MediaKind::Epub) {
        return Err(PlanError::NotDocument { kind: info.kind });
    }
    let level = recipe.quality as usize;
    let (png_min_quality, png_max_quality) =
        [(90, 100), (75, 95), (65, 85), (50, 75), (30, 60)][level];
    Ok(DocumentPlan {
        jpeg_quality: [90, 85, 78, 70, 60][level],
        png_min_quality,
        png_max_quality,
        webp_quality: [90, 84, 76, 68, 58][level],
        // Enough for print at the higher levels and for screens and e-readers at the lower ones;
        // an explicit bounding box wins.
        max_image_side: recipe.image.max_width.max(recipe.image.max_height).or([
            None,
            Some(3000),
            Some(2400),
            Some(2000),
            Some(1600),
        ][level]),
    })
}

/// How a trim is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrimMethod {
    /// The streams are copied: instant and lossless.
    StreamCopy,
    /// The clip is re-encoded, because the start is not on a keyframe.
    Reencode,
}

/// Cutting a clip out of a video without compressing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrimPlan {
    pub method: TrimMethod,
    /// Where the clip starts, after snapping to a keyframe when asked.
    pub start_ms: u64,
    /// Where to seek for a stream copy: just after the keyframe, so rounding to milliseconds can
    /// never land on the keyframe before it.
    #[serde(skip)]
    pub seek_ms: u64,
    pub duration_ms: Option<u64>,
    /// Why the clip is re-encoded, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// File extension of the output (the input's container when copying, `mp4` otherwise).
    pub extension: String,
    /// FFmpeg arguments for a stream copy.
    pub args: Vec<String>,
    /// The encode, when the clip is re-encoded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoPlan>,
}

/// A start within this distance of a keyframe counts as on it (about one frame at 25 fps).
const KEYFRAME_TOLERANCE_MS: u64 = 40;

/// Containers a stream copy can write, by input extension, with their FFmpeg muxer.
const COPY_CONTAINERS: &[(&str, &str)] = &[
    ("mp4", "mp4"),
    ("m4v", "mp4"),
    ("mov", "mov"),
    ("mkv", "matroska"),
    ("webm", "webm"),
];

/// Plans a trim. `keyframes_ms` are the video keyframe times at or before the start (any order);
/// `extension` is the input file's extension.
pub fn plan_trim(
    info: &MediaInfo,
    recipe: &Recipe,
    keyframes_ms: &[u64],
    extension: &str,
    has_encoder: impl Fn(&str) -> bool,
) -> Result<TrimPlan, PlanError> {
    if info.kind != MediaKind::Video {
        return Err(PlanError::NotVideo { kind: info.kind });
    }
    let wanted = recipe.clip.start_ms.unwrap_or(0);
    let duration_ms = recipe.clip.duration_ms;
    let previous = keyframes_ms
        .iter()
        .copied()
        .filter(|k| *k <= wanted + KEYFRAME_TOLERANCE_MS)
        .max();
    let on_keyframe =
        wanted == 0 || previous.is_some_and(|k| k.abs_diff(wanted) <= KEYFRAME_TOLERANCE_MS);
    let muxer = COPY_CONTAINERS
        .iter()
        .find(|(ext, _)| ext.eq_ignore_ascii_case(extension))
        .map(|(_, muxer)| *muxer);

    let copy_from = match (muxer, on_keyframe, recipe.video.snap_to_keyframe, previous) {
        (Some(_), true, _, _) => Some(if wanted == 0 {
            0
        } else {
            previous.unwrap_or(wanted)
        }),
        (Some(_), false, true, Some(keyframe)) => Some(keyframe),
        _ => None,
    };
    if let (Some(start_ms), Some(muxer)) = (copy_from, muxer) {
        // Snapping moves the start back; keep the requested end so nothing asked for is lost.
        let duration_ms = duration_ms.map(|d| d + wanted.saturating_sub(start_ms));
        let mut args: Vec<String> = [
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-map_metadata",
            "0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if matches!(muxer, "mp4" | "mov") {
            args.extend(["-movflags".to_owned(), "+faststart".to_owned()]);
        }
        args.extend(["-f".to_owned(), muxer.to_owned()]);
        return Ok(TrimPlan {
            method: TrimMethod::StreamCopy,
            start_ms,
            seek_ms: if start_ms == 0 { 0 } else { start_ms + 1 },
            duration_ms,
            reason: None,
            extension: extension.to_ascii_lowercase(),
            args,
            video: None,
        });
    }

    let seconds = |ms: u64| format!("{}.{:03} s", ms / 1000, ms % 1000);
    let reason = match (muxer, previous) {
        (None, _) => format!(
            "streams in a .{extension} file cannot be copied into a clip; re-encoded to MP4"
        ),
        (Some(_), Some(keyframe)) => format!(
            "the start ({}) is not on a keyframe; the one before it is at {} (use --snap-to-keyframe to copy from there)",
            seconds(wanted),
            seconds(keyframe)
        ),
        (Some(_), None) => format!("no keyframe at or before the start ({})", seconds(wanted)),
    };
    let mut encode = recipe.clone();
    encode.video.output = VideoOutput::Mp4;
    let video = plan_video(info, &encode, has_encoder)?;
    Ok(TrimPlan {
        method: TrimMethod::Reencode,
        start_ms: wanted,
        seek_ms: wanted,
        duration_ms,
        reason: Some(reason),
        extension: "mp4".to_owned(),
        args: Vec::new(),
        video: Some(video),
    })
}

/// Scales `(width, height)` down to fit the bounding box, keeping the aspect ratio and even
/// dimensions (required by 4:2:0 chroma). Returns `None` when no scaling is needed.
pub fn fit_within(
    (width, height): (u32, u32),
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Option<(u32, u32)> {
    let factor_w = max_width.map_or(1.0, |m| f64::from(m) / f64::from(width));
    let factor_h = max_height.map_or(1.0, |m| f64::from(m) / f64::from(height));
    let factor = factor_w.min(factor_h);
    if factor >= 1.0 {
        return None;
    }
    let even = |v: u32, max: Option<u32>| {
        // Round down to even so the result never exceeds the bound.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0 < value < v
        let scaled = (f64::from(v) * factor).round() as u32;
        let capped = max.map_or(scaled, |m| scaled.min(m));
        (capped & !1).max(2)
    };
    Some((even(width, max_width), even(height, max_height)))
}

/// The output codec: the one asked for, or H.264 unless the video is HDR, which only HEVC keeps.
fn video_codec(chosen: Option<VideoCodec>, video: &VideoTrack) -> Result<VideoCodec, PlanError> {
    match (chosen, video.color.is_hdr()) {
        (Some(VideoCodec::H264), true) => Err(PlanError::HdrNeedsHevc),
        (Some(codec), _) => Ok(codec),
        (None, true) => Ok(VideoCodec::Hevc),
        (None, false) => Ok(VideoCodec::H264),
    }
}

fn pixel_format(encoder: VideoEncoder, video: &VideoTrack) -> &'static str {
    let ten_bit = video
        .pixel_format
        .as_deref()
        .is_some_and(|p| p.contains("10"));
    match encoder {
        VideoEncoder::Libx265 if ten_bit => "yuv420p10le",
        VideoEncoder::HevcVideoToolbox if ten_bit => "p010le",
        // 8-bit 4:2:0 is what every H.264 player supports.
        _ => "yuv420p",
    }
}

fn audio_action(track: &AudioTrack, quality: Quality) -> AudioAction {
    let target: u64 = match quality {
        Quality::Highest => 192_000,
        Quality::High => 160_000,
        Quality::Balanced => 128_000,
        Quality::Small => 96_000,
        Quality::Smallest => 64_000,
    };
    // Scale the target with the channel count: the figures above are for stereo.
    let target = match track.channels {
        0..=2 => target,
        channels => target * u64::from(channels) / 2,
    };
    let copyable = MP4_AUDIO.contains(&track.codec.as_str());
    // Copy when re-encoding would not save much (or the rate is unknown, to avoid a
    // generation loss for nothing).
    let small_enough = track
        .bit_rate
        .is_none_or(|rate| rate <= target + target / 4);
    if copyable && small_enough {
        AudioAction::Copy
    } else {
        AudioAction::Aac { bit_rate: target }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // tests tweak one field at a time
mod tests {
    use super::*;

    fn video_info(width: u32, height: u32, audio: Vec<AudioTrack>) -> MediaInfo {
        MediaInfo {
            kind: MediaKind::Video,
            container: "mov,mp4,m4a,3gp,3g2,mj2".into(),
            size_bytes: 10_000_000,
            duration_seconds: Some(10.0),
            bit_rate: Some(8_000_000),
            video: Some(VideoTrack {
                codec: "h264".into(),
                width,
                height,
                rotation: 0,
                frame_rate: Some(Rational { num: 30, den: 1 }),
                frame_count: Some(300),
                pixel_format: Some("yuv420p".into()),
                color: ColorInfo::default(),
                alpha: Alpha::Absent,
                bit_rate: Some(7_800_000),
            }),
            audio,
        }
    }

    fn aac(bit_rate: u64) -> AudioTrack {
        AudioTrack {
            codec: "aac".into(),
            channels: 2,
            sample_rate: Some(48_000),
            bit_rate: Some(bit_rate),
        }
    }

    fn everything(_: &str) -> bool {
        true
    }

    #[test]
    fn golden_default_h264() {
        let plan = plan_video(
            &video_info(1920, 1080, vec![aac(192_000)]),
            &Recipe::default(),
            everything,
        )
        .unwrap();
        assert_eq!(
            plan.args.join(" "),
            "-map 0:v:0 -map 0:a:0 -map_metadata 0 -c:v libx264 -crf 24 -pix_fmt yuv420p \
             -c:a:0 aac -b:a:0 128k -movflags +faststart -f mp4"
        );
    }

    #[test]
    fn golden_hevc_scaled_without_audio() {
        let mut recipe = Recipe::default();
        recipe.quality = Quality::Small;
        recipe.video.codec = Some(VideoCodec::Hevc);
        recipe.video.max_width = Some(1280);
        recipe.video.remove_audio = true;
        let plan = plan_video(
            &video_info(3840, 2160, vec![aac(128_000)]),
            &recipe,
            everything,
        )
        .unwrap();
        assert_eq!(
            plan.args.join(" "),
            "-map 0:v:0 -map_metadata 0 -vf scale=1280:720:flags=lanczos -c:v libx265 -crf 28 \
             -tag:v hvc1 -pix_fmt yuv420p -an -movflags +faststart -f mp4"
        );
    }

    #[test]
    fn a_vmaf_target_searches_crf_on_software_encoders_only() {
        let mut recipe = Recipe::default();
        recipe.video.target_vmaf = Some(93);
        let plan = plan_video(&video_info(1280, 720, vec![]), &recipe, |_| true).unwrap();
        assert_eq!(plan.encoder, VideoEncoder::Libx264);
        assert_eq!(
            plan.score_target,
            Some(ScoreTarget {
                vmaf: 93,
                crf: 24,
                probes: 0,
                sampled_ms: 0
            }),
            "starts from the quality level's CRF"
        );
        assert!(plan.args.join(" ").contains("-crf 24"));

        let only_vt = |name: &str| name.ends_with("videotoolbox");
        assert!(
            matches!(
                plan_video(&video_info(1280, 720, vec![]), &recipe, only_vt),
                Err(PlanError::NoEncoder { .. })
            ),
            "no fallback to hardware, which has no CRF"
        );
        recipe.video.encoder = EncoderPreference::Hardware;
        assert_eq!(
            plan_video(&video_info(1280, 720, vec![]), &recipe, |_| true),
            Err(PlanError::ScoreNeedsSoftware)
        );
    }

    #[test]
    fn auto_falls_back_to_hardware_and_explains_missing_encoders() {
        let only_vt = |name: &str| name.ends_with("videotoolbox");
        let plan = plan_video(&video_info(1280, 720, vec![]), &Recipe::default(), only_vt).unwrap();
        assert_eq!(plan.encoder, VideoEncoder::H264VideoToolbox);
        assert!(plan.args.join(" ").contains("-q:v 70"));

        let err = plan_video(&video_info(1280, 720, vec![]), &Recipe::default(), |_| {
            false
        })
        .unwrap_err();
        assert_eq!(
            err,
            PlanError::NoEncoder {
                codec: VideoCodec::H264,
                tried: vec!["libx264", "h264_videotoolbox"]
            }
        );
    }

    #[test]
    fn hardware_preference_never_uses_software() {
        let mut recipe = Recipe::default();
        recipe.video.encoder = EncoderPreference::Hardware;
        let err =
            plan_video(&video_info(1280, 720, vec![]), &recipe, |n| n == "libx264").unwrap_err();
        assert!(matches!(err, PlanError::NoEncoder { .. }));
    }

    #[test]
    fn rejects_non_video() {
        let mut info = video_info(100, 100, vec![]);
        info.kind = MediaKind::Image;
        assert_eq!(
            plan_video(&info, &Recipe::default(), everything),
            Err(PlanError::NotVideo {
                kind: MediaKind::Image
            })
        );
    }

    #[test]
    fn audio_is_copied_when_already_small() {
        assert_eq!(
            audio_action(&aac(128_000), Quality::Balanced),
            AudioAction::Copy
        );
        assert_eq!(
            audio_action(&aac(160_000), Quality::Balanced),
            AudioAction::Copy
        );
        assert_eq!(
            audio_action(&aac(320_000), Quality::Balanced),
            AudioAction::Aac { bit_rate: 128_000 }
        );
        let pcm = AudioTrack {
            codec: "pcm_s16le".into(),
            channels: 2,
            sample_rate: None,
            bit_rate: None,
        };
        assert_eq!(
            audio_action(&pcm, Quality::High),
            AudioAction::Aac { bit_rate: 160_000 }
        );
        let surround = AudioTrack {
            channels: 6,
            ..aac(1_000_000)
        };
        assert_eq!(
            audio_action(&surround, Quality::Balanced),
            AudioAction::Aac { bit_rate: 384_000 }
        );
    }

    #[test]
    fn fit_within_never_upscales_and_keeps_even_sizes() {
        assert_eq!(fit_within((1920, 1080), Some(3840), None), None);
        assert_eq!(fit_within((1920, 1080), None, None), None);
        assert_eq!(
            fit_within((1920, 1080), Some(1280), None),
            Some((1280, 720))
        );
        assert_eq!(
            fit_within((1080, 1920), Some(1280), Some(1280)),
            Some((720, 1280))
        );
        assert_eq!(
            fit_within((1918, 1080), Some(1001), None),
            Some((1000, 564))
        );
        let (w, h) = fit_within((4000, 3), Some(100), None).unwrap();
        assert_eq!((w, h), (100, 2));
    }

    #[test]
    fn rotation_applies_before_the_bounding_box() {
        let mut info = video_info(1920, 1080, vec![]);
        info.video.as_mut().unwrap().rotation = 90; // portrait phone video
        let mut recipe = Recipe::default();
        recipe.video.max_height = Some(1280);
        let plan = plan_video(&info, &recipe, everything).unwrap();
        assert_eq!(plan.scale_to, Some((720, 1280)));
    }

    fn image_info(codec: &str, container: &str, alpha: Alpha) -> MediaInfo {
        let mut info = video_info(4000, 3000, vec![]);
        info.kind = MediaKind::Image;
        info.container = container.into();
        let picture = info.video.as_mut().unwrap();
        picture.codec = codec.into();
        picture.alpha = alpha;
        info
    }

    #[test]
    fn images_keep_their_format_by_default() {
        let jpeg = plan_image(
            &image_info("mjpeg", "image2", Alpha::Absent),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(jpeg.encoding, ImageEncoding::Jpeg { quality: 78 });
        assert_eq!(jpeg.extension, "jpg");
        let png = plan_image(
            &image_info("png", "png_pipe", Alpha::Present),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(
            png.encoding,
            ImageEncoding::Png {
                min_quality: 65,
                max_quality: 85
            }
        );
        assert!(!png.flatten_alpha);
    }

    #[test]
    fn converting_transparent_png_to_jpeg_flattens_alpha() {
        let mut recipe = Recipe::default();
        recipe.image.format = ImageFormat::Jpeg;
        recipe.image.max_width = Some(2000);
        let plan = plan_image(&image_info("png", "png_pipe", Alpha::Present), &recipe).unwrap();
        assert!(plan.flatten_alpha);
        assert_eq!(plan.max_width, Some(2000));
    }

    #[test]
    fn unsupported_images_explain_themselves() {
        // A still GIF is decoded by FFmpeg and becomes PNG, kept only when smaller.
        let gif = plan_image(
            &image_info("gif", "gif", Alpha::Unknown),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(
            (gif.decoder, gif.extension, gif.converts_format),
            (ImageDecoder::Ffmpeg, "png", false)
        );
        let other = plan_image(
            &image_info("cdxl", "cdxl", Alpha::Absent),
            &Recipe::default(),
        );
        assert!(matches!(other, Err(PlanError::UnsupportedImage { .. })));
        let bmp = plan_image(
            &image_info("bmp", "bmp_pipe", Alpha::Absent),
            &Recipe::default(),
        )
        .unwrap_err();
        assert!(bmp.to_string().contains("--format jpeg"));
        let mut recipe = Recipe::default();
        recipe.image.format = ImageFormat::Png;
        assert!(plan_image(&image_info("webp", "webp_pipe", Alpha::Absent), &recipe).is_ok());
        assert_eq!(
            plan_image(&video_info(10, 10, vec![]), &recipe),
            Err(PlanError::NotImage {
                kind: MediaKind::Video
            })
        );
    }

    #[test]
    fn webp_and_avif_keep_transparency() {
        let png = image_info("png", "png_pipe", Alpha::Present);
        for (format, ext) in [(ImageFormat::Webp, "webp"), (ImageFormat::Avif, "avif")] {
            let mut recipe = Recipe::default();
            recipe.image.format = format;
            let plan = plan_image(&png, &recipe).unwrap();
            assert_eq!(plan.extension, ext);
            assert!(!plan.flatten_alpha);
        }
        let webp = plan_image(
            &image_info("webp", "webp_pipe", Alpha::Absent),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(webp.encoding, ImageEncoding::Webp { quality: 76 });
    }

    #[test]
    fn heic_becomes_jpeg_and_avif_stays_avif() {
        let heic = plan_image(
            &image_info("hevc", "mov,mp4,m4a,3gp,3g2,mj2", Alpha::Unknown),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(heic.extension, "jpg");
        assert_eq!(heic.decoder, ImageDecoder::Ffmpeg);
        assert!(heic.converts_format);
        assert!(
            heic.flatten_alpha,
            "unknown alpha is flattened rather than turned black"
        );
        let avif = plan_image(
            &image_info("av1", "mov,mp4,m4a,3gp,3g2,mj2", Alpha::Unknown),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(avif.extension, "avif");
        assert!(!avif.converts_format);
        let jpeg = plan_image(
            &image_info("mjpeg", "image2", Alpha::Absent),
            &Recipe::default(),
        )
        .unwrap();
        assert_eq!(
            (jpeg.decoder, jpeg.converts_format),
            (ImageDecoder::Native, false)
        );
    }

    #[test]
    fn video_to_gif_uses_level_defaults_and_never_upscales() {
        let mut recipe = Recipe::default();
        recipe.video.output = VideoOutput::Gif;
        let plan = plan_gif(&video_info(1920, 1080, vec![]), &recipe).unwrap();
        assert_eq!((plan.width, plan.height, plan.fps), (480, 270, 12));
        assert!(plan.converts_format);

        recipe.gif.fps = Some(60);
        recipe.video.max_width = Some(4000);
        let plan = plan_gif(&video_info(320, 240, vec![]), &recipe).unwrap();
        assert_eq!((plan.width, plan.height), (320, 240), "never upscales");
        assert_eq!(plan.fps, 30, "never above the source frame rate");
    }

    #[test]
    fn gif_denoising_grows_with_the_level_and_is_lighter_on_gifs() {
        let mut recipe = Recipe::default();
        recipe.video.output = VideoOutput::Gif;
        let mut gif = video_info(480, 270, vec![]);
        gif.kind = MediaKind::Animation;
        let video = video_info(480, 270, vec![]);
        let denoise = |info: &MediaInfo, quality| {
            let mut recipe = recipe.clone();
            recipe.quality = quality;
            plan_gif(info, &recipe).unwrap().denoise
        };
        assert_eq!(
            denoise(&video, Quality::Highest),
            0,
            "highest keeps every pixel"
        );
        assert_eq!(denoise(&gif, Quality::Highest), 0);
        let levels = [
            Quality::High,
            Quality::Balanced,
            Quality::Small,
            Quality::Smallest,
        ];
        for pair in levels.windows(2) {
            assert!(denoise(&video, pair[0]) < denoise(&video, pair[1]));
            assert!(denoise(&gif, pair[0]) < denoise(&gif, pair[1]));
        }
        for level in levels {
            assert!(denoise(&gif, level) < denoise(&video, level));
        }
    }

    #[test]
    fn gif_plans_only_for_animations_or_requested_video() {
        let mut gif = video_info(480, 270, vec![]);
        gif.kind = MediaKind::Animation;
        let plan = plan_gif(&gif, &Recipe::default()).unwrap();
        assert!(!plan.converts_format);
        assert_eq!(
            plan_gif(&video_info(480, 270, vec![]), &Recipe::default()),
            Err(PlanError::NotAnimation {
                kind: MediaKind::Video
            })
        );
    }

    #[test]
    fn size_target_splits_bits_and_keeps_small_audio() {
        // 10 s, 25 MB: 19.4 Mb/s total after the 3% allowance.
        let mut recipe = Recipe::default();
        recipe.video.target_bytes = Some(25_000_000);
        let plan = plan_video(
            &video_info(1920, 1080, vec![aac(96_000)]),
            &recipe,
            everything,
        )
        .unwrap();
        let target = plan.target.as_ref().unwrap();
        assert_eq!(
            target.audio_bit_rate, 96_000,
            "small AAC is copied and counted"
        );
        assert_eq!(target.video_bit_rate, 19_400_000 - 96_000);
        assert!(target.two_pass);
        assert_eq!(plan.scale_to, None, "plenty of bits for 1080p");
        assert!(
            plan.args.join(" ").contains("-b:v 19304k"),
            "{}",
            plan.args.join(" ")
        );
    }

    #[test]
    fn tight_targets_scale_down_and_impossible_ones_explain() {
        let mut recipe = Recipe::default();
        recipe.video.target_bytes = Some(1_000_000); // 10 s in 1 MB: 776 kb/s in total, a tenth for audio
        let plan = plan_video(
            &video_info(1920, 1080, vec![aac(128_000)]),
            &recipe,
            everything,
        )
        .unwrap();
        let (w, h) = plan.scale_to.unwrap();
        assert!(
            h < 1080 && w * 9 == h * 16,
            "scaled down keeping 16:9, got {w}x{h}"
        );
        assert_eq!(
            plan.audio,
            AudioPlan::Tracks {
                tracks: vec![AudioAction::Aac { bit_rate: 77_600 }]
            }
        );

        recipe.video.max_width = Some(1920); // the user's box wins over automatic scaling
        let plan = plan_video(&video_info(1920, 1080, vec![]), &recipe, everything).unwrap();
        assert_eq!(plan.scale_to, None);

        recipe.video.target_bytes = Some(40_000);
        let err = plan_video(
            &video_info(1920, 1080, vec![aac(128_000)]),
            &recipe,
            everything,
        )
        .unwrap_err();
        let PlanError::TargetTooSmall { minimum_bytes, .. } = err else {
            panic!("{err:?}")
        };
        assert!(minimum_bytes > 40_000);

        let mut no_duration = video_info(1920, 1080, vec![]);
        no_duration.duration_seconds = None;
        recipe.video.target_bytes = Some(5_000_000);
        assert_eq!(
            plan_video(&no_duration, &recipe, everything),
            Err(PlanError::UnknownDuration)
        );
    }

    #[test]
    fn two_pass_arguments() {
        let mut recipe = Recipe::default();
        recipe.video.target_bytes = Some(5_000_000);
        let plan = plan_video(
            &video_info(1280, 720, vec![aac(128_000)]),
            &recipe,
            everything,
        )
        .unwrap();
        let first = plan
            .ffmpeg_args(
                RateControl::Bitrate(2_000_000),
                &Pass::First { log: "p".into() },
            )
            .join(" ");
        assert_eq!(
            first,
            "-map 0:v:0 -map_metadata 0 -c:v libx264 -b:v 2000k -pass 1 -passlogfile p -pix_fmt yuv420p -an -f null"
        );
        let second = plan
            .ffmpeg_args(
                RateControl::Bitrate(1_500_000),
                &Pass::Second { log: "p".into() },
            )
            .join(" ");
        assert!(
            second.contains("-map 0:a:0")
                && second.contains("-b:v 1500k -pass 2 -passlogfile p")
                && second.ends_with("-f mp4")
        );

        recipe.video.codec = Some(VideoCodec::Hevc);
        let plan = plan_video(&video_info(1280, 720, vec![]), &recipe, everything).unwrap();
        let first = plan
            .ffmpeg_args(
                RateControl::Bitrate(1_000_000),
                &Pass::First { log: "p".into() },
            )
            .join(" ");
        assert!(first.contains("-x265-params pass=1:stats=p"), "{first}");

        recipe.video.encoder = EncoderPreference::Hardware;
        let plan = plan_video(&video_info(1280, 720, vec![]), &recipe, everything).unwrap();
        assert!(
            !plan.target.unwrap().two_pass,
            "hardware encoders run one pass"
        );
    }

    #[test]
    fn resolution_ladder() {
        // Plenty of bits: keep the size.
        assert_eq!(
            resolution_for_bit_rate((1920, 1080), 30.0, 5_000_000, VideoCodec::H264),
            None
        );
        // 1 Mb/s at 30 fps for H.264: 720p is under the floor, 540p fits.
        assert_eq!(
            resolution_for_bit_rate((1920, 1080), 30.0, 1_000_000, VideoCodec::H264),
            Some((960, 540))
        );
        // Portrait keeps its orientation.
        assert_eq!(
            resolution_for_bit_rate((1080, 1920), 30.0, 1_000_000, VideoCodec::H264),
            Some((540, 960))
        );
        // Nothing fits: the smallest rung.
        assert_eq!(
            resolution_for_bit_rate((1920, 1080), 30.0, 10_000, VideoCodec::H264),
            Some((426, 240))
        );
    }

    #[test]
    fn animated_webp_and_avif() {
        let mut recipe = Recipe::default();
        recipe.video.output = VideoOutput::Webp;
        let webp = plan_animated(&video_info(1920, 1080, vec![]), &recipe, everything).unwrap();
        assert_eq!(
            (webp.width, webp.height, webp.fps, webp.extension),
            (720, 404, 20, "webp")
        );
        assert!(
            webp.args
                .join(" ")
                .contains("-c:v libwebp_anim -quality 76")
        );

        let mut gif = video_info(301, 201, vec![]);
        gif.kind = MediaKind::Animation;
        recipe.video.output = VideoOutput::Avif;
        let avif = plan_animated(&gif, &recipe, |n| n == "libaom-av1").unwrap();
        assert_eq!(
            (avif.width, avif.height),
            (300, 200),
            "even sizes for 4:2:0"
        );
        assert!(avif.flatten_alpha);
        assert!(avif.args.join(" ").contains("overlay=shortest=1"));
        assert!(avif.args.contains(&"libaom-av1".to_owned()));

        let err = plan_animated(&gif, &recipe, |_| false).unwrap_err();
        assert!(err.to_string().contains("libsvtav1, libaom-av1"));
        recipe.video.output = VideoOutput::Gif;
        assert!(plan_animated(&gif, &recipe, everything).is_err());
    }

    #[test]
    fn gif_to_mp4_is_planned_as_video_with_even_size() {
        let mut gif = video_info(301, 201, vec![]);
        gif.kind = MediaKind::Animation;
        let mut recipe = Recipe::default();
        assert!(
            plan_video(&gif, &recipe, everything).is_err(),
            "GIFs stay GIF unless asked"
        );
        recipe.video.output = VideoOutput::Mp4;
        let plan = plan_video(&gif, &recipe, everything).unwrap();
        assert_eq!(plan.scale_to, Some((300, 200)));
        assert_eq!(plan.audio, AudioPlan::None);
    }

    #[test]
    fn document_plans_by_level() {
        let mut pdf = video_info(1, 1, vec![]);
        pdf.kind = MediaKind::Pdf;
        pdf.video = None;
        let plan = plan_document(&pdf, &Recipe::default()).unwrap();
        assert_eq!((plan.jpeg_quality, plan.max_image_side), (78, Some(2400)));
        let picture = plan.image_plan(ImageFormat::Png);
        assert_eq!(
            picture.encoding,
            ImageEncoding::Png {
                min_quality: 65,
                max_quality: 85
            }
        );
        assert_eq!(
            (picture.max_width, picture.max_height),
            (Some(2400), Some(2400))
        );

        let mut recipe = Recipe::default();
        recipe.quality = Quality::Highest;
        assert_eq!(plan_document(&pdf, &recipe).unwrap().max_image_side, None);
        recipe.image.max_width = Some(1200);
        assert_eq!(
            plan_document(&pdf, &recipe).unwrap().max_image_side,
            Some(1200)
        );
        assert!(plan_document(&video_info(1, 1, vec![]), &recipe).is_err());
    }

    #[test]
    fn trims_copy_on_keyframes_and_explain_re_encodes() {
        let info = video_info(1920, 1080, vec![aac(128_000)]);
        let keyframes = [0, 2_000, 4_000, 6_000];
        let mut recipe = Recipe::default();
        recipe.video.trim_only = true;
        recipe.clip = crate::recipe::Clip {
            start_ms: Some(4_010),
            duration_ms: Some(3_000),
        };
        let copy = plan_trim(&info, &recipe, &keyframes, "MOV", everything).unwrap();
        assert_eq!(
            (copy.method, copy.start_ms, copy.extension.as_str()),
            (TrimMethod::StreamCopy, 4_000, "mov")
        );
        assert!(
            copy.args.join(" ").contains("-c copy")
                && copy.args.ends_with(&["-f".into(), "mov".into()])
        );

        recipe.clip.start_ms = Some(5_000);
        let encode = plan_trim(&info, &recipe, &keyframes, "mp4", everything).unwrap();
        assert_eq!(encode.method, TrimMethod::Reencode);
        assert!(
            encode.reason.as_deref().unwrap().contains("4.000 s"),
            "{:?}",
            encode.reason
        );
        assert!(encode.video.is_some());

        recipe.video.snap_to_keyframe = true;
        let snapped = plan_trim(&info, &recipe, &keyframes, "mp4", everything).unwrap();
        assert_eq!(
            (snapped.method, snapped.start_ms, snapped.seek_ms),
            (TrimMethod::StreamCopy, 4_000, 4_001)
        );
        assert_eq!(
            snapped.duration_ms,
            Some(4_000),
            "the requested end (8 s) is kept"
        );

        let avi = plan_trim(&info, &recipe, &keyframes, "avi", everything).unwrap();
        assert_eq!(
            (avi.method, avi.extension.as_str()),
            (TrimMethod::Reencode, "mp4")
        );
    }

    #[test]
    fn size_targets_use_the_clip_length() {
        let mut recipe = Recipe::default();
        recipe.video.target_bytes = Some(5_000_000);
        let whole = plan_video(&video_info(1920, 1080, vec![]), &recipe, everything).unwrap();
        recipe.clip = crate::recipe::Clip {
            start_ms: Some(5_000),
            duration_ms: Some(2_500),
        };
        let clip = plan_video(&video_info(1920, 1080, vec![]), &recipe, everything).unwrap();
        let rate = |p: &VideoPlan| p.target.as_ref().unwrap().video_bit_rate;
        assert_eq!(
            rate(&clip),
            rate(&whole) * 4,
            "a quarter of the length gets four times the bit rate"
        );
    }

    #[test]
    fn ten_bit_hevc_stays_ten_bit() {
        let mut info = video_info(1920, 1080, vec![]);
        info.video.as_mut().unwrap().pixel_format = Some("yuv420p10le".into());
        let mut recipe = Recipe::default();
        recipe.video.codec = Some(VideoCodec::Hevc);
        assert_eq!(
            plan_video(&info, &recipe, everything).unwrap().pixel_format,
            "yuv420p10le"
        );
        recipe.video.codec = Some(VideoCodec::H264);
        assert_eq!(
            plan_video(&info, &recipe, everything).unwrap().pixel_format,
            "yuv420p"
        );
    }

    fn hdr_info(transfer: &str) -> MediaInfo {
        let mut info = video_info(3840, 2160, vec![]);
        let video = info.video.as_mut().unwrap();
        video.codec = "hevc".into();
        video.pixel_format = Some("yuv420p10le".into());
        video.color = ColorInfo {
            primaries: Some("bt2020".into()),
            transfer: Some(transfer.into()),
            matrix: Some("bt2020nc".into()),
            range: Some("tv".into()),
        };
        info
    }

    #[test]
    fn hdr_becomes_ten_bit_hevc_with_its_color_description() {
        for transfer in ["smpte2084", "arib-std-b67"] {
            let plan = plan_video(&hdr_info(transfer), &Recipe::default(), everything).unwrap();
            assert_eq!(plan.encoder, VideoEncoder::Libx265, "{transfer}");
            assert_eq!(plan.pixel_format, "yuv420p10le");
            assert_eq!(plan.encoder_label(), "libx265, HDR kept");
            let args = plan.args.join(" ");
            assert!(
                args.contains(&format!(
                    "-color_primaries bt2020 -color_trc {transfer} -colorspace bt2020nc -color_range tv"
                )),
                "{args}"
            );
        }
        // Hardware HEVC keeps 10 bits too.
        let mut recipe = Recipe::default();
        recipe.video.encoder = EncoderPreference::Hardware;
        let plan = plan_video(&hdr_info("arib-std-b67"), &recipe, everything).unwrap();
        assert_eq!(
            (plan.encoder, plan.pixel_format),
            (VideoEncoder::HevcVideoToolbox, "p010le")
        );
    }

    #[test]
    fn hdr_refuses_h264_when_asked_for() {
        let mut recipe = Recipe::default();
        recipe.video.codec = Some(VideoCodec::H264);
        let err = plan_video(&hdr_info("smpte2084"), &recipe, everything).unwrap_err();
        assert_eq!(err, PlanError::HdrNeedsHevc);
        assert!(err.to_string().contains("--codec hevc"));
    }

    #[test]
    fn sdr_color_tags_are_carried_over_and_missing_ones_left_out() {
        let mut info = video_info(1920, 1080, vec![]);
        info.video.as_mut().unwrap().color = ColorInfo {
            primaries: Some("bt709".into()),
            transfer: Some("bt709".into()),
            matrix: Some("bt709".into()),
            range: None,
        };
        let plan = plan_video(&info, &Recipe::default(), everything).unwrap();
        assert_eq!(plan.encoder, VideoEncoder::Libx264);
        assert_eq!(plan.encoder_label(), "libx264");
        let args = plan.args.join(" ");
        assert!(
            args.contains("-color_primaries bt709 -color_trc bt709 -colorspace bt709 -an"),
            "{args}"
        );
        let untagged = plan_video(
            &video_info(1920, 1080, vec![]),
            &Recipe::default(),
            everything,
        );
        assert!(!untagged.unwrap().args.join(" ").contains("-color"));
    }
}
