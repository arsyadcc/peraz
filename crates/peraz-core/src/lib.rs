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

pub use media::{Alpha, AudioTrack, MediaInfo, MediaKind, Rational, VideoTrack};
pub use plan::{
    AnimatedPlan, DocumentPlan, GifPlan, ImageDecoder, ImageEncoding, ImagePlan, Pass, PlanError,
    RateControl, SizeTarget, VideoPlan, plan_animated, plan_document, plan_gif, plan_image,
    plan_video,
};
pub use recipe::{
    EncoderPreference, GifRecipe, ImageFormat, ImageRecipe, Quality, Recipe, VideoCodec,
    VideoOutput, VideoRecipe,
};
pub use report::{DocumentStats, Job, KeepReason, Outcome, OutputTarget, Plan, Report};

/// Peraz version, taken from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_semver() {
        assert_eq!(VERSION.split('.').count(), 3);
    }
}
