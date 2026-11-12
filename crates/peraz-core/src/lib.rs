//! Peraz domain model.
//!
//! This crate is pure: no I/O, no processes, no UI. It holds the model types shared by the
//! engine, the CLI, and future front ends.

pub mod media;
pub mod plan;
pub mod recipe;
pub mod report;
pub mod settings;
pub mod units;

pub use media::{Alpha, AudioTrack, ColorInfo, MediaInfo, MediaKind, Rational, VideoTrack};
pub use plan::{
    AnimatedPlan, DocumentPlan, GifPlan, ImageDecoder, ImageEncoding, ImagePlan, Pass, PlanError,
    RateControl, SizeTarget, TrimMethod, TrimPlan, VideoPlan, plan_animated, plan_document,
    plan_gif, plan_image, plan_trim, plan_video,
};
pub use recipe::{
    Clip, EncoderPreference, GifRecipe, ImageFormat, ImageRecipe, Quality, Recipe, VideoCodec,
    VideoOutput, VideoRecipe,
};
pub use report::{
    DocumentStats, ExistingOutput, Job, KeepReason, Outcome, OutputTarget, Plan, Report,
};

/// Peraz version, taken from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_semver() {
        // MAJOR.MINOR.PATCH, optionally followed by a pre-release or build suffix (0.1.0-alpha.1)
        let core = VERSION.split(['-', '+']).next().unwrap_or_default();
        let parts: Vec<&str> = core.split('.').collect();
        assert_eq!(parts.len(), 3, "{VERSION}");
        assert!(parts.iter().all(|p| p.parse::<u64>().is_ok()), "{VERSION}");
    }
}
