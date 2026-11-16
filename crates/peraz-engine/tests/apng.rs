//! Animated PNG, end to end. An animated PNG is read and written in process, so most of these
//! tests make their own inputs and need neither FFmpeg nor the fixtures; the conversions from a
//! GIF, an animated WebP, and a video are skipped (with a note) without FFmpeg.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::RgbaImage;
use peraz_core::{
    ExistingOutput, Job, KeepReason, MediaKind, Outcome, OutputTarget, Plan, Quality, Recipe,
    Report, VideoOutput,
};
use peraz_engine::apng::{Source, decode, png_animation};
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
    let dir = std::env::temp_dir().join(format!("peraz-apng-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(tools: Option<&Toolchain>, input: &Path, output: OutputTarget, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output,
        recipe,
        existing: ExistingOutput::Fail,
    };
    let caps = tools.map_or_else(Capabilities::default, |t| t.capabilities().unwrap());
    compress(tools, &caps, &job, &mut |_| {}, &NOT_CANCELLED)
}

fn at_quality(quality: Quality) -> Recipe {
    Recipe {
        quality,
        ..Recipe::default()
    }
}

const SIZE: (u32, u32) = (96, 64);

/// A sticker: a gradient disc on a transparent background, with a square that moves one step
/// per frame.
#[allow(clippy::cast_possible_truncation)]
fn sticker(step: u32) -> RgbaImage {
    RgbaImage::from_fn(SIZE.0, SIZE.1, |x, y| {
        let (dx, dy) = (x.abs_diff(48), y.abs_diff(32));
        if (step * 8..step * 8 + 12).contains(&x) && (26..38).contains(&y) {
            image::Rgba([250, 40, 40, 255])
        } else if dx * dx + dy * dy < 30 * 30 {
            image::Rgba([(x * 2) as u8, (y * 3) as u8, 90, 255])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    })
}

/// Writes whole frames as an animated PNG the way simple tools do: every frame in full, barely
/// compressed. Frame `i` is shown for `delays[i]` hundredths of a second.
fn write_apng(path: &Path, frames: &[RgbaImage], delays: &[u16], plays: u32) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, SIZE.0, SIZE.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fastest);
    encoder
        .set_animated(u32::try_from(frames.len()).unwrap(), plays)
        .unwrap();
    let mut writer = encoder.write_header().unwrap();
    for (frame, delay) in frames.iter().zip(delays) {
        writer.set_frame_delay(*delay, 100).unwrap();
        writer.write_image_data(frame.as_raw()).unwrap();
    }
    writer.finish().unwrap();
}

/// The whole frames of an animated PNG, each with its delay in milliseconds, and the play count.
fn read_apng(path: &Path) -> (Vec<(RgbaImage, f64)>, u32) {
    let frames = decode(path, Source::Apng).unwrap();
    let plays = frames.plays;
    let frames = frames
        .map(|frame| {
            let (pixels, delay) = frame.unwrap();
            (pixels, delay.milliseconds())
        })
        .collect();
    (frames, plays)
}

fn compressed(report: &Report) -> (PathBuf, u64) {
    let Outcome::Compressed {
        output,
        output_bytes,
    } = &report.outcome
    else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    (output.clone(), *output_bytes)
}

