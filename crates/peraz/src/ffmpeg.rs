//! `peraz ffmpeg`: installs the FFmpeg build Peraz is tested with in `peraz-ffmpeg` next to the
//! executable, where Peraz looks for FFmpeg after `--ffmpeg` and `PERAZ_FFMPEG`.
//!
//! The build is a release of the Peraz repository (`ffmpeg-release.txt` names it); the download
//! is checked against the SHA-256 compiled into this binary before anything is written.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use peraz_engine::ffmpeg::{BUNDLED_DIR, Toolchain, bundled_dir};
use sha2::{Digest as _, Sha256};

use crate::exit;

const REPOSITORY: &str = "https://github.com/arsyadcc/peraz";
const RELEASE: &str = include_str!("ffmpeg-release.txt");
/// A file in the installed folder that names the release it came from.
const MARKER: &str = "RELEASE";
/// Far more than a build needs; stops a wrong download from filling memory.
const MAX_DOWNLOAD: u64 = 300_000_000;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Download the FFmpeg build Peraz is tested with into a peraz-ffmpeg folder next to peraz.
    Install(InstallArgs),
    /// Remove the peraz-ffmpeg folder next to peraz.
    Uninstall,
}

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Download and install the build again, even if it is already installed.
    #[arg(long)]
    force: bool,
}

pub fn run(command: &Command) -> u8 {
    let result = match command {
        Command::Install(args) => install(args.force),
        Command::Uninstall => uninstall(),
    };
    match result {
        Ok(()) => exit::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            exit::ENVIRONMENT
        }
    }
}

/// The pinned release: its tag, and the SHA-256 of the archive for each target.
#[derive(Debug)]
struct Release {
    tag: &'static str,
    checksums: Vec<(&'static str, &'static str)>,
}

impl Release {
    fn pinned() -> Self {
        let mut tag = "";
        let mut checksums = Vec::new();
        for line in RELEASE
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            match line.split_once(' ') {
                Some(("tag", value)) => tag = value,
                Some((target, sum)) => checksums.push((target, sum)),
                None => {}
            }
        }
        Self { tag, checksums }
    }

    fn checksum(&self, target: &str) -> Option<&'static str> {
        self.checksums
            .iter()
            .find(|(t, _)| *t == target)
            .map(|(_, sum)| *sum)
    }
}

/// The release target of this binary, as the release archives name it.
fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "aarch64"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "x86_64"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

fn install(force: bool) -> Result<(), String> {
    let release = Release::pinned();
    let (target, checksum) = target()
        .and_then(|t| Some((t, release.checksum(t)?)))
        .ok_or("there is no FFmpeg build for this platform: install FFmpeg yourself")?;
    let dir = bundled_dir().ok_or("cannot find the folder of the peraz executable")?;
    if !force && installed_release(&dir).as_deref() == Some(release.tag) {
        println!("{} is already installed in {}", release.tag, dir.display());
        return Ok(());
    }

    let name = format!("peraz-ffmpeg-{target}.tar.xz");
    let url = format!("{REPOSITORY}/releases/download/{}/{name}", release.tag);
    eprintln!("Downloading {url}");
    let archive = download(&url)?;
    if sha256(&archive) != checksum {
        return Err(format!(
            "{name} does not match its published checksum; nothing was installed"
        ));
    }

    // Unpacked next to the final folder, then moved into place: an interrupted install never
    // leaves a half-written peraz-ffmpeg behind.
    let parent = dir.parent().ok_or("the peraz executable has no folder")?;
    let staging = parent.join(format!(".{BUNDLED_DIR}-{}", std::process::id()));
    let result = unpack(&archive, &staging)
        .map_err(|e| format!("cannot write to {}: {e}", parent.display()))
        .and_then(|()| {
            let new = staging.join(BUNDLED_DIR);
            std::fs::write(new.join(MARKER), format!("{}\n", release.tag))
                .map_err(|e| e.to_string())?;
            let version = Toolchain::locate(Some(&new))
                .and_then(|tools| tools.build_info())
                .map_err(|e| format!("the downloaded FFmpeg does not run: {e}"))?
                .version;
            replace(&new, &dir, &staging)
                .map_err(|e| format!("cannot replace {}: {e}", dir.display()))?;
            Ok(version)
        });
    let _ = std::fs::remove_dir_all(&staging);
    let version = result?;
    println!(
        "Installed FFmpeg {version} ({}) in {}",
        release.tag,
        dir.display()
    );
    Ok(())
}

