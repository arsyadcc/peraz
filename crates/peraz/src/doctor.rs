//! `peraz doctor`.

use std::path::Path;

use peraz_engine::ffmpeg::{BuildInfo, BuildLicense, Capabilities, ToolSource, Toolchain};
use serde::Serialize;

use crate::{exit, locate, print_json};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    peraz: &'a str,
    toolchain: &'a Toolchain,
    /// Version and license of the FFmpeg build (`ffmpeg` in `compress --json` too).
    ffmpeg: &'a BuildInfo,
    hardware_video_encoders: Vec<HardwareEncoder<'a>>,
    hwaccels: &'a [String],
    encoders: Vec<&'a str>,
}

#[derive(Serialize)]
struct HardwareEncoder<'a> {
    name: &'a str,
    api: &'a str,
}

/// Software encoders Peraz cares about, with the license they impose on an FFmpeg build.
const NOTABLE_SOFTWARE_ENCODERS: &[(&str, &str)] = &[
    ("libx264", "GPL"),
    ("libx265", "GPL"),
    ("libopenh264", "BSD"),
    ("libsvtav1", "BSD"),
    ("libaom-av1", "BSD"),
    ("librav1e", "BSD"),
    ("libvpx-vp9", "BSD"),
    ("libwebp", "BSD"),
    ("libopus", "BSD"),
    ("aac", "LGPL"),
];

pub fn run(ffmpeg: Option<&Path>, args: &Args) -> u8 {
    let tools = match locate(ffmpeg) {
        Ok(tools) => tools,
        Err(code) => return code,
    };
    let (build, caps) = match tools
        .build_info()
        .and_then(|b| Ok((b, tools.capabilities()?)))
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::ENVIRONMENT;
        }
    };
    let report = Report {
        peraz: peraz_engine::VERSION,
        toolchain: &tools,
        ffmpeg: &build,
        hardware_video_encoders: caps
            .hardware_video_encoders()
            .map(|(name, api)| HardwareEncoder { name, api })
            .collect(),
        hwaccels: &caps.hwaccels,
        encoders: caps.encoders.keys().map(String::as_str).collect(),
    };
    if args.json {
        print_json(report);
    } else {
        print_human(&report, &caps);
    }
    exit::SUCCESS
}

fn print_human(report: &Report, caps: &Capabilities) {
    let source = match report.toolchain.source {
        ToolSource::Flag => "--ffmpeg",
        ToolSource::Env => "PERAZ_FFMPEG",
        ToolSource::Path => "PATH",
    };
    let license = match report.ffmpeg.license {
        BuildLicense::Lgpl => "LGPL",
        BuildLicense::Gpl => "GPL",
        BuildLicense::Nonfree => "nonfree (not redistributable)",
    };
    println!("peraz    {}", report.peraz);
    println!(
        "ffmpeg   {} (from {source})",
        report.toolchain.ffmpeg.display()
    );
    println!("ffprobe  {}", report.toolchain.ffprobe.display());
    println!(
        "build    {} · {license} · {} encoders",
        report.ffmpeg.version,
        caps.encoders.len()
    );
    println!(
        "hwaccel  {}",
        list_or_none(report.hwaccels.iter().map(String::as_str))
    );
    println!(
        "hardware {}",
        list_or_none(report.hardware_video_encoders.iter().map(|e| e.name))
    );
    println!(
        "software {}",
        list_or_none(
            NOTABLE_SOFTWARE_ENCODERS
                .iter()
                .filter(|(name, _)| caps.has_encoder(name))
                .map(|(name, lic)| format!("{name} ({lic})"))
        )
    );
    println!(
        "\nHardware encoders are compiled in; whether this machine can use them is checked on first use."
    );
}

fn list_or_none<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let items: Vec<String> = items.into_iter().map(|s| s.as_ref().to_owned()).collect();
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}
