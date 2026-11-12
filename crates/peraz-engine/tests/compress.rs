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
    let output = env.out.join("clip-1.mp4");
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
    assert_eq!(leftovers, [std::ffi::OsString::from("clip-1.mp4")]);
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

/// Asserts that `path` is the upright fixture picture with no orientation tag left: red, green,
/// blue, and yellow corners at top-left, top-right, bottom-left, bottom-right.
fn assert_upright(path: &Path, (width, height): (u32, u32), name: &str) {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap();
    assert_eq!(
        decoder.orientation().unwrap(),
        image::metadata::Orientation::NoTransforms,
        "{name}: the orientation is applied to the pixels, so the tag must be gone"
    );
    let pixels = image::DynamicImage::from_decoder(decoder)
        .unwrap()
        .to_rgb8();
    assert_eq!(pixels.dimensions(), (width, height), "{name}");
    // Sample the middle of each corner box (100 px of 800).
    let inset = width / 16;
    let corners = [
        ("top-left", inset, inset, [255, 0, 0]),
        ("top-right", width - inset, inset, [0, 255, 0]),
        ("bottom-left", inset, height - inset, [0, 0, 255]),
        ("bottom-right", width - inset, height - inset, [255, 255, 0]),
    ];
    for (corner, x, y, expected) in corners {
        let got = pixels.get_pixel(x, y).0;
        assert!(
            got.iter().zip(expected).all(|(g, e)| g.abs_diff(e) < 48),
            "{name}: {corner} is {got:?}, expected {expected:?}"
        );
    }
}

#[test]
fn applies_every_exif_orientation_to_the_pixels() {
    let Some(env) = setup("orientation") else {
        return;
    };
    if !env.fixtures.join("image-orientation1.jpg").is_file() {
        eprintln!("skipped: run scripts/make-fixtures.sh again to add the orientation samples");
        return;
    }
    for n in 1..=8 {
        let name = format!("image-orientation{n}.jpg");
        let output = env.out.join(&name);
        let report = compress(
            &env.tools,
            &env.caps,
            &job(env.fixtures.join(&name), output.clone(), Quality::Balanced),
            &mut no_progress,
            &NOT_CANCELLED,
        );
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{name}: {:?}",
            report.outcome
        );
        assert_upright(&output, (800, 600), &name);
    }
    // The size limit applies to the upright picture: a landscape photo stored as portrait is
    // limited by its displayed width.
    let output = env.out.join("resized.jpg");
    let mut job = job(
        env.fixtures.join("image-orientation6.jpg"),
        output.clone(),
        Quality::Balanced,
    );
    job.recipe.image.max_width = Some(400);
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
    assert_upright(&output, (400, 300), "resized orientation 6");
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

/// Writes a 256×256 PNG: a gradient with a solid 64×64 patch of `color` in the middle, tagged with `icc`.
fn write_png_with_profile(path: &Path, color: [u8; 3], icc: Option<Vec<u8>>) {
    use image::ImageEncoder;
    let pixels = image::RgbImage::from_fn(256, 256, |x, y| {
        if (96..160).contains(&x) && (96..160).contains(&y) {
            image::Rgb(color)
        } else {
            image::Rgb([x, y, 128].map(|v| u8::try_from(v).unwrap()))
        }
    });
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = image::codecs::png::PngEncoder::new(file);
    if let Some(icc) = icc {
        encoder.set_icc_profile(icc).unwrap();
    }
    encoder
        .write_image(pixels.as_raw(), 256, 256, image::ExtendedColorType::Rgb8)
        .unwrap();
}

/// Mean color of the middle patch written by [`write_png_with_profile`].
fn patch_color(picture: &image::DynamicImage) -> [u32; 3] {
    let rgb = picture.to_rgb8();
    let mut sum = [0u32; 3];
    for y in 104..152 {
        for x in 104..152 {
            for (s, c) in sum.iter_mut().zip(rgb.get_pixel(x, y).0) {
                *s += u32::from(c);
            }
        }
    }
    sum.map(|s| s / (48 * 48))
}

