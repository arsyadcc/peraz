//! PNG details: every color type and bit depth comes back with the same pixels, a 16-bit
//! picture stays 16-bit when it is kept lossless, and the chunks that say what the colors mean
//! (gAMA, cHRM, sRGB, cICP) are carried over. The inputs are made here, with the `png` crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::ImageDecoder as _;
use peraz_core::{
    ExistingOutput, ImageFormat, Job, KeepReason, Outcome, OutputTarget, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-png-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, format: ImageFormat, quality: Quality) -> Report {
    let mut recipe = Recipe {
        quality,
        ..Recipe::default()
    };
    recipe.image.format = format;
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

const SIDE: u32 = 64;

/// What a test PNG is made of.
struct Kind {
    name: &'static str,
    color: png::ColorType,
    depth: png::BitDepth,
    /// A palette (RGB per entry), for indexed pictures.
    palette: Option<Vec<u8>>,
    /// The `tRNS` chunk: alpha per palette entry, or the one transparent gray or color.
    transparency: Option<Vec<u8>>,
}

/// Writes a barely compressed PNG of `kind`: four bands of the values it can hold, so that
/// every picture has a few distinct colors.
fn write_png(
    path: &Path,
    kind: &Kind,
    configure: impl FnOnce(&mut png::Encoder<'_, std::fs::File>),
) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(file, SIDE, SIDE);
    encoder.set_color(kind.color);
    encoder.set_depth(kind.depth);
    encoder.set_compression(png::Compression::NoCompression);
    if let Some(palette) = &kind.palette {
        encoder.set_palette(palette.clone());
    }
    if let Some(transparency) = &kind.transparency {
        encoder.set_trns(transparency.clone());
    }
    configure(&mut encoder);
    let bits = match kind.depth {
        png::BitDepth::One => 1,
        png::BitDepth::Two => 2,
        png::BitDepth::Four => 4,
        png::BitDepth::Eight => 8,
        png::BitDepth::Sixteen => 16,
    };
    let channels = match kind.color {
        png::ColorType::Grayscale | png::ColorType::Indexed => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
    };
    // The largest sample value: for an indexed picture, the last palette entry.
    let top: u32 = match &kind.palette {
        Some(palette) => u32::try_from(palette.len() / 3 - 1).unwrap(),
        None => (1u32 << bits) - 1,
    };
    let mut data = Vec::new();
    for y in 0..SIDE {
        let mut row_bits: Vec<bool> = Vec::new();
        for x in 0..SIDE {
            // Four bands across, and a second set of values in the lower half.
            let band = x * 4 / SIDE + u32::from(y >= SIDE / 2);
            for channel in 0..channels {
                let value = match (band + channel) % 4 {
                    0 => 0,
                    1 => top / 3,
                    2 => top * 2 / 3,
                    _ => top,
                };
                // 16-bit samples that 8 bits can hold too (the same byte twice): they survive
                // a palette as well as a lossless copy.
                let value = if bits == 16 {
                    (value >> 8) * 257
                } else {
                    value
                };
                for bit in (0..bits).rev() {
                    row_bits.push(value >> bit & 1 == 1);
                }
            }
        }
        for byte in row_bits.chunks(8) {
            let mut packed = 0u8;
            for (i, bit) in byte.iter().enumerate() {
                packed |= u8::from(*bit) << (7 - i);
            }
            data.push(packed);
        }
    }
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&data).unwrap();
    writer.finish().unwrap();
}

fn kinds() -> Vec<Kind> {
    use png::{BitDepth as Depth, ColorType as Color};
    let plain = |name, color, depth| Kind {
        name,
        color,
        depth,
        palette: None,
        transparency: None,
    };
    let palette = |entries: u8| -> Vec<u8> {
        (0..entries)
            .flat_map(|i| {
                [
                    i.wrapping_mul(37),
                    255 - i.wrapping_mul(29),
                    i.wrapping_mul(91),
                ]
            })
            .collect()
    };
    let indexed = |name, depth, entries: u8, transparency: Option<Vec<u8>>| Kind {
        name,
        color: Color::Indexed,
        depth,
        palette: Some(palette(entries)),
        transparency,
    };
    vec![
        plain("gray-1", Color::Grayscale, Depth::One),
        plain("gray-2", Color::Grayscale, Depth::Two),
        plain("gray-4", Color::Grayscale, Depth::Four),
        plain("gray-8", Color::Grayscale, Depth::Eight),
        plain("gray-16", Color::Grayscale, Depth::Sixteen),
        plain("gray-alpha-8", Color::GrayscaleAlpha, Depth::Eight),
        plain("gray-alpha-16", Color::GrayscaleAlpha, Depth::Sixteen),
        plain("rgb-8", Color::Rgb, Depth::Eight),
        plain("rgb-16", Color::Rgb, Depth::Sixteen),
        plain("rgba-8", Color::Rgba, Depth::Eight),
        plain("rgba-16", Color::Rgba, Depth::Sixteen),
        indexed("indexed-1", Depth::One, 2, None),
        indexed("indexed-2", Depth::Two, 4, Some(vec![0, 128])),
        indexed("indexed-4", Depth::Four, 16, Some(vec![255, 0, 200])),
        indexed("indexed-8", Depth::Eight, 200, Some(vec![64; 50])),
        // One gray, and one color, marked as transparent.
        Kind {
            transparency: Some(vec![0, 85]),
            ..plain("gray-8-key", Color::Grayscale, Depth::Eight)
        },
        Kind {
            transparency: Some(vec![0, 0, 0, 85, 0, 170]),
            ..plain("rgb-8-key", Color::Rgb, Depth::Eight)
        },
    ]
}

