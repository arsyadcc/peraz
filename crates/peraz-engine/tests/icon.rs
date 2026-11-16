//! Icon files (ICO, CUR, ICNS), end to end. They are read and written in process, so these tests
//! build their own files and need neither FFmpeg nor the fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::RgbaImage;
use peraz_core::{
    ExistingOutput, ImageEncoding, ImageFormat, Job, KeepReason, MediaKind, Outcome, OutputTarget,
    Plan, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;
use peraz_engine::icon::{Container, sniff};
use peraz_engine::probe::probe_without_ffmpeg;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1A\n";

/// An app icon at `size` pixels: a shaded rounded shape with a soft edge on a transparent
/// background, and a little grain.
#[allow(clippy::cast_possible_truncation)]
fn picture(size: u32) -> RgbaImage {
    RgbaImage::from_fn(size, size, |x, y| {
        let half = size / 2;
        let (dx, dy) = (x.abs_diff(half), y.abs_diff(half));
        let distance = dx * dx + dy * dy;
        let edge = half * half * 4 / 5;
        if distance > edge {
            return image::Rgba([0, 0, 0, 0]);
        }
        let grain = x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 27;
        let alpha = if distance * 20 > edge * 19 { 128 } else { 255 };
        image::Rgba([
            (x * 200 / size + grain) as u8,
            (y * 200 / size + grain) as u8,
            180,
            alpha,
        ])
    })
}

/// A PNG as simple tools write it: correct, barely compressed.
fn loose_png(pixels: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, pixels.width(), pixels.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fastest);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(pixels.as_raw()).unwrap();
    writer.finish().unwrap();
    out
}

/// A 32-bit bitmap as an ICO stores it: the header, the rows bottom-up as BGRA, then the mask.
fn bitmap(pixels: &RgbaImage) -> Vec<u8> {
    let (width, height) = pixels.dimensions();
    let mut out = Vec::new();
    out.extend(40u32.to_le_bytes());
    out.extend(width.to_le_bytes());
    out.extend((height * 2).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend([0u8; 24]);
    for y in (0..height).rev() {
        for x in 0..width {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            out.extend([b, g, r, a]);
        }
    }
    // One mask bit per pixel, rows padded to 32 bits; unused with 32-bit color.
    let mask_row = width.div_ceil(32) as usize * 4;
    out.extend(std::iter::repeat_n(0u8, mask_row * height as usize));
    out
}

/// Builds an ICO (`kind` 1) or CUR (`kind` 2) file from pictures with their directory fields:
/// size in pixels, planes (or hotspot x), bit count (or hotspot y), and data.
fn ico_file(kind: u16, pictures: &[(u32, u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend(kind.to_le_bytes());
    out.extend(u16::try_from(pictures.len()).unwrap().to_le_bytes());
    let mut offset = 6 + pictures.len() * 16;
    for (size, planes, bits, data) in pictures {
        // 256 pixels is written as 0.
        let side = u8::try_from(*size % 256).unwrap();
        out.extend([side, side, 0, 0]);
        out.extend(planes.to_le_bytes());
        out.extend(bits.to_le_bytes());
        out.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend(u32::try_from(offset).unwrap().to_le_bytes());
        offset += data.len();
    }
    for (_, _, _, data) in pictures {
        out.extend_from_slice(data);
    }
    out
}

/// The directory of an ICO or CUR file: per picture, the width and height bytes, planes, bit
/// count, and data.
fn read_ico(bytes: &[u8]) -> Vec<(u8, u8, u16, u16, &[u8])> {
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };
    (0..usize::from(u16_at(4)))
        .map(|index| {
            let at = 6 + index * 16;
            let (size, offset) = (u32_at(at + 8), u32_at(at + 12));
            (
                bytes[at],
                bytes[at + 1],
                u16_at(at + 4),
                u16_at(at + 6),
                &bytes[offset..offset + size],
            )
        })
        .collect()
}

/// Builds an ICNS file from elements.
fn icns_file(elements: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"icns\0\0\0\0".to_vec();
    for (kind, data) in elements {
        out.extend_from_slice(*kind);
        out.extend(u32::try_from(data.len() + 8).unwrap().to_be_bytes());
        out.extend_from_slice(data);
    }
    let total = u32::try_from(out.len()).unwrap();
    out[4..8].copy_from_slice(&total.to_be_bytes());
    out
}

/// The elements of an ICNS file, after checking its length field.
fn read_icns(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    assert_eq!(&bytes[..4], b"icns");
    let be = |at: usize| {
        u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };
    assert_eq!(be(4), bytes.len(), "the length field covers the whole file");
    let mut elements = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let length = be(at + 4);
        elements.push((
            [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]],
            &bytes[at + 8..at + length],
        ));
        at += length;
    }
    elements
}

/// A table of contents for the elements that follow it.
fn contents(elements: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, data) in elements {
        out.extend_from_slice(*kind);
        out.extend(u32::try_from(data.len() + 8).unwrap().to_be_bytes());
    }
    out
}

