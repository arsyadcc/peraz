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
        recompress: true,
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

/// One entry of a test directory: tag, type, count, and the value's bytes (big-endian).
type Entry = (u16, u16, u32, Vec<u8>);

fn ascii(tag: u16, text: &str) -> Entry {
    let value = [text.as_bytes(), &[0]].concat();
    (tag, 2, u32::try_from(value.len()).unwrap(), value)
}

fn short(tag: u16, value: u16) -> Entry {
    (tag, 3, 1, value.to_be_bytes().to_vec())
}

fn rationals(tag: u16, values: &[(u32, u32)]) -> Entry {
    let bytes = values
        .iter()
        .flat_map(|(n, d)| [n.to_be_bytes(), d.to_be_bytes()].concat())
        .collect();
    (tag, 5, u32::try_from(values.len()).unwrap(), bytes)
}

fn pointer(tag: u16, offset: usize) -> Entry {
    (
        tag,
        4,
        1,
        u32::try_from(offset).unwrap().to_be_bytes().to_vec(),
    )
}

/// A directory that starts at `at` in its block: the entries, then the values that do not fit
/// in an entry.
fn directory(entries: &[Entry], at: usize) -> Vec<u8> {
    let mut entries = entries.to_vec();
    entries.sort_by_key(|entry| entry.0);
    let mut head = u16::try_from(entries.len()).unwrap().to_be_bytes().to_vec();
    let mut values = Vec::new();
    let values_at = at + 2 + entries.len() * 12 + 4;
    for (tag, kind, count, value) in &entries {
        head.extend(tag.to_be_bytes());
        head.extend(kind.to_be_bytes());
        head.extend(count.to_be_bytes());
        if value.len() <= 4 {
            let mut inline = value.clone();
            inline.resize(4, 0);
            head.extend(inline);
        } else {
            head.extend(
                u32::try_from(values_at + values.len())
                    .unwrap()
                    .to_be_bytes(),
            );
            values.extend_from_slice(value);
            if values.len() % 2 == 1 {
                values.push(0);
            }
        }
    }
    head.extend([0; 4]);
    [head, values].concat()
}

const TAKEN: &str = "2026:05:04 10:20:30";

/// An EXIF block as a camera writes it (big-endian): the camera and the date in the first
/// directory, exposure details in the EXIF directory, a position in the GPS directory, and an
/// orientation that says the picture lies on its side.
fn camera_exif() -> Vec<u8> {
    let details = [
        ascii(0x9003, TAKEN),               // DateTimeOriginal
        rationals(0x829A, &[(1, 250)]),     // ExposureTime
        short(0x8827, 200),                 // ISO
        (0x927C, 7, 6, b"maker!".to_vec()), // MakerNote
    ];
    let position = [
        ascii(0x0001, "N"),
        rationals(0x0002, &[(52, 1), (30, 1), (0, 1)]),
        ascii(0x0003, "E"),
        rationals(0x0004, &[(13, 1), (24, 1), (0, 1)]),
    ];
    let main = |details_at: usize, position_at: usize| {
        vec![
            ascii(0x010F, "Peraz"),
            ascii(0x0110, "Sampler 2"),
            short(0x0112, 6),
            ascii(0x0132, TAKEN),
            ascii(0x8298, "Sample Author"),
            pointer(0x8769, details_at),
            pointer(0x8825, position_at),
        ]
    };
    // The directories follow one another; where each starts depends on the ones before.
    let details_at = 8 + directory(&main(0, 0), 8).len();
    let position_at = details_at + directory(&details, details_at).len();
    [
        b"MM\0*\0\0\0\x08".to_vec(),
        directory(&main(details_at, position_at), 8),
        directory(&details, details_at),
        directory(&position, position_at),
    ]
    .concat()
}

