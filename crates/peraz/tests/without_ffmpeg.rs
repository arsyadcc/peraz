//! `peraz` without FFmpeg: images, PDFs, and EPUBs still compress; anything else stops the run
//! with exit code 3 before any work. The inputs are made here, so these tests need nothing
//! installed and run on every platform.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const ENVIRONMENT: i32 = 3;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A folder with an empty `bin` (the only `PATH` entry, so no FFmpeg is found), a PNG photo,
/// and a text file.
fn setup(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-cli-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    image::RgbImage::from_fn(256, 256, |x, y| image::Rgb([byte(x), byte(y), byte(x ^ y)]))
        .save(dir.join("photo.png"))
        .unwrap();
    std::fs::write(dir.join("notes.txt"), "plain text, not media").unwrap();
    Dir(dir)
}

fn peraz(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_peraz"))
        .args(args)
        .current_dir(dir)
        .env("PATH", dir.join("bin"))
        .env_remove("PERAZ_FFMPEG")
        .env_remove("PERAZ_CJXL")
        .env("PERAZ_CONFIG_DIR", dir.join("config"))
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn images_compress_without_ffmpeg() {
    let dir = setup("images");
    let output = peraz(
        &dir.0,
        &[
            "compress",
            "--json",
            "--format",
            "jpeg",
            "--out-dir",
            "out",
            "photo.png",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ffmpeg"], Value::Null);
    assert_eq!(
        json["results"][0]["outcome"]["status"], "compressed",
        "{json}"
    );
    assert!(dir.0.join("out/photo-1.jpg").is_file());
}

#[test]
fn a_file_that_needs_ffmpeg_stops_the_run_before_any_work() {
    let dir = setup("needs");
    let output = peraz(
        &dir.0,
        &["compress", "--out-dir", "out", "photo.png", "notes.txt"],
    );
    assert_eq!(output.status.code(), Some(ENVIRONMENT));
    let message = stderr(&output);
    assert!(message.contains("ffmpeg not found"), "{message}");
    assert!(message.contains("notes.txt needs FFmpeg"), "{message}");
    assert!(!dir.0.join("out/photo-1.png").exists());
}

#[test]
fn jpeg_xl_without_cjxl_stops_the_run_before_any_work() {
    let dir = setup("jxl");
    let output = peraz(
        &dir.0,
        &[
            "compress",
            "--format",
            "jxl",
            "--out-dir",
            "out",
            "photo.png",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(ENVIRONMENT),
        "{}",
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(message.contains("--format jxl needs cjxl"), "{message}");
    assert!(message.contains("PERAZ_CJXL"), "{message}");
    assert!(!dir.0.join("out/photo-1.jxl").exists());
}

#[test]
fn an_explicit_ffmpeg_that_does_not_exist_is_never_ignored() {
    let dir = setup("explicit");
    let missing = dir.0.join("bin").join("ffmpeg-missing");
    let flag = peraz(
        &dir.0,
        &[
            "--ffmpeg",
            missing.to_str().unwrap(),
            "compress",
            "photo.png",
        ],
    );
    assert_eq!(flag.status.code(), Some(ENVIRONMENT), "{}", stderr(&flag));
    assert!(stderr(&flag).contains("--ffmpeg"), "{}", stderr(&flag));

    let env = Command::new(env!("CARGO_BIN_EXE_peraz"))
        .args(["compress", "photo.png"])
        .current_dir(&dir.0)
        .env("PATH", dir.0.join("bin"))
        .env("PERAZ_FFMPEG", &missing)
        .env("PERAZ_CONFIG_DIR", dir.0.join("config"))
        .output()
        .unwrap();
    assert_eq!(env.status.code(), Some(ENVIRONMENT), "{}", stderr(&env));
    assert!(stderr(&env).contains("PERAZ_FFMPEG"), "{}", stderr(&env));
    assert!(!dir.0.join("photo-1.png").exists());
}

#[test]
fn doctor_says_what_works_without_ffmpeg() {
    let dir = setup("doctor");
    let output = peraz(&dir.0, &["doctor"]);
    assert_eq!(output.status.code(), Some(ENVIRONMENT));
    assert!(
        stderr(&output).contains("Without FFmpeg, Peraz compresses only JPEG"),
        "{}",
        stderr(&output)
    );
}

/// The low 8 bits, for making test pictures.
fn byte(value: u32) -> u8 {
    u8::try_from(value % 256).unwrap()
}
