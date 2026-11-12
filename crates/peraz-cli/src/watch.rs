//! `peraz watch`: compress new files as they appear in folders.
//!
//! Folders are scanned once a second. A file is compressed once its size and modification time
//! have not changed for [`SETTLE`], so files still being copied or recorded are never read half
//! written. Polling instead of OS notifications works the same on every platform and on network
//! drives.
//!
//! Files that were already there when watching started are left alone (unless
//! `--include-existing`), and a file whose output already exists counts as done, so restarting a
//! watch never compresses anything twice. Peraz's own outputs, hidden files, and partial files
//! are ignored.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use peraz_core::{ExistingOutput, Job, OutputTarget, Report};
use peraz_engine::batch::Batch;
use serde::Serialize;

use crate::compress::{SettingsArgs, install_cancel_handler, print_human, recipe, run_parallel};
use crate::{SCHEMA_VERSION, exit, locate};

/// How long a file must stay unchanged before it is compressed.
const SETTLE: Duration = Duration::from_secs(2);
/// How often folders are scanned.
const INTERVAL: Duration = Duration::from_secs(1);

/// File extensions Peraz can compress.
const MEDIA: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "heic", "heif", "avif", "gif", "bmp", "tif", "tiff", "mp4",
    "mov", "m4v", "mkv", "webm", "avi", "pdf", "epub",
];

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Folders to watch.
    #[arg(required = true)]
    folders: Vec<PathBuf>,

    #[command(flatten)]
    settings: SettingsArgs,

    /// Write outputs into this folder (subfolders are mirrored with --recursive). Default: next to
    /// each input as `<name>-peraz.<ext>`.
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Also watch subfolders.
    #[arg(long)]
    recursive: bool,

    /// Also compress files that were already there when watching started (unless their output
    /// already exists).
    #[arg(long)]
    include_existing: bool,

    /// How many files to work on at once (default: one per logical core).
    #[arg(short, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,

    /// Print one JSON report per line for each file, instead of text.
    #[arg(long)]
    json: bool,
}

/// One line of `--json` output.
#[derive(Serialize)]
struct Line<'a> {
    schema_version: u32,
    #[serde(flatten)]
    report: &'a Report,
}

/// A file's identity for change detection.
type Stamp = (u64, Option<SystemTime>);

pub fn run(ffmpeg: Option<&Path>, args: &Args) -> u8 {
    for folder in &args.folders {
        if !folder.is_dir() {
            eprintln!("error: {} is not a folder", folder.display());
            return exit::USAGE;
        }
    }
    let (recipe, _) = match recipe(&args.settings) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::USAGE;
        }
    };
    let tools = match locate(ffmpeg) {
        Ok(tools) => tools,
        Err(code) => return code,
    };
    let caps = match tools.capabilities() {
        Ok(caps) => caps,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::ENVIRONMENT;
        }
    };
    let cancel = install_cancel_handler();
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let workers = args.jobs.map_or(cores, |n| n as usize).max(1);
    let out_dir = args
        .out_dir
        .as_ref()
        .map(|d| std::path::absolute(d).unwrap_or_else(|_| d.clone()));

    let mut tracker = Tracker::default();
    if !args.include_existing {
        tracker.mark_done(scan(&args.folders, args.recursive, out_dir.as_deref()));
    }
    if !args.json {
        let names: Vec<String> = args
            .folders
            .iter()
            .map(|f| f.display().to_string())
            .collect();
        eprintln!("watching {} (Ctrl-C to stop)", names.join(", "));
    }
    while !cancel.load(Ordering::SeqCst) {
        let files = tracker.ready(
            scan(&args.folders, args.recursive, out_dir.as_deref()),
            Instant::now(),
        );
        if !files.is_empty() {
            let batch = Batch::for_this_machine();
            let job_for = |input: &PathBuf| {
                let output = match &out_dir {
                    Some(dir) => {
                        let target = mirrored_dir(dir, &args.folders, input);
                        let _ = std::fs::create_dir_all(&target);
                        OutputTarget::Folder(target)
                    }
                    None => OutputTarget::NextToInput,
                };
                Job {
                    input: input.clone(),
                    output,
                    recipe: recipe.clone(),
                    existing: ExistingOutput::Skip,
                }
            };
            let reports = run_parallel(&files, workers.min(files.len()), true, |input| {
                peraz_engine::compress::compress_in(
                    &batch,
                    &tools,
                    &caps,
                    &job_for(input),
                    &mut |_| {},
                    cancel,
                )
            });
            print_reports(&reports, args.json);
        }
        sleep_unless_cancelled(INTERVAL, cancel);
    }
    exit::CANCELLED
}

fn print_reports(reports: &[Report], json: bool) {
    let mut out = std::io::stdout().lock();
    for report in reports {
        if json {
            let _ = serde_json::to_writer(
                &mut out,
                &Line {
                    schema_version: SCHEMA_VERSION,
                    report,
                },
            );
            let _ = writeln!(out);
        } else {
            print_human(report);
        }
    }
    let _ = out.flush();
}

/// Decides when a file is ready: unchanged for [`SETTLE`], and not compressed before.
#[derive(Debug, Default)]
struct Tracker {
    done: HashSet<(PathBuf, Stamp)>,
    waiting: HashMap<PathBuf, (Stamp, Instant)>,
}

impl Tracker {
    fn mark_done(&mut self, files: Vec<(PathBuf, Stamp)>) {
        self.done.extend(files);
    }