#[test]
fn an_animated_png_stays_one_with_every_frame_and_its_timing() {
    let dir = dir("keep");
    let pictures: Vec<RgbaImage> = (0..8).map(sticker).collect();
    let delays = [8, 8, 20, 8, 8, 8, 50, 8];
    let input = dir.0.join("sticker.png");
    write_apng(&input, &pictures, &delays, 3);
    // Read without FFmpeg, as an animation.
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Animation, "apng")
    );

    // The highest level is lossless.
    let report = run(
        None,
        &input,
        OutputTarget::Folder(dir.0.join(".")),
        at_quality(Quality::Highest),
    );
    let (output, output_bytes) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "sticker-1.png");
    assert!(output_bytes * 3 < report.input_bytes.unwrap(), "{report:?}");
    assert_eq!(report.kind, Some(MediaKind::Animation));
    let Some(Plan::Animated(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(
        (plan.format, plan.encoder, plan.width, plan.height),
        (VideoOutput::Apng, "apng", SIZE.0, SIZE.1)
    );
    assert_eq!(
        report.encoder.as_deref(),
        Some("oxipng (lossless) · 8 frames")
    );
    let (frames, plays) = read_apng(&output);
    assert_eq!(plays, 3);
    assert_eq!(frames.len(), 8);
    for ((frame, shown), (picture, delay)) in frames.iter().zip(pictures.iter().zip(delays)) {
        assert_eq!(frame, picture, "lossless");
        assert!((shown - f64::from(delay) * 10.0).abs() < 1e-6, "{shown}");
    }

    // What is already this small is kept as it is.
    let report = run(
        None,
        &output,
        OutputTarget::File(dir.0.join("again.png")),
        at_quality(Quality::Highest),
    );
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );

    // Lower levels may share a palette; the frames, timing, and shape stay.
    let report = run(
        None,
        &input,
        OutputTarget::File(dir.0.join("small.png")),
        at_quality(Quality::Smallest),
    );
    let (small, small_bytes) = compressed(&report);
    assert!(small_bytes <= output_bytes);
    let (frames, plays) = read_apng(&small);
    assert_eq!((frames.len(), plays), (8, 3));
    for ((frame, shown), (picture, delay)) in frames.iter().zip(pictures.iter().zip(delays)) {
        assert!((shown - f64::from(delay) * 10.0).abs() < 1e-6, "{shown}");
        for (got, want) in frame.pixels().zip(picture.pixels()) {
            assert_eq!(got[3], want[3], "transparency is exact");
        }
    }
}

#[test]
fn an_apng_named_apng_keeps_its_name_and_can_be_clipped_and_scaled() {
    let dir = dir("clip");
    let pictures: Vec<RgbaImage> = (0..8).map(sticker).collect();
    let input = dir.0.join("sticker.apng");
    write_apng(&input, &pictures, &[10; 8], 0);
    let mut recipe = at_quality(Quality::Highest);
    recipe.video.max_width = Some(48);
    // Frames start at 0, 100, …, 700 ms: this keeps the four that start from 200 ms on.
    recipe.clip.start_ms = Some(200);
    recipe.clip.duration_ms = Some(400);
    // The frame rate is the source's: `--fps` does not drop frames of an animation.
    recipe.gif.fps = Some(2);
    let report = run(None, &input, OutputTarget::Folder(dir.0.clone()), recipe);
    let (output, _) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "sticker-1.apng");
    let Some(Plan::Animated(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!((plan.width, plan.height, plan.fps), (48, 32, 10));
    let (frames, plays) = read_apng(&output);
    assert_eq!(plays, 0);
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].0.dimensions(), (48, 32));
    assert!(frames.iter().all(|(_, shown)| (shown - 100.0).abs() < 1e-6));
    let header = png_animation(&output).unwrap().unwrap();
    assert_eq!((header.width, header.height, header.frames), (48, 32, 4));
}

#[test]
fn an_animated_png_with_one_frame_is_compressed_as_a_png() {
    let dir = dir("single");
    let input = dir.0.join("still.png");
    write_apng(&input, &[sticker(0)], &[10], 0);
    assert_eq!(png_animation(&input).unwrap().unwrap().frames, 1);
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(info.kind, MediaKind::Image);
    let report = run(
        None,
        &input,
        OutputTarget::File(dir.0.join("still-1.png")),
        at_quality(Quality::Highest),
    );
    let (output, _) = compressed(&report);
    assert!(
        matches!(report.plan, Some(Plan::Image(_))),
        "{:?}",
        report.plan
    );
    assert_eq!(png_animation(&output).unwrap(), None, "a plain PNG");
    // The PNG pipeline may reduce it to a palette: the shape is exact, the colors close.
    let back = image::open(&output).unwrap().to_rgba8();
    for (got, want) in back.pixels().zip(sticker(0).pixels()) {
        assert_eq!(got[3], want[3]);
        if want[3] == 255 {
            for channel in 0..3 {
                assert!(
                    got[channel].abs_diff(want[channel]) < 24,
                    "{got:?} {want:?}"
                );
            }
        }
    }

    // FFmpeg agrees on what the file is.
    if let Ok(tools) = Toolchain::locate(None) {
        let info = probe(&tools, &input).unwrap();
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!(info.duration_seconds, None);
        let report = run(
            Some(&tools),
            &input,
            OutputTarget::File(dir.0.join("still-2.png")),
            at_quality(Quality::Highest),
        );
        compressed(&report);
        assert!(matches!(report.plan, Some(Plan::Image(_))));
    }
}