#[test]
fn every_color_type_and_depth_keeps_its_pixels() {
    let dir = dir("kinds");
    for kind in kinds() {
        let input = dir.0.join(format!("{}.png", kind.name));
        write_png(&input, &kind, |_| {});
        let source = image::open(&input).unwrap().to_rgba16();
        let output = dir.0.join(format!("{}-1.png", kind.name));
        let report = run(&input, &output, ImageFormat::Keep, Quality::Highest);
        let Outcome::Compressed { output_bytes, .. } = &report.outcome else {
            panic!("{}: unexpected outcome: {:?}", kind.name, report.outcome)
        };
        assert!(*output_bytes < report.input_bytes.unwrap(), "{}", kind.name);
        let result = image::open(&output).unwrap().to_rgba16();
        assert_eq!(result.dimensions(), (SIDE, SIDE), "{}", kind.name);
        // The color stored under a fully transparent pixel is never seen: it may change.
        let differing = result
            .pixels()
            .zip(source.pixels())
            .filter(|(got, want)| got != want && (got[3] != 0 || want[3] != 0))
            .count();
        assert_eq!(differing, 0, "{}: pixels changed", kind.name);
    }
}

/// A 16-bit picture with far more colors than a palette holds, using all 16 bits.
fn deep_gradient(path: &Path) -> image::ImageBuffer<image::Rgb<u16>, Vec<u16>> {
    let pixels = image::ImageBuffer::from_fn(256, 256, |x, y| {
        let grain =
            u16::try_from(x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 22).unwrap();
        image::Rgb([
            u16::try_from(x * 257).unwrap() ^ grain,
            u16::try_from(y * 257).unwrap().wrapping_add(grain),
            u16::try_from((x + y) * 128).unwrap() ^ (grain << 3),
        ])
    });
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, 256, 256);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Sixteen);
    encoder.set_compression(png::Compression::NoCompression);
    let mut writer = encoder.write_header().unwrap();
    let bytes: Vec<u8> = pixels.iter().flat_map(|v| v.to_be_bytes()).collect();
    writer.write_image_data(&bytes).unwrap();
    writer.finish().unwrap();
    pixels
}

#[test]
fn a_16_bit_picture_stays_16_bit_when_lossless_and_says_so_when_reduced() {
    let dir = dir("deep");
    let input = dir.0.join("deep.png");
    let source = deep_gradient(&input);
    // At the highest level no palette is good enough for this picture: it is kept lossless,
    // at its own depth.
    let output = dir.0.join("deep-1.png");
    let report = run(&input, &output, ImageFormat::Keep, Quality::Highest);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.encoder.as_deref(), Some("oxipng (lossless)"));
    assert_eq!(report.warnings, Vec::<String>::new());
    let decoded = image::open(&output).unwrap();
    assert_eq!(decoded.color(), image::ColorType::Rgb16);
    assert_eq!(decoded.to_rgb16(), source, "every sample is kept");

    // At the lowest level a palette is used: 8-bit colors, and the report says so.
    let small = dir.0.join("deep-small.png");
    let report = run(&input, &small, ImageFormat::Keep, Quality::Smallest);
    assert!(matches!(report.outcome, Outcome::Compressed { .. }));
    assert!(
        report.encoder.as_deref().unwrap().starts_with("imagequant"),
        "{:?}",
        report.encoder
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("16-bit")),
        "{:?}",
        report.warnings
    );
    // An 8-bit picture reduced to a palette has nothing to say.
    let plain = dir.0.join("plain.png");
    image::open(&input).unwrap().to_rgb8().save(&plain).unwrap();
    let report = run(
        &plain,
        &dir.0.join("plain-1.png"),
        ImageFormat::Keep,
        Quality::Smallest,
    );
    assert_eq!(report.warnings, Vec::<String>::new());
}

/// The chunks of a PNG before its picture data, by name.
fn chunks(path: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).unwrap();
    let mut found = Vec::new();
    let mut at = 8;
    while at + 8 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let name = String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned();
        if name == "IDAT" {
            break;
        }
        found.push((name, bytes[at + 8..at + 8 + length].to_vec()));
        at += length + 12;
    }
    found
}