#[test]
fn avif_keeps_the_color_profile_instead_of_converting_to_srgb() {
    let Some(env) = setup("avif-icc") else { return };
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let patch = [200, 100, 50];
    // In sRGB this color would be noticeably different, so a conversion cannot pass for a copy.
    let in_srgb = {
        let transform = moxcms::ColorProfile::new_from_slice(&p3)
            .unwrap()
            .create_transform_8bit(
                moxcms::Layout::Rgb,
                &moxcms::ColorProfile::new_srgb(),
                moxcms::Layout::Rgb,
                moxcms::TransformOptions::default(),
            )
            .unwrap();
        let mut out = [0u8; 3];
        transform.transform(&patch, &mut out).unwrap();
        out
    };
    assert!(
        patch.iter().zip(in_srgb).any(|(a, b)| a.abs_diff(b) > 12),
        "{in_srgb:?}"
    );

    for (name, icc) in [("p3", Some(p3.clone())), ("untagged", None)] {
        let input = env.out.join(format!("{name}.png"));
        write_png_with_profile(&input, patch, icc.clone());
        let output = env.out.join(format!("{name}.avif"));
        let mut job = job(input, output.clone(), Quality::High);
        job.recipe.image.format = ImageFormat::Avif;
        let report = compress(
            &env.tools,
            &env.caps,
            &job,
            &mut no_progress,
            &NOT_CANCELLED,
        );
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{name}: {:?}",
            report.outcome
        );
        let encoder = report.encoder.unwrap();
        assert!(!encoder.contains("sRGB"), "{name}: {encoder}");

        let decoded = peraz_engine::heif::decode(&env.tools, &output).unwrap();
        assert_eq!(
            decoded.icc, icc,
            "{name}: the profile is carried over as is"
        );
        let got = patch_color(&decoded.image);
        assert!(
            got.iter()
                .zip(patch)
                .all(|(g, p)| g.abs_diff(u32::from(p)) <= 4),
            "{name}: pixel values are kept, got {got:?}, expected {patch:?}"
        );
    }
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
fn a_batch_numbers_outputs_that_would_share_a_name() {
    let Some(env) = setup("batch") else { return };
    let batch = peraz_engine::batch::Batch::new(1, 4);
    let make = |input: &str| {
        let mut job = job(env.fixtures.join(input), PathBuf::new(), Quality::Balanced);
        job.output = OutputTarget::Folder(env.out.clone());
        job.recipe.image.format = ImageFormat::Webp;
        job
    };
    // Both inputs are named `image-photo.*`, so both would be `image-photo-1.webp`.
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
    let mut outputs: Vec<String> = [&first.outcome, &second.outcome]
        .iter()
        .map(|o| match o {
            Outcome::Compressed { output, .. } => {
                output.file_name().unwrap().to_string_lossy().into_owned()
            }
            other => panic!("unexpected outcome: {other:?}"),
        })
        .collect();
    outputs.sort();
    assert_eq!(outputs, ["image-photo-1.webp", "image-photo-2.webp"]);
}

