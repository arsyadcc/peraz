//! End-to-end compression tests against `./fixtures`, skipped (with a note) when the fixtures or
//! FFmpeg are missing. Run `scripts/make-fixtures.sh` first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use peraz_core::{
    ImageFormat, Job, KeepReason, MediaKind, Outcome, OutputTarget, Quality, Recipe, VideoOutput,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::{Capabilities, Toolchain};
use peraz_engine::probe::probe;

struct Env {
    tools: Toolchain,
    caps: Capabilities,
    fixtures: PathBuf,
    out: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.out);
    }
}

fn setup(test: &str) -> Option<Env> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    if !fixtures.join("video-720p-noaudio.mp4").is_file() {
        eprintln!("skipped: run scripts/make-fixtures.sh to enable compression tests");
        return None;
    }
    let tools = match Toolchain::locate(None) {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("skipped: {e}");
            return None;
        }
    };
    let caps = tools.capabilities().unwrap();
    if !caps.has_encoder("libx264") {
        eprintln!("skipped: this FFmpeg has no libx264");
        return None;
    }
    let out = std::env::temp_dir().join(format!("peraz-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    Some(Env {
        tools,
        caps,
        fixtures,
        out,
    })
}

fn job(input: PathBuf, output: PathBuf, quality: Quality) -> Job {
    Job {
        input,
        output: OutputTarget::File(output),
        recipe: Recipe {
            quality,
            ..Recipe::default()
        },
        existing: peraz_core::ExistingOutput::Fail,
    }
}

fn no_progress(_: f64) {}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

#[test]
fn compresses_video_and_writes_only_the_final_file() {
    let Some(env) = setup("basic") else { return };
    let output = env.out.join("clip-peraz.mp4");
    let job = job(
        env.fixtures.join("video-720p-noaudio.mp4"),
        output.clone(),
        Quality::Small,
    );
    let mut last = 0.0;
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut |f| last = f,
        &NOT_CANCELLED,
    );

    let Outcome::Compressed { output_bytes, .. } = report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(output_bytes < report.input_bytes.unwrap());
    assert!((last - 1.0).abs() < f64::EPSILON, "progress ends at 100%");
    let info = probe(&env.tools, &output).unwrap();
    assert_eq!(info.kind, MediaKind::Video);
    assert_eq!(info.video.unwrap().codec, "h264");
    let leftovers: Vec<_> = std::fs::read_dir(&env.out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, [std::ffi::OsString::from("clip-peraz.mp4")]);
}

#[test]
fn keeps_the_original_when_compression_does_not_help() {
    let Some(env) = setup("keep") else { return };
    // Re-encode an already compressed file at the highest quality: it cannot get smaller.
    let first = env.out.join("small.mp4");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(
            env.fixtures.join("video-720p-noaudio.mp4"),
            first.clone(),
            Quality::Smallest,
        ),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );

    let second = env.out.join("again.mp4");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(first, second.clone(), Quality::Highest),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );
    assert!(!second.exists());
}

#[test]
fn refuses_to_overwrite_existing_files() {
    let Some(env) = setup("guards") else { return };
    let existing = env.out.join("exists.mp4");
    std::fs::write(&existing, b"keep me").unwrap();
    let report = compress(
        &env.tools,
        &env.caps,
        &job(
            env.fixtures.join("video-720p-noaudio.mp4"),
            existing.clone(),
            Quality::Balanced,
        ),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(report.is_failure());
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
}

#[test]
fn cancelling_stops_ffmpeg_and_leaves_nothing_behind() {
    let Some(env) = setup("cancel") else { return };
    let output = env.out.join("cancelled.mp4");
    let job = job(
        env.fixtures.join("video-1080p-audio.mp4"),
        output.clone(),
        Quality::Highest,
    );
    let cancel = AtomicBool::new(false);
    // Cancel as soon as encoding reports progress.
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut |_| cancel.store(true, Ordering::SeqCst),
        &cancel,
    );
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(
        std::fs::read_dir(&env.out).unwrap().count(),
        0,
        "no output or partial file"
    );

    let report = compress(&env.tools, &env.caps, &job, &mut no_progress, &cancel);
    assert_eq!(
        report.outcome,
        Outcome::Cancelled,
        "an already-cancelled job does not start"
    );
}

