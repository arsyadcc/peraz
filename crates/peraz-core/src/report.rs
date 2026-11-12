//! Jobs and reports: one input in, one honest account of what happened out.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::media::MediaKind;
use crate::plan::{AnimatedPlan, DocumentPlan, GifPlan, ImagePlan, VideoPlan};
use crate::recipe::Recipe;

/// Where the result of a job goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// `<name>-peraz.<ext>` next to the input.
    NextToInput,
    /// `<name>-peraz.<ext>` in this folder.
    Folder(PathBuf),
    /// Exactly this file.
    File(PathBuf),
}

impl OutputTarget {
    /// The output path for `input`, given the extension the plan produces.
    pub fn resolve(&self, input: &Path, extension: &str) -> PathBuf {
        let name = || {
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{stem}-peraz.{extension}")
        };
        match self {
            Self::NextToInput => input.with_file_name(name()),
            Self::Folder(dir) => dir.join(name()),
            Self::File(path) => path.clone(),
        }
    }
}

/// One file to compress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub input: PathBuf,
    pub output: OutputTarget,
    pub recipe: Recipe,
    /// Replace `output` if it already exists.
    pub overwrite: bool,
}

/// Why the original file was kept instead of the compressed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum KeepReason {
    /// The compressed file was not smaller than the input, so it was discarded.
    NotSmaller { compressed_bytes: u64 },
    /// The input already fits the requested size target.
    AlreadyWithinTarget { target_bytes: u64 },
}

/// The result of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Outcome {
    /// A smaller file was written to the output path.
    Compressed { output: PathBuf, output_bytes: u64 },
    /// Nothing was written; the original is the better file.
    KeptOriginal(KeepReason),
    /// The input is valid but Peraz cannot compress this kind of file yet.
    Skipped { message: String },
    /// Something went wrong; nothing was written.
    Failed { message: String },
    /// Stopped by the user (Ctrl-C or a termination signal); nothing was written.
    Cancelled,
}

/// The plan that was executed for one input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Plan {
    Video(VideoPlan),
    Image(ImagePlan),
    Gif(GifPlan),
    Animated(AnimatedPlan),
    Document(DocumentPlan),
}

/// What happened to the pictures inside a PDF or EPUB.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DocumentStats {
    /// Pictures found.
    pub images: u32,
    /// Pictures replaced by a smaller version.
    pub recompressed: u32,
    /// Pictures Peraz could process but whose new version was not smaller.
    pub kept: u32,
    /// Pictures left alone (formats or color spaces Peraz does not rewrite).
    pub skipped: u32,
}

/// What happened to one input.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub input: PathBuf,
    pub kind: Option<MediaKind>,
    pub input_bytes: Option<u64>,
    pub outcome: Outcome,
    /// The plan that was executed, when one was made.
    pub plan: Option<Plan>,
    /// What actually did the encoding, e.g. `libx264`, `mozjpeg`, `imagequant + oxipng`.
    pub encoder: Option<String>,
    /// For PDF and EPUB: what happened to the pictures inside.
    pub document: Option<DocumentStats>,
    pub elapsed_seconds: f64,
}

impl Report {
    /// Bytes saved, when a smaller file was written.
    pub fn saved_bytes(&self) -> Option<u64> {
        match (&self.outcome, self.input_bytes) {
            (Outcome::Compressed { output_bytes, .. }, Some(input)) => {
                Some(input.saturating_sub(*output_bytes))
            }
            _ => None,
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self.outcome, Outcome::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_names_follow_the_plan_extension() {
        let input = Path::new("shots/photo.HEIC");
        assert_eq!(
            OutputTarget::NextToInput.resolve(input, "jpg"),
            PathBuf::from("shots/photo-peraz.jpg")
        );
        assert_eq!(
            OutputTarget::NextToInput.resolve(Path::new("clip.mov"), "mp4"),
            PathBuf::from("clip-peraz.mp4")
        );
        assert_eq!(
            OutputTarget::Folder("out".into()).resolve(input, "png"),
            PathBuf::from("out/photo-peraz.png")
        );
        assert_eq!(
            OutputTarget::File("x.bin".into()).resolve(input, "png"),
            PathBuf::from("x.bin")
        );
    }
}
