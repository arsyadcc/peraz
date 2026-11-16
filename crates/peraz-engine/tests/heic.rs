//! HEIC output, end to end. FFmpeg encodes the HEVC picture with libx265 and reads the results
//! back, so these tests are skipped (with a note) without such an FFmpeg. The inputs are made
//! here: no fixtures are needed.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::{DynamicImage, ImageEncoder as _, RgbaImage};
use peraz_core::{
    Alpha, ExistingOutput, ImageEncoding, ImageFormat, Job, KeepReason, MediaKind, Metric, Outcome,
    OutputTarget, Plan, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::{Capabilities, Toolchain};
use peraz_engine::probe::probe;

struct Env {
    tools: Toolchain,
    caps: Capabilities,
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(test: &str) -> Option<Env> {
    let tools = match Toolchain::locate(None) {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("skipped: {e}");
            return None;
        }
    };
    let caps = tools.capabilities().unwrap();
    if !caps.has_encoder("libx265") {
        eprintln!("skipped: this FFmpeg has no libx265");
        return None;
    }
    // The results are read back with FFmpeg, which reads HEIC tile grids from version 7 on.
    let version = tools
        .build_info()
        .map(|build| build.version)
        .unwrap_or_default();
    let major: Option<u32> = version
        .trim_start_matches('n')
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok());
    if major.is_some_and(|major| major < 7) {
        eprintln!("skipped: FFmpeg {version} cannot read HEIC tile grids back");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("peraz-heic-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Some(Env { tools, caps, dir })
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(env: &Env, input: &Path, output: &Path, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Fail,
        recompress: true,
    };
    compress(
        Some(&env.tools),
        &env.caps,
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

fn to_heic(quality: Quality) -> Recipe {
    let mut recipe = Recipe {
        quality,
        ..Recipe::default()
    };
    recipe.image.format = ImageFormat::Heic;
    recipe
}

fn compressed(report: &Report) -> u64 {
    let Outcome::Compressed { output_bytes, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    *output_bytes
}

/// A picture with large areas of plain color (to check the colors) and a gradient with some
/// texture; the left quarter is transparent when `transparent`.
#[allow(clippy::cast_possible_truncation)]
fn picture(width: u32, height: u32, transparent: bool) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        let alpha = if transparent && x < width / 4 { 0 } else { 255 };
        let color = if y < height / 3 {
            // Saturated blocks: a wrong color matrix or range shows here.
            [[230, 30, 30], [30, 200, 60], [40, 60, 220], [240, 220, 40]]
                [(x * 4 / width).min(3) as usize]
        } else {
            let texture = ((x / 4 + y / 4) % 2) * 12;
            [
                (x * 255 / width) as u8,
                (y * 255 / height) as u8,
                (100 + texture) as u8,
            ]
        };
        image::Rgba([color[0], color[1], color[2], alpha])
    })
}

/// Saves a picture as a PNG, with a color profile when given.
fn save_png(path: &Path, pixels: &RgbaImage, icc: Option<&[u8]>) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = image::codecs::png::PngEncoder::new(file);
    if let Some(icc) = icc {
        encoder.set_icc_profile(icc.to_vec()).unwrap();
    }
    encoder
        .write_image(
            pixels.as_raw(),
            pixels.width(),
            pixels.height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
}

/// The mean color of a block of opaque pixels.
fn block(image: &RgbaImage, x0: u32, y0: u32, size: u32) -> [u32; 3] {
    let mut sum = [0u32; 3];
    for y in y0..y0 + size {
        for x in x0..x0 + size {
            for (total, channel) in sum.iter_mut().zip(image.get_pixel(x, y).0) {
                *total += u32::from(channel);
            }
        }
    }
    sum.map(|total| total / (size * size))
}

/// Checks that `decoded` shows the same picture as `source`: block by block, away from the
/// edges between colors.
fn assert_same_picture(decoded: &RgbaImage, source: &RgbaImage, tolerance: u32) {
    assert_eq!(decoded.dimensions(), source.dimensions());
    let (width, height) = source.dimensions();
    for (fx, fy) in [(3, 1), (5, 1), (7, 1), (3, 5), (5, 6), (7, 7), (6, 4)] {
        // Eighths of the picture, inside the opaque part and inside one color block.
        let (x, y) = (width * fx / 8 - 12, height * fy / 8 - 8);
        let (got, want) = (block(decoded, x, y, 8), block(source, x, y, 8));
        for (g, w) in got.iter().zip(want) {
            assert!(
                g.abs_diff(w) <= tolerance,
                "block at {x},{y}: got {got:?}, expected {want:?}"
            );
        }
    }
}

#[test]
fn a_picture_becomes_a_heic_with_its_colors_and_profile() {
    let Some(env) = setup("convert") else { return };
    let source = picture(640, 480, false);
    let icc = b"a stand-in for a color profile".to_vec();
    let input = env.dir.join("photo.png");
    save_png(&input, &source, Some(&icc));
    let output = env.dir.join("photo.heic");
    let report = run(&env, &input, &output, to_heic(Quality::High));
    compressed(&report);
    assert_eq!(report.encoder.as_deref(), Some("x265 crf 21"));
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.encoding, ImageEncoding::Heic { quality: 58 });
    assert!(plan.converts_format);

    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[4..12], b"ftypheic");
    let info = probe(&env.tools, &output).unwrap();
    assert_eq!(info.kind, MediaKind::Image);
    let stored = info.video.unwrap();
    assert_eq!(
        (stored.codec.as_str(), stored.width, stored.height),
        ("hevc", 640, 480)
    );
    assert_eq!(stored.alpha, Alpha::Absent);
    let decoded = peraz_engine::heif::decode(&env.tools, &output).unwrap();
    assert_eq!(decoded.icc, Some(icc));
    assert_same_picture(&decoded.image.to_rgba8(), &source, 6);
}

