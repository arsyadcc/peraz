//! JPEG details: an HDR gain map stored after the picture, grayscale, and metadata that needs
//! care when it is kept. The inputs are made here; the JPEG XL part needs libjxl's tools and is
//! skipped (with a note) without them.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::{DynamicImage, ImageDecoder as _};
use peraz_core::{
    ExistingOutput, ImageFormat, Job, Outcome, OutputTarget, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;
use peraz_engine::metadata::{Metadata, has_gain_map, jpeg_end};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-jpeg-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, format: ImageFormat, quality: Quality, keep: bool) -> Report {
    let mut recipe = Recipe {
        quality,
        ..Recipe::default()
    };
    recipe.image.format = format;
    recipe.image.keep_metadata = keep;
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let report = compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{}: {:?}",
        output.display(),
        report.outcome
    );
    report
}

/// A photo-like JPEG, `width` by `height`.
fn jpeg(width: u32, height: u32, gray: bool) -> Vec<u8> {
    let pixels = image::RgbImage::from_fn(width, height, |x, y| {
        let grain = (x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 28) & 15;
        let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
        image::Rgb([
            channel(x * 255 / width + grain),
            channel(y * 255 / height + grain),
            channel(90 + grain),
        ])
    });
    let image = if gray {
        DynamicImage::ImageLuma8(DynamicImage::ImageRgb8(pixels).to_luma8())
    } else {
        DynamicImage::ImageRgb8(pixels)
    };
    let mut out = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    out
}

fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
    let length = u16::try_from(body.len() + 2).unwrap().to_be_bytes();
    [&[0xFF, marker], &length[..], body].concat()
}

/// `jpeg` with segments added right after its start.
fn with_segments(jpeg: &[u8], segments: &[Vec<u8>]) -> Vec<u8> {
    [&jpeg[..2], &segments.concat(), &jpeg[2..]].concat()
}

fn xmp_segment(packet: &str) -> Vec<u8> {
    segment(
        0xE1,
        &[b"http://ns.adobe.com/xap/1.0/\0", packet.as_bytes()].concat(),
    )
}

/// A JPEG as phones write HDR photos: the picture, marked as having a gain map and with an
/// index of the pictures in the file (MPF), then the gain map as a second JPEG after it.
fn ultra_hdr() -> (Vec<u8>, Vec<u8>) {
    let main = with_segments(
        &jpeg(240, 160, false),
        &[
            xmp_segment(
                "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:Description \
                 xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" \
                 hdrgm:Version=\"1.0\"/></x:xmpmeta>",
            ),
            segment(0xE2, b"MPF\0MM\0\x2A\0\0\0\x08 an index of two pictures"),
        ],
    );
    let gain_map = with_segments(
        &jpeg(60, 40, true),
        &[xmp_segment(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:Description \
             xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" hdrgm:Version=\"1.0\" \
             hdrgm:GainMapMax=\"2.5\"/></x:xmpmeta>",
        )],
    );
    ([main.as_slice(), gain_map.as_slice()].concat(), main)
}

#[test]
fn the_end_of_the_picture_and_a_gain_map_after_it_are_found() {
    let (file, main) = ultra_hdr();
    assert_eq!(jpeg_end(&file), Some(main.len()));
    assert_eq!(jpeg_end(&main), Some(main.len()));
    assert!(has_gain_map(&file));
    assert!(!has_gain_map(&jpeg(64, 48, false)));
    // Another picture after the end that is not a gain map (a preview) does not count.
    let with_preview = [jpeg(64, 48, false), jpeg(16, 12, false)].concat();
    assert!(!has_gain_map(&with_preview));
    assert_eq!(jpeg_end(&with_preview), Some(jpeg(64, 48, false).len()));
    // Bytes that only look like an end marker inside the compressed data do not end it:
    // there, a 0xFF byte is always followed by a zero.
    assert_eq!(jpeg_end(b"not a jpeg"), None);
    assert_eq!(
        jpeg_end(&main[..main.len() / 2]),
        None,
        "cut off before its end"
    );
}

