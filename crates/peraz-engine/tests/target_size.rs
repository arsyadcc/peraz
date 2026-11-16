//! A size target for still images, end to end: lower quality first, a smaller picture only when
//! that is not enough, and a result that is known to fit. JPEG, PNG, and WebP are encoded in
//! process, so these tests need neither FFmpeg nor the fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use peraz_core::{
    ExistingOutput, ImageEncoding, ImageFormat, Job, KeepReason, Outcome, OutputTarget, Plan,
    Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A folder with a detailed 640x480 photo-like PNG in it.
fn setup(test: &str) -> (Dir, PathBuf) {
    let dir = std::env::temp_dir().join(format!("peraz-fit-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("photo.png");
    image::RgbImage::from_fn(640, 480, |x, y| {
        // Smooth shapes with grain: costly at high quality, cheap at low.
        let grain = (x
            .wrapping_mul(2_654_435_761)
            .wrapping_add(y.wrapping_mul(40_503))
            >> 27)
            & 31;
        let wave = ((x / 40 + y / 30) % 2) * 40;
        let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
        image::Rgb([
            channel(x * 255 / 640 + grain),
            channel(y * 255 / 480 + grain),
            channel(80 + wave + grain),
        ])
    })
    .save(&input)
    .unwrap();
    (Dir(dir), input)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
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

fn fit(format: ImageFormat, target_bytes: u64) -> Recipe {
    let mut recipe = Recipe::default();
    recipe.image.format = format;
    recipe.image.target_bytes = Some(target_bytes);
    recipe
}

fn output_bytes(report: &Report) -> u64 {
    let Outcome::Compressed { output_bytes, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    *output_bytes
}

fn image_plan(report: &Report) -> &peraz_core::ImagePlan {
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    plan
}

#[test]
fn quality_is_lowered_first_and_the_picture_keeps_its_size() {
    let (dir, input) = setup("quality");
    // What the quality level alone gives.
    let mut usual_recipe = Recipe::default();
    usual_recipe.image.format = ImageFormat::Jpeg;
    let free = output_bytes(&run(&input, &dir.0.join("free.jpg"), usual_recipe));

    // A little less than that: a lower setting fits, at the same size in pixels.
    let target = free * 7 / 10;
    let output = dir.0.join("fit.jpg");
    let report = run(&input, &output, fit(ImageFormat::Jpeg, target));
    let bytes = output_bytes(&report);
    assert!(bytes <= target, "{bytes} > {target}");
    assert_eq!(bytes, std::fs::metadata(&output).unwrap().len());
    // The best that fits: not much below the target.
    assert!(bytes * 10 >= target * 8, "{bytes} for a target of {target}");
    let plan = image_plan(&report);
    let found = plan.target.expect("a size target");
    assert!(found.reached && found.attempts > 1, "{found:?}");
    assert_eq!((found.target_bytes, found.scaled_to), (target, None));
    let ImageEncoding::Jpeg { quality } = plan.encoding else {
        panic!("{:?}", plan.encoding)
    };
    assert!((35..78).contains(&quality), "quality {quality}");
    let picture = image::open(&output).unwrap();
    assert_eq!((picture.width(), picture.height()), (640, 480));

    // A target the quality level already meets takes one encode.
    let report = run(&input, &output, fit(ImageFormat::Jpeg, free * 2));
    assert_eq!(output_bytes(&report), free);
    let found = image_plan(&report).target.unwrap();
    assert_eq!((found.reached, found.attempts), (true, 1));
}

#[test]
fn the_picture_gets_smaller_only_when_quality_is_not_enough() {
    let (dir, input) = setup("shrink");
    // Far below what each format's lowest setting gives at 640x480.
    for (format, name, target) in [
        (ImageFormat::Jpeg, "small.jpg", 6_000),
        (ImageFormat::Webp, "small.webp", 1_200),
        (ImageFormat::Png, "small.png", 1_500),
    ] {
        let output = dir.0.join(name);
        let mut recipe = fit(format, target);
        recipe.measure_quality = true;
        let report = run(&input, &output, recipe);
        let bytes = output_bytes(&report);
        assert!(bytes <= target, "{name}: {bytes}");
        let found = image_plan(&report).target.unwrap();
        let (width, height) = found
            .scaled_to
            .unwrap_or_else(|| panic!("{name}: not scaled"));
        assert!(width < 640 && height < 480, "{name}: {width}x{height}");
        // The shape is kept.
        assert!(
            (width * 3).abs_diff(height * 4) <= 4,
            "{name}: {width}x{height}"
        );
        let picture = image::open(&output).unwrap();
        assert_eq!(
            (picture.width(), picture.height()),
            (width, height),
            "{name}"
        );
        // The result is scored against the picture at the size it was given.
        assert!(report.score.is_some(), "{name}: no score");
    }
}

#[test]
fn keeping_the_dimensions_fails_instead_of_scaling() {
    let (dir, input) = setup("keep");
    let output = dir.0.join("out.jpg");
    let mut recipe = fit(ImageFormat::Jpeg, 6_000);
    recipe.image.keep_dimensions = true;
    let report = run(&input, &output, recipe);
    let Outcome::Failed { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(
        message.contains("cannot fit under 6.00 kB") && message.contains("--keep-dimensions"),
        "{message}"
    );
    assert!(!output.exists(), "nothing is written");
    let found = image_plan(&report).target.unwrap();
    assert_eq!((found.reached, found.scaled_to), (false, None));
    assert_eq!(found.attempts, 2, "the level's setting, then the floor");
    // With enough room it fits at its size.
    let mut recipe = fit(ImageFormat::Jpeg, 60_000);
    recipe.image.keep_dimensions = true;
    assert!(output_bytes(&run(&input, &output, recipe)) <= 60_000);

    // A target nothing can reach fails too, and says how far it got.
    let report = run(&input, &output, fit(ImageFormat::Jpeg, 40));
    let Outcome::Failed { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("could not fit under 40 B"), "{message}");
}

#[test]
fn a_picture_within_the_target_is_compressed_as_usual() {
    let (dir, input) = setup("within");
    let size = std::fs::metadata(&input).unwrap().len();
    // Already small enough: the target is a limit, so the picture gets the usual compression
    // at the quality level, in one encode, and the result is kept because it is smaller.
    let mut usual_recipe = Recipe::default();
    usual_recipe.image.format = ImageFormat::Keep;
    let usual = output_bytes(&run(&input, &dir.0.join("usual.png"), usual_recipe));
    let report = run(
        &input,
        &dir.0.join("out.png"),
        fit(ImageFormat::Keep, size + 1),
    );
    assert_eq!(output_bytes(&report), usual);
    assert_eq!(image_plan(&report).target.unwrap().attempts, 1);
    // What cannot get smaller is kept, as without a target.
    let report = run(
        &dir.0.join("out.png"),
        &dir.0.join("again.png"),
        fit(ImageFormat::Keep, size + 1),
    );
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );

    // A score target takes the size target's place.
    let mut recipe = fit(ImageFormat::Jpeg, 6_000);
    recipe.image.target_ssimulacra2 = Some(60);
    let report = run(&input, &dir.0.join("scored.jpg"), recipe);
    assert!(output_bytes(&report) > 6_000);
    assert_eq!(image_plan(&report).target, None);

    // A lossless TIFF has no setting to lower: it fits as it is, or the file fails and says
    // that the size needs a lossy format.
    let tiff = dir.0.join("out.tiff");
    let report = run(&input, &tiff, fit(ImageFormat::Tiff, 6_000));
    let Outcome::Failed { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(
        message.contains("not reachable without lossy encoding"),
        "{message}"
    );
    assert!(!tiff.exists());
    let found = image_plan(&report).target.unwrap();
    assert_eq!(
        (found.reached, found.attempts, found.scaled_to),
        (false, 1, None)
    );
    let report = run(&input, &tiff, fit(ImageFormat::Tiff, 50_000_000));
    assert!(output_bytes(&report) <= 50_000_000);
    assert!(image_plan(&report).target.unwrap().reached);
}
