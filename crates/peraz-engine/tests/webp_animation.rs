//! Animated WebP stays animated WebP by default, re-encoded in process: every frame, its
//! timing, the loop count, and transparency are kept. Other formats are made on request, with
//! FFmpeg; that part is skipped (with a note) without it.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::RgbaImage;
use peraz_core::{
    ExistingOutput, Job, KeepReason, MediaKind, Outcome, OutputTarget, Plan, Quality, Recipe,
    Report, VideoOutput,
};
use peraz_engine::apng::{Source, decode, sniff};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::{Capabilities, Toolchain};
use peraz_engine::probe::{probe, probe_without_ffmpeg};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-webp-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(tools: Option<&Toolchain>, input: &Path, output: &Path, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let caps = tools.map_or_else(Capabilities::default, |t| t.capabilities().unwrap());
    compress(tools, &caps, &job, &mut |_| {}, &NOT_CANCELLED)
}

const SIZE: (u32, u32) = (160, 120);

/// A grainy picture with a transparent corner and a square that moves one step per frame.
#[allow(clippy::cast_possible_truncation)]
fn frame(step: u32) -> RgbaImage {
    RgbaImage::from_fn(SIZE.0, SIZE.1, |x, y| {
        if x < 40 && y < 30 {
            return image::Rgba([0, 0, 0, 0]);
        }
        if (step * 20..step * 20 + 24).contains(&x) && (50..74).contains(&y) {
            return image::Rgba([250, 40, 40, 255]);
        }
        // Grain without a pattern: costly to keep exactly, cheap to approximate.
        let mixed = x
            .wrapping_mul(374_761_393)
            .wrapping_add(y.wrapping_mul(668_265_263));
        let grain = ((mixed ^ (mixed >> 13)).wrapping_mul(1_274_126_177) >> 27) & 31;
        image::Rgba([
            (x + grain) as u8,
            (y * 2 + grain) as u8,
            (90 + grain) as u8,
            255,
        ])
    })
}

/// Writes a lossless animated WebP: five frames starting at these times, played three times.
fn write_animation(path: &Path) {
    let frames: Vec<RgbaImage> = (0..5).map(frame).collect();
    let mut config = webp::WebPConfig::new().unwrap();
    config.lossless = 1;
    let mut encoder = webp::AnimEncoder::new(SIZE.0, SIZE.1, &config);
    encoder.set_loop_count(3);
    for (pixels, at) in frames.iter().zip([0, 80, 200, 260, 500]) {
        encoder.add_frame(webp::AnimFrame::from_rgba(
            pixels.as_raw(),
            SIZE.0,
            SIZE.1,
            at,
        ));
    }
    std::fs::write(path, &*encoder.encode()).unwrap();
}

/// The frames of an animated WebP with how long each is shown (milliseconds), and its play
/// count.
fn read_animation(path: &Path) -> (Vec<(RgbaImage, f64)>, u32) {
    assert_eq!(sniff(path).unwrap(), Some(Source::Webp), "an animated WebP");
    let frames = decode(path, Source::Webp).unwrap();
    let plays = frames.plays;
    let frames = frames
        .map(|f| {
            let (pixels, delay) = f.unwrap();
            (pixels, delay.milliseconds())
        })
        .collect();
    (frames, plays)
}

#[test]
fn an_animated_webp_stays_one_with_its_frames_timing_and_loops() {
    let dir = dir("keep");
    let input = dir.0.join("moving.webp");
    write_animation(&input);
    let (source, plays) = read_animation(&input);
    assert_eq!((source.len(), plays), (5, 3));
    // Read without FFmpeg, as an animation.
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(info.kind, MediaKind::Animation);
    let stored = info.video.unwrap();
    assert_eq!(
        (stored.codec.as_str(), stored.width, stored.height),
        ("webp", SIZE.0, SIZE.1)
    );

    let output = dir.0.join("moving-1.webp");
    let recipe = Recipe {
        quality: Quality::Small,
        ..Recipe::default()
    };
    let report = run(None, &input, &output, recipe.clone());
    let Outcome::Compressed { output_bytes, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(*output_bytes < report.input_bytes.unwrap());
    assert_eq!(report.kind, Some(MediaKind::Animation));
    let Some(Plan::Animated(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(
        (plan.format, plan.extension, plan.width, plan.height),
        (VideoOutput::Webp, "webp", SIZE.0, SIZE.1)
    );

    let (frames, plays) = read_animation(&output);
    assert_eq!(plays, 3, "the loop count is kept");
    assert_eq!(frames.len(), source.len(), "every frame is kept");
    for ((pixels, shown), (wanted, wanted_shown)) in frames.iter().zip(&source) {
        assert!(
            (shown - wanted_shown).abs() < 1.0,
            "{shown} ms for {wanted_shown} ms"
        );
        // Transparent stays transparent, and the picture stays that picture.
        assert!(pixels.get_pixel(10, 10)[3] < 16);
        assert!(pixels.get_pixel(100, 100)[3] > 240);
        let (got, want) = (pixels.get_pixel(100, 100), wanted.get_pixel(100, 100));
        assert!(
            (0..3).all(|c| got[c].abs_diff(want[c]) < 48),
            "{got:?} for {want:?}"
        );
    }

    // `--format webp` asks for the same; a result that cannot get smaller is kept.
    let mut asked = recipe;
    asked.video.output = VideoOutput::Webp;
    let again = dir.0.join("again.webp");
    let report = run(None, &input, &again, asked);
    assert!(matches!(report.outcome, Outcome::Compressed { .. }));
    let report = run(
        None,
        &output,
        &dir.0.join("twice.webp"),
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

    // A smaller size on request, and only a part of it.
    let mut smaller = Recipe::default();
    smaller.video.max_width = Some(80);
    smaller.clip.start_ms = Some(100);
    let half = dir.0.join("half.webp");
    let report = run(None, &input, &half, smaller);
    assert!(matches!(report.outcome, Outcome::Compressed { .. }));
    let (frames, _) = read_animation(&half);
    assert_eq!(frames.len(), 3, "the frames that start from 100 ms on");
    assert_eq!(frames[0].0.dimensions(), (80, 60));
}

#[test]
fn other_formats_are_made_from_it_on_request() {
    let Ok(tools) = Toolchain::locate(None) else {
        eprintln!("skipped: converting an animated WebP needs FFmpeg");
        return;
    };
    let dir = dir("convert");
    let input = dir.0.join("moving.webp");
    write_animation(&input);
    let info = probe(&tools, &input).unwrap();
    assert_eq!(
        info.kind,
        MediaKind::Animation,
        "whatever this FFmpeg reads"
    );
    let caps = tools.capabilities().unwrap();
    for (format, name, kind, codec) in [
        (VideoOutput::Gif, "out.gif", MediaKind::Animation, "gif"),
        (VideoOutput::Apng, "out.apng", MediaKind::Animation, "apng"),
        (VideoOutput::Mp4, "out.mp4", MediaKind::Video, "h264"),
    ] {
        if format == VideoOutput::Mp4 && !caps.has_encoder("libx264") {
            continue;
        }
        let mut recipe = Recipe::default();
        recipe.video.output = format;
        let output = dir.0.join(name);
        let report = run(Some(&tools), &input, &output, recipe);
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{name}: {:?}",
            report.outcome
        );
        let made = probe(&tools, &output).unwrap();
        assert_eq!(made.kind, kind, "{name}");
        assert_eq!(made.video.unwrap().codec, codec, "{name}");
    }
}
