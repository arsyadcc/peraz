use std::path::PathBuf;
use std::process::ExitStatus;

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A required external tool could not be found.
    #[error(
        "{tool} not found ({searched}). Run `peraz ffmpeg install` to download the build Peraz is tested with, install FFmpeg yourself, or point Peraz to it with --ffmpeg or PERAZ_FFMPEG"
    )]
    ToolNotFound {
        tool: &'static str,
        searched: String,
    },

    /// A tool exists but could not be started.
    #[error("could not run {path}: {source}", path = path.display())]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A tool ran and reported failure.
    #[error("{tool}: {message}")]
    ToolFailed {
        tool: &'static str,
        status: ExitStatus,
        message: String,
    },

    /// `ffprobe` could not read the file: it is damaged, empty, or not a media file.
    #[error("not a media file Peraz can read ({message})")]
    NotMedia { message: String },

    /// A tool produced output Peraz does not understand.
    #[error("unexpected output from {tool}: {message}")]
    BadOutput { tool: &'static str, message: String },

    /// An input file could not be read.
    #[error("cannot read {path}: {source}", path = path.display())]
    Input {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl EngineError {
    /// True when the problem is the environment (missing or broken tools) rather than one input.
    pub fn is_environment(&self) -> bool {
        matches!(self, Self::ToolNotFound { .. } | Self::Spawn { .. })
    }

    /// What kind of failure this is, for a file that fails because of it.
    pub fn fail_reason(&self) -> peraz_core::FailReason {
        use peraz_core::FailReason;
        match self {
            Self::Input { .. } => FailReason::CannotRead,
            Self::NotMedia { .. } => FailReason::CannotDecode,
            Self::ToolNotFound { .. } => FailReason::NeedsFfmpeg,
            Self::Spawn { .. } | Self::ToolFailed { .. } | Self::BadOutput { .. } => {
                FailReason::Other
            }
        }
    }
}
