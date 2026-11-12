//! Probe tests.
//!
//! `golden_*` tests parse ffprobe JSON snapshots in `tests/data/` (taken from the files that
//! `scripts/make-fixtures.sh` generates) and need nothing installed. `live_*` tests run the real
//! ffprobe against `./fixtures` and are skipped, with a note, when either is missing.

use std::path::{Path, PathBuf};

use peraz_core::{Alpha, ColorInfo, MediaInfo, MediaKind, Rational};
use peraz_engine::ffmpeg::Toolchain;
use peraz_engine::probe::{parse_probe_json, probe, probe_without_ffmpeg};

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
fn golden_hdr10_color_description() {
    let video = golden("video-hdr10-hevc.mp4").video.unwrap();
    assert_eq!(video.pixel_format.as_deref(), Some("yuv420p10le"));
    assert_eq!(
        video.color,
        ColorInfo {
            primaries: Some("bt2020".into()),
            transfer: Some("smpte2084".into()),
            matrix: Some("bt2020nc".into()),
            range: Some("tv".into()),
        }
    );
    assert!(video.color.is_hdr());
    // An untagged file reports `unknown`, which is not carried over.
    let sdr = golden("video-4k60-hevc.mp4").video.unwrap();
    assert_eq!(sdr.color.primaries, None);
    assert!(!sdr.color.is_hdr());
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
    let text = std::env::temp_dir().join(format!("peraz-notes-{}.txt", std::process::id()));
    std::fs::write(&text, "plain text, not media").unwrap();
    let not_media = probe(&tools, &text).unwrap_err();
    std::fs::remove_file(&text).unwrap();
    assert!(!not_media.is_environment());
    assert!(!not_media.to_string().contains("file:"), "{not_media}");
    // Documents are recognized from their first bytes.
    assert_eq!(
        probe(&tools, &fixtures.join("book.epub")).unwrap().kind,
        MediaKind::Epub
    );
    if fixtures.join("document.pdf").is_file() {
        assert_eq!(
            probe(&tools, &fixtures.join("document.pdf")).unwrap().kind,
            MediaKind::Pdf
        );
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

/// A fresh, empty folder for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("peraz-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn without_ffmpeg_reads_still_images_and_documents() {
    let dir = temp_dir("probe-without-ffmpeg");
    let opaque =
        image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([byte(x * 4), byte(y * 5), 90]));
    let clear = image::RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 128]));
    let files = [
        ("photo.jpg", "mjpeg", false),
        ("photo.png", "png", false),
        ("photo.webp", "webp", false),
        ("photo.bmp", "bmp", false),
        ("photo.tiff", "tiff", false),
        ("clear.png", "png", true),
    ];
    for (name, codec, transparent) in files {
        let path = dir.join(name);
        if transparent {
            clear.save(&path).unwrap();
        } else {
            opaque.save(&path).unwrap();
        }
        let info = probe_without_ffmpeg(&path)
            .unwrap()
            .unwrap_or_else(|| panic!("{name}"));
        assert_eq!(info.kind, MediaKind::Image, "{name}");
        assert_eq!(info.size_bytes, std::fs::metadata(&path).unwrap().len());
        let video = info.video.unwrap();
        assert_eq!(
            (video.codec.as_str(), video.width, video.height),
            (codec, 64, 48),
            "{name}"
        );
        if codec == "png" {
            let alpha = if transparent {
                Alpha::Present
            } else {
                Alpha::Absent
            };
            assert_eq!(video.alpha, alpha, "{name}");
        }
    }

    let pdf = dir.join("doc.pdf");
    std::fs::write(&pdf, b"%PDF-1.7\n").unwrap();
    assert_eq!(
        probe_without_ffmpeg(&pdf).unwrap().unwrap().kind,
        MediaKind::Pdf
    );
    assert!(probe_without_ffmpeg(&dir.join("missing.png")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn without_ffmpeg_leaves_other_files_to_ffmpeg() {
    let dir = temp_dir("probe-needs-ffmpeg");
    // An animated PNG: an `acTL` chunk right after the header.
    let mut png = Vec::new();
    image::RgbImage::new(8, 8)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let ihdr_end = 8 + 4 + 4 + 13 + 4;
    let mut actl = vec![0, 0, 0, 8];
    actl.extend_from_slice(b"acTL");
    actl.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    png.splice(ihdr_end..ihdr_end, actl);
    // An animated WebP: a `VP8X` chunk with the animation flag.
    let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x02".to_vec();
    webp.resize(64, 0);
    let files: [(&str, &[u8]); 4] = [
        ("animated.png", &png),
        ("animated.webp", &webp),
        ("still.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;"),
        ("notes.txt", b"plain text, not media"),
    ];
    for (name, bytes) in files {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        assert!(probe_without_ffmpeg(&path).unwrap().is_none(), "{name}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn live_without_ffmpeg_agrees_with_ffprobe() {
    let Some((tools, fixtures)) = live_setup() else {
        return;
    };
    for name in ["image-photo.jpg", "image-alpha.png", "image-opaque.png"] {
        let path = fixtures.join(name);
        let ours = probe_without_ffmpeg(&path).unwrap().unwrap();
        let theirs = probe(&tools, &path).unwrap();
        assert_eq!(ours.kind, theirs.kind, "{name}");
        let (ours, theirs) = (ours.video.unwrap(), theirs.video.unwrap());
        assert_eq!(
            (&ours.codec, ours.width, ours.height),
            (&theirs.codec, theirs.width, theirs.height),
            "{name}"
        );
        if theirs.alpha != Alpha::Unknown {
            assert_eq!(ours.alpha, theirs.alpha, "{name}");
        }
    }
    for name in [
        "video-720p-noaudio.mp4",
        "animation.gif",
        "image-photo.heic",
    ] {
        assert!(
            probe_without_ffmpeg(&fixtures.join(name))
                .unwrap()
                .is_none(),
            "{name}"
        );
    }
}

/// The low 8 bits, for making test pictures.
fn byte(value: u32) -> u8 {
    u8::try_from(value % 256).unwrap()
}