#[test]
fn compresses_jpeg_with_mozjpeg() {
    let Some(env) = setup("jpeg") else { return };
    let output = env.out.join("photo.jpg");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(
            env.fixtures.join("image-photo.jpg"),
            output.clone(),
            Quality::Balanced,
        ),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    assert!(report.encoder.as_deref().unwrap().starts_with("mozjpeg"));
    let info = probe(&env.tools, &output).unwrap();
    assert_eq!(info.kind, MediaKind::Image);
    let picture = info.video.unwrap();
    assert_eq!(
        (picture.codec.as_str(), picture.width, picture.height),
        ("mjpeg", 1600, 1200)
    );
}

#[test]
fn quantizes_png_and_keeps_transparency() {
    let Some(env) = setup("png") else { return };
    let output = env.out.join("alpha.png");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(
            env.fixtures.join("image-alpha.png"),
            output.clone(),
            Quality::Balanced,
        ),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let decoded = image::open(&output).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1200, 900));
    assert!(decoded.color().has_alpha());
    assert!(
        decoded.to_rgba8().pixels().any(|p| p[3] < 255),
        "transparent pixels survive"
    );
}

#[test]
fn converts_transparent_png_to_jpeg_on_white_and_resizes() {
    let Some(env) = setup("convert") else { return };
    let output = env.out.join("flat.jpg");
    let mut job = job(
        env.fixtures.join("image-alpha.png"),
        output.clone(),
        Quality::High,
    );
    job.recipe.image.format = ImageFormat::Jpeg;
    job.recipe.image.max_width = Some(600);
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let decoded = image::open(&output).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (600, 450));
    // The fixture's left edge is fully transparent outside the central circle: now near white.
    let corner = decoded.get_pixel(0, 0);
    assert!(corner.0.iter().all(|c| *c > 240), "{corner:?}");
}

#[test]
fn converts_to_webp_and_avif() {
    let Some(env) = setup("modern") else { return };
    for (format, ext, magic) in [
        (ImageFormat::Webp, "webp", &b"WEBP"[..]),
        (ImageFormat::Avif, "avif", &b"ftypavif"[..]),
    ] {
        let output = env.out.join(format!("alpha.{ext}"));
        let mut job = job(
            env.fixtures.join("image-alpha.png"),
            output.clone(),
            Quality::Balanced,
        );
        job.recipe.image.format = format;
        let report = compress(
            &env.tools,
            &env.caps,
            &job,
            &mut no_progress,
            &NOT_CANCELLED,
        );
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{ext}: {:?}",
            report.outcome
        );
        let bytes = std::fs::read(&output).unwrap();
        assert!(
            bytes.windows(magic.len()).take(16).any(|w| w == magic),
            "{ext} signature"
        );
    }
    // WebP decodes back with its transparency.
    let decoded = image::open(env.out.join("alpha.webp")).unwrap();
    assert!(decoded.to_rgba8().pixels().any(|p| p[3] < 255));
}