/// Decodes a PNG picture from an icon, checking that it is the 8-bit RGBA kind icons need.
fn icon_pixels(png: &[u8]) -> RgbaImage {
    assert!(png.starts_with(PNG_SIGNATURE));
    assert_eq!((png[24], png[25]), (8, 6), "8 bits per channel, RGBA");
    image::load_from_memory(png).unwrap().to_rgba8()
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-icon-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: OutputTarget, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output,
        recipe,
        existing: ExistingOutput::Fail,
    };
    compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

fn at_quality(quality: Quality) -> Recipe {
    Recipe {
        quality,
        ..Recipe::default()
    }
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
fn an_ico_keeps_every_size_and_shrinks_its_large_pictures() {
    let dir = dir("ico");
    let small = bitmap(&picture(16));
    let medium = bitmap(&picture(48));
    let file = ico_file(
        1,
        &[
            (16, 1, 32, small.clone()),
            (48, 1, 32, medium.clone()),
            (64, 1, 32, loose_png(&picture(64))),
            (256, 1, 32, bitmap(&picture(256))),
        ],
    );
    let input = dir.0.join("app.ico");
    std::fs::write(&input, &file).unwrap();
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Image, "ico")
    );
    let largest = info.video.unwrap();
    assert_eq!(
        (largest.codec.as_str(), largest.width, largest.height),
        ("ico", 256, 256)
    );

    let report = run(
        &input,
        OutputTarget::Folder(dir.0.clone()),
        at_quality(Quality::Highest),
    );
    let (output, output_bytes) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "app-1.ico");
    assert!(output_bytes * 2 < report.input_bytes.unwrap(), "{report:?}");
    assert_eq!(
        report.encoder.as_deref(),
        Some("oxipng (lossless) · 2 of 4 pictures recompressed")
    );
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(
        plan.encoding,
        ImageEncoding::Icon {
            palette_quality: None
        }
    );

    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[..6], [0, 0, 1, 0, 4, 0], "an ICO with four pictures");
    let pictures = read_ico(&bytes);
    // Small bitmaps stay as they are, for programs that read nothing else.
    assert_eq!(pictures[0], (16, 16, 1, 32, small.as_slice()));
    assert_eq!(pictures[1], (48, 48, 1, 32, medium.as_slice()));
    // The PNG is recompressed and the large bitmap becomes one, pixel for pixel.
    assert_eq!((pictures[2].0, pictures[2].1), (64, 64));
    assert_eq!(icon_pixels(pictures[2].4), picture(64));
    assert_eq!(
        (pictures[3].0, pictures[3].1, pictures[3].2, pictures[3].3),
        (0, 0, 1, 32),
        "256 pixels is written as 0"
    );
    assert_eq!(icon_pixels(pictures[3].4), picture(256));
    // Another reader opens it and finds the largest picture.
    let largest = image::load_from_memory_with_format(&bytes, image::ImageFormat::Ico).unwrap();
    assert_eq!(largest.to_rgba8(), picture(256));

    // Compressed once, it stays as it is.
    let report = run(
        &output,
        OutputTarget::File(dir.0.join("again.ico")),
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

    // A lower level reduces the colors, and the pictures stay 8-bit RGBA at their sizes.
    let report = run(
        &input,
        OutputTarget::File(dir.0.join("small.ico")),
        at_quality(Quality::Small),
    );
    let (small_file, small_bytes) = compressed(&report);
    // Never larger: reduced colors are used only where they make a picture smaller.
    assert!(small_bytes <= output_bytes);
    let bytes = std::fs::read(small_file).unwrap();
    let pictures = read_ico(&bytes);
    assert_eq!(pictures.len(), 4);
    assert_eq!(icon_pixels(pictures[3].4).dimensions(), (256, 256));
    for (got, want) in icon_pixels(pictures[2].4)
        .pixels()
        .zip(picture(64).pixels())
    {
        assert!(got[3].abs_diff(want[3]) < 32, "{got:?} {want:?}");
    }

    // It is not turned into a single picture.
    let mut recipe = Recipe::default();
    recipe.image.format = ImageFormat::Png;
    let report = run(&input, OutputTarget::File(dir.0.join("out.png")), recipe);
    let Outcome::Skipped { message } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("not converted to png"), "{message}");
}

#[test]
fn a_cursor_keeps_its_hotspots() {
    let dir = dir("cur");
    let small = bitmap(&picture(32));
    let file = ico_file(
        2,
        &[
            (32, 4, 7, small.clone()),
            (64, 9, 15, loose_png(&picture(64))),
            (256, 100, 120, bitmap(&picture(256))),
        ],
    );
    let input = dir.0.join("arrow.cur");
    std::fs::write(&input, &file).unwrap();
    assert_eq!(sniff(&input).unwrap().unwrap().container, Container::Cur);
    let report = run(
        &input,
        OutputTarget::Folder(dir.0.clone()),
        at_quality(Quality::Highest),
    );
    let (output, _) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "arrow-1.cur");
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[..6], [0, 0, 2, 0, 3, 0], "a CUR with three pictures");
    let pictures = read_ico(&bytes);
    assert_eq!(pictures[0], (32, 32, 4, 7, small.as_slice()));
    assert_eq!((pictures[1].2, pictures[1].3), (9, 15));
    assert_eq!(icon_pixels(pictures[1].4), picture(64));
    assert_eq!((pictures[2].2, pictures[2].3), (100, 120));
    assert_eq!(icon_pixels(pictures[2].4), picture(256));
}