/// The entries of a directory in an EXIF block, in either byte order: tag and value bytes.
fn entries_of(block: &[u8], at: usize) -> Vec<(u16, Vec<u8>)> {
    let big = block.starts_with(b"MM");
    let u16_at = |at: usize| {
        let bytes = [block[at], block[at + 1]];
        if big {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    };
    let u32_at = |at: usize| {
        let bytes = [block[at], block[at + 1], block[at + 2], block[at + 3]];
        if big {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };
    (0..usize::from(u16_at(at)))
        .map(|index| {
            let entry = at + 2 + index * 12;
            let unit = match u16_at(entry + 2) {
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => 1,
            };
            let length = unit * u32_at(entry + 4) as usize;
            let start = if length <= 4 {
                entry + 8
            } else {
                u32_at(entry + 8) as usize
            };
            (u16_at(entry), block[start..start + length].to_vec())
        })
        .collect()
}

/// The value of `tag` in the directory at `at`, with numbers read in the block's byte order.
fn value_of(block: &[u8], at: usize, tag: u16) -> Option<Vec<u8>> {
    entries_of(block, at)
        .into_iter()
        .find(|(found, _)| *found == tag)
        .map(|(_, value)| value)
}

fn number(block: &[u8], value: &[u8]) -> u32 {
    let bytes = [value[0], value[1], value[2], value[3]];
    if block.starts_with(b"MM") {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

/// A JPEG stored on its side with the camera's EXIF and an XMP packet.
fn camera_photo(path: &Path) {
    photo(path);
    let mut kept = Metadata::from_jpeg(&std::fs::read(path).unwrap());
    kept.exif = Some(camera_exif());
    let plain = image::open(path).unwrap();
    let mut jpeg = Vec::new();
    plain
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    std::fs::write(path, kept.into_jpeg(&jpeg)).unwrap();
}

#[test]
fn a_tiff_carries_exif_in_its_directories_both_ways() {
    use tiff::decoder::ifd::Value;
    use tiff::tags::{IfdPointer, Tag};
    let dir = dir("tiff-exif");
    let input = dir.0.join("photo.jpg");
    camera_photo(&input);

    // Into a TIFF: the camera and date among the picture's own tags, the rest in the EXIF and
    // GPS directories they point to. Another reader finds them there.
    let output = dir.0.join("out.tiff");
    run(
        None,
        &input,
        &output,
        ImageFormat::Tiff,
        true,
        Quality::Balanced,
    );
    let bytes = std::fs::read(&output).unwrap();
    let mut decoder = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.dimensions().unwrap(), (80, 120), "stored upright");
    assert_eq!(decoder.get_tag_ascii_string(Tag::Make).unwrap(), "Peraz");
    assert_eq!(
        decoder.get_tag_ascii_string(Tag::Model).unwrap(),
        "Sampler 2"
    );
    assert_eq!(decoder.get_tag_ascii_string(Tag::DateTime).unwrap(), TAKEN);
    assert_eq!(
        decoder.get_tag_ascii_string(Tag::Copyright).unwrap(),
        "Sample Author"
    );
    // No orientation, or "upright": the pixels are not turned a second time.
    let orientation = decoder.find_tag(Tag::Orientation).unwrap();
    assert!(
        matches!(orientation, None | Some(Value::Short(1))),
        "{orientation:?}"
    );
    let details_at = decoder.get_tag_u32(Tag::Unknown(0x8769)).unwrap();
    let details = decoder
        .read_directory(IfdPointer(u64::from(details_at)))
        .unwrap();
    let mut details = decoder.read_directory_tags(&details);
    let find = |tags: &mut tiff::decoder::IfdDecoder<'_>, tag: u16| {
        tags.find_tag(Tag::Unknown(tag)).unwrap()
    };
    assert_eq!(
        find(&mut details, 0x9003),
        Some(Value::Ascii(TAKEN.to_owned()))
    );
    assert_eq!(find(&mut details, 0x829A), Some(Value::Rational(1, 250)));
    assert_eq!(find(&mut details, 0x8827), Some(Value::Short(200)));
    let position_at = decoder.get_tag_u32(Tag::Unknown(0x8825)).unwrap();
    let position = decoder
        .read_directory(IfdPointer(u64::from(position_at)))
        .unwrap();
    let mut position = decoder.read_directory_tags(&position);
    assert_eq!(find(&mut position, 1), Some(Value::Ascii("N".to_owned())));
    assert_eq!(
        find(&mut position, 2),
        Some(Value::List(vec![
            Value::Rational(52, 1),
            Value::Rational(30, 1),
            Value::Rational(0, 1)
        ]))
    );
    // The picture, its XMP packet, and its IPTC data are still there.
    assert!(image::open(&output).is_ok());
    let mut decoder = image::codecs::tiff::TiffDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.xmp_metadata().unwrap().as_deref(), Some(XMP));

    // Without --keep-metadata, none of it is written.
    let bare = dir.0.join("bare.tiff");
    run(
        None,
        &input,
        &bare,
        ImageFormat::Tiff,
        false,
        Quality::Balanced,
    );
    let bytes = std::fs::read(&bare).unwrap();
    let mut decoder = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.find_tag(Tag::Make).unwrap(), None);
    assert_eq!(decoder.find_tag(Tag::Unknown(0x8769)).unwrap(), None);
    assert_eq!(decoder.find_tag(Tag::Unknown(0x8825)).unwrap(), None);
}