#[test]
fn transparency_and_odd_sizes_survive() {
    let Some(env) = setup("alpha") else { return };
    // Odd in both directions: HEVC codes it one pixel larger, and the file crops it back.
    let source = picture(401, 301, true);
    let input = env.dir.join("logo.png");
    save_png(&input, &source, None);
    let output = env.dir.join("logo.heic");
    compressed(&run(&env, &input, &output, to_heic(Quality::Balanced)));
    let info = probe(&env.tools, &output).unwrap();
    let stored = info.video.unwrap();
    assert_eq!((stored.width, stored.height), (401, 301));
    assert_ne!(stored.alpha, Alpha::Absent);
    let decoded = peraz_engine::heif::decode(&env.tools, &output)
        .unwrap()
        .image
        .to_rgba8();
    assert_same_picture(&decoded, &source, 8);
    assert!(
        decoded.get_pixel(20, 150)[3] < 16,
        "the left is transparent"
    );
    assert!(decoded.get_pixel(300, 150)[3] > 240, "the rest is opaque");

    // The same with an even size: the alpha plane next to a plain picture.
    let source = picture(400, 300, true);
    save_png(&input, &source, None);
    let even = env.dir.join("even.heic");
    compressed(&run(&env, &input, &even, to_heic(Quality::Balanced)));
    let stored = probe(&env.tools, &even).unwrap().video.unwrap();
    assert_eq!((stored.width, stored.height), (400, 300));
    assert_ne!(stored.alpha, Alpha::Absent);
    let decoded = peraz_engine::heif::decode(&env.tools, &even)
        .unwrap()
        .image
        .to_rgba8();
    assert_same_picture(&decoded, &source, 8);
    assert!(decoded.get_pixel(20, 150)[3] < 16);

    // Turned into a JPEG, the transparent part is white, not black.
    let mut recipe = Recipe::default();
    recipe.image.format = ImageFormat::Jpeg;
    let jpeg = env.dir.join("flat.jpg");
    compressed(&run(&env, &even, &jpeg, recipe));
    let flat = image::open(&jpeg).unwrap().to_rgb8();
    assert!(flat.get_pixel(20, 150).0.iter().all(|c| *c > 235));
}

#[test]
fn a_heic_stays_a_heic_only_when_smaller() {
    let Some(env) = setup("keep") else { return };
    let source = picture(640, 480, false);
    let input = env.dir.join("photo.png");
    save_png(&input, &source, None);
    let fine = env.dir.join("fine.heic");
    let fine_bytes = compressed(&run(&env, &input, &fine, to_heic(Quality::Highest)));

    // By default a HEIC is compressed as a HEIC, at the size asked for.
    let mut recipe = Recipe {
        quality: Quality::Smallest,
        ..Recipe::default()
    };
    recipe.image.max_width = Some(320);
    let small = env.dir.join("small.heic");
    let report = run(&env, &fine, &small, recipe);
    assert!(compressed(&report) < fine_bytes);
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.encoding, ImageEncoding::Heic { quality: 40 });
    assert!(!plan.converts_format);
    let stored = probe(&env.tools, &small).unwrap().video.unwrap();
    assert_eq!(
        (stored.codec.as_str(), stored.width, stored.height),
        ("hevc", 320, 240)
    );

    // Encoding it again at a finer setting cannot make it smaller: the original is kept.
    let again = env.dir.join("again.heic");
    let report = run(
        &env,
        &small,
        &again,
        Recipe {
            quality: Quality::Highest,
            ..Recipe::default()
        },
    );
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );
    assert!(!again.exists());
}

#[test]
fn scores_are_searched_and_measured_for_heic() {
    let Some(env) = setup("score") else { return };
    let source = picture(320, 240, false);
    let input = env.dir.join("photo.png");
    save_png(&input, &source, None);
    let mut recipe = to_heic(Quality::Balanced);
    recipe.image.target_ssimulacra2 = Some(70);
    let report = run(&env, &input, &env.dir.join("target.heic"), recipe);
    compressed(&report);
    let score = report.score.expect("a score");
    assert_eq!(
        (score.metric, score.target),
        (Metric::Ssimulacra2, Some(70))
    );
    assert!(score.score >= 70.0, "{score:?}");

    let mut recipe = to_heic(Quality::Balanced);
    recipe.measure_quality = true;
    let report = run(&env, &input, &env.dir.join("measured.heic"), recipe);
    compressed(&report);
    let score = report.score.expect("a measured score");
    assert_eq!(score.target, None);
    assert!(score.score > 30.0 && score.score < 100.0, "{score:?}");
}

#[test]
fn an_ffmpeg_without_the_encoder_says_so() {
    let Some(env) = setup("encoder") else { return };
    let input = env.dir.join("photo.png");
    DynamicImage::ImageRgba8(picture(64, 48, false))
        .save(&input)
        .unwrap();
    let job = Job {
        input,
        output: OutputTarget::File(env.dir.join("photo.heic")),
        recipe: to_heic(Quality::Balanced),
        existing: ExistingOutput::Fail,
        recompress: true,
    };
    // The same FFmpeg, as if it had no encoders.
    let report = compress(
        Some(&env.tools),
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    );
    let Outcome::Failed { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("libx265"), "{message}");
    // And without FFmpeg at all.
    let report = compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    );
    let Outcome::Failed { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("needs FFmpeg"), "{message}");
}
