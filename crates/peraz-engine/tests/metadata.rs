//! Metadata, end to end: removed by default, kept on request in every format that can hold it,
//! with the orientation reset because the pixels are stored upright. The inputs are made here.
//! JPEG XL needs libjxl's tools and HEIC needs FFmpeg with libx265: those parts are skipped
//! (with a note) without them.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::{DynamicImage, ImageDecoder as _};
use peraz_core::{ExistingOutput, ImageFormat, Job, Outcome, OutputTarget, Quality, Recipe};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::{Capabilities, Toolchain};
use peraz_engine::metadata::Metadata;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-metadata-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

const XMP: &[u8] =
    b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><caption>Harbor at dusk</caption></x:xmpmeta>";
const PHOTOSHOP: &[u8] = b"8BIM\x04\x04\0\0\0\0\0\x10\x1c\x02\x78\0\x0bHarbor dusk";

/// An EXIF block (big-endian) with a camera maker, an orientation, and a GPS directory.
fn exif(orientation: u8) -> Vec<u8> {
    let mut out = b"MM\0*\0\0\0\x08".to_vec();
    out.extend([0, 3]);
    // Make: ASCII, 6 bytes at offset 50.
    out.extend([0x01, 0x0F, 0, 2, 0, 0, 0, 6, 0, 0, 0, 50]);
    // Orientation: one SHORT.
    out.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0]);
    // GPS directory at offset 56.
    out.extend([0x88, 0x25, 0, 4, 0, 0, 0, 1, 0, 0, 0, 56]);
    out.extend([0, 0, 0, 0]);
    out.extend_from_slice(b"Peraz\0");
    // The GPS directory: latitude reference "N".
    out.extend([0, 1, 0, 1, 0, 2, 0, 0, 0, 2, b'N', 0, 0, 0, 0, 0, 0, 0]);
    out
}

/// A 120x80 JPEG stored on its side (orientation 6: shown as 80x120), with EXIF, XMP, and IPTC.
fn photo(path: &Path) {
    let pixels = image::RgbImage::from_fn(120, 80, |x, y| {
        image::Rgb([
            u8::try_from(x * 2).unwrap(),
            u8::try_from(y * 3).unwrap(),
            90,
        ])
    });
    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let segment = |marker: u8, body: Vec<u8>| {
        let length = u16::try_from(body.len() + 2).unwrap().to_be_bytes();
        [&[0xFF, marker], &length[..], &body].concat()
    };
    let tagged = [
        &jpeg[..2],
        &segment(0xE1, [&b"Exif\0\0"[..], &exif(6)].concat()),
        &segment(0xE1, [&b"http://ns.adobe.com/xap/1.0/\0"[..], XMP].concat()),
        &segment(0xED, [&b"Photoshop 3.0\0"[..], PHOTOSHOP].concat()),
        &jpeg[2..],
    ]
    .concat();
    std::fs::write(path, tagged).unwrap();
}

fn run(
    tools: Option<&Toolchain>,
    input: &Path,
    output: &Path,
    format: ImageFormat,
    keep: bool,
    quality: Quality,
) {
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
        existing: ExistingOutput::Fail,
    };
    let caps = tools.map_or_else(Capabilities::default, |t| t.capabilities().unwrap());
    let report = compress(tools, &caps, &job, &mut |_| {}, &NOT_CANCELLED);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{}: {:?}",
        output.display(),
        report.outcome
    );
}

/// What `image` reads from a file: the EXIF block, the XMP packet, and the size.
type Found = (Option<Vec<u8>>, Option<Vec<u8>>, (u32, u32));

fn read_with_image(path: &Path) -> Found {
    let mut decoder = image::ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap();
    assert_eq!(
        decoder.orientation().unwrap(),
        image::metadata::Orientation::NoTransforms,
        "the picture is not turned a second time"
    );
    (
        decoder.exif_metadata().unwrap(),
        decoder.xmp_metadata().unwrap(),
        decoder.dimensions(),
    )
}

#[test]
fn metadata_is_removed_by_default() {
    let dir = dir("default");
    let input = dir.0.join("photo.jpg");
    photo(&input);
    assert!(!Metadata::from_jpeg(&std::fs::read(&input).unwrap()).is_empty());
    for (format, name) in [
        (ImageFormat::Jpeg, "out.jpg"),
        (ImageFormat::Png, "out.png"),
        (ImageFormat::Webp, "out.webp"),
    ] {
        let output = dir.0.join(name);
        run(None, &input, &output, format, false, Quality::Balanced);
        let (exif, xmp, size) = read_with_image(&output);
        assert_eq!((exif, xmp), (None, None), "{name}");
        assert_eq!(size, (80, 120), "{name} is upright");
    }
    assert!(Metadata::from_jpeg(&std::fs::read(dir.0.join("out.jpg")).unwrap()).is_empty());
}