#[test]
fn converts_heic_to_jpeg_with_full_tile_grid() {
    let Some(env) = setup("heic") else { return };
    let input = env.fixtures.join("image-photo.heic");
    if !input.is_file() {
        eprintln!("skipped: the HEIC fixture is only generated on macOS");
        return;
    }
    let output = env.out.join("photo.jpg");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(input, output.clone(), Quality::Balanced),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    // Converting a format keeps the result even when it is larger than the HEIC.
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let decoded = image::open(&output).unwrap();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (1600, 1200),
        "all tiles, cropped to the image"
    );
    // The fixture is the JPEG sample: the stitched picture must match it, not just one tile.
    let reference = image::open(env.fixtures.join("image-photo.jpg"))
        .unwrap()
        .to_rgb8();
    let pixels = decoded.to_rgb8();
    // Compare 32×32 block averages: single pixels differ at sharp edges after two lossy codecs.
    let mean = |img: &image::RgbImage, x0: u32, y0: u32| {
        let mut sum = [0u32; 3];
        for y in y0..y0 + 32 {
            for x in x0..x0 + 32 {
                for (s, c) in sum.iter_mut().zip(img.get_pixel(x, y).0) {
                    *s += u32::from(c);
                }
            }
        }
        sum.map(|s| s / 1024)
    };
    let close = |x: u32, y: u32| {
        let (a, b) = (mean(&pixels, x, y), mean(&reference, x, y));
        let ok = a.iter().zip(b).all(|(p, q)| p.abs_diff(q) < 16);
        if !ok {
            eprintln!("block at {x},{y}: got {a:?}, expected {b:?}");
        }
        ok
    };
    assert!(
        close(100, 100) && close(1500, 1100) && close(800, 900),
        "tiles land in the right places"
    );
}

fn gif_frames(path: &Path) -> (u16, u16, usize, u32) {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(std::fs::File::open(path).unwrap())
        .unwrap();
    let (w, h) = (decoder.width(), decoder.height());
    let (mut frames, mut centiseconds) = (0, 0u32);
    while let Some(frame) = decoder.read_next_frame().unwrap() {
        frames += 1;
        centiseconds += u32::from(frame.delay);
    }
    (w, h, frames, centiseconds)
}

#[test]
fn makes_a_gif_from_a_video_range() {
    let Some(env) = setup("vgif") else { return };
    let output = env.out.join("clip.gif");
    let mut job = job(
        env.fixtures.join("video-1080p-audio.mp4"),
        output.clone(),
        Quality::Balanced,
    );
    job.recipe.video.output = VideoOutput::Gif;
    job.recipe.clip.start_ms = Some(2_000);
    job.recipe.clip.duration_ms = Some(3_000);
    let mut last = 0.0;
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut |f| last = f,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    assert!((last - 1.0).abs() < f64::EPSILON);
    let (w, h, frames, centiseconds) = gif_frames(&output);
    assert_eq!((w, h), (480, 270), "balanced GIFs default to 480 px wide");
    assert!(
        frames > 1 && frames <= 36,
        "{frames} frames at 12 fps for 3 s"
    );
    assert!(
        (295..=305).contains(&centiseconds),
        "plays for 3 s, got {centiseconds} cs"
    );
}

