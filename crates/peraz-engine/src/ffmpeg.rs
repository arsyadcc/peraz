//! Locating FFmpeg and detecting what it can do.
//!
//! Lookup order: `--ffmpeg`, then `PERAZ_FFMPEG`, then the bundled build in a `peraz-ffmpeg`
//! folder next to the `peraz` executable, then `PATH`. Encoders are detected from the
//! build, never assumed; a compiled-in hardware encoder still needs a trial encode to prove the
//! hardware exists.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Serialize;

use crate::EngineError;

/// Environment variable that points Peraz at a specific `ffmpeg` binary.
pub const FFMPEG_ENV: &str = "PERAZ_FFMPEG";

/// Folder next to the `peraz` executable that holds the bundled `ffmpeg` and `ffprobe`. It is a
/// folder of its own so that installing Peraz never puts an `ffmpeg` on `PATH`.
pub const BUNDLED_DIR: &str = "peraz-ffmpeg";

/// Where the `ffmpeg` binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    /// The `--ffmpeg` command-line flag.
    Flag,
    /// The `PERAZ_FFMPEG` environment variable.
    Env,
    /// The bundled build next to the `peraz` executable.
    Bundled,
    /// Found on `PATH`.
    Path,
}

/// The `ffmpeg` and `ffprobe` binaries Peraz will run.
#[derive(Debug, Clone, Serialize)]
pub struct Toolchain {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub source: ToolSource,
}

impl Toolchain {
    /// Finds FFmpeg using the process environment: `explicit` (the `--ffmpeg` flag), then
    /// `PERAZ_FFMPEG`, then the bundled build ([`bundled_dir`]), then `PATH`.
    pub fn locate(explicit: Option<&Path>) -> Result<Self, EngineError> {
        Self::locate_with(
            explicit,
            std::env::var_os(FFMPEG_ENV),
            bundled_dir().as_deref(),
            std::env::var_os("PATH").as_deref(),
        )
    }

    /// Same as [`Toolchain::locate`], with the environment and the bundled folder passed in.
    pub fn locate_with(
        explicit: Option<&Path>,
        env_ffmpeg: Option<OsString>,
        bundled: Option<&Path>,
        path_var: Option<&OsStr>,
    ) -> Result<Self, EngineError> {
        let bundled = bundled
            .map(|dir| dir.join(exe_name("ffmpeg")))
            .filter(|path| path.is_file());
        let (ffmpeg, source) = if let Some(path) = explicit {
            (existing_tool(path, "ffmpeg", "--ffmpeg")?, ToolSource::Flag)
        } else if let Some(path) = env_ffmpeg.filter(|v| !v.is_empty()) {
            (
                existing_tool(Path::new(&path), "ffmpeg", FFMPEG_ENV)?,
                ToolSource::Env,
            )
        } else if let Some(path) = bundled {
            (path, ToolSource::Bundled)
        } else {
            let found = find_on_path("ffmpeg", path_var).ok_or(EngineError::ToolNotFound {
                tool: "ffmpeg",
                searched: "PATH".to_owned(),
            })?;
            (found, ToolSource::Path)
        };

        let sibling = ffmpeg.with_file_name(exe_name("ffprobe"));
        let ffprobe = if sibling.is_file() {
            sibling
        } else {
            find_on_path("ffprobe", path_var).ok_or_else(|| EngineError::ToolNotFound {
                tool: "ffprobe",
                searched: format!("next to {} and on PATH", ffmpeg.display()),
            })?
        };
        Ok(Self {
            ffmpeg,
            ffprobe,
            source,
        })
    }

    /// Reads the FFmpeg version and build license.
    pub fn build_info(&self) -> Result<BuildInfo, EngineError> {
        let out = run(&self.ffmpeg, "ffmpeg", &["-version"])?;
        BuildInfo::parse(&out).ok_or(EngineError::BadOutput {
            tool: "ffmpeg",
            message: "no version line in `ffmpeg -version`".to_owned(),
        })
    }

