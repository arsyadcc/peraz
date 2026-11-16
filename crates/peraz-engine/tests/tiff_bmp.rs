//! TIFF and BMP, end to end. Both are read and written in process, so these tests make their own
//! inputs and need neither FFmpeg nor the fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::{DynamicImage, ImageDecoder as _, ImageEncoder as _};
use peraz_core::{
    ExistingOutput, ImageEncoding, ImageFormat, Job, KeepReason, Metric, Outcome, OutputTarget,
    Plan, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-tiff-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Fail,
        recompress: true,
    };
    compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

/// A smooth 16-bit picture: what a scanner or a photo editor exports.
#[allow(clippy::cast_possible_truncation)]
fn deep_picture(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb16(image::ImageBuffer::from_fn(width, height, |x, y| {
        image::Rgb([(x * 200) as u16, (y * 260) as u16, ((x + y) * 90) as u16])
    }))
}

/// Writes `picture` as an uncompressed TIFF with a color profile.
fn write_tiff(path: &Path, picture: &DynamicImage, icc: &[u8]) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = image::codecs::tiff::TiffEncoder::new(file);
    encoder.set_icc_profile(icc.to_vec()).unwrap();
    encoder
        .write_image(
            picture.as_bytes(),
            picture.width(),
            picture.height(),
            picture.color().into(),
        )
        .unwrap();
}

fn read_tiff(path: &Path) -> (DynamicImage, Option<Vec<u8>>) {
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut decoder = image::codecs::tiff::TiffDecoder::new(file).unwrap();
    let icc = decoder.icc_profile().unwrap();
    (DynamicImage::from_decoder(decoder).unwrap(), icc)
}

#[test]
fn a_tiff_stays_a_tiff_with_its_depth_and_profile() {
    let dir = dir("keep");
    let input = dir.0.join("scan.tif");
    let picture = deep_picture(300, 200);
    let icc = b"stand-in for a color profile".to_vec();
    write_tiff(&input, &picture, &icc);

    let output = dir.0.join("scan-1.tiff");
    let report = run(&input, &output, Recipe::default());
    let Outcome::Compressed { output_bytes, .. } = report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(output_bytes * 4 < report.input_bytes.unwrap(), "{report:?}");
    assert_eq!(report.encoder.as_deref(), Some("TIFF Deflate (lossless)"));
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.encoding, ImageEncoding::Tiff);
    assert!(!plan.converts_format);
    let (back, profile) = read_tiff(&output);
    assert_eq!(back.color(), image::ColorType::Rgb16);
    assert_eq!(back.as_bytes(), picture.as_bytes(), "lossless");
    assert_eq!(profile, Some(icc));

    // What is already compressed this way does not get smaller: the original is kept.
    let again = dir.0.join("scan-2.tiff");
    let report = run(&output, &again, Recipe::default());
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
fn a_tiff_is_scaled_down_at_its_own_depth_and_scores_as_lossless() {
    let dir = dir("resize");
    let input = dir.0.join("scan.tiff");
    write_tiff(&input, &deep_picture(300, 200), b"profile");
    let mut recipe = Recipe::default();
    recipe.image.max_width = Some(150);
    recipe.image.target_ssimulacra2 = Some(90);
    let output = dir.0.join("small.tiff");
    let report = run(&input, &output, recipe);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let (back, _) = read_tiff(&output);
    assert_eq!((back.width(), back.height()), (150, 100));
    assert_eq!(back.color(), image::ColorType::Rgb16);
    // Lossless, so the score is the top one however low the target.
    let score = report.score.expect("a score");
    assert_eq!(
        (score.metric, score.target),
        (Metric::Ssimulacra2, Some(90))
    );
    assert!(score.score > 99.0, "{score:?}");

    let mut recipe = Recipe {
        measure_quality: true,
        ..Recipe::default()
    };
    recipe.image.max_width = Some(150);
    let report = run(&input, &dir.0.join("measured.tiff"), recipe);
    let score = report.score.expect("a measured score");
    assert_eq!(score.target, None);
    assert!(score.score > 99.0, "{score:?}");
}

#[test]
fn other_pictures_become_tiff_on_request_with_their_transparency() {
    let dir = dir("convert");
    let input = dir.0.join("logo.png");
    let pixels = image::RgbaImage::from_fn(120, 80, |x, y| {
        let alpha = if x < 30 { 0 } else { 255 };
        image::Rgba([byte(x * 2), byte(y * 3), 90, alpha])
    });
    pixels.save(&input).unwrap();
    let mut recipe = Recipe {
        quality: Quality::Smallest,
        ..Recipe::default()
    };
    recipe.image.format = ImageFormat::Tiff;
    let output = dir.0.join("logo.tiff");
    let report = run(&input, &output, recipe);
    // The format was asked for: the TIFF is kept even though the PNG was smaller.
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let (back, _) = read_tiff(&output);
    assert_eq!(back.color(), image::ColorType::Rgba8);
    assert_eq!(
        back.as_bytes(),
        pixels.as_raw().as_slice(),
        "lossless at every level"
    );
}

#[test]
fn a_bmp_becomes_a_png_by_default() {
    let dir = dir("bmp");
    let input = dir.0.join("screenshot.bmp");
    let pixels = image::RgbImage::from_fn(200, 150, |x, y| {
        // Flat areas with a few colors, like a screenshot.
        image::Rgb([byte(x / 50 * 60), byte(y / 50 * 80), 200])
    });
    pixels.save(&input).unwrap();
    let report = run(
        &input,
        &dir.0.join("screenshot-1.png"),
        Recipe {
            quality: Quality::Highest,
            ..Recipe::default()
        },
    );
    let Outcome::Compressed {
        output,
        output_bytes,
    } = &report.outcome
    else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(*output_bytes * 10 < report.input_bytes.unwrap());
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    // The report shows the new format; it is kept only because it is smaller.
    assert_eq!(plan.extension, "png");
    assert!(matches!(plan.encoding, ImageEncoding::Png { .. }));
    assert!(!plan.converts_format);
    assert_eq!(image::open(output).unwrap().to_rgb8(), pixels);
}

#[test]
fn a_tiff_with_several_pages_is_left_alone() {
    let dir = dir("pages");
    let input = dir.0.join("fax.tiff");
    {
        let file = std::io::BufWriter::new(std::fs::File::create(&input).unwrap());
        let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
        for shade in [40u8, 200] {
            encoder
                .write_image::<tiff::encoder::colortype::Gray8>(64, 64, &[shade; 64 * 64])
                .unwrap();
        }
    }
    for format in [ImageFormat::Keep, ImageFormat::Png] {
        let mut recipe = Recipe::default();
        recipe.image.format = format;
        let output = dir.0.join("out.bin");
        let report = run(&input, &output, recipe);
        let Outcome::Skipped { message, .. } = &report.outcome else {
            panic!("unexpected outcome: {:?}", report.outcome)
        };
        assert!(message.contains("2 pages"), "{message}");
        assert!(!output.exists());
    }
}

fn byte(value: u32) -> u8 {
    u8::try_from(value.min(255)).unwrap()
}