#[test]
fn a_gain_map_is_kept_in_a_jpeg_and_reported_where_it_is_left_out() {
    let dir = dir("gain-map");
    let (file, main) = ultra_hdr();
    let input = dir.0.join("hdr.jpg");
    std::fs::write(&input, &file).unwrap();
    let plain = dir.0.join("sdr.jpg");
    std::fs::write(&plain, &main).unwrap();

    // Encoded anew as a JPEG: the picture, then its gain map, and nothing after that.
    let output = dir.0.join("out.jpg");
    let report = run(&input, &output, ImageFormat::Jpeg, Quality::Small, false);
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(report.warnings, Vec::<String>::new());
    assert!(has_gain_map(&bytes));
    let picture_end = jpeg_end(&bytes).unwrap();
    assert_eq!(
        jpeg_end(&bytes[picture_end..]),
        Some(bytes.len() - picture_end),
        "the gain map is the last thing in the file"
    );
    // In a format that has no place for it, the report says that it was removed.
    let report = run(
        &input,
        &dir.0.join("out.png"),
        ImageFormat::Png,
        Quality::Small,
        false,
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("HDR gain map")),
        "{:?}",
        report.warnings
    );
    // A photo without one has nothing to report, and nothing after its end.
    let report = run(&plain, &output, ImageFormat::Jpeg, Quality::Small, false);
    assert_eq!(report.warnings, Vec::<String>::new());
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(jpeg_end(&bytes), Some(bytes.len()));

    if peraz_engine::jxl::JxlTools::locate().is_none() || peraz_engine::jxl::djxl().is_err() {
        eprintln!("skipped: the JPEG XL part needs cjxl and djxl");
        return;
    }
    // Recompressed losslessly into JPEG XL: as small as the same photo without a gain map,
    // and the JPEG rebuilt from it ends where its picture ends.
    let with = peraz_engine::jxl::recompress_jpeg(&file, 1).unwrap();
    let without = peraz_engine::jxl::recompress_jpeg(&main, 1).unwrap();
    assert!(
        with.len() <= without.len() + 64,
        "{} bytes against {}",
        with.len(),
        without.len()
    );
    let stripped = peraz_engine::jxl::without_metadata(&file, 1).unwrap();
    assert_eq!(jpeg_end(&stripped), Some(stripped.len()));
    assert!(!has_gain_map(&stripped));

    // The whole route, whichever encode it picks: the report says what was lost, and a JPEG
    // made from the result has nothing after its end.
    let jxl = dir.0.join("hdr.jxl");
    let report = run(&input, &jxl, ImageFormat::Jxl, Quality::Highest, false);
    assert!(report.warnings.iter().any(|w| w.contains("HDR gain map")));
    let rebuilt = dir.0.join("rebuilt.jpg");
    let report = run(&jxl, &rebuilt, ImageFormat::Jpeg, Quality::Highest, false);
    assert_eq!(report.warnings, Vec::<String>::new());
    let bytes = std::fs::read(&rebuilt).unwrap();
    assert_eq!(jpeg_end(&bytes), Some(bytes.len()));
}

#[test]
fn a_grayscale_jpeg_stays_grayscale() {
    let dir = dir("gray");
    let input = dir.0.join("scan.jpg");
    std::fs::write(&input, jpeg(320, 240, true)).unwrap();
    let output = dir.0.join("scan-1.jpg");
    run(&input, &output, ImageFormat::Jpeg, Quality::Small, false);
    let file = std::io::BufReader::new(std::fs::File::open(&output).unwrap());
    let decoder = image::codecs::jpeg::JpegDecoder::new(file).unwrap();
    assert_eq!(decoder.color_type(), image::ColorType::L8, "one channel");
}

