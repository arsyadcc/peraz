use std::path::PathBuf;
use std::process::ExitStatus;

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A required external tool could not be found.
    #[error(
        "{tool} not found ({searched}). Install FFmpeg, or point Peraz to it with --ffmpeg or PERAZ_FFMPEG"
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
}