#[test]
fn recompresses_a_gif_and_keeps_its_timing() {
    let Some(env) = setup("gifgif") else { return };
    let output = env.out.join("smaller.gif");
    let report = compress(
        &env.tools,
        &env.caps,
        &job(
            env.fixtures.join("animation.gif"),
            output.clone(),
            Quality::Small,
        ),
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let (w, h, _, centiseconds) = gif_frames(&output);
    assert_eq!(
        (w, h),
        (400, 224),
        "small GIFs default to 400 px wide (even height)"
    );
    assert!(
        (295..=305).contains(&centiseconds),
        "still 3 s long, got {centiseconds} cs"
    );
}

#[test]
fn fits_a_size_target_and_never_exceeds_it() {
    let Some(env) = setup("target") else { return };
    let output = env.out.join("fit.mp4");
    let mut job = job(
        env.fixtures.join("video-1080p-audio.mp4"),
        output.clone(),
        Quality::Balanced,
    );
    job.recipe.video.target_bytes = Some(2_000_000);
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    let Outcome::Compressed { output_bytes, .. } = report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(
        output_bytes <= 2_000_000,
        "{output_bytes} bytes is over the target"
    );
    assert!(
        output_bytes > 1_600_000,
        "{output_bytes} bytes wastes too much of the budget"
    );
    let Some(peraz_core::Plan::Video(plan)) = &report.plan else {
        panic!("no video plan")
    };
    let target = plan.target.as_ref().unwrap();
    assert!(target.two_pass && target.attempts >= 1);
    assert_eq!(
        probe(&env.tools, &output).unwrap().audio.len(),
        1,
        "audio kept"
    );
    let leftovers = std::fs::read_dir(&env.out).unwrap().count();
    assert_eq!(leftovers, 1, "no partial or pass-log files left behind");

    // Already small enough: the original is kept.
    let mut job = job.clone();
    job.output = OutputTarget::File(env.out.join("keep.mp4"));
    job.recipe.video.target_bytes = Some(50_000_000);
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert_eq!(
        report.outcome,
        Outcome::KeptOriginal(KeepReason::AlreadyWithinTarget {
            target_bytes: 50_000_000
        })
    );
}

#[test]
fn makes_animated_webp_and_avif_and_turns_gifs_into_mp4() {
    let Some(env) = setup("animated") else { return };
    for (format, encoders, magic) in [
        (VideoOutput::Webp, &["libwebp_anim"][..], &b"WEBP"[..]),
        (
            VideoOutput::Avif,
            &["libsvtav1", "libaom-av1"][..],
            &b"ftypavis"[..],
        ),
    ] {
        if !encoders.iter().any(|e| env.caps.has_encoder(e)) {
            eprintln!("skipped {format}: this FFmpeg has none of {encoders:?}");
            continue;
        }
        let output = env.out.join(format!("clip.{format}"));
        let mut job = job(
            env.fixtures.join("video-720p-noaudio.mp4"),
            output.clone(),
            Quality::Small,
        );
        job.recipe.video.output = format;
        job.recipe.clip.duration_ms = Some(2_000);
        let report = compress(
            &env.tools,
            &env.caps,
            &job,
            &mut no_progress,
            &NOT_CANCELLED,
        );
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{format}: {:?}",
            report.outcome
        );
        let bytes = std::fs::read(&output).unwrap();
        assert!(
            bytes.windows(magic.len()).take(32).any(|w| w == magic),
            "{format} signature"
        );
    }

    let output = env.out.join("anim.mp4");
    let mut job = job(
        env.fixtures.join("animation.gif"),
        output.clone(),
        Quality::Balanced,
    );
    job.recipe.video.output = VideoOutput::Mp4;
    let report = compress(
        &env.tools,
        &env.caps,
        &job,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let info = probe(&env.tools, &output).unwrap();
    assert_eq!(info.kind, MediaKind::Video);
    assert_eq!(info.video.unwrap().codec, "h264");
}

#[test]
fn a_batch_never_writes_one_output_twice() {
    let Some(env) = setup("batch") else { return };
    let batch = peraz_engine::batch::Batch::new(1, 4);
    let make = |input: &str| {
        let mut job = job(env.fixtures.join(input), PathBuf::new(), Quality::Balanced);
        job.output = OutputTarget::Folder(env.out.clone());
        job.recipe.image.format = ImageFormat::Webp;
        job
    };
    // Both inputs are named `image-photo.*`, so both want `image-photo-peraz.webp`.
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            peraz_engine::compress::compress_in(
                &batch,
                &env.tools,
                &env.caps,
                &make("image-photo.jpg"),
                &mut no_progress,
                &NOT_CANCELLED,
            )
        });
        let b = scope.spawn(|| {
            let heic = if env.fixtures.join("image-photo.heic").is_file() {
                "image-photo.heic"
            } else {
                "image-photo.jpg"
            };
            peraz_engine::compress::compress_in(
                &batch,
                &env.tools,
                &env.caps,
                &make(heic),
                &mut no_progress,
                &NOT_CANCELLED,
            )
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    let outcomes = [&first.outcome, &second.outcome];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Compressed { .. }))
            .count(),
        1,
        "{outcomes:?}"
    );
    let failure = outcomes.iter().find_map(|o| match o {
        Outcome::Failed { message } => Some(message.clone()),
        _ => None,
    });
    assert!(failure.unwrap().contains("another file in this batch"));
}

