//! GIF to GIF, in process: every frame with its exact delay, the loop count (also "play
//! once"), transparency, and what the disposal methods leave on screen. The inputs are made
//! here with the `gif` crate; nothing needs FFmpeg.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::RgbaImage;
use peraz_core::{
    ExistingOutput, Job, MediaKind, Outcome, OutputTarget, Plan, Quality, Recipe, Report,
};
use peraz_engine::apng::{Source, decode};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;
use peraz_engine::probe::probe_without_ffmpeg;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-gif-{test}-{}", std::process::id()));
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

const SIZE: (u16, u16) = (120, 90);

/// A grainy opaque picture with a square that moves one step per frame.
#[allow(clippy::cast_possible_truncation)]
fn moving(step: u32) -> RgbaImage {
    RgbaImage::from_fn(u32::from(SIZE.0), u32::from(SIZE.1), |x, y| {
        if (step * 12..step * 12 + 16).contains(&x) && (30..46).contains(&y) {
            return image::Rgba([250, 40, 40, 255]);
        }
        let mixed = x
            .wrapping_mul(374_761_393)
            .wrapping_add(y.wrapping_mul(668_265_263));
        let grain = ((mixed ^ (mixed >> 13)).wrapping_mul(1_274_126_177) >> 28) & 15;
        image::Rgba([
            (x * 2 + grain) as u8,
            (y * 2 + grain) as u8,
            (90 + grain) as u8,
            255,
        ])
    })
}

/// Writes a GIF: each frame with its own palette, its delay in hundredths of a second, and
/// how it is cleared before the next one. `repeat`: `None` writes no loop count at all, which
/// means "play once".
fn write_gif(
    path: &Path,
    frames: &[(RgbaImage, u16, gif::DisposalMethod)],
    repeat: Option<gif::Repeat>,
) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = gif::Encoder::new(file, SIZE.0, SIZE.1, &[]).unwrap();
    if let Some(repeat) = repeat {
        encoder.set_repeat(repeat).unwrap();
    }
    for (pixels, delay, dispose) in frames {
        let mut raw = pixels.clone().into_raw();
        let mut frame = gif::Frame::from_rgba_speed(SIZE.0, SIZE.1, &mut raw, 10);
        frame.delay = *delay;
        frame.dispose = *dispose;
        encoder.write_frame(&frame).unwrap();
    }
}

/// What a viewer sees of a GIF: each frame as the whole picture, with how long it is shown
/// (in milliseconds).
fn shown(path: &Path) -> Vec<(RgbaImage, f64)> {
    decode(path, Source::Gif)
        .unwrap()
        .map(|frame| {
            let (pixels, delay) = frame.unwrap();
            (pixels, delay.milliseconds())
        })
        .collect()
}

/// The loop count stored in a GIF file: `Finite(0)` when it stores none.
fn repeat_of(path: &Path) -> gif::Repeat {
    let file = std::fs::File::open(path).unwrap();
    let mut decoder = gif::DecodeOptions::new().read_info(file).unwrap();
    // The loop count comes before the first frame.
    let _ = decoder.next_frame_info();
    decoder.repeat()
}

