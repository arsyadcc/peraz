//! `--cancel-on-stdin`: a program without a console (a Windows app) cancels a run by sending a
//! line `cancel` or by closing standard input. Either way the current video stops cleanly: exit
//! code 130, no partial file, no FFmpeg left running. Needs FFmpeg, and is skipped without it.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const CANCELLED: i32 = 130;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn peraz() -> Command {
    Command::new(env!("CARGO_BIN_EXE_peraz"))
}

/// The FFmpeg that `peraz` would use, if there is one.
fn ffmpeg() -> Option<PathBuf> {
    let output = peraz().args(["doctor", "--json"]).output().ok()?;
    let json: Value = serde_json::from_slice(&output.stdout).ok()?;
    json["toolchain"]["ffmpeg"].as_str().map(PathBuf::from)
}

/// A folder with `config/` and a 30-second 720p video that takes far longer than that to start
/// compressing, made here so the test needs no fixtures.
fn setup(test: &str, ffmpeg: &Path) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-cancel-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc2=size=1280x720:rate=30:duration=30")
        .args(["-c:v", "mpeg4", "-q:v", "2", "-pix_fmt", "yuv420p"])
        .arg(dir.join("clip.mp4"))
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not make the test video");
    Dir(dir)
}

/// Starts `peraz compress --events --cancel-on-stdin` on the video and waits for its first
/// progress event, so the encode is running.
fn start(dir: &Path) -> (Child, ChildStdin, BufReader<std::process::ChildStdout>) {
    let mut child = peraz()
        .args([
            "compress",
            "--events",
            "--cancel-on-stdin",
            "--out-dir",
            "out",
            "clip.mp4",
        ])
        .current_dir(dir)
        .env("PERAZ_CONFIG_DIR", dir.join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "no progress event");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "progress" {
            break;
        }
    }
    (child, stdin, stdout)
}

/// Waits for `peraz` to exit and checks what a clean cancel leaves behind.
fn check_cancelled(dir: &Path, mut child: Child, stdout: BufReader<std::process::ChildStdout>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("peraz did not stop within 60 seconds of the cancel");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(CANCELLED));

    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    let done = events
        .iter()
        .find(|event| event["event"] == "file_done")
        .expect("a file_done event");
    assert_eq!(done["report"]["outcome"]["status"], "cancelled", "{done}");
    assert_eq!(events.last().unwrap()["event"], "done");

    let left: Vec<_> = std::fs::read_dir(dir.join("out"))
        .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "partial output left behind: {left:?}");
    let running = processes_mentioning(dir);
    assert!(running.is_empty(), "still running: {running:?}");
}

/// The command lines of running processes that mention `dir`.
fn processes_mentioning(dir: &Path) -> Vec<String> {
    let output = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | ForEach-Object { $_.CommandLine }",
            ])
            .output()
    } else {
        Command::new("ps").args(["-A", "-o", "args="]).output()
    }
    .unwrap();
    let needle = dir.file_name().unwrap().to_string_lossy().into_owned();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&needle))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_cancel_line_stops_the_run_cleanly() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: needs FFmpeg");
        return;
    };
    let dir = setup("line", &ffmpeg);
    let (child, mut stdin, stdout) = start(&dir.0);
    stdin.write_all(b"progress?\ncancel\n").unwrap();
    stdin.flush().unwrap();
    check_cancelled(&dir.0, child, stdout);
    drop(stdin);
}

#[test]
fn closing_stdin_stops_the_run_cleanly() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: needs FFmpeg");
        return;
    };
    let dir = setup("closed", &ffmpeg);
    let (child, stdin, stdout) = start(&dir.0);
    drop(stdin);
    check_cancelled(&dir.0, child, stdout);
}