fn chunk<'a>(chunks: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
    chunks
        .iter()
        .find(|(found, _)| found == name)
        .map(|(_, data)| data.as_slice())
}

/// The color profile of a JPEG file.
fn jpeg_profile(path: &Path) -> Option<Vec<u8>> {
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    image::codecs::jpeg::JpegDecoder::new(file)
        .unwrap()
        .icc_profile()
        .unwrap()
}

#[test]
fn what_the_colors_mean_is_carried_over() {
    let dir = dir("colors");
    let rgb = Kind {
        name: "rgb",
        color: png::ColorType::Rgb,
        depth: png::BitDepth::Eight,
        palette: None,
        transparency: None,
    };
    // A picture that states its gamma (1/1.8) and primaries, without a profile.
    let gamma_only = dir.0.join("gamma.png");
    write_png(&gamma_only, &rgb, |encoder| {
        encoder.set_source_gamma(png::ScaledFloat::new(1.0 / 1.8));
        encoder.set_source_chromaticities(png::SourceChromaticities::new(
            (0.3127, 0.3290),
            (0.64, 0.33),
            (0.21, 0.71),
            (0.15, 0.06),
        ));
    });
    let before = chunks(&gamma_only);
    assert!(chunk(&before, "gAMA").is_some() && chunk(&before, "cHRM").is_some());

    // As a PNG, it states the same.
    let output = dir.0.join("gamma-1.png");
    let report = run(&gamma_only, &output, ImageFormat::Keep, Quality::Highest);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    let after = chunks(&output);
    assert_eq!(chunk(&after, "gAMA"), chunk(&before, "gAMA"));
    assert_eq!(chunk(&after, "cHRM"), chunk(&before, "cHRM"));
    assert_eq!(
        chunk(&after, "iCCP"),
        None,
        "no profile is added to a PNG that has these"
    );

    // In a format without such chunks, a profile says the same.
    let jpeg = dir.0.join("gamma.jpg");
    let report = run(&gamma_only, &jpeg, ImageFormat::Jpeg, Quality::Highest);
    assert!(matches!(report.outcome, Outcome::Compressed { .. }));
    let icc = jpeg_profile(&jpeg).expect("a profile for the gamma");
    let profile = moxcms::ColorProfile::new_from_slice(&icc).unwrap();
    let Some(moxcms::ToneReprCurve::Parametric(curve)) = &profile.red_trc else {
        panic!("{:?}", profile.red_trc)
    };
    assert!((curve[0] - 1.8).abs() < 0.01, "a gamma of {curve:?}");

    // A picture marked as sRGB, and one with a cICP chunk (here: Display P3), stay marked.
    let srgb = dir.0.join("srgb.png");
    write_png(&srgb, &rgb, |encoder| {
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    });
    let output = dir.0.join("srgb-1.png");
    run(&srgb, &output, ImageFormat::Keep, Quality::Highest);
    assert_eq!(chunk(&chunks(&output), "sRGB"), Some(&[0u8][..]));

    let cicp = dir.0.join("cicp.png");
    write_png(&cicp, &rgb, |_| {});
    // The chunk is added by hand, right after the header: primaries 12 (P3), transfer 13
    // (sRGB), matrix 0 (RGB), full range.
    let mut bytes = std::fs::read(&cicp).unwrap();
    let mut added = vec![0, 0, 0, 4];
    added.extend_from_slice(b"cICP\x0C\x0D\x00\x01");
    let mut crc = flate2::Crc::new();
    crc.update(&added[4..]);
    added.extend(crc.sum().to_be_bytes());
    bytes.splice(33..33, added);
    std::fs::write(&cicp, bytes).unwrap();
    let output = dir.0.join("cicp-1.png");
    let report = run(&cicp, &output, ImageFormat::Keep, Quality::Highest);
    assert!(
        !matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. }) | Outcome::Failed { .. }
        ),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        chunk(&chunks(&output), "cICP"),
        Some(&[0x0C, 0x0D, 0x00, 0x01][..])
    );
    assert_eq!(chunk(&chunks(&output), "iCCP"), None);
    // As a JPEG, a profile says that the picture is Display P3.
    let jpeg = dir.0.join("cicp.jpg");
    run(&cicp, &jpeg, ImageFormat::Jpeg, Quality::Highest);
    let icc = jpeg_profile(&jpeg).expect("a profile for the cICP chunk");
    let profile = moxcms::ColorProfile::new_from_slice(&icc).unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3();
    assert!(
        (profile.red_colorant.x - p3.red_colorant.x).abs() < 0.001,
        "{:?} against {:?}",
        profile.red_colorant,
        p3.red_colorant
    );
    // A picture marked as sRGB needs no profile anywhere.
    let jpeg = dir.0.join("srgb.jpg");
    run(&srgb, &jpeg, ImageFormat::Jpeg, Quality::Highest);
    assert_eq!(jpeg_profile(&jpeg), None);
}
