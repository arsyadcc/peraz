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
    /// About 30–50% smaller than H.264 at the same quality; less universal playback.
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
    pub codec: VideoCodec,
    pub encoder: EncoderPreference,
    /// Bounding box for the displayed picture. Never upscales; aspect ratio is kept.
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub remove_audio: bool,
    /// Container/format of the output.
    pub output: VideoOutput,
}

/// What a video becomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoOutput {
    /// A compressed MP4 video.
    #[default]
    Mp4,
    /// An animated GIF (silent, palette-based).
    Gif,
}

/// Settings for animated GIF output (from a video or a GIF).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifRecipe {
    /// Frames per second; default depends on the quality level. Never above the source.
    pub fps: Option<u32>,
    /// Start this many milliseconds into the source.
    pub start_ms: Option<u64>,
    /// Use at most this many milliseconds of the source.
    pub duration_ms: Option<u64>,
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
}

/// Everything that decides how a file is compressed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub quality: Quality,
    pub video: VideoRecipe,
    pub image: ImageRecipe,
    pub gif: GifRecipe,
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
text_enum!(VideoOutput { Mp4 => "mp4", Gif => "gif" });
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
