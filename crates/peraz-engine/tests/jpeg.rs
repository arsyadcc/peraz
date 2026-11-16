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
fn a_gain_map_is_not_left_behind_and_the_report_says_it_was_removed() {
    let dir = dir("gain-map");
    let (file, main) = ultra_hdr();
    let input = dir.0.join("hdr.jpg");
    std::fs::write(&input, &file).unwrap();
    let plain = dir.0.join("sdr.jpg");
    std::fs::write(&plain, &main).unwrap();

    // Encoded anew as a JPEG: one picture, nothing after its end.
    let output = dir.0.join("out.jpg");
    let report = run(&input, &output, ImageFormat::Jpeg, Quality::Small, false);
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(jpeg_end(&bytes), Some(bytes.len()));
    assert!(
        report.warnings.iter().any(|w| w.contains("HDR gain map")),
        "{:?}",
        report.warnings
    );
    // A photo without one has nothing to report.
    let report = run(&plain, &output, ImageFormat::Jpeg, Quality::Small, false);
    assert_eq!(report.warnings, Vec::<String>::new());

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
