//! Peraz engine adapters.
//!
//! Turns the pure model in `peraz-core` into work on real files: locating FFmpeg, detecting what it
//! can do, probing media, and running compression jobs. FFmpeg always runs as a separate
//! process: never linked, so a crash or hang in FFmpeg cannot take Peraz down and a job is
//! cancelled by stopping the process.

pub mod batch;
pub mod compress;
pub mod epub;
mod error;
pub mod ffmpeg;
pub mod gif;
pub mod heif;
pub mod image;
pub mod jxl;
pub mod pdf;
pub mod probe;
mod score;

pub use error::EngineError;
pub use peraz_core::VERSION;
