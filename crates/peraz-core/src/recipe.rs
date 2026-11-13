//! Recipes: what the user wants, independent of any particular file.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A quality level. Each level maps to encoder-specific settings in the planner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Visually indistinguishable from the source for nearly all content.
    Highest,
    High,
    /// A good default: clearly smaller, differences hard to spot.
    #[default]
    Balanced,
    Small,
    /// Smallest files; visible artifacts are acceptable.
    Smallest,
}

/// Video codec of the output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    /// Plays everywhere.
    #[default]
    H264,
    /// Often smaller than H.264 at the same quality (most clearly on screen recordings and
    /// camera footage); plays on fewer devices.
    Hevc,
}

/// Which kind of encoder to prefer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPreference {
    /// Best compression first (software), hardware when no software encoder is available.
    #[default]
    Auto,
    /// Software encoders only: smallest files, slower.
    Software,
    /// Hardware encoders only: much faster, larger files at the same quality.
    Hardware,
}

/// Settings for video output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoRecipe {
    /// `None` chooses: H.264, or HEVC for HDR videos (H.264 output would lose the HDR).
    pub codec: Option<VideoCodec>,
    pub encoder: EncoderPreference,
    /// Bounding box for the displayed picture. Never upscales; aspect ratio is kept.
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub remove_audio: bool,
    /// Container/format of the output.
    pub output: VideoOutput,
    /// Fit the output in this many bytes (SI). Replaces the quality level for video.
    pub target_bytes: Option<u64>,
    /// Reach this VMAF score (0–100) with the smallest file: the encoder setting is searched on
    /// samples of the video. Replaces the quality level for video.
    pub target_vmaf: Option<u8>,
    /// Only cut the clip: copy the streams when the start is on a keyframe, re-encode otherwise.
    pub trim_only: bool,
    /// With `trim_only`, move the start back to the previous keyframe so the streams can be copied.
    pub snap_to_keyframe: bool,
}

/// What a video becomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoOutput {
    /// Videos become MP4, GIFs stay GIF.
    #[default]
    Keep,
    /// A compressed H.264/HEVC MP4 video (also from a GIF).
    Mp4,
    /// An animated GIF (silent, palette-based).
    Gif,
    /// An animated WebP (silent; keeps transparency).
    Webp,
    /// An animated AVIF (silent; AV1, the smallest).
    Avif,
}

/// Settings for animated GIF output (from a video or a GIF).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifRecipe {
    /// Frames per second; default depends on the quality level. Never above the source.
    pub fps: Option<u32>,
}

/// The part of a video or animation to use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    /// Start this many milliseconds into the source.
    pub start_ms: Option<u64>,
    /// Use at most this many milliseconds of the source.
    pub duration_ms: Option<u64>,
}

impl Clip {
    pub fn is_set(self) -> bool {
        self.start_ms.is_some() || self.duration_ms.is_some()
    }

    /// The playing time of the clip, given the source duration (seconds).
    #[allow(clippy::cast_precision_loss)] // milliseconds in f64 are exact far beyond any video length
    pub fn seconds(self, source: Option<f64>) -> Option<f64> {
        let start = self.start_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
        let rest = source.map(|d| (d - start).max(0.0));
        match (self.duration_ms.map(|ms| ms as f64 / 1000.0), rest) {
            (Some(want), Some(rest)) => Some(want.min(rest)),
            (Some(want), None) => Some(want),
            (None, rest) => rest,
        }
    }
}

/// Output format for still images.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// Same format as the input.
    #[default]
    Keep,
    Jpeg,
    Png,
    /// Lossy WebP: smaller than JPEG, supports transparency, plays in every current browser.
    Webp,
    /// AVIF (AV1): smallest files, supports transparency; slower to encode.
    Avif,
}

/// Settings for still images.
///
/// EXIF/XMP metadata is always removed (it can hold location and device details); the EXIF
/// orientation is applied to the pixels first, and the color profile is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecipe {
    pub format: ImageFormat,
    /// Bounding box for the displayed picture. Never upscales; aspect ratio is kept.
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    /// Reach this SSIMULACRA2 score (up to 100) with the smallest file: the encoder setting is
    /// searched. Replaces the quality level for still images.
    pub target_ssimulacra2: Option<u8>,
}

/// Everything that decides how a file is compressed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub quality: Quality,
    pub video: VideoRecipe,
    pub image: ImageRecipe,
    pub gif: GifRecipe,
    /// The part of the source to use (videos and animations).
    pub clip: Clip,
}

macro_rules! text_enum {
    ($ty:ty { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $ty {
            /// All accepted spellings, for help text and validation.
            pub const NAMES: &'static [&'static str] = &[$($text),+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = UnknownValue;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(UnknownValue { value: s.to_owned(), expected: Self::NAMES }),
                }
            }
        }
    };
}

text_enum!(Quality { Highest => "highest", High => "high", Balanced => "balanced", Small => "small", Smallest => "smallest" });
text_enum!(VideoCodec { H264 => "h264", Hevc => "hevc" });
text_enum!(EncoderPreference { Auto => "auto", Software => "software", Hardware => "hardware" });
text_enum!(VideoOutput { Keep => "keep", Mp4 => "mp4", Gif => "gif", Webp => "webp", Avif => "avif" });
text_enum!(ImageFormat { Keep => "keep", Jpeg => "jpeg", Png => "png", Webp => "webp", Avif => "avif" });

/// A string that is not one of an enum's accepted spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownValue {
    pub value: String,
    pub expected: &'static [&'static str],
}

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown value `{}`; expected one of: {}",
            self.value,
            self.expected.join(", ")
        )
    }
}

impl std::error::Error for UnknownValue {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        for name in Quality::NAMES {
            assert_eq!(name.parse::<Quality>().unwrap().as_str(), *name);
        }
        assert_eq!("hevc".parse::<VideoCodec>(), Ok(VideoCodec::Hevc));
        let err = "ultra".parse::<Quality>().unwrap_err();
        assert!(
            err.to_string()
                .contains("highest, high, balanced, small, smallest")
        );
    }

    #[test]
    fn serde_matches_text() {
        let json = serde_json::to_string(&EncoderPreference::Hardware).unwrap();
        assert_eq!(json, "\"hardware\"");
    }
}
