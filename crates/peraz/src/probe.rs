//! `peraz probe`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use peraz_core::units::{format_bit_rate, format_bytes, format_duration};
use peraz_core::{Alpha, MediaInfo};
use peraz_engine::EngineError;
use serde::Serialize;

use crate::{exit, locate, print_json};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Files to inspect.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct ProbeResult<'a> {
    path: &'a Path,
    media: Option<MediaInfo>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Output<'a> {
    files: Vec<ProbeResult<'a>>,
}

pub fn run(ffmpeg: Option<&Path>, args: &Args) -> u8 {
    let tools = match locate(ffmpeg) {
        Ok(tools) => tools,
        Err(code) => return code,
    };
    let mut files = Vec::with_capacity(args.files.len());
    let mut code = exit::SUCCESS;
    for path in &args.files {
        let result = peraz_engine::probe::probe(&tools, path);
        if let Err(e) = &result {
            code = if e.is_environment() {
                exit::ENVIRONMENT
            } else {
                code.max(exit::INPUT_FAILED)
            };
        }
        if !args.json {
            print_human(path, &result);
        }
        let (media, error) = match result {
            Ok(info) => (Some(info), None),
            Err(e) => (None, Some(e.to_string())),
        };
        files.push(ProbeResult { path, media, error });
    }
    if args.json {
        print_json(Output { files });
    }
    code
}

fn print_human(path: &Path, result: &Result<MediaInfo, EngineError>) {
    let mut out = std::io::stdout().lock();
    let info = match result {
        Ok(info) => info,
        Err(e) => {
            let _ = out.flush();
            eprintln!("{}: error: {e}", path.display());
            return;
        }
    };
    let mut summary = vec![
        format!("{:?}", info.kind).to_lowercase(),
        format_bytes(info.size_bytes),
    ];
    if let Some(seconds) = info.duration_seconds {
        summary.push(format_duration(seconds));
    }
    if let Some(rate) = info.bit_rate {
        summary.push(format_bit_rate(rate));
    }
    summary.push(info.container.clone());
    let _ = writeln!(out, "{}\n  {}", path.display(), summary.join(" · "));

    if let Some(v) = &info.video {
        let (w, h) = v.display_size();
        let mut parts = vec![v.codec.clone(), format!("{w}×{h}")];
        if v.rotation != 0 {
            parts.push(format!("rotated {}°", v.rotation));
        }
        if let Some(fps) = v.frame_rate {
            parts.push(format!("{:.2} fps", fps.as_f64()).replace(".00 ", " "));
        }
        if let Some(frames) = v.frame_count.filter(|n| *n > 1) {
            parts.push(format!("{frames} frames"));
        }
        if let Some(pix) = &v.pixel_format {
            parts.push(pix.clone());
        }
        match v.color.transfer.as_deref() {
            Some("smpte2084") => parts.push("HDR (PQ)".to_owned()),
            Some("arib-std-b67") => parts.push("HDR (HLG)".to_owned()),
            _ => {}
        }
        parts.push(
            match v.alpha {
                Alpha::Present => "alpha",
                Alpha::Absent => "opaque",
                Alpha::Unknown => "alpha unknown",
            }
            .to_owned(),
        );
        let _ = writeln!(out, "  {}", parts.join(" "));
    }
    for a in &info.audio {
        let mut parts = vec![a.codec.clone(), format!("{} ch", a.channels)];
        if let Some(rate) = a.sample_rate {
            parts.push(format!("{} kHz", f64::from(rate) / 1000.0));
        }
        if let Some(rate) = a.bit_rate {
            parts.push(format_bit_rate(rate));
        }
        let _ = writeln!(out, "  {}", parts.join(" "));
    }
}
