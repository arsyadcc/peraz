//! Probe tests.
//!
//! `golden_*` tests parse ffprobe JSON snapshots in `tests/data/` (taken from the files that
//! `scripts/make-fixtures.sh` generates) and need nothing installed. `live_*` tests run the real
//! ffprobe against `./fixtures` and are skipped, with a note, when either is missing.

use std::path::{Path, PathBuf};

use peraz_core::{Alpha, MediaInfo, MediaKind, Rational};
use peraz_engine::ffmpeg::Toolchain;
use peraz_engine::probe::{parse_probe_json, probe};

fn golden(name: &str) -> MediaInfo {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(format!("{name}.json"));
    let json = std::fs::read_to_string(&path).unwrap();
    parse_probe_json(&json, 1234).unwrap()
}

#[test]
fn golden_video_with_audio() {
    let info = golden("video-1080p-audio.mp4");
    assert_eq!(info.kind, MediaKind::Video);
    assert_eq!(info.duration_seconds, Some(10.0));
    let video = info.video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.width, video.height),
        ("h264", 1920, 1080)
    );
    assert_eq!(video.frame_rate, Some(Rational { num: 30, den: 1 }));
    assert_eq!(video.frame_count, Some(300));
    assert_eq!(video.alpha, Alpha::Absent);
    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].channels, 2);
    assert_eq!(info.audio[0].sample_rate, Some(48_000));
}

#[test]
fn golden_video_without_audio() {
    let info = golden("video-720p-noaudio.mp4");
    assert_eq!(info.kind, MediaKind::Video);
    assert_eq!(info.audio.len(), 0);
}

#[test]
fn golden_hevc_4k60() {
    let video = golden("video-4k60-hevc.mp4").video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.width, video.height),
        ("hevc", 3840, 2160)
    );
    assert_eq!(video.frame_rate, Some(Rational { num: 60, den: 1 }));
}

#[test]
fn golden_png_alpha() {
    let opaque = golden("image-opaque.png");
    let alpha = golden("image-alpha.png");
    assert_eq!(opaque.kind, MediaKind::Image);
    assert_eq!(alpha.kind, MediaKind::Image);
    assert_eq!(opaque.video.unwrap().alpha, Alpha::Absent);
    let alpha = alpha.video.unwrap();
    assert_eq!(alpha.alpha, Alpha::Present);
    assert_eq!(alpha.frame_rate, None, "still images have no frame rate");
}

#[test]
fn golden_jpeg_is_a_still_image() {
    let info = golden("image-photo.jpg");
    assert_eq!(info.kind, MediaKind::Image);
    assert_eq!(info.duration_seconds, None);
    assert_eq!(info.bit_rate, None);
    let video = info.video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.width, video.height),
        ("mjpeg", 1600, 1200)
    );
}

#[test]
fn golden_heic_reports_full_tile_grid_size() {
    let info = golden("image-photo.heic");
    assert_eq!(info.kind, MediaKind::Image);
    let video = info.video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.width, video.height),
        ("hevc", 1600, 1200)
    );
    assert_eq!(video.alpha, Alpha::Unknown);
}

#[test]
fn golden_gif_is_an_animation() {
    let info = golden("animation.gif");
    assert_eq!(info.kind, MediaKind::Animation);
    let video = info.video.unwrap();
    assert_eq!(video.frame_count, Some(45));
    assert_eq!(video.frame_rate, Some(Rational { num: 15, den: 1 }));
    assert_eq!(
        video.alpha,
        Alpha::Unknown,
        "GIF decodes to bgra regardless of transparency"
    );
    assert_eq!(info.duration_seconds, Some(3.0));
}

fn live_setup() -> Option<(Toolchain, PathBuf)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    if !fixtures.join("video-1080p-audio.mp4").is_file() {
        eprintln!("skipped: run scripts/make-fixtures.sh to enable live probe tests");
        return None;
    }
    match Toolchain::locate(None) {
        Ok(tools) => Some((tools, fixtures)),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

#[test]
fn live_probe_matches_file_size_and_kind() {
    let Some((tools, fixtures)) = live_setup() else {
        return;
    };
    for (name, kind) in [
        ("video-1080p-audio.mp4", MediaKind::Video),
        ("image-alpha.png", MediaKind::Image),
        ("image-photo.jpg", MediaKind::Image),
        ("animation.gif", MediaKind::Animation),
    ] {
        let path = fixtures.join(name);
        let info = probe(&tools, &path).unwrap();
        assert_eq!(info.kind, kind, "{name}");
        assert_eq!(
            info.size_bytes,
            std::fs::metadata(&path).unwrap().len(),
            "{name}"
        );
    }
}

#[test]
fn live_probe_rejects_non_media_and_missing_files() {
    let Some((tools, fixtures)) = live_setup() else {
        return;
    };
    let missing = probe(&tools, &fixtures.join("does-not-exist.mp4")).unwrap_err();
    assert!(!missing.is_environment());
    let not_media = probe(&tools, &fixtures.join("book.epub")).unwrap_err();
    assert!(!not_media.is_environment());
    assert!(!not_media.to_string().contains("file:"), "{not_media}");
    // A PDF has no audio/video streams ffprobe understands.
    if fixtures.join("document.pdf").is_file() {
        let pdf = probe(&tools, &fixtures.join("document.pdf"));
        assert!(pdf.is_err() || pdf.unwrap().kind == MediaKind::Unknown);
    }
}

#[cfg(unix)] // `:` is not allowed in Windows file names
#[test]
fn live_probe_treats_protocol_like_names_as_local_files() {
    let Some((tools, fixtures)) = live_setup() else {
        return;
    };
    // Without the `file:` prefix, ffprobe would parse this name as the `concat` protocol.
    let dir = std::env::temp_dir().join(format!("peraz-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tricky = dir.join("concat:a.mp4|b.mp4");
    std::fs::copy(fixtures.join("video-720p-noaudio.mp4"), &tricky).unwrap();
    let result = probe(&tools, &tricky);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(result.unwrap().kind, MediaKind::Video);
}