#[test]
fn other_animations_and_videos_become_animated_pngs_on_request() {
    let Ok(tools) = Toolchain::locate(None) else {
        eprintln!("skipped: converting to an animated PNG needs FFmpeg to read the input");
        return;
    };
    let dir = dir("convert");
    let pictures: Vec<RgbaImage> = (0..4).map(sticker).collect();
    let mut recipe = at_quality(Quality::Highest);
    recipe.video.output = VideoOutput::Apng;

    // An animated PNG read through FFmpeg is an animation too, and stays one by default.
    let apng = dir.0.join("sticker.png");
    write_apng(&apng, &pictures, &[10, 25, 7, 10], 0);
    let info = probe(&tools, &apng).unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Animation, "apng")
    );
    let report = run(
        Some(&tools),
        &apng,
        OutputTarget::File(dir.0.join("kept.png")),
        at_quality(Quality::Highest),
    );
    let (kept, _) = compressed(&report);
    assert_eq!(read_apng(&kept).0.len(), 4);

    // A GIF: every frame, with its own delay and its play count.
    let gif = dir.0.join("sticker.gif");
    {
        let file = std::fs::File::create(&gif).unwrap();
        let mut encoder = gif::Encoder::new(
            file,
            SIZE.0.try_into().unwrap(),
            SIZE.1.try_into().unwrap(),
            &[],
        )
        .unwrap();
        encoder.set_repeat(gif::Repeat::Finite(5)).unwrap();
        for (picture, centiseconds) in pictures.iter().zip([10u16, 25, 7, 10]) {
            let mut pixels = picture.clone().into_raw();
            let mut frame = gif::Frame::from_rgba_speed(
                SIZE.0.try_into().unwrap(),
                SIZE.1.try_into().unwrap(),
                &mut pixels,
                10,
            );
            frame.delay = centiseconds;
            frame.dispose = gif::DisposalMethod::Background;
            encoder.write_frame(&frame).unwrap();
        }
    }
    let report = run(
        Some(&tools),
        &gif,
        OutputTarget::Folder(dir.0.clone()),
        recipe.clone(),
    );
    let (output, _) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "sticker-1.apng");
    let (frames, plays) = read_apng(&output);
    assert_eq!(plays, 5);
    let shown: Vec<f64> = frames.iter().map(|(_, shown)| *shown).collect();
    assert_eq!(shown, [100.0, 250.0, 70.0, 100.0]);
    // The GIF's transparent background is still transparent.
    assert_eq!(frames[0].0.get_pixel(0, 0)[3], 0);
    assert_eq!(frames[0].0.get_pixel(48, 10)[3], 255);

    // An animated WebP, whatever this FFmpeg makes of it.
    let webp = dir.0.join("moving.webp");
    {
        let mut config = webp::WebPConfig::new().unwrap();
        config.lossless = 1;
        let mut encoder = webp::AnimEncoder::new(SIZE.0, SIZE.1, &config);
        for (picture, at) in pictures.iter().zip([0, 100, 350, 420]) {
            encoder.add_frame(webp::AnimFrame::from_rgba(
                picture.as_raw(),
                SIZE.0,
                SIZE.1,
                at,
            ));
        }
        std::fs::write(&webp, &*encoder.encode()).unwrap();
    }
    let report = run(
        Some(&tools),
        &webp,
        OutputTarget::File(dir.0.join("from-webp.apng")),
        recipe.clone(),
    );
    let (output, _) = compressed(&report);
    let (frames, _) = read_apng(&output);
    assert_eq!(frames.len(), 4);
    let shown: Vec<f64> = frames.iter().take(3).map(|(_, shown)| *shown).collect();
    assert_eq!(shown, [100.0, 250.0, 70.0]);
    assert_eq!(frames[0].0.get_pixel(0, 0)[3], 0);
}

