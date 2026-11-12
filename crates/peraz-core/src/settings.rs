//! Settings as people write them: command-line flags and recipe files share one shape, so a
//! recipe can be overridden flag by flag.
//!
//! A recipe file is TOML with the same keys as the `peraz compress` flags:
//!
//! ```toml
//! version = 1                      # recipe format version (required to be 1 for now)
//! description = "Small pictures for websites"
//! quality = "balanced"             # highest, high, balanced, small, smallest
//! format = "webp"                  # keep, jpeg, png, webp, avif, gif, mp4
//! max_width = 2000                 # pixels; never scales up
//! max_height = 2000
//! codec = "h264"                   # video codec: h264, hevc (default: h264, hevc for HDR)
//! encoder = "auto"                 # auto, software, hardware
//! no_audio = false
//! target_size = "25MB"             # SI units; videos only
//! fps = 12                         # GIF and animated WebP/AVIF frame rate
//! ```
//!
//! Every key except `version` is optional. Unknown keys are errors, so typos never pass silently.

use serde::{Deserialize, Serialize};

use crate::recipe::{
    Clip, EncoderPreference, GifRecipe, ImageFormat, ImageRecipe, Quality, Recipe, VideoCodec,
    VideoOutput, VideoRecipe,
};
use crate::units::parse_size;

/// The recipe file format version this build reads and writes.
pub const RECIPE_VERSION: u32 = 1;

/// A partial set of settings: from a recipe file, from command-line flags, or both merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<VideoCodec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<EncoderPreference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

fn current_version() -> u32 {
    RECIPE_VERSION
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: RECIPE_VERSION,
            description: None,
            quality: None,
            format: None,
            max_width: None,
            max_height: None,
            codec: None,
            encoder: None,
            no_audio: None,
            target_size: None,
            fps: None,
        }
    }
}

/// Why a recipe could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(pub String);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingsError {}

/// What `format` means for each kind of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    pub image: ImageFormat,
    pub video: VideoOutput,
}

impl std::str::FromStr for OutputFormat {
    type Err = SettingsError;

    /// | value | still images | videos        | GIFs          |
    /// |-------|--------------|---------------|---------------|
    /// | keep  | same format  | MP4           | GIF           |
    /// | jpeg  | JPEG         | MP4           | GIF           |
    /// | png   | PNG          | MP4           | GIF           |
    /// | webp  | WebP         | animated WebP | animated WebP |
    /// | avif  | AVIF         | animated AVIF | animated AVIF |
    /// | gif   | same format  | GIF           | GIF           |
    /// | mp4   | same format  | MP4           | MP4           |
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (image, video) = match s {
            "keep" => (ImageFormat::Keep, VideoOutput::Keep),
            "jpeg" | "jpg" => (ImageFormat::Jpeg, VideoOutput::Keep),
            "png" => (ImageFormat::Png, VideoOutput::Keep),
            "webp" => (ImageFormat::Webp, VideoOutput::Webp),
            "avif" => (ImageFormat::Avif, VideoOutput::Avif),
            "gif" => (ImageFormat::Keep, VideoOutput::Gif),
            "mp4" => (ImageFormat::Keep, VideoOutput::Mp4),
            other => {
                return Err(SettingsError(format!(
                    "unknown format `{other}`; expected one of: keep, jpeg, png, webp, avif, gif, mp4"
                )));
            }
        };
        Ok(Self { image, video })
    }
}

impl Settings {
    /// Parses and checks a recipe file.
    pub fn from_toml(text: &str) -> Result<Self, SettingsError> {
        let settings: Self =
            toml::from_str(text).map_err(|e| SettingsError(e.message().to_owned()))?;
        settings.check()?;
        Ok(settings)
    }

    /// Writes these settings as a recipe file.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Checks every value, so mistakes show up when a recipe is loaded rather than mid-batch.
    pub fn check(&self) -> Result<(), SettingsError> {
        if self.version != RECIPE_VERSION {
            return Err(SettingsError(format!(
                "recipe version {} is not supported; this Peraz reads version {RECIPE_VERSION}",
                self.version
            )));
        }
        if let Some(format) = &self.format {
            format.parse::<OutputFormat>()?;
        }
        if let Some(size) = &self.target_size {
            parse_size(size).map_err(|e| SettingsError(format!("target_size: {e}")))?;
        }
        if let Some(fps) = self.fps
            && !(1..=60).contains(&fps)
        {
            return Err(SettingsError(format!(
                "fps must be between 1 and 60, not {fps}"
            )));
        }
        for (key, value) in [
            ("max_width", self.max_width),
            ("max_height", self.max_height),
        ] {
            if value == Some(0) {
                return Err(SettingsError(format!("{key} must be at least 1")));
            }
        }
        Ok(())
    }

    /// These settings, with every unset key taken from `below`.
    #[must_use]
    pub fn over(self, below: &Self) -> Self {
        Self {
            version: RECIPE_VERSION,
            description: self.description.or_else(|| below.description.clone()),
            quality: self.quality.or(below.quality),
            format: self.format.or_else(|| below.format.clone()),
            max_width: self.max_width.or(below.max_width),
            max_height: self.max_height.or(below.max_height),
            codec: self.codec.or(below.codec),
            encoder: self.encoder.or(below.encoder),
            no_audio: self.no_audio.or(below.no_audio),
            target_size: self.target_size.or_else(|| below.target_size.clone()),
            fps: self.fps.or(below.fps),
        }
    }