fn compressed(report: &Report) -> u64 {
    let Outcome::Compressed { output_bytes, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    *output_bytes
}

#[test]
fn every_delay_and_the_loop_count_are_kept_exactly() {
    let dir = dir("delays");
    let keep = gif::DisposalMethod::Keep;
    // Delays as GIFs have them: a zero and a one (which browsers show in their own way), a
    // few usual ones, and a long hold.
    let delays = [0u16, 1, 7, 10, 3, 250];
    let frames: Vec<_> = delays
        .iter()
        .enumerate()
        .map(|(i, delay)| (moving(u32::try_from(i).unwrap()), *delay, keep))
        .collect();
    for (name, repeat) in [
        ("thrice", Some(gif::Repeat::Finite(3))),
        ("forever", Some(gif::Repeat::Infinite)),
        ("once", None),
    ] {
        let input = dir.0.join(format!("{name}.gif"));
        write_gif(&input, &frames, repeat);
        // Read without FFmpeg, as an animation.
        let info = probe_without_ffmpeg(&input).unwrap().unwrap();
        assert_eq!(info.kind, MediaKind::Animation, "{name}");
        let output = dir.0.join(format!("{name}-1.gif"));
        let report = run(
            &input,
            &output,
            Recipe {
                quality: Quality::Small,
                ..Recipe::default()
            },
        );
        assert!(compressed(&report) < report.input_bytes.unwrap(), "{name}");
        assert!(matches!(report.plan, Some(Plan::Gif(_))), "{name}");
        assert_eq!(
            repeat_of(&output),
            repeat_of(&input),
            "{name}: the loop count"
        );
        let (before, after) = (shown(&input), shown(&output));
        assert_eq!(after.len(), before.len(), "{name}: every frame");
        for (i, ((_, got), (_, want))) in after.iter().zip(&before).enumerate() {
            assert!(
                (got - want).abs() < 1e-6,
                "{name}, frame {i}: {got} ms for {want} ms"
            );
        }
        assert_eq!(after[0].0.dimensions(), (120, 90), "{name}: the size");
    }
    assert_eq!(repeat_of(&dir.0.join("once-1.gif")), gif::Repeat::Finite(0));
    assert_eq!(
        repeat_of(&dir.0.join("thrice-1.gif")),
        gif::Repeat::Finite(3)
    );
}

#[test]
fn transparency_and_disposal_show_the_same_pictures() {
    let dir = dir("disposal");
    // A sticker on a transparent background, drawn in ways that depend on how each frame is
    // cleared: left in place, cleared to the background, and put back to what was before.
    let sticker = |step: u32| {
        RgbaImage::from_fn(u32::from(SIZE.0), u32::from(SIZE.1), |x, y| {
            let (dx, dy) = (x.abs_diff(20 + step * 20), y.abs_diff(45));
            if dx * dx + dy * dy < 15 * 15 {
                image::Rgba([40, u8::try_from(60 + step * 40).unwrap(), 220, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        })
    };
    let frames = vec![
        (sticker(0), 10, gif::DisposalMethod::Keep),
        (sticker(1), 10, gif::DisposalMethod::Background),
        (sticker(2), 10, gif::DisposalMethod::Previous),
        (sticker(3), 10, gif::DisposalMethod::Keep),
        (sticker(4), 10, gif::DisposalMethod::Any),
    ];
    let input = dir.0.join("sticker.gif");
    write_gif(&input, &frames, Some(gif::Repeat::Infinite));
    let output = dir.0.join("sticker-1.gif");
    let report = run(&input, &output, Recipe::default());
    assert!(
        !matches!(
            report.outcome,
            Outcome::Failed { .. } | Outcome::Skipped { .. }
        ),
        "{:?}",
        report.outcome
    );
    if !matches!(report.outcome, Outcome::Compressed { .. }) {
        // Already as small as it gets: the original stays, which shows the same by definition.
        return;
    }
    let (before, after) = (shown(&input), shown(&output));
    assert_eq!(after.len(), before.len());
    for (i, ((got, _), (want, _))) in after.iter().zip(&before).enumerate() {
        let mut wrong = 0;
        for (g, w) in got.pixels().zip(want.pixels()) {
            let same_shape = (g[3] < 128) == (w[3] < 128);
            let same_color = w[3] < 128 || (0..3).all(|c| g[c].abs_diff(w[c]) < 40);
            wrong += usize::from(!(same_shape && same_color));
        }
        assert_eq!(wrong, 0, "frame {i} shows something else");
    }
}

#[test]
fn a_lower_frame_rate_on_request_keeps_the_length() {
    let dir = dir("fps");
    let keep = gif::DisposalMethod::Keep;
    // Twenty frames of 5 hundredths each: one second at 20 frames per second.
    let frames: Vec<_> = (0..20).map(|i| (moving(i % 8), 5u16, keep)).collect();
    let input = dir.0.join("fast.gif");
    write_gif(&input, &frames, Some(gif::Repeat::Infinite));
    let mut recipe = Recipe::default();
    recipe.gif.fps = Some(5);
    recipe.video.max_width = Some(60);
    let output = dir.0.join("slow.gif");
    let report = run(&input, &output, recipe);
    compressed(&report);
    let after = shown(&output);
    assert_eq!(
        after.len(),
        5,
        "five frames for one second at 5 frames per second"
    );
    let total: f64 = after.iter().map(|(_, shown)| shown).sum();
    assert!((total - 1000.0).abs() < 1e-6, "{total} ms");
    assert_eq!(
        after[0].0.dimensions(),
        (60, 44),
        "an even height, as for every GIF"
    );
    // A rate that does not divide the source's: twelve of the twenty frames, each with the
    // time of the frames dropped after it, and the same second in all.
    let mut recipe = Recipe::default();
    recipe.gif.fps = Some(12);
    let output = dir.0.join("twelve.gif");
    compressed(&run(&input, &output, recipe));
    let after = shown(&output);
    assert_eq!(after.len(), 12);
    let total: f64 = after.iter().map(|(_, shown)| shown).sum();
    assert!((total - 1000.0).abs() < 1e-6, "{total} ms");
    assert!(
        after.iter().all(|(_, shown)| [50.0, 100.0].contains(shown)),
        "one or two source frames each"
    );
}

#[test]
fn a_gif_with_one_frame_is_a_picture() {
    let dir = dir("still");
    let input = dir.0.join("still.gif");
    write_gif(&input, &[(moving(0), 0, gif::DisposalMethod::Keep)], None);
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!((info.kind, info.duration_seconds), (MediaKind::Image, None));
    // It becomes a PNG when that is smaller, without FFmpeg.
    let output = dir.0.join("still-1.png");
    let report = run(&input, &output, Recipe::default());
    assert!(
        !matches!(
            report.outcome,
            Outcome::Failed { .. } | Outcome::Skipped { .. }
        ),
        "{:?}",
        report.outcome
    );
    assert!(
        matches!(report.plan, Some(Plan::Image(_))),
        "{:?}",
        report.plan
    );
    if matches!(report.outcome, Outcome::Compressed { .. }) {
        let picture = image::open(&output).unwrap();
        assert_eq!((picture.width(), picture.height()), (120, 90));
    }
}