    /// Lists compiled-in encoders and hardware acceleration methods.
    pub fn capabilities(&self) -> Result<Capabilities, EngineError> {
        let encoders = run(&self.ffmpeg, "ffmpeg", &["-hide_banner", "-encoders"])?;
        let hwaccels = run(&self.ffmpeg, "ffmpeg", &["-hide_banner", "-hwaccels"])?;
        let filters = run(&self.ffmpeg, "ffmpeg", &["-hide_banner", "-filters"])?;
        Ok(Capabilities {
            encoders: parse_encoders(&encoders),
            hwaccels: parse_hwaccels(&hwaccels),
            vmaf: has_filter(&filters, "libvmaf"),
        })
    }
}

/// The license an FFmpeg build is distributed under, from its `configure` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLicense {
    Lgpl,
    Gpl,
    /// Built with `--enable-nonfree`: not redistributable.
    Nonfree,
}

/// Version and license of an FFmpeg build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    pub version: String,
    pub license: BuildLicense,
}

impl BuildInfo {
    /// Parses the output of `ffmpeg -version`.
    pub fn parse(output: &str) -> Option<Self> {
        let version = output
            .lines()
            .find_map(|line| line.strip_prefix("ffmpeg version "))?
            .split_whitespace()
            .next()?
            .to_owned();
        let configuration = output
            .lines()
            .find_map(|line| line.strip_prefix("configuration:"))
            .unwrap_or_default();
        let has = |flag: &str| configuration.split_whitespace().any(|f| f == flag);
        let license = if has("--enable-nonfree") {
            BuildLicense::Nonfree
        } else if has("--enable-gpl") {
            BuildLicense::Gpl
        } else {
            BuildLicense::Lgpl
        };
        Some(Self { version, license })
    }
}

/// What an encoder produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderKind {
    Video,
    Audio,
    Subtitle,
}

/// One compiled-in encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Encoder {
    pub kind: EncoderKind,
    pub description: String,
    /// The hardware API behind this encoder, if any (e.g. `videotoolbox`, `nvenc`).
    pub hardware: Option<&'static str>,
}

/// Encoders and hardware acceleration methods compiled into an FFmpeg build.
///
/// Compiled in does not mean usable: an NVENC encoder is listed even on a machine without an
/// NVIDIA GPU. Usability is confirmed with a trial encode before first use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub encoders: BTreeMap<String, Encoder>,
    pub hwaccels: Vec<String>,
    /// The `libvmaf` filter is compiled in, which `--target-vmaf` needs.
    pub vmaf: bool,
}

impl Capabilities {
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.contains_key(name)
    }

    /// Hardware video encoders, by name.
    pub fn hardware_video_encoders(&self) -> impl Iterator<Item = (&str, &'static str)> {
        self.encoders.iter().filter_map(|(name, e)| {
            (e.kind == EncoderKind::Video).then_some(())?;
            Some((name.as_str(), e.hardware?))
        })
    }
}

/// Encoder name suffixes that identify hardware APIs.
const HARDWARE_SUFFIXES: &[&str] = &[
    "videotoolbox",
    "nvenc",
    "qsv",
    "amf",
    "vaapi",
    "vulkan",
    "d3d12va",
    "mf",
    "v4l2m2m",
    "mediacodec",
];

/// Whether `ffmpeg -filters` output lists the filter `name` (lines look like
/// ` .. libvmaf           VV->V      Calculate the VMAF between two video streams.`).
pub fn has_filter(output: &str, name: &str) -> bool {
    output
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(name))
}

/// Parses `ffmpeg -encoders` output.
pub fn parse_encoders(output: &str) -> BTreeMap<String, Encoder> {
    output
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            let kind = match flags.chars().next()? {
                'V' => EncoderKind::Video,
                'A' => EncoderKind::Audio,
                'S' => EncoderKind::Subtitle,
                _ => return None,
            };
            let description = parts.collect::<Vec<_>>().join(" ");
            let hardware = name
                .rsplit_once('_')
                .and_then(|(_, suffix)| HARDWARE_SUFFIXES.iter().find(|s| **s == suffix).copied());
            Some((
                name.to_owned(),
                Encoder {
                    kind,
                    description,
                    hardware,
                },
            ))
        })
        .collect()
}

