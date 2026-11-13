//! Jobs and reports: one input in, one honest account of what happened out.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::media::MediaKind;
use crate::plan::{AnimatedPlan, DocumentPlan, GifPlan, ImagePlan, TrimPlan, VideoPlan};
use crate::recipe::Recipe;

/// Where the result of a job goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// `<name>-<number>.<ext>` next to the input, with the first number not taken yet.
    NextToInput,
    /// `<name>-<number>.<ext>` in this folder, with the first number not taken yet.
    Folder(PathBuf),
    /// Exactly this file.
    File(PathBuf),
}

impl OutputTarget {
    /// The output path for `input`, given the extension the plan produces. Numbered targets use
    /// `number` (from 1); the caller picks the first one whose file does not exist yet.
    pub fn resolve(&self, input: &Path, extension: &str, number: u32) -> PathBuf {
        let name = || {
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let suffix = format!("-{number}.{extension}");
            // File systems allow 255 bytes per name: shorten a long stem to make room.
            let stem = truncate_bytes(&stem, MAX_NAME_BYTES.saturating_sub(suffix.len()));
            format!("{stem}{suffix}")
        };
        match self {
            Self::NextToInput => input.with_file_name(name()),
            Self::Folder(dir) => dir.join(name()),
            Self::File(path) => path.clone(),
        }
    }

    /// Whether the name carries a number, so an existing file is never replaced.
    pub fn is_numbered(&self) -> bool {
        !matches!(self, Self::File(_))
    }
}

/// The longest file name, in bytes, that common file systems accept.
pub const MAX_NAME_BYTES: usize = 255;

/// `text` cut to at most `max` bytes, on a character boundary.
pub fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// One file to compress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub input: PathBuf,
    pub output: OutputTarget,
    pub recipe: Recipe,
    /// What to do when the output file already exists.
    pub existing: ExistingOutput,
}

/// What to do when a job's output file already exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExistingOutput {
    /// Fail the job, so nothing is overwritten by accident.
    #[default]
    Fail,
    /// Replace the existing file.
    Replace,
    /// Skip the input: it was compressed before (used by watch folders).
    Skip,
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
    Trim(TrimPlan),
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

/// Which visual quality metric a score is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// VMAF (model `vmaf_v0.6.1`), 0–100, for videos.
    Vmaf,
    /// SSIMULACRA2, up to 100, for still images.
    Ssimulacra2,
}

/// The visual quality a score target asked for, and what the result measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Score {
    pub metric: Metric,
    /// The score asked for.
    pub target: u8,
    /// The score measured on the result, against the input (at the output's size).
    pub score: f64,
    /// Measured on samples of the video rather than the whole of it.
    pub sampled: bool,
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
    /// With a score target: the score asked for and the score measured.
    pub score: Option<Score>,
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
    fn long_names_are_shortened_to_fit_the_file_system() {
        // 251 bytes of stem: `-1.jpg` would make the name 257 bytes.
        let stem = "é".repeat(125) + "x";
        let output = OutputTarget::NextToInput.resolve(Path::new(&format!("{stem}.jpg")), "jpg", 1);
        let name = output.file_name().unwrap().to_str().unwrap();
        assert!(name.len() <= MAX_NAME_BYTES, "{}", name.len());
        assert!(name.ends_with("-1.jpg"));
        assert!(stem.starts_with(name.trim_end_matches("-1.jpg")));
        // Cut on a character boundary, never inside `é`.
        assert_eq!(truncate_bytes("éé", 3), "é");
        assert_eq!(truncate_bytes("short", 10), "short");
    }

    #[test]
    fn output_names_follow_the_plan_extension() {
        let input = Path::new("shots/photo.HEIC");
        assert_eq!(
            OutputTarget::NextToInput.resolve(input, "jpg", 1),
            PathBuf::from("shots/photo-1.jpg")
        );
        assert_eq!(
            OutputTarget::NextToInput.resolve(Path::new("clip.mov"), "mp4", 12),
            PathBuf::from("clip-12.mp4")
        );
        assert_eq!(
            OutputTarget::Folder("out".into()).resolve(input, "png", 2),
            PathBuf::from("out/photo-2.png")
        );
        assert_eq!(
            OutputTarget::File("x.bin".into()).resolve(input, "png", 3),
            PathBuf::from("x.bin")
        );
        assert!(OutputTarget::NextToInput.is_numbered());
        assert!(!OutputTarget::File("x.bin".into()).is_numbered());
    }
}
