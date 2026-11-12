//! The planner: turns a file's [`MediaInfo`] and a [`Recipe`] into concrete encoder settings.
//!
//! Pure functions only, so every decision is unit-testable and the resulting FFmpeg arguments can
//! be golden-tested.

use serde::Serialize;

use crate::media::Alpha;
use crate::media::{AudioTrack, MediaInfo, MediaKind, Rational, VideoTrack};
use crate::recipe::{EncoderPreference, ImageFormat, Quality, Recipe, VideoCodec};

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
    /// Starting points from encoder documentation, to be calibrated with VMAF measurements on our
    /// own samples:
    /// - x264 CRF: 0–51, default 23, about 18 is visually lossless, +6 roughly halves the bit rate.
    /// - x265 CRF: same scale shifted; default 28 is comparable to x264's 23.
    /// - VideoToolbox `-q:v`: 1–100, higher is better (constant quality, Apple Silicon).
    fn quality_args(self, quality: Quality) -> Vec<String> {
        let level = quality as usize; // Highest = 0 … Smallest = 4
        let (flag, value) = match self {
            Self::Libx264 => ("-crf", [18, 20, 23, 26, 29][level]),
            Self::Libx265 => ("-crf", [22, 24, 27, 30, 33][level]),
            Self::H264VideoToolbox | Self::HevcVideoToolbox => {
                ("-q:v", [80, 70, 60, 50, 40][level])
            }
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
    pub audio: AudioPlan,
    /// FFmpeg arguments between the input and the output, for transparency in reports.
    pub args: Vec<String>,
}

/// Why a plan could not be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum PlanError {
    /// The file is not a video.
    NotVideo { kind: MediaKind },
    /// No usable encoder for the requested codec and preference.
    NoEncoder {
        codec: VideoCodec,
        tried: Vec<&'static str>,
    },
    /// The file is not a still image.
    NotImage { kind: MediaKind },
    /// The file cannot become an animated GIF.
    NotAnimation { kind: MediaKind },
    /// Peraz cannot read, or cannot write, this image format yet.
    UnsupportedImage { codec: String, hint: &'static str },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotVideo { kind } => write!(f, "not a video (detected: {kind:?})"),
            Self::NoEncoder { codec, tried } => write!(
                f,
                "no {codec} encoder available (needs one of: {}); run `peraz doctor`",
                tried.join(", ")
            ),
            Self::NotImage { kind } => write!(f, "not a still image (detected: {kind:?})"),
            Self::NotAnimation { kind } => {
                write!(f, "cannot make a GIF from this file (detected: {kind:?})")
            }
            Self::UnsupportedImage { codec, hint } => {
                write!(f, "{codec} images are not supported yet{hint}")
            }
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
    let (Some(video), MediaKind::Video) = (&info.video, info.kind) else {
        return Err(PlanError::NotVideo { kind: info.kind });
    };
    let candidates = VideoEncoder::candidates(recipe.video.codec, recipe.video.encoder);
    let encoder = candidates
        .iter()
        .copied()
        .find(|e| has_encoder(e.ffmpeg_name()))
        .ok_or_else(|| PlanError::NoEncoder {
            codec: recipe.video.codec,
            tried: candidates.iter().map(|e| e.ffmpeg_name()).collect(),
        })?;

    let scale_to = fit_within(
        video.display_size(),
        recipe.video.max_width,
        recipe.video.max_height,
    );
    let pixel_format = pixel_format(encoder, video);
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

    let mut args: Vec<String> = vec!["-map".into(), "0:v:0".into()];
    if let AudioPlan::Tracks { tracks } = &audio {
        for i in 0..tracks.len() {
            args.extend(["-map".into(), format!("0:a:{i}")]);
        }
    }
    args.extend(["-map_metadata".into(), "0".into()]);
    if let Some((w, h)) = scale_to {
        args.extend(["-vf".into(), format!("scale={w}:{h}:flags=lanczos")]);
    }
    args.extend(["-c:v".into(), encoder.ffmpeg_name().into()]);
    args.extend(encoder.quality_args(recipe.quality));
    if encoder.codec() == VideoCodec::Hevc {
        // Apple players only accept HEVC in MP4 with the `hvc1` tag.
        args.extend(["-tag:v".into(), "hvc1".into()]);
    }
    args.extend(["-pix_fmt".into(), pixel_format.into()]);
    match &audio {
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

    Ok(VideoPlan {
        encoder,
        scale_to,
        pixel_format,
        audio,
        args,
    })
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
/// Image codecs decoded through FFmpeg: HEIC/HEIF (HEVC) and AVIF (AV1) stills.
const FFMPEG_IMAGES: &[&str] = &["hevc", "av1"];

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
        converts_format: source != Some(format),
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
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// The output is a GIF made from something else (a video): kept even when larger.
    pub converts_format: bool,
}

/// Plans an animated GIF from a video (when the recipe asks for GIF) or from a GIF.
pub fn plan_gif(info: &MediaInfo, recipe: &Recipe) -> Result<GifPlan, PlanError> {
    let wants_gif = info.kind == MediaKind::Animation
        || (info.kind == MediaKind::Video
            && recipe.video.output == crate::recipe::VideoOutput::Gif);
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
        start_ms: recipe.gif.start_ms,
        duration_ms: recipe.gif.duration_ms,
        converts_format: info.kind != MediaKind::Animation,
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
            "-map 0:v:0 -map 0:a:0 -map_metadata 0 -c:v libx264 -crf 23 -pix_fmt yuv420p \
             -c:a:0 aac -b:a:0 128k -movflags +faststart -f mp4"
        );
    }

    #[test]
    fn golden_hevc_scaled_without_audio() {
        let mut recipe = Recipe::default();
        recipe.quality = Quality::Small;
        recipe.video.codec = VideoCodec::Hevc;
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
            "-map 0:v:0 -map_metadata 0 -vf scale=1280:720:flags=lanczos -c:v libx265 -crf 30 \
             -tag:v hvc1 -pix_fmt yuv420p -an -movflags +faststart -f mp4"
        );
    }

    #[test]
    fn auto_falls_back_to_hardware_and_explains_missing_encoders() {
        let only_vt = |name: &str| name.ends_with("videotoolbox");
        let plan = plan_video(&video_info(1280, 720, vec![]), &Recipe::default(), only_vt).unwrap();
        assert_eq!(plan.encoder, VideoEncoder::H264VideoToolbox);
        assert!(plan.args.join(" ").contains("-q:v 60"));

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
        let gif = plan_image(
            &image_info("gif", "gif", Alpha::Unknown),
            &Recipe::default(),
        );
        assert!(matches!(gif, Err(PlanError::UnsupportedImage { .. })));
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
        recipe.video.output = crate::recipe::VideoOutput::Gif;
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
    fn ten_bit_hevc_stays_ten_bit() {
        let mut info = video_info(1920, 1080, vec![]);
        info.video.as_mut().unwrap().pixel_format = Some("yuv420p10le".into());
        let mut recipe = Recipe::default();
        recipe.video.codec = VideoCodec::Hevc;
        assert_eq!(
            plan_video(&info, &recipe, everything).unwrap().pixel_format,
            "yuv420p10le"
        );
        recipe.video.codec = VideoCodec::H264;
        assert_eq!(
            plan_video(&info, &recipe, everything).unwrap().pixel_format,
            "yuv420p"
        );
    }
}