#[test]
fn an_icns_keeps_its_elements_and_shrinks_its_pngs() {
    let dir = dir("icns");
    // Old run-length pictures and masks, a version tag, and JPEG 2000 data: not touched.
    let old_picture: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    let mask = vec![0xAA; 1024];
    let jpeg_2000 = [&b"\x00\x00\x00\x0CjP  \r\n\x87\n"[..], &[0x55; 400]].concat();
    let dark = icns_file(&[(b"ic07", loose_png(&picture(128)))]);
    let elements: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"icnV", vec![0x42, 0x02, 0, 0]),
        (b"il32", old_picture.clone()),
        (b"l8mk", mask.clone()),
        (b"ic07", loose_png(&picture(128))),
        (b"ic13", loose_png(&picture(256))),
        (b"ic09", jpeg_2000.clone()),
        (b"\xFD\xD9\x2F\xA8", dark),
    ];
    let mut with_contents = vec![(b"TOC ", contents(&elements))];
    with_contents.extend(elements);
    let file = icns_file(&with_contents);
    let input = dir.0.join("App.icns");
    std::fs::write(&input, &file).unwrap();
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Image, "icns")
    );
    let largest = info.video.unwrap();
    assert_eq!(
        (largest.codec.as_str(), largest.width, largest.height),
        ("icns", 512, 512)
    );

    let report = run(
        &input,
        OutputTarget::Folder(dir.0.clone()),
        at_quality(Quality::Highest),
    );
    let (output, output_bytes) = compressed(&report);
    assert_eq!(output.file_name().unwrap(), "App-1.icns");
    assert!(output_bytes < report.input_bytes.unwrap());
    // Six pictures: two old ones, three PNGs (one in the dark variant), one JPEG 2000.
    assert_eq!(
        report.encoder.as_deref(),
        Some("oxipng (lossless) · 3 of 6 pictures recompressed")
    );

    let bytes = std::fs::read(&output).unwrap();
    let after = read_icns(&bytes);
    let kinds: Vec<[u8; 4]> = after.iter().map(|(kind, _)| *kind).collect();
    let before: Vec<[u8; 4]> = with_contents.iter().map(|(kind, _)| **kind).collect();
    assert_eq!(kinds, before, "the same elements in the same order");
    assert_eq!(after[1].1, [0x42, 0x02, 0, 0]);
    assert_eq!(after[2].1, old_picture.as_slice());
    assert_eq!(after[3].1, mask.as_slice());
    assert_eq!(icon_pixels(after[4].1), picture(128));
    assert_eq!(icon_pixels(after[5].1), picture(256));
    assert_eq!(after[6].1, jpeg_2000.as_slice());
    // The dark variant is an icon file of its own, compressed the same way.
    let dark = read_icns(after[7].1);
    assert_eq!(dark.len(), 1);
    assert_eq!(icon_pixels(dark[0].1), picture(128));
    // The table of contents lists what follows it, with the new lengths.
    let mut listed = Vec::new();
    for (kind, data) in &after[1..] {
        listed.extend_from_slice(kind);
        listed.extend(u32::try_from(data.len() + 8).unwrap().to_be_bytes());
    }
    assert_eq!(after[0].1, listed.as_slice());

    // A lower level reduces colors; every element is still there.
    let report = run(
        &input,
        OutputTarget::File(dir.0.join("small.icns")),
        at_quality(Quality::Balanced),
    );
    let (small, small_bytes) = compressed(&report);
    // Never larger: reduced colors are used only where they make a picture smaller.
    assert!(small_bytes <= output_bytes);
    let bytes = std::fs::read(small).unwrap();
    let elements = read_icns(&bytes);
    assert_eq!(elements.len(), 8);
    assert_eq!(icon_pixels(elements[5].1).dimensions(), (256, 256));
}

#[test]
fn other_files_are_not_icons() {
    let dir = dir("not");
    // A video stream can start with the same four bytes as an ICO.
    let files: [(&str, Vec<u8>); 4] = [
        ("stream.m2v", [&[0, 0, 1, 0][..], &[0x12; 600]].concat()),
        ("empty.ico", vec![0, 0, 1, 0, 0, 0]),
        (
            "cut.ico",
            ico_file(1, &[(16, 1, 32, bitmap(&picture(16)))])[..200].to_vec(),
        ),
        ("cut.icns", b"icns\0\0\x10\0ic07".to_vec()),
    ];
    for (name, bytes) in files {
        let path = dir.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(sniff(&path).unwrap(), None, "{name}");
    }
}