#[test]
fn compressing_again_takes_the_next_number_and_replaces_nothing() {
    let Some(env) = setup("numbered") else { return };
    let input = env.out.join("photo.jpg");
    std::fs::copy(env.fixtures.join("image-photo.jpg"), &input).unwrap();
    // A file of the user's that already has the first name.
    std::fs::write(env.out.join("photo-1.jpg"), b"keep me").unwrap();
    let mut job = job(input, PathBuf::new(), Quality::Small);
    job.output = OutputTarget::NextToInput;
    let mut names = Vec::new();
    for _ in 0..2 {
        let report = compress(
            &env.tools,
            &env.caps,
            &job,
            &mut no_progress,
            &NOT_CANCELLED,
        );
        let Outcome::Compressed { output, .. } = report.outcome else {
            panic!("unexpected outcome: {:?}", report.outcome)
        };
        names.push(output.file_name().unwrap().to_string_lossy().into_owned());
    }
    assert_eq!(names, ["photo-2.jpg", "photo-3.jpg"]);
    assert_eq!(
        std::fs::read(env.out.join("photo-1.jpg")).unwrap(),
        b"keep me"
    );
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

#[test]
fn hdr_video_stays_hdr_and_refuses_h264() {
    let Some(env) = setup("hdr") else { return };
    let input = env.fixtures.join("video-hdr10-hevc.mp4");
    if !input.is_file() || !env.caps.has_encoder("libx265") {
        eprintln!("skipped: the HDR sample needs an FFmpeg with libx265");
        return;
    }
    let source = probe(&env.tools, &input).unwrap().video.unwrap();
    assert!(source.color.is_hdr(), "{:?}", source.color);

    // Without --codec, HDR becomes 10-bit HEVC with the same color description.
    let output = env.out.join("hdr-1.mp4");
    let mut hevc = job(input.clone(), output.clone(), Quality::Small);
    hevc.recipe.video.encoder = peraz_core::EncoderPreference::Software;
    let report = compress(
        &env.tools,
        &env.caps,
        &hevc,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.encoder.as_deref(), Some("libx265, HDR kept"));
    let result = probe(&env.tools, &output).unwrap().video.unwrap();
    assert_eq!(result.codec, "hevc");
    assert_eq!(result.pixel_format.as_deref(), Some("yuv420p10le"));
    assert_eq!(result.color, source.color);

    // Asking for H.264 explicitly fails with advice, and writes nothing.
    let refused = env.out.join("hdr-h264.mp4");
    let mut h264 = job(input, refused.clone(), Quality::Small);
    h264.recipe.video.codec = Some(peraz_core::VideoCodec::H264);
    let report = compress(
        &env.tools,
        &env.caps,
        &h264,
        &mut no_progress,
        &NOT_CANCELLED,
    );
    let Outcome::Failed { message } = report.outcome else {
        panic!("{:?}", report.outcome)
    };
    assert!(message.contains("--codec hevc"), "{message}");
    assert!(!refused.exists());
}

/// Percentage of fully transparent pixels in the fixture's left edge, where `image-alpha.png` is
/// transparent outside its central circle.
fn transparent_left_edge(picture: &image::DynamicImage) -> usize {
    let rgba = picture.to_rgba8();
    let edge: Vec<u8> = (0..rgba.height())
        .map(|y| rgba.get_pixel(2, y)[3])
        .collect();
    edge.iter().filter(|a| **a < 16).count() * 100 / edge.len()
}

#[test]
fn heic_and_avif_alpha_planes_are_kept() {
    let Some(env) = setup("heic-alpha") else {
        return;
    };
    let heic = env.fixtures.join("image-alpha.heic");
    if !heic.is_file() {
        eprintln!("skipped: the transparent HEIC fixture is only generated on macOS");
        return;
    }
    let reference =
        transparent_left_edge(&image::open(env.fixtures.join("image-alpha.png")).unwrap());
    assert!(reference > 50, "{reference}");

    let convert = |input: &Path, format: ImageFormat, name: &str| {
        let output = env.out.join(name);
        let mut job = job(input.to_owned(), output.clone(), Quality::High);
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
            "{name}: {:?}",
            report.outcome
        );
        output
    };

    for (format, name) in [
        (ImageFormat::Png, "from-heic.png"),
        (ImageFormat::Webp, "from-heic.webp"),
    ] {
        let output = convert(&heic, format, name);
        let decoded = image::open(&output).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1200, 900), "{name}");
        let edge = transparent_left_edge(&decoded);
        assert!(
            edge.abs_diff(reference) < 10,
            "{name}: {edge} vs {reference}"
        );
    }

    // JPEG has no alpha: transparent pixels are placed on white.
    let jpeg = image::open(convert(&heic, ImageFormat::Jpeg, "from-heic.jpg"))
        .unwrap()
        .to_rgb8();
    let corner = jpeg.get_pixel(2, 2);
    assert!(corner.0.iter().all(|c| *c > 240), "{corner:?}");

    // AVIF output keeps an alpha item, and reading it back (AVIF input) keeps the transparency.
    let avif = convert(&heic, ImageFormat::Avif, "from-heic.avif");
    let properties = peraz_engine::heif::read_properties(&std::fs::read(&avif).unwrap());
    assert!(properties.alpha.is_some(), "{properties:?}");
    let back = image::open(convert(&avif, ImageFormat::Png, "from-avif.png")).unwrap();
    let edge = transparent_left_edge(&back);
    assert!(
        edge.abs_diff(reference) < 10,
        "AVIF round trip: {edge} vs {reference}"
    );
}