#[test]
fn an_orientation_in_xmp_is_reset_with_the_pixels() {
    let dir = dir("xmp-orientation");
    for (name, packet, expected) in [
        (
            "attribute",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:Description \
             xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"6\" \
             tiff:Make=\"Peraz\"/></x:xmpmeta>",
            "tiff:Orientation=\"1\" tiff:Make=\"Peraz\"",
        ),
        (
            "element",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:Description \
             xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"><tiff:Orientation>8</tiff:Orientation>\
             </rdf:Description></x:xmpmeta>",
            "<tiff:Orientation>1</tiff:Orientation>",
        ),
    ] {
        let input = dir.0.join(format!("{name}.jpg"));
        std::fs::write(
            &input,
            with_segments(&jpeg(64, 48, false), &[xmp_segment(packet)]),
        )
        .unwrap();
        let output = dir.0.join(format!("{name}-1.jpg"));
        run(&input, &output, ImageFormat::Jpeg, Quality::Balanced, true);
        let kept = Metadata::from_jpeg(&std::fs::read(&output).unwrap());
        let xmp = String::from_utf8(kept.xmp.expect("the XMP packet")).unwrap();
        assert!(xmp.contains(expected), "{name}: {xmp}");
        assert!(!xmp.contains("Orientation=\"6\""), "{name}: {xmp}");
    }
}

#[test]
fn metadata_too_large_for_a_jpeg_segment_is_reported() {
    let dir = dir("large");
    let large = format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><history>{}</history></x:xmpmeta>",
        "edit ".repeat(14_000)
    );
    assert!(large.len() > 65_536);
    // A PNG can hold a packet of any size.
    let input = dir.0.join("edited.png");
    let metadata = Metadata {
        xmp: Some(large.into_bytes()),
        ..Metadata::default()
    };
    let mut png = Vec::new();
    image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([
            u8::try_from(x * 4).unwrap(),
            u8::try_from(y * 5).unwrap(),
            90,
        ])
    })
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .unwrap();
    std::fs::write(&input, metadata.into_png(&png)).unwrap();

    let output = dir.0.join("edited.jpg");
    let report = run(&input, &output, ImageFormat::Jpeg, Quality::Balanced, true);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("XMP") && w.contains("not kept")),
        "{:?}",
        report.warnings
    );
    assert!(image::open(&output).is_ok());
    // Without the request to keep metadata there is nothing to report.
    let report = run(&input, &output, ImageFormat::Jpeg, Quality::Balanced, false);
    assert_eq!(report.warnings, Vec::<String>::new());
    // And a format that holds it keeps it without a warning.
    let report = run(
        &input,
        &dir.0.join("again.webp"),
        ImageFormat::Webp,
        Quality::Balanced,
        true,
    );
    assert_eq!(report.warnings, Vec::<String>::new());
}

fn run_recipe(input: &Path, output: &Path, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let report = compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    );
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{}: {:?}",
        output.display(),
        report.outcome
    );
    report
}

/// How a JPEG file stores its picture: the marker of its frame header (0xC0 for a baseline
/// JPEG, 0xC2 for a progressive one), each component's sampling factors (across, down), and
/// the number of scans.
fn structure(path: &Path) -> (u8, Vec<(u8, u8)>, usize) {
    let bytes = std::fs::read(path).unwrap();
    // In picture data a 0xFF is always followed by a zero, so these bytes are a marker.
    let scans = bytes.windows(2).filter(|w| w == &[0xFF, 0xDA]).count();
    let mut at = 2;
    loop {
        assert_eq!(bytes[at], 0xFF, "a marker at {at}");
        let marker = bytes[at + 1];
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if matches!(marker, 0xC0..=0xC3) {
            let body = &bytes[at + 4..at + 2 + length];
            let sampling = body[6..]
                .chunks(3)
                .map(|component| (component[1] >> 4, component[1] & 15))
                .collect();
            return (marker, sampling, scans);
        }
        at += 2 + length;
    }
}

/// The quantization tables of a JPEG file, as stored.
fn quantization_tables(path: &Path) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(path).unwrap();
    let mut tables = Vec::new();
    let mut at = 2;
    while bytes[at + 1] != 0xDA {
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if bytes[at + 1] == 0xDB {
            tables.push(bytes[at + 4..at + 2 + length].to_vec());
        }
        at += 2 + length;
    }
    tables
}

