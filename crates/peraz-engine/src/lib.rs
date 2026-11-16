//! Peraz engine adapters.
//!
//! Turns the pure model in `peraz-core` into work on real files: locating FFmpeg, detecting what it
//! can do, probing media, and running compression jobs. FFmpeg always runs as a separate
//! process: never linked, so a crash or hang in FFmpeg cannot take Peraz down and a job is
//! cancelled by stopping the process.

pub mod apng;
mod av1;
pub mod avif;
pub mod batch;
pub mod compress;
pub mod epub;
mod error;
pub mod exif;
pub mod ffmpeg;
mod fit;
pub mod gain_params;
pub mod gainmap;
pub mod gif;
pub mod hdr;
pub mod heic;
pub mod heif;
pub mod icon;
pub mod image;
pub mod jxl;
pub mod jxl_header;
pub mod lzw;
pub mod mark;
pub mod metadata;
pub mod pdf;
pub mod probe;
mod score;
pub mod svg;
pub mod tiff;
pub mod webp_animation;

pub use error::EngineError;
pub use peraz_core::VERSION;