#[test]
fn metadata_is_kept_on_request_with_the_orientation_reset() {
    let dir = dir("keep");
    let input = dir.0.join("photo.jpg");
    photo(&input);
    // The same block, saying "upright" now.
    let upright = exif(1);

    let jpeg = dir.0.join("out.jpg");
    run(
        None,
        &input,
        &jpeg,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    let kept = Metadata::from_jpeg(&std::fs::read(&jpeg).unwrap());
    assert_eq!(
        kept,
        Metadata {
            exif: Some(upright.clone()),
            xmp: Some(XMP.to_vec()),
            photoshop: Some(PHOTOSHOP.to_vec()),
        }
    );
    assert_eq!(read_with_image(&jpeg).2, (80, 120));

    for (format, name) in [
        (ImageFormat::Png, "out.png"),
        (ImageFormat::Webp, "out.webp"),
    ] {
        let output = dir.0.join(name);
        run(None, &input, &output, format, true, Quality::Balanced);
        let (exif, xmp, size) = read_with_image(&output);
        assert_eq!(exif.as_deref(), Some(upright.as_slice()), "{name}");
        assert_eq!(xmp.as_deref(), Some(XMP), "{name}");
        assert_eq!(size, (80, 120), "{name}");
        // From that file on, the metadata keeps travelling.
        let again = dir.0.join(format!("again-{name}.jpg"));
        run(
            None,
            &output,
            &again,
            ImageFormat::Jpeg,
            true,
            Quality::Small,
        );
        let kept = Metadata::from_jpeg(&std::fs::read(&again).unwrap());
        assert_eq!(kept.exif.as_deref(), Some(upright.as_slice()), "{name}");
        assert_eq!(kept.xmp.as_deref(), Some(XMP), "{name}");
    }

    // AVIF holds the EXIF block.
    let avif = dir.0.join("out.avif");
    run(
        None,
        &input,
        &avif,
        ImageFormat::Avif,
        true,
        Quality::Balanced,
    );
    let kept = peraz_engine::heif::read_metadata(&std::fs::read(&avif).unwrap());
    assert_eq!(kept.exif.as_deref(), Some(upright.as_slice()));

    // TIFF holds the XMP packet and the Photoshop resources (IPTC).
    let tiff = dir.0.join("out.tiff");
    run(
        None,
        &input,
        &tiff,
        ImageFormat::Tiff,
        true,
        Quality::Balanced,
    );
    let bytes = std::fs::read(&tiff).unwrap();
    let mut decoder = image::codecs::tiff::TiffDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.xmp_metadata().unwrap().as_deref(), Some(XMP));
    assert_eq!(
        peraz_engine::tiff::photoshop_resources(&bytes).as_deref(),
        Some(PHOTOSHOP)
    );
    // And back into a JPEG.
    let back = dir.0.join("from-tiff.jpg");
    run(
        None,
        &tiff,
        &back,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    let kept = Metadata::from_jpeg(&std::fs::read(&back).unwrap());
    assert_eq!(kept.xmp.as_deref(), Some(XMP));
    assert_eq!(kept.photoshop.as_deref(), Some(PHOTOSHOP));
}

#[test]
fn heic_carries_metadata_both_ways() {
    let Ok(tools) = Toolchain::locate(None) else {
        eprintln!("skipped: HEIC needs FFmpeg");
        return;
    };
    if !tools.capabilities().unwrap().has_encoder("libx265") {
        eprintln!("skipped: this FFmpeg has no libx265");
        return;
    }
    let dir = dir("heic");
    let input = dir.0.join("photo.jpg");
    photo(&input);
    let heic = dir.0.join("out.heic");
    run(
        Some(&tools),
        &input,
        &heic,
        ImageFormat::Heic,
        true,
        Quality::Balanced,
    );
    let kept = peraz_engine::heif::read_metadata(&std::fs::read(&heic).unwrap());
    assert_eq!(kept.exif, Some(exif(1)));
    assert_eq!(kept.xmp.as_deref(), Some(XMP));

    let jpeg = dir.0.join("back.jpg");
    run(
        Some(&tools),
        &heic,
        &jpeg,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    let kept = Metadata::from_jpeg(&std::fs::read(&jpeg).unwrap());
    assert_eq!(kept.exif, Some(exif(1)));
    assert_eq!(kept.xmp.as_deref(), Some(XMP));
    assert_eq!(read_with_image(&jpeg).2, (80, 120));

    // Without the request, a HEIC carries none of it.
    let plain = dir.0.join("plain.heic");
    run(
        Some(&tools),
        &input,
        &plain,
        ImageFormat::Heic,
        false,
        Quality::Balanced,
    );
    assert!(peraz_engine::heif::read_metadata(&std::fs::read(&plain).unwrap()).is_empty());
}

#[test]
fn jpeg_xl_carries_metadata_both_ways() {
    if peraz_engine::jxl::JxlTools::locate().is_none() || peraz_engine::jxl::djxl().is_err() {
        eprintln!("skipped: JPEG XL needs cjxl and djxl");
        return;
    }
    let dir = dir("jxl");
    let input = dir.0.join("photo.jpg");
    photo(&input);
    // At a lower level the pixels are encoded anew; at the highest the JPEG's own data is
    // recompressed losslessly, whole.
    for (quality, name) in [(Quality::Small, "lossy"), (Quality::Highest, "lossless")] {
        let jxl = dir.0.join(format!("{name}.jxl"));
        run(None, &input, &jxl, ImageFormat::Jxl, true, quality);
        let jpeg = dir.0.join(format!("{name}.jpg"));
        run(
            None,
            &jxl,
            &jpeg,
            ImageFormat::Jpeg,
            true,
            Quality::Balanced,
        );
        let kept = Metadata::from_jpeg(&std::fs::read(&jpeg).unwrap());
        assert_eq!(kept.xmp.as_deref(), Some(XMP), "{name}");
        let exif = kept.exif.unwrap_or_else(|| panic!("{name}: no EXIF"));
        assert!(
            exif.windows(5).any(|w| w == b"Peraz"),
            "{name}: the camera maker is kept"
        );
        let image = image::open(&jpeg).unwrap();
        assert_eq!((image.width(), image.height()), (80, 120), "{name}");
    }
    // Without the request, none of it.
    let jxl = dir.0.join("plain.jxl");
    run(None, &input, &jxl, ImageFormat::Jxl, false, Quality::Small);
    let jpeg = dir.0.join("plain.jpg");
    run(
        None,
        &jxl,
        &jpeg,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    assert!(Metadata::from_jpeg(&std::fs::read(&jpeg).unwrap()).is_empty());
}