/// A photo-like PNG: smooth color with grain.
fn photo_png(path: &Path, width: u32, height: u32) {
    image::RgbImage::from_fn(width, height, |x, y| {
        let grain = (x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 28) & 15;
        let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
        image::Rgb([
            channel(x * 220 / width + grain),
            channel(y * 220 / height + grain),
            channel(90 + grain),
        ])
    })
    .save(path)
    .unwrap();
}

fn jpeg_recipe(quality: Quality) -> Recipe {
    let mut recipe = Recipe {
        quality,
        ..Recipe::default()
    };
    recipe.image.format = ImageFormat::Jpeg;
    recipe
}

#[test]
fn a_baseline_jpeg_on_request_really_is_one() {
    let dir = dir("baseline");
    let input = dir.0.join("photo.png");
    photo_png(&input, 480, 360);
    for quality in [
        Quality::Highest,
        Quality::High,
        Quality::Balanced,
        Quality::Small,
        Quality::Smallest,
    ] {
        // As usual: progressive, in several scans.
        let usual = dir.0.join("usual.jpg");
        let report = run_recipe(&input, &usual, jpeg_recipe(quality));
        let (frame, sampling, scans) = structure(&usual);
        assert_eq!(frame, 0xC2, "{quality:?}: progressive");
        assert!(scans > 1, "{quality:?}: {scans} scans");
        assert_eq!(sampling, [(2, 2), (1, 1), (1, 1)], "{quality:?}: 4:2:0");
        assert!(!report.encoder.as_deref().unwrap().contains("baseline"));

        // On request: the baseline frame header, and the whole picture in one scan.
        let mut recipe = jpeg_recipe(quality);
        recipe.image.jpeg_mode = peraz_core::JpegMode::Baseline;
        let plain = dir.0.join("baseline.jpg");
        let report = run_recipe(&input, &plain, recipe);
        let (frame, sampling, scans) = structure(&plain);
        assert_eq!((frame, scans), (0xC0, 1), "{quality:?}: baseline");
        assert_eq!(sampling, [(2, 2), (1, 1), (1, 1)], "{quality:?}");
        let encoder = report.encoder.as_deref().unwrap();
        assert!(encoder.ends_with("baseline"), "{encoder}");
        // Baseline tables hold 8-bit values (a table with larger ones starts with 0x10 or
        // 0x11), and they are the usual tables wherever those fit in 8 bits.
        let tables = quantization_tables(&plain);
        assert!(
            tables.iter().all(|table| table[0] >> 4 == 0),
            "{quality:?}: 8-bit tables"
        );
        if quality <= Quality::Small {
            assert_eq!(tables, quantization_tables(&usual), "{quality:?}");
        }
        // The same picture either way, and not much larger.
        let (a, b) = (
            image::open(&usual).unwrap().to_rgb8(),
            image::open(&plain).unwrap().to_rgb8(),
        );
        let apart = a
            .pixels()
            .zip(b.pixels())
            .map(|(p, q)| (0..3).map(|c| u64::from(p[c].abs_diff(q[c]))).sum::<u64>())
            .sum::<u64>()
            / (480 * 360 * 3);
        assert!(apart <= 2, "{quality:?}: {apart} apart on average");
        let (usual, plain) = (
            std::fs::metadata(&usual).unwrap().len(),
            std::fs::metadata(&plain).unwrap().len(),
        );
        assert!(
            plain * 10 <= usual * 12,
            "{quality:?}: {plain} against {usual}"
        );
    }
    // The lowest setting a size target goes to is baseline as well.
    let mut recipe = jpeg_recipe(Quality::Balanced);
    recipe.image.jpeg_mode = peraz_core::JpegMode::Baseline;
    recipe.image.target_bytes = Some(9_000);
    recipe.image.keep_dimensions = true;
    let fitted = dir.0.join("fitted.jpg");
    run_recipe(&input, &fitted, recipe);
    assert_eq!(structure(&fitted).0, 0xC0);
}