/// Parses `ffmpeg -hwaccels` output.
pub fn parse_hwaccels(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|line| !line.starts_with("Hardware acceleration methods:"))
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Runs a tool and returns its stdout, mapping failures to [`EngineError`].
pub(crate) fn run(
    program: &Path,
    tool: &'static str,
    args: &[&str],
) -> Result<String, EngineError> {
    let output = command(program)
        .args(args)
        .output()
        .map_err(|source| EngineError::Spawn {
            path: program.to_owned(),
            source,
        })?;
    check(tool, output)
}

pub(crate) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

pub(crate) fn check(tool: &'static str, output: Output) -> Result<String, EngineError> {
    if output.status.success() {
        return String::from_utf8(output.stdout).map_err(|_| EngineError::BadOutput {
            tool,
            message: "output is not UTF-8".to_owned(),
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = error_line(&stderr);
    Err(EngineError::ToolFailed {
        tool,
        status: output.status,
        message,
    })
}

/// The line of FFmpeg's error output that says what went wrong: the last one that is not a
/// generic follow-up, without FFmpeg's `[component @ 0x…]` prefixes.
fn error_line(stderr: &str) -> String {
    /// Lines FFmpeg prints after the real error, which say nothing about it.
    const GENERIC: &[&str] = &[
        "Conversion failed!",
        "Terminating thread with return code",
        "Nothing was written into output file",
        "Error number",
        "Exiting normally",
    ];
    let clean = |line: &str| {
        let mut rest = line.trim();
        while let Some(after) = rest
            .strip_prefix('[')
            .and_then(|r| r.split_once("] "))
            .map(|(_, after)| after.trim_start())
        {
            rest = after;
        }
        rest.to_owned()
    };
    let lines: Vec<String> = stderr
        .lines()
        .map(clean)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|line| !GENERIC.iter().any(|g| line.contains(g)))
        .or_else(|| lines.last())
        .cloned()
        .unwrap_or_else(|| "no error message".to_owned())
}

/// The folder where a bundled build would be: [`BUNDLED_DIR`] next to the running executable,
/// after resolving symbolic links (Homebrew and other package managers link `peraz` into a `bin`
/// folder elsewhere).
pub fn bundled_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Some(exe.parent()?.join(BUNDLED_DIR))
}

fn existing_tool(path: &Path, tool: &'static str, origin: &str) -> Result<PathBuf, EngineError> {
    let path = if path.is_dir() {
        path.join(exe_name(tool))
    } else {
        path.to_owned()
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(EngineError::ToolNotFound {
            tool,
            searched: format!("{origin} points to {}", path.display()),
        })
    }
}

pub(crate) fn find_on_path(tool: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let name = exe_name(tool);
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_line_skips_prefixes_and_generic_follow_ups() {
        let stderr = "[vist#0:1/hevc @ 0x7a5] [dec:hevc @ 0x7a5] Invalid NAL unit size (8 > 4).\n\
                      [vist#0:1/hevc @ 0x7a5] [dec:hevc @ 0x7a5] Terminating thread with return \
                      code -1145393733 (Error number -1145393733 occurred)\n\
                      Conversion failed!\n";
        assert_eq!(super::error_line(stderr), "Invalid NAL unit size (8 > 4).");
        assert_eq!(
            super::error_line("[out#0/mp4 @ 0x1] Error closing file: No space left on device\n"),
            "Error closing file: No space left on device"
        );
        assert_eq!(
            super::error_line("Conversion failed!\n"),
            "Conversion failed!"
        );
        assert_eq!(super::error_line(""), "no error message");
    }

    use super::*;

    const ENCODERS: &str = "Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
 S..... ass                  ASS (Advanced SubStation Alpha) subtitle
";

    #[test]
    fn finds_filters_by_name() {
        let filters = "Filters:\n  T.. = Timeline support\n ------\n .. libvmaf           VV->V      Calculate the VMAF between two video streams.\n .. vmafmotion        V->V       Calculate the VMAF Motion score.\n";
        assert!(has_filter(filters, "libvmaf"));
        assert!(!has_filter(filters, "vmaf"));
        assert!(!has_filter(filters, "Calculate"));
    }

    #[test]
    fn parses_encoders_and_hardware_apis() {
        let caps = Capabilities {
            encoders: parse_encoders(ENCODERS),
            hwaccels: vec![],
            vmaf: false,
        };
        assert_eq!(caps.encoders.len(), 5);
        assert_eq!(caps.encoders["aac"].kind, EncoderKind::Audio);
        assert_eq!(caps.encoders["libx264"].hardware, None);
        assert!(caps.has_encoder("h264_videotoolbox"));
        let hw: Vec<_> = caps.hardware_video_encoders().collect();
        assert_eq!(
            hw,
            [
                ("h264_nvenc", "nvenc"),
                ("h264_videotoolbox", "videotoolbox")
            ]
        );
    }

    #[test]
    fn parses_hwaccels() {
        let out = "Hardware acceleration methods:\nvideotoolbox\nvulkan\n\n";
        assert_eq!(parse_hwaccels(out), ["videotoolbox", "vulkan"]);
        assert_eq!(parse_hwaccels(""), Vec::<String>::new());
    }

    #[test]
    fn parses_build_info() {
        let gpl = "ffmpeg version 8.0.1 Copyright (c) 2000-2025 the FFmpeg developers\n\
                   configuration: --prefix=/x --enable-gpl --enable-libx264\n";
        assert_eq!(
            BuildInfo::parse(gpl),
            Some(BuildInfo {
                version: "8.0.1".into(),
                license: BuildLicense::Gpl
            })
        );
        let lgpl = "ffmpeg version n7.1 Copyright\nconfiguration: --enable-gplv3-not-a-flag\n";
        assert_eq!(BuildInfo::parse(lgpl).unwrap().license, BuildLicense::Lgpl);
        let nonfree = "ffmpeg version 6.0\nconfiguration: --enable-gpl --enable-nonfree\n";
        assert_eq!(
            BuildInfo::parse(nonfree).unwrap().license,
            BuildLicense::Nonfree
        );
        assert_eq!(BuildInfo::parse("nothing"), None);
    }

    #[test]
    fn explicit_path_must_exist() {
        let err =
            Toolchain::locate_with(Some(Path::new("/definitely/not/ffmpeg")), None, None, None)
                .unwrap_err();
        assert!(err.is_environment());
        assert!(err.to_string().contains("--ffmpeg points to"));
    }

    #[test]
    fn missing_path_is_an_environment_error() {
        let err = Toolchain::locate_with(None, None, None, Some(OsStr::new(""))).unwrap_err();
        assert!(matches!(
            err,
            EngineError::ToolNotFound { tool: "ffmpeg", .. }
        ));
    }

    /// A folder with empty `ffmpeg` and `ffprobe` files, which is all lookup checks.
    fn fake_build(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peraz-lookup-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for tool in ["ffmpeg", "ffprobe"] {
            std::fs::write(dir.join(exe_name(tool)), b"").unwrap();
        }
        dir
    }

    #[test]
    fn bundled_build_comes_after_env_and_before_path() {
        let bundled = fake_build("bundled");
        let on_path = fake_build("path");
        let path_var = std::env::join_paths([&on_path]).unwrap();

        let found = Toolchain::locate_with(None, None, Some(&bundled), Some(&path_var)).unwrap();
        assert_eq!(found.source, ToolSource::Bundled);
        assert_eq!(found.ffmpeg, bundled.join(exe_name("ffmpeg")));
        assert_eq!(found.ffprobe, bundled.join(exe_name("ffprobe")));

        let env = Some(on_path.clone().into_os_string());
        let found = Toolchain::locate_with(None, env, Some(&bundled), Some(&path_var)).unwrap();
        assert_eq!(found.source, ToolSource::Env);

        // No bundled build: PATH, as before.
        let missing = bundled.join("missing");
        let found = Toolchain::locate_with(None, None, Some(&missing), Some(&path_var)).unwrap();
        assert_eq!(found.source, ToolSource::Path);

        std::fs::remove_dir_all(bundled).unwrap();
        std::fs::remove_dir_all(on_path).unwrap();
    }

    #[test]
    fn bundled_dir_is_next_to_the_executable() {
        let dir = bundled_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), BUNDLED_DIR);
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(dir.parent(), exe.parent());
    }
}