fn uninstall() -> Result<(), String> {
    let dir = bundled_dir().ok_or("cannot find the folder of the peraz executable")?;
    if !dir.is_dir() {
        println!("Nothing to remove: {} does not exist", dir.display());
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    println!("Removed {}", dir.display());
    Ok(())
}

/// The release recorded in an installed folder.
fn installed_release(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(MARKER)).ok()?;
    Some(text.trim().to_owned())
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .user_agent(concat!("peraz/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let failed = |e: ureq::Error| format!("cannot download {url}: {e}");
    agent
        .get(url)
        .call()
        .map_err(failed)?
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(failed)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// Unpacks a `.tar.xz` archive into a new folder.
fn unpack(archive: &[u8], into: &Path) -> std::io::Result<()> {
    let mut tar = Vec::new();
    lzma_rs::xz_decompress(&mut &archive[..], &mut tar)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let _ = std::fs::remove_dir_all(into);
    std::fs::create_dir_all(into)?;
    tar::Archive::new(&tar[..]).unpack(into)
}

/// Puts `new` where `dir` is, moving an existing `dir` into `staging` first.
fn replace(new: &Path, dir: &Path, staging: &Path) -> std::io::Result<()> {
    if dir.exists() {
        std::fs::rename(dir, staging.join("old"))?;
    }
    std::fs::rename(new, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_release_has_a_build_for_every_release_target() {
        let release = Release::pinned();
        assert!(release.tag.starts_with("ffmpeg-"), "{}", release.tag);
        for (_, sum) in &release.checksums {
            assert!(
                sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()),
                "{sum}"
            );
        }
        // The targets dist releases Peraz for.
        let Ok(config) = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist-workspace.toml"),
        ) else {
            return;
        };
        let line = config
            .lines()
            .find(|l| l.starts_with("targets = "))
            .unwrap();
        let mut targets: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        let mut pinned: Vec<&str> = release.checksums.iter().map(|(t, _)| *t).collect();
        targets.sort_unstable();
        pinned.sort_unstable();
        assert_eq!(targets, pinned);
    }

    #[test]
    fn this_platform_has_a_build() {
        if let Some(target) = target() {
            assert!(Release::pinned().checksum(target).is_some(), "{target}");
        }
    }

    #[test]
    fn unpacks_an_archive_with_its_permissions() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "peraz-ffmpeg/ffmpeg", &b"tool"[..])
            .unwrap();
        let tar = builder.into_inner().unwrap();
        let mut archive = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut archive).unwrap();

        let dir = std::env::temp_dir().join(format!("peraz-unpack-{}", std::process::id()));
        unpack(&archive, &dir).unwrap();
        let tool = dir.join("peraz-ffmpeg/ffmpeg");
        assert_eq!(std::fs::read(&tool).unwrap(), b"tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&tool).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        }
        assert!(unpack(b"not an archive", &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checksums_are_lowercase_hex() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn replaces_an_existing_folder() {
        let root = std::env::temp_dir().join(format!("peraz-replace-{}", std::process::id()));
        let (new, dir, staging) = (root.join("new"), root.join("dir"), root.join("staging"));
        for d in [&new, &dir, &staging] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(new.join("a"), "new").unwrap();
        std::fs::write(dir.join("a"), "old").unwrap();
        replace(&new, &dir, &staging).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "new");
        assert!(!new.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