    /// The recipe these settings describe; unset keys use Peraz's defaults. `start_ms` and
    /// `duration_ms` are per-run choices and never come from a recipe file.
    pub fn to_recipe(
        &self,
        start_ms: Option<u64>,
        duration_ms: Option<u64>,
    ) -> Result<Recipe, SettingsError> {
        self.check()?;
        let format: OutputFormat = self.format.as_deref().unwrap_or("keep").parse()?;
        let target_bytes = match &self.target_size {
            Some(size) => Some(parse_size(size).map_err(SettingsError)?),
            None => None,
        };
        Ok(Recipe {
            quality: self.quality.unwrap_or_default(),
            video: VideoRecipe {
                codec: self.codec,
                encoder: self.encoder.unwrap_or_default(),
                max_width: self.max_width,
                max_height: self.max_height,
                remove_audio: self.no_audio.unwrap_or(false),
                output: format.video,
                target_bytes,
                trim_only: false,
                snap_to_keyframe: false,
            },
            image: ImageRecipe {
                format: format.image,
                max_width: self.max_width,
                max_height: self.max_height,
            },
            gif: GifRecipe { fps: self.fps },
            clip: Clip {
                start_ms,
                duration_ms,
            },
        })
    }
}

/// A recipe that ships with Peraz.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinRecipe {
    pub name: &'static str,
    pub toml: &'static str,
}

/// Recipes that ship with Peraz. Users can add their own, or replace these by name.
pub const BUILTIN_RECIPES: &[BuiltinRecipe] = &[
    BuiltinRecipe {
        name: "web-images",
        toml: "version = 1\ndescription = \"Pictures for websites: WebP, at most 2000 px\"\nquality = \"balanced\"\nformat = \"webp\"\nmax_width = 2000\nmax_height = 2000\n",
    },
    BuiltinRecipe {
        name: "web-images-avif",
        toml: "version = 1\ndescription = \"Smallest pictures for modern browsers: AVIF, at most 2000 px\"\nquality = \"balanced\"\nformat = \"avif\"\nmax_width = 2000\nmax_height = 2000\n",
    },
    BuiltinRecipe {
        name: "photos-archive",
        toml: "version = 1\ndescription = \"Keep photos and videos close to the original, in their own formats\"\nquality = \"high\"\nformat = \"keep\"\n",
    },
    BuiltinRecipe {
        name: "small-video",
        toml: "version = 1\ndescription = \"Small HEVC videos, at most 1080p\"\nquality = \"small\"\ncodec = \"hevc\"\nmax_width = 1920\nmax_height = 1920\n",
    },
    BuiltinRecipe {
        name: "gif-preview",
        toml: "version = 1\ndescription = \"Short previews as GIF: 480 px, 12 fps\"\nquality = \"balanced\"\nformat = \"gif\"\nmax_width = 480\nfps = 12\n",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_recipes_are_valid() {
        for builtin in BUILTIN_RECIPES {
            let settings = Settings::from_toml(builtin.toml)
                .unwrap_or_else(|e| panic!("{}: {e}", builtin.name));
            assert!(
                settings.description.is_some(),
                "{} has a description",
                builtin.name
            );
            settings.to_recipe(None, None).unwrap();
        }
    }

    #[test]
    fn flags_override_recipes_key_by_key() {
        let recipe = Settings::from_toml(
            "version = 1\nquality = \"small\"\nformat = \"webp\"\nmax_width = 2000\n",
        )
        .unwrap();
        let flags = Settings {
            quality: Some(Quality::High),
            ..Settings::default()
        };
        let merged = flags.over(&recipe).to_recipe(None, None).unwrap();
        assert_eq!(merged.quality, Quality::High, "the flag wins");
        assert_eq!(
            merged.image.format,
            ImageFormat::Webp,
            "the recipe fills the rest"
        );
        assert_eq!(merged.video.output, VideoOutput::Webp);
        assert_eq!(merged.image.max_width, Some(2000));
        let defaults = Settings::default().to_recipe(None, None).unwrap();
        assert_eq!(defaults, Recipe::default());
    }

    #[test]
    fn mistakes_are_reported_when_loading() {
        let typo = Settings::from_toml("version = 1\nqualty = \"small\"\n").unwrap_err();
        assert!(typo.0.contains("qualty"), "{typo}");
        assert!(
            Settings::from_toml("version = 2\n")
                .unwrap_err()
                .0
                .contains("version 2")
        );
        assert!(
            Settings::from_toml("format = \"bmp\"\n")
                .unwrap_err()
                .0
                .contains("unknown format")
        );
        assert!(
            Settings::from_toml("target_size = \"25MiB\"\n")
                .unwrap_err()
                .0
                .contains("SI")
        );
        assert!(Settings::from_toml("quality = \"ultra\"\n").is_err());
        assert!(Settings::from_toml("fps = 0\n").is_err());
        assert_eq!(
            Settings::from_toml("").unwrap().version,
            RECIPE_VERSION,
            "version defaults to current"
        );
    }

    #[test]
    fn round_trips_through_toml() {
        let settings = Settings {
            description: Some("x".into()),
            quality: Some(Quality::Small),
            format: Some("gif".into()),
            fps: Some(10),
            target_size: Some("8MB".into()),
            ..Settings::default()
        };
        assert_eq!(Settings::from_toml(&settings.to_toml()).unwrap(), settings);
    }
}