#[test]
fn compresses_pictures_inside_epub_and_pdf() {
    let Some(env) = setup("documents") else {
        return;
    };
    let mut inputs = vec![("book.epub", MediaKind::Epub)];
    if env.fixtures.join("document.pdf").is_file() {
        inputs.push(("document.pdf", MediaKind::Pdf));
    }
    for (name, kind) in inputs {
        let input = env.fixtures.join(name);
        assert_eq!(probe(&env.tools, &input).unwrap().kind, kind);
        let output = env.out.join(name);
        let report = compress(
            &env.tools,
            &env.caps,
            &job(input, output.clone(), Quality::Balanced),
            &mut no_progress,
            &NOT_CANCELLED,
        );
        let Outcome::Compressed { output_bytes, .. } = report.outcome else {
            panic!("{name}: unexpected outcome {:?}", report.outcome)
        };
        assert!(output_bytes < report.input_bytes.unwrap());
        let stats = report.document.unwrap();
        assert!(
            stats.images > 0 && stats.recompressed > 0,
            "{name}: {stats:?}"
        );
        assert_eq!(
            probe(&env.tools, &output).unwrap().kind,
            kind,
            "{name} is still a {kind:?}"
        );
    }
}

#[test]
fn trims_by_copying_streams_when_possible() {
    let Some(env) = setup("trim") else { return };
    let input = env.fixtures.join("video-1080p-audio.mp4");
    let trim = |name: &str, start_ms: u64, snap: bool| {
        let mut job = job(input.clone(), env.out.join(name), Quality::Balanced);
        job.recipe.video.trim_only = true;
        job.recipe.video.snap_to_keyframe = snap;
        job.recipe.clip = peraz_core::Clip {
            start_ms: Some(start_ms),
            duration_ms: Some(3_000),
        };
        compress(
            &env.tools,
            &env.caps,
            &job,
            &mut no_progress,
            &NOT_CANCELLED,
        )
    };
    let method = |report: &peraz_core::Report| match &report.plan {
        Some(peraz_core::Plan::Trim(plan)) => (plan.method, plan.reason.is_some()),
        other => panic!("no trim plan: {other:?}"),
    };
    // The sample has a keyframe at 0 s and none at 1.5 s.
    let copied = trim("copy.mp4", 0, false);
    assert!(
        matches!(copied.outcome, Outcome::Compressed { .. }),
        "{:?}",
        copied.outcome
    );
    assert_eq!(method(&copied), (peraz_core::TrimMethod::StreamCopy, false));
    let encoded = trim("encoded.mp4", 1_500, false);
    assert_eq!(
        method(&encoded),
        (peraz_core::TrimMethod::Reencode, true),
        "explains the re-encode"
    );
    let snapped = trim("snapped.mp4", 1_500, true);
    assert_eq!(
        method(&snapped),
        (peraz_core::TrimMethod::StreamCopy, false)
    );

    for name in ["copy.mp4", "encoded.mp4"] {
        let seconds = probe(&env.tools, &env.out.join(name))
            .unwrap()
            .duration_seconds
            .unwrap();
        assert!((2.9..3.3).contains(&seconds), "{name} lasts {seconds} s");
    }
    let snapped_seconds = probe(&env.tools, &env.out.join("snapped.mp4"))
        .unwrap()
        .duration_seconds
        .unwrap();
    assert!(
        snapped_seconds >= 4.4,
        "snapping keeps the requested end (4.5 s): {snapped_seconds} s"
    );
}