    /// The files from this scan that are ready now; they are marked done.
    fn ready(&mut self, seen: Vec<(PathBuf, Stamp)>, now: Instant) -> Vec<PathBuf> {
        let present: HashSet<PathBuf> = seen.iter().map(|(p, _)| p.clone()).collect();
        self.waiting.retain(|path, _| present.contains(path));
        let mut ready = Vec::new();
        for (path, stamp) in seen {
            if self.done.contains(&(path.clone(), stamp)) {
                continue;
            }
            match self.waiting.get(&path) {
                Some((previous, since)) if *previous == stamp => {
                    if now.duration_since(*since) >= SETTLE {
                        self.waiting.remove(&path);
                        self.done.insert((path.clone(), stamp));
                        ready.push(path);
                    }
                }
                _ => {
                    self.waiting.insert(path, (stamp, now));
                }
            }
        }
        ready
    }
}

fn sleep_unless_cancelled(duration: Duration, cancel: &std::sync::atomic::AtomicBool) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < duration && !cancel.load(Ordering::SeqCst) {
        std::thread::sleep(step);
        slept += step;
    }
}

/// Media files in the folders, with their size and modification time.
fn scan(folders: &[PathBuf], recursive: bool, out_dir: Option<&Path>) -> Vec<(PathBuf, Stamp)> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = folders.to_vec();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if is_ignored(&path) {
                continue;
            }
            if meta.is_dir() {
                let inside_out_dir = out_dir.is_some_and(|out| {
                    std::path::absolute(&path).is_ok_and(|p| p.starts_with(out))
                });
                if recursive && !inside_out_dir {
                    pending.push(path);
                }
            } else if meta.is_file() && is_media(&path) {
                found.push((path, (meta.len(), meta.modified().ok())));
            }
        }
    }
    found
}

/// Hidden files, partial files, and Peraz's own outputs.
fn is_ignored(path: &Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
        return true;
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    name.starts_with('.') || name.ends_with(".peraz-partial") || stem.ends_with("-peraz")
}

fn is_media(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| MEDIA.contains(&e.as_str()))
}

/// `out_dir` plus the input's folder relative to the watched folder that contains it.
fn mirrored_dir(out_dir: &Path, folders: &[PathBuf], input: &Path) -> PathBuf {
    let parent = input.parent().unwrap_or(Path::new(""));
    folders
        .iter()
        .find_map(|folder| parent.strip_prefix(folder).ok())
        .map_or_else(|| out_dir.to_path_buf(), |relative| out_dir.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_ready_once_they_stop_changing() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let file = |size: u64| vec![(PathBuf::from("in/a.mov"), (size, None))];
        let old = vec![(PathBuf::from("in/old.jpg"), (5, None))];
        tracker.mark_done(old.clone());
        assert!(
            tracker
                .ready([file(100), old.clone()].concat(), t0)
                .is_empty(),
            "just appeared"
        );
        assert!(
            tracker
                .ready(file(200), t0 + Duration::from_secs(1))
                .is_empty(),
            "still growing"
        );
        assert!(
            tracker
                .ready(file(200), t0 + Duration::from_secs(2))
                .is_empty(),
            "stable for 1 s"
        );
        assert_eq!(
            tracker.ready(file(200), t0 + Duration::from_secs(3)),
            [PathBuf::from("in/a.mov")]
        );
        assert!(
            tracker
                .ready(file(200), t0 + Duration::from_secs(9))
                .is_empty(),
            "never twice"
        );
        assert!(
            tracker.ready(old, t0 + Duration::from_secs(9)).is_empty(),
            "files present at start stay done"
        );
    }

    #[test]
    fn ignores_outputs_hidden_and_partial_files() {
        assert!(is_ignored(Path::new("a/clip-peraz.mp4")));
        assert!(is_ignored(Path::new("a/.DS_Store")));
        assert!(is_ignored(Path::new("a/.clip-peraz.mp4.peraz-partial")));
        assert!(!is_ignored(Path::new("a/clip.mp4")));
        assert!(is_media(Path::new("Shot.PNG")) && is_media(Path::new("book.epub")));
        assert!(!is_media(Path::new("notes.txt")));
    }

    #[test]
    fn subfolders_are_mirrored_in_the_output_folder() {
        let folders = vec![PathBuf::from("/in")];
        assert_eq!(
            mirrored_dir(Path::new("/out"), &folders, Path::new("/in/a/b/x.png")),
            PathBuf::from("/out/a/b")
        );
        assert_eq!(
            mirrored_dir(Path::new("/out"), &folders, Path::new("/in/x.png")),
            PathBuf::from("/out")
        );
    }

    #[test]
    fn scan_finds_media_recursively_and_skips_the_output_folder() {
        let root = std::env::temp_dir().join(format!("peraz-watch-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        for file in [
            "a.jpg",
            "sub/b.png",
            "out/c-peraz.png",
            "out/d.png",
            "notes.txt",
            "e-peraz.jpg",
        ] {
            std::fs::write(root.join(file), b"x").unwrap();
        }
        let out = std::path::absolute(root.join("out")).unwrap();
        let mut names: Vec<String> = scan(std::slice::from_ref(&root), true, Some(&out))
            .into_iter()
            .map(|(p, _)| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        assert_eq!(names, ["a.jpg", "sub/b.png"]);
        let flat = scan(std::slice::from_ref(&root), false, Some(&out));
        assert_eq!(flat.len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