#[test]
fn a_video_becomes_an_animated_png_at_the_planned_rate_and_size() {
    let Ok(tools) = Toolchain::locate(None) else {
        eprintln!("skipped: needs FFmpeg");
        return;
    };
    let dir = dir("video");
    let video = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/video-720p-noaudio.mp4");
    if !video.is_file() {
        eprintln!("skipped: run scripts/make-fixtures.sh");
        return;
    }
    let mut recipe = at_quality(Quality::Small);
    recipe.video.output = VideoOutput::Apng;
    recipe.video.max_width = Some(160);
    recipe.gif.fps = Some(5);
    recipe.clip.duration_ms = Some(2000);
    let report = run(
        Some(&tools),
        &video,
        OutputTarget::File(dir.0.join("clip.apng")),
        recipe,
    );
    let (output, _) = compressed(&report);
    let Some(Plan::Animated(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!((plan.width, plan.height, plan.fps), (160, 90, 5));
    let (frames, plays) = read_apng(&output);
    assert_eq!(plays, 0);
    let seconds: f64 = frames.iter().map(|(_, shown)| shown / 1000.0).sum();
    assert!((seconds - 2.0).abs() < 0.01, "{seconds}");
    assert_eq!(frames[0].0.dimensions(), (160, 90));
}

/// Older FFmpeg builds cannot read animated WebP: `ffprobe` fails on it, sees no picture, or
/// takes it for a still. Peraz reads the frames itself, so the result does not depend on that.
#[cfg(unix)]
#[test]
fn an_animated_webp_becomes_an_animated_png_whatever_ffprobe_makes_of_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("old-ffmpeg");
    let pictures: Vec<RgbaImage> = (0..4).map(sticker).collect();
    let webp = dir.0.join("moving.webp");
    {
        let mut config = webp::WebPConfig::new().unwrap();
        config.lossless = 1;
        let mut encoder = webp::AnimEncoder::new(SIZE.0, SIZE.1, &config);
        for (picture, at) in pictures.iter().zip([0, 100, 350, 420]) {
            encoder.add_frame(webp::AnimFrame::from_rgba(
                picture.as_raw(),
                SIZE.0,
                SIZE.1,
                at,
            ));
        }
        std::fs::write(&webp, &*encoder.encode()).unwrap();
    }
    let answers = [
        (
            "fails",
            "echo 'moving.webp: Invalid data found when processing input' >&2; exit 1",
        ),
        (
            "no-picture",
            r#"echo '{"streams":[{"codec_type":"video","codec_name":"webp"}],"format":{"format_name":"webp_pipe"}}'"#,
        ),
        (
            "still",
            r#"echo '{"streams":[{"codec_type":"video","codec_name":"webp","width":96,"height":64,"pix_fmt":"argb","nb_frames":"1"}],"format":{"format_name":"webp_pipe"}}'"#,
        ),
    ];
    for (name, answer) in answers {
        let ffprobe = dir.0.join(format!("ffprobe-{name}"));
        std::fs::write(&ffprobe, format!("#!/bin/sh\n{answer}\n")).unwrap();
        std::fs::set_permissions(&ffprobe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // FFmpeg itself is never run for this conversion.
        let tools = Toolchain {
            ffmpeg: dir.0.join("no-ffmpeg"),
            ffprobe,
            source: peraz_engine::ffmpeg::ToolSource::Flag,
        };
        let info = probe(&tools, &webp).unwrap();
        let picture = info.video.expect("a picture");
        assert_eq!((picture.width, picture.height), SIZE, "{name}");

        let mut recipe = at_quality(Quality::Highest);
        recipe.video.output = VideoOutput::Apng;
        let job = Job {
            input: webp.clone(),
            output: OutputTarget::File(dir.0.join(format!("{name}.apng"))),
            recipe,
            existing: ExistingOutput::Fail,
        };
        let report = compress(
            Some(&tools),
            &Capabilities::default(),
            &job,
            &mut |_| {},
            &NOT_CANCELLED,
        );
        let (output, _) = compressed(&report);
        let (frames, _) = read_apng(&output);
        assert_eq!(frames.len(), 4, "{name}");
        let shown: Vec<f64> = frames.iter().take(3).map(|(_, shown)| *shown).collect();
        assert_eq!(shown, [100.0, 250.0, 70.0], "{name}");
    }
}