#[test]
fn transparency_goes_on_the_color_asked_for() {
    let dir = dir("background");
    let input = dir.0.join("logo.png");
    // Left: see-through. Middle: half see-through black. Right: an opaque color.
    image::RgbaImage::from_fn(240, 80, |x, _| match x / 80 {
        0 => image::Rgba([255, 0, 0, 0]),
        1 => image::Rgba([0, 0, 0, 128]),
        _ => image::Rgba([40, 160, 90, 255]),
    })
    .save(&input)
    .unwrap();
    let shown = |path: &Path, x: u32| image::open(path).unwrap().to_rgb8().get_pixel(x, 40).0;
    let near = |got: [u8; 3], want: [u8; 3]| (0..3).all(|c| got[c].abs_diff(want[c]) <= 6);

    // Without a color: white.
    let white = dir.0.join("white.jpg");
    let report = run_recipe(&input, &white, jpeg_recipe(Quality::Highest));
    assert!(
        near(shown(&white, 40), [255, 255, 255]),
        "{:?}",
        shown(&white, 40)
    );
    assert!(
        near(shown(&white, 120), [127, 127, 127]),
        "{:?}",
        shown(&white, 120)
    );
    let Some(peraz_core::Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.background, Some(peraz_core::Color::WHITE));

    // With one: that color shows where the picture was see-through.
    let mut recipe = jpeg_recipe(Quality::Highest);
    recipe.background = Some(peraz_core::Color([30, 30, 46]));
    let dark = dir.0.join("dark.jpg");
    let report = run_recipe(&input, &dark, recipe.clone());
    assert!(
        near(shown(&dark, 40), [30, 30, 46]),
        "{:?}",
        shown(&dark, 40)
    );
    assert!(
        near(shown(&dark, 120), [15, 15, 23]),
        "{:?}",
        shown(&dark, 120)
    );
    assert!(
        near(shown(&dark, 200), [40, 160, 90]),
        "{:?}",
        shown(&dark, 200)
    );
    let Some(peraz_core::Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.background, Some(peraz_core::Color([30, 30, 46])));

    // A format that keeps transparency has no use for the color.
    recipe.image.format = ImageFormat::Webp;
    let kept = dir.0.join("kept.webp");
    let report = run_recipe(&input, &kept, recipe);
    assert_eq!(
        image::open(&kept).unwrap().to_rgba8().get_pixel(40, 40)[3],
        0
    );
    let Some(peraz_core::Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.background, None);
}

#[test]
fn colored_text_keeps_its_color_at_every_pixel() {
    let dir = dir("chroma");
    // A screenshot of colored text: thin red and blue strokes on a dark background.
    let input = dir.0.join("text.png");
    image::RgbImage::from_fn(480, 360, |x, y| {
        if y % 16 < 9 && x % 7 == 3 {
            image::Rgb(if (x / 60) % 2 == 0 {
                [240, 70, 80]
            } else {
                [90, 160, 250]
            })
        } else {
            image::Rgb([30, 30, 46])
        }
    })
    .save(&input)
    .unwrap();
    let output = dir.0.join("text.jpg");
    let report = run_recipe(&input, &output, jpeg_recipe(Quality::Balanced));
    let (_, sampling, _) = structure(&output);
    assert_eq!(sampling, [(1, 1), (1, 1), (1, 1)], "4:4:4");
    assert_eq!(report.encoder.as_deref(), Some("mozjpeg q78 4:4:4"));
    // The strokes keep their colors: red stays red, blue stays blue.
    let shown = image::open(&output).unwrap().to_rgb8();
    let red = shown.get_pixel(3, 4).0;
    let blue = shown.get_pixel(66, 4).0;
    assert!(red[0] > 170 && red[2] < 140, "{red:?}");
    assert!(blue[2] > 180 && blue[0] < 150, "{blue:?}");

    // A photo keeps the usual sampling, and a gray picture has no color to sample.
    let photo = dir.0.join("photo.png");
    photo_png(&photo, 480, 360);
    let output = dir.0.join("photo.jpg");
    let report = run_recipe(&photo, &output, jpeg_recipe(Quality::Balanced));
    assert_eq!(structure(&output).1, [(2, 2), (1, 1), (1, 1)]);
    assert_eq!(report.encoder.as_deref(), Some("mozjpeg q78"));
}
