//! Results are marked as made by Peraz, and a marked file that has not changed is skipped
//! instead of being compressed a second time. The mark needs a file system with extended
//! attributes (or alternate data streams on Windows): without one, nothing is skipped.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use peraz_core::{ExistingOutput, Job, MediaKind, Outcome, OutputTarget, Recipe, SkipReason};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, recompress: bool) -> peraz_core::Report {
    let mut recipe = Recipe::default();
    recipe.image.format = peraz_core::ImageFormat::Jpeg;
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress,
    };
    compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

#[test]
fn a_result_is_not_compressed_twice_unless_asked_or_edited() {
    let dir: PathBuf = std::env::temp_dir().join(format!("peraz-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("photo.png");
    image::RgbImage::from_fn(200, 150, |x, y| {
        image::Rgb([
            u8::try_from(x).unwrap(),
            u8::try_from(y).unwrap(),
            u8::try_from((x ^ y) & 255).unwrap(),
        ])
    })
    .save(&input)
    .unwrap();

    let result = dir.join("photo-1.jpg");
    let report = run(&input, &result, false);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let bytes = std::fs::read(&result).unwrap();
    if !peraz_engine::mark::is_current(&result) {
        // No extended attributes here: the result is simply not marked.
        let report = run(&result, &dir.join("again.jpg"), false);
        assert!(!matches!(report.outcome, Outcome::Skipped { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }

    // The result is skipped, and nothing is read or written for it.
    let again = dir.join("again.jpg");
    let report = run(&result, &again, false);
    let Outcome::Skipped { reason, message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert_eq!(*reason, SkipReason::AlreadyCompressed);
    assert!(message.contains("--recompress"), "{message}");
    assert_eq!(report.input_bytes, Some(bytes.len() as u64));
    assert_eq!(report.kind, None);
    assert!(!again.exists());
    assert_eq!(
        std::fs::read(&result).unwrap(),
        bytes,
        "the mark is not content"
    );

    // On request it is compressed again.
    let report = run(&result, &again, true);
    assert!(
        !matches!(report.outcome, Outcome::Skipped { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.kind, Some(MediaKind::Image));

    // A copy that was edited afterwards is a new picture: compressed like any other.
    image::open(&input)
        .unwrap()
        .crop_imm(0, 0, 120, 90)
        .save(&result)
        .unwrap();
    let report = run(&result, &again, false);
    assert!(
        !matches!(report.outcome, Outcome::Skipped { .. }),
        "{:?}",
        report.outcome
    );
    // The original input was never a result.
    assert!(!peraz_engine::mark::is_current(&input));
    std::fs::remove_dir_all(&dir).unwrap();
}