/// Runs FFmpeg with `args`, for samples made inside a test.
fn ffmpeg(env: &Env, args: &[&str]) {
    let status = std::process::Command::new(&env.tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg {args:?}");
}

fn run_job(env: &Env, job: &Job) -> Outcome {
    compress(&env.tools, &env.caps, job, &mut no_progress, &NOT_CANCELLED).outcome
}

#[test]
fn still_gifs_become_png_and_keep_their_transparency() {
    let Some(env) = setup("still-gif") else {
        return;
    };
    // One frame, with a transparent color (alpha below 128 becomes transparent).
    let gif = env.out.join("still.gif");
    ffmpeg(
        &env,
        &[
            "-i",
            env.fixtures.join("image-alpha.png").to_str().unwrap(),
            "-vf",
            "split[a][b];[a]palettegen=reserve_transparent=1[p];[b][p]paletteuse=alpha_threshold=128",
            "-frames:v",
            "1",
            gif.to_str().unwrap(),
        ],
    );
    let info = probe(&env.tools, &gif).unwrap();
    assert_eq!(info.kind, MediaKind::Image);

    let output = env.out.join("still-1.png");
    let mut keep = job(gif.clone(), output.clone(), Quality::Balanced);
    keep.output = OutputTarget::Folder(env.out.clone());
    let outcome = run_job(&env, &keep);
    let Outcome::Compressed {
        output: written, ..
    } = &outcome
    else {
        panic!("{outcome:?}")
    };
    assert_eq!(written, &output);
    let decoded = image::open(&output).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1200, 900));
    assert!(transparent_left_edge(&decoded) > 50, "transparency kept");

    // Asked for explicitly, other formats work too.
    for (format, ext) in [(ImageFormat::Webp, "webp"), (ImageFormat::Jpeg, "jpg")] {
        let mut converted = job(
            gif.clone(),
            env.out.join(format!("still.{ext}")),
            Quality::Balanced,
        );
        converted.recipe.image.format = format;
        let outcome = run_job(&env, &converted);
        assert!(
            matches!(outcome, Outcome::Compressed { .. }),
            "{ext}: {outcome:?}"
        );
    }
}

#[test]
fn cut_off_files_fail_and_free_their_output_name() {
    let Some(env) = setup("cut-off") else {
        return;
    };
    let photo = std::fs::read(env.fixtures.join("image-photo.jpg")).unwrap();
    let gif = std::fs::read(env.fixtures.join("animation.gif")).unwrap();
    let cut_jpeg = env.out.join("shot.jpg");
    std::fs::write(&cut_jpeg, &photo[..photo.len() / 2]).unwrap();
    let cut_gif = env.out.join("loop.gif");
    std::fs::write(&cut_gif, &gif[..gif.len() / 2]).unwrap();
    for input in [&cut_jpeg, &cut_gif] {
        let mut cut = job(input.clone(), PathBuf::new(), Quality::Balanced);
        cut.output = OutputTarget::Folder(env.out.join("results"));
        std::fs::create_dir_all(env.out.join("results")).unwrap();
        let Outcome::Failed { message } = run_job(&env, &cut) else {
            panic!("{} was compressed", input.display())
        };
        assert!(message.contains("is incomplete"), "{message}");
    }

    // In a batch, a failed job gives its name back: the next file with that name gets `-1`.
    let batch = peraz_engine::batch::Batch::new(1, 1);
    let whole = env.out.join("whole");
    std::fs::create_dir_all(&whole).unwrap();
    // A cut-off PNG passes probing and fails while decoding, after its output name was taken.
    let png = std::fs::read(env.fixtures.join("image-alpha.png")).unwrap();
    std::fs::write(whole.join("shot.png"), &png[..3000]).unwrap();
    std::fs::copy(env.fixtures.join("image-photo.jpg"), whole.join("shot.jpg")).unwrap();
    let mut outcomes = Vec::new();
    for name in ["shot.png", "shot.jpg"] {
        let mut one = job(whole.join(name), PathBuf::new(), Quality::Balanced);
        one.output = OutputTarget::Folder(env.out.join("results"));
        one.recipe.image.format = ImageFormat::Jpeg;
        outcomes.push(
            peraz_engine::compress::compress_in(
                &batch,
                &env.tools,
                &env.caps,
                &one,
                &mut no_progress,
                &NOT_CANCELLED,
            )
            .outcome,
        );
    }
    let Outcome::Failed { message } = &outcomes[0] else {
        panic!("{:?}", outcomes[0])
    };
    assert!(message.contains("cannot decode"), "{message}");
    let Outcome::Compressed { output, .. } = &outcomes[1] else {
        panic!("{:?}", outcomes[1])
    };
    assert_eq!(output.file_name().unwrap(), "shot-1.jpg");
}

#[test]
fn names_at_the_file_system_limit_still_work() {
    let Some(env) = setup("long-name") else {
        return;
    };
    // 251 bytes plus `.jpg`: the longest name most file systems allow.
    let input = env.out.join(format!("{}.jpg", "n".repeat(251)));
    std::fs::copy(env.fixtures.join("image-photo.jpg"), &input).unwrap();
    let mut long = job(input, PathBuf::new(), Quality::Balanced);
    long.output = OutputTarget::NextToInput;
    let outcome = run_job(&env, &long);
    let Outcome::Compressed { output, .. } = &outcome else {
        panic!("{outcome:?}")
    };
    let name = output.file_name().unwrap().to_str().unwrap();
    assert!(name.len() <= 255 && name.ends_with("-1.jpg"), "{name}");
    assert!(output.is_file());
}