#[test]
fn the_exif_of_a_tiff_is_kept_in_other_formats() {
    let dir = dir("tiff-exif-back");
    let input = dir.0.join("photo.jpg");
    camera_photo(&input);
    let output = dir.0.join("out.tiff");
    run(
        None,
        &input,
        &output,
        ImageFormat::Tiff,
        true,
        Quality::Balanced,
    );
    // And from that TIFF into a JPEG: one EXIF block again, with all three directories.
    let back = dir.0.join("back.jpg");
    run(
        None,
        &output,
        &back,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    let block = Metadata::from_jpeg(&std::fs::read(&back).unwrap())
        .exif
        .expect("the TIFF's EXIF, as a block");
    assert_eq!(
        value_of(&block, 8, 0x010F).as_deref(),
        Some(&b"Peraz\0"[..])
    );
    assert_eq!(
        value_of(&block, 8, 0x0110).as_deref(),
        Some(&b"Sampler 2\0"[..])
    );
    assert_eq!(
        value_of(&block, 8, 0x0132).unwrap(),
        [TAKEN.as_bytes(), &[0]].concat()
    );
    let details = number(&block, &value_of(&block, 8, 0x8769).unwrap()) as usize;
    assert_eq!(
        value_of(&block, details, 0x9003).unwrap(),
        [TAKEN.as_bytes(), &[0]].concat()
    );
    let exposure = value_of(&block, details, 0x829A).unwrap();
    assert_eq!(
        (number(&block, &exposure), number(&block, &exposure[4..])),
        (1, 250)
    );
    let position = number(&block, &value_of(&block, 8, 0x8825).unwrap()) as usize;
    assert_eq!(value_of(&block, position, 1).as_deref(), Some(&b"N\0"[..]));
    let latitude = value_of(&block, position, 2).unwrap();
    assert_eq!(
        (number(&block, &latitude), number(&block, &latitude[8..])),
        (52, 30)
    );
    assert_eq!(read_with_image(&back).2, (80, 120));
}

#[test]
fn an_avif_carries_the_xmp_packet() {
    let dir = dir("avif-xmp");
    let input = dir.0.join("photo.jpg");
    photo(&input);
    let avif = dir.0.join("out.avif");
    run(
        None,
        &input,
        &avif,
        ImageFormat::Avif,
        true,
        Quality::Balanced,
    );
    let bytes = std::fs::read(&avif).unwrap();
    let kept = peraz_engine::heif::read_metadata(&bytes);
    assert_eq!(kept.xmp.as_deref(), Some(XMP));
    assert_eq!(kept.exif.as_deref(), Some(exif(1).as_slice()));
    // As a `mime` item of the type XMP has, which describes the picture.
    let holds = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    assert!(holds(b"mime") && holds(b"application/rdf+xml\0") && holds(b"cdsc"));
    assert!(
        peraz_engine::heif::read_properties(&bytes)
            .primary
            .is_some()
    );

    // Without --keep-metadata, there is none.
    let bare = dir.0.join("bare.avif");
    run(
        None,
        &input,
        &bare,
        ImageFormat::Avif,
        false,
        Quality::Balanced,
    );
    let kept = peraz_engine::heif::read_metadata(&std::fs::read(&bare).unwrap());
    assert_eq!((kept.exif, kept.xmp), (None, None));

    // The picture still decodes, and the packet travels on from the AVIF.
    let Ok(tools) = Toolchain::locate(None) else {
        eprintln!("skipped: reading the AVIF back needs FFmpeg");
        return;
    };
    let back = dir.0.join("back.jpg");
    run(
        Some(&tools),
        &avif,
        &back,
        ImageFormat::Jpeg,
        true,
        Quality::Balanced,
    );
    let kept = Metadata::from_jpeg(&std::fs::read(&back).unwrap());
    assert_eq!(kept.xmp.as_deref(), Some(XMP));
    assert_eq!(read_with_image(&back).2, (80, 120));
}
