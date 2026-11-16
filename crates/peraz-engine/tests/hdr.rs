//! HDR pictures: gain maps that are kept, or reported when they are left out, and pictures
//! that are HDR by their transfer function, which are kept or converted to standard range.
//! The JPEG XL parts need libjxl's tools and are skipped (with a note) without them.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use peraz_core::{
    ExistingOutput, HdrHandling, ImageFormat, Job, Outcome, OutputTarget, Plan, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::{Capabilities, Toolchain};
use peraz_engine::jxl::JxlTools;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-hdr-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path, format: ImageFormat) -> Report {
    run_with(None, input, output, format)
}

fn run_with(
    tools: Option<&(Toolchain, Capabilities)>,
    input: &Path,
    output: &Path,
    format: ImageFormat,
) -> Report {
    let mut recipe = Recipe::default();
    recipe.image.format = format;
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let none = Capabilities::default();
    compress(
        tools.map(|(tools, _)| tools),
        tools.map_or(&none, |(_, caps)| caps),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

/// An HDR sample from `scripts/make-fixtures.sh`, or `None` (with a note) when it is not there.
fn sample(name: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    if path.is_file() {
        Some(path)
    } else {
        eprintln!("skipped: fixtures/{name} not found (run scripts/make-fixtures.sh)");
        None
    }
}

/// An FFmpeg that reads HEIC and AVIF pictures with several items (version 7 on), or `None`
/// with a note.
fn ffmpeg() -> Option<(Toolchain, Capabilities)> {
    let tools = match Toolchain::locate(None) {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("skipped: {e}");
            return None;
        }
    };
    let version = tools
        .build_info()
        .map(|build| build.version)
        .unwrap_or_default();
    let major: Option<u32> = version
        .trim_start_matches('n')
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok());
    if major.is_some_and(|major| major < 7) {
        eprintln!("skipped: FFmpeg {version} does not read these HEIC and AVIF files");
        return None;
    }
    let caps = tools.capabilities().unwrap();
    Some((tools, caps))
}

fn warns_about_the_gain_map(report: &Report) -> bool {
    report.warnings.iter().any(|w| w.contains("HDR gain map"))
}

/// The size of a JPEG picture, as stored.
fn jpeg_size(jpeg: &[u8]) -> (u32, u32) {
    let picture = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).unwrap();
    (picture.width(), picture.height())
}

/// Where the second picture of a JPEG file starts, as its MPF segment says.
fn second_picture(file: &[u8]) -> usize {
    let at = file
        .windows(4)
        .position(|w| w == b"MPF\0")
        .expect("an MPF segment");
    // The byte order mark follows; the second entry's offset counts from it.
    let base = at + 4;
    let number = |at: usize| u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
    assert_eq!(&file[base..base + 4], b"MM\0\x2A");
    base + number(base + 50 + 16 + 8)
}

fn run_recipe(input: &Path, output: &Path, recipe: Recipe) -> Report {
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

#[test]
fn a_jpeg_keeps_its_gain_map() {
    let dir = dir("jpeg");
    // As Android phones and Photoshop store it (Ultra HDR), and as iPhones do.
    for name in ["hdr-ultrahdr.jpg", "hdr-apple-gainmap.jpg"] {
        let Some(input) = sample(name) else { continue };
        let source = std::fs::read(&input).unwrap();
        let before = peraz_engine::gainmap::of_jpeg(&source).expect("the sample's gain map");
        let (picture, map) = (jpeg_size(&source), jpeg_size(&before.jpeg));

        // Compressed as a JPEG, the photo is smaller and still has its gain map, with the
        // same description: nothing to warn about.
        let output = dir.0.join("out.jpg");
        let report = run(&input, &output, ImageFormat::Keep);
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{name}: {:?}",
            report.outcome
        );
        assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
        assert!(
            report.encoder.unwrap().contains("HDR gain map kept"),
            "{name}"
        );
        let result = std::fs::read(&output).unwrap();
        let after = peraz_engine::gainmap::of_jpeg(&result).expect("the result's gain map");
        assert_eq!(jpeg_size(&result), picture, "{name}");
        assert_eq!(
            jpeg_size(&after.jpeg),
            map,
            "{name}: the gain map keeps its size"
        );
        assert_eq!(after.names_ultra_hdr, before.names_ultra_hdr, "{name}");
        for mark in ["hdrgm:GainMapMax", "HDRGainMapHeadroom"] {
            let has = |jpeg: &[u8]| jpeg.windows(mark.len()).any(|w| w == mark.as_bytes());
            assert_eq!(has(&after.jpeg), has(&before.jpeg), "{name}: {mark}");
        }
        // The file says where the gain map starts, and the picture ends there.
        let start = second_picture(&result);
        assert_eq!(&result[start..], after.jpeg.as_slice(), "{name}");
        assert_eq!(
            peraz_engine::metadata::jpeg_end(&result),
            Some(start),
            "{name}"
        );

        // Scaled down, the gain map is scaled with the picture.
        let mut recipe = Recipe::default();
        recipe.image.max_width = Some(picture.0 / 2);
        let report = run_recipe(&input, &output, recipe);
        assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
        let result = std::fs::read(&output).unwrap();
        let after = peraz_engine::gainmap::of_jpeg(&result).unwrap();
        assert_eq!(jpeg_size(&result), (picture.0 / 2, picture.1 / 2), "{name}");
        assert_eq!(jpeg_size(&after.jpeg), (map.0 / 2, map.1 / 2), "{name}");

        // With a size to fit, the gain map counts.
        let mut recipe = Recipe::default();
        recipe.image.target_bytes = Some(result.len() as u64 * 3 / 4);
        recipe.image.max_width = Some(picture.0 / 2);
        let report = run_recipe(&input, &output, recipe);
        let fitted = std::fs::read(&output).unwrap();
        assert!(
            fitted.len() * 4 <= result.len() * 3,
            "{name}: {:?}",
            report.outcome
        );
        assert!(peraz_engine::gainmap::of_jpeg(&fitted).is_some(), "{name}");

        // Another format has no place for it yet: the report says so.
        let report = run(&input, &dir.0.join("out.webp"), ImageFormat::Webp);
        assert!(
            warns_about_the_gain_map(&report),
            "{name}: {:?}",
            report.warnings
        );
    }
}

#[test]
fn a_gain_map_turns_with_a_picture_stored_on_its_side() {
    let dir = dir("turned");
    let Some(input) = sample("hdr-apple-gainmap.jpg") else {
        return;
    };
    let mut source = std::fs::read(&input).unwrap();
    let upright = peraz_engine::gainmap::of_jpeg(&source).unwrap();
    let (picture, map) = (jpeg_size(&source), jpeg_size(&upright.jpeg));
    // The sample's EXIF (big-endian) says "upright": make it say "turn a quarter clockwise".
    let orientation = source
        .windows(10)
        .position(|w| w == [0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 1])
        .expect("the orientation tag");
    source[orientation + 9] = 6;
    let turned = dir.0.join("turned.jpg");
    std::fs::write(&turned, &source).unwrap();

    let output = dir.0.join("out.jpg");
    let report = run(&turned, &output, ImageFormat::Keep);
    assert_eq!(report.warnings, Vec::<String>::new());
    let result = std::fs::read(&output).unwrap();
    let after = peraz_engine::gainmap::of_jpeg(&result).unwrap();
    // The pixels are stored upright now, the gain map's too.
    assert_eq!(jpeg_size(&result), (picture.1, picture.0));
    assert_eq!(jpeg_size(&after.jpeg), (map.1, map.0));
    // The gain map was turned the same way as the picture: its brightest part is where the
    // picture's is.
    let brightest = |jpeg: &[u8]| {
        let gray = image::load_from_memory(jpeg).unwrap().to_luma8();
        let (width, height) = gray.dimensions();
        let quarter = |x: u32, y: u32| u8::from(x >= width / 2) + 2 * u8::from(y >= height / 2);
        let mut sums = [0u64; 4];
        for (x, y, pixel) in gray.enumerate_pixels() {
            sums[usize::from(quarter(x, y))] += u64::from(pixel[0]);
        }
        (0..4).max_by_key(|&q| sums[q]).unwrap()
    };
    assert_eq!(
        brightest(&after.jpeg),
        brightest(&result[..second_picture(&result)])
    );
}

#[test]
fn a_heic_or_avif_gain_map_that_is_left_out_is_reported() {
    let dir = dir("heif");
    // The HEIC is made on macOS only.
    for (name, everywhere) in [("hdr-gainmap.avif", true), ("hdr-gainmap.heic", false)] {
        if !everywhere && !cfg!(target_os = "macos") {
            continue;
        }
        let Some(input) = sample(name) else { continue };
        // The file says so itself, whatever FFmpeg is installed.
        let properties = peraz_engine::heif::read_properties(&std::fs::read(&input).unwrap());
        assert!(properties.gain_map, "{name}");
        let Some(tools) = ffmpeg() else { continue };
        for (format, output) in [
            (ImageFormat::Webp, "out.webp"),
            (ImageFormat::Png, "out.png"),
        ] {
            let report = run_with(Some(&tools), &input, &dir.0.join(output), format);
            assert!(
                matches!(report.outcome, Outcome::Compressed { .. }),
                "{name}: {:?}",
                report.outcome
            );
            assert!(
                warns_about_the_gain_map(&report),
                "{name}: {:?}",
                report.warnings
            );
            assert!(image::open(dir.0.join(output)).is_ok(), "{name}");
        }
    }
}

/// What the report's plan says became of a picture's HDR.
fn hdr_of(report: &Report) -> Option<HdrHandling> {
    match &report.plan {
        Some(Plan::Image(plan)) => plan.hdr,
        _ => None,
    }
}

/// The mean of a standard picture's stored values (0 to 255).
fn mean_level(path: &Path) -> f64 {
    let pixels = image::open(path).unwrap().to_rgb8();
    let sum: u64 = pixels.as_raw().iter().map(|v| u64::from(*v)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / pixels.as_raw().len() as f64;
    mean
}

/// How far two pictures' stored values are apart on average, as a part of their range.
fn apart(a: &image::DynamicImage, b: &image::DynamicImage) -> f64 {
    let (a, b) = (a.to_rgb16(), b.to_rgb16());
    assert_eq!(a.dimensions(), b.dimensions());
    let sum: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(p, q)| u64::from(p.abs_diff(*q)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / a.as_raw().len() as f64 / 65535.0;
    mean
}

/// Checks that a compressed HDR picture was reported as kept.
fn assert_kept(report: &Report, name: &str) {
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{name}: {:?}",
        report.outcome
    );
    assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
    assert_eq!(hdr_of(report), Some(HdrHandling::Kept), "{name}");
}

/// Checks that a compressed HDR picture was reported as converted to standard range, and
/// that the result is a standard picture of ordinary brightness.
fn assert_converted(report: &Report, output: &Path, name: &str) {
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{name}: {:?}",
        report.outcome
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("converted to standard range")),
        "{name}: {:?}",
        report.warnings
    );
    assert_eq!(hdr_of(report), Some(HdrHandling::Converted), "{name}");
    let level = mean_level(output);
    assert!(
        (60.0..=220.0).contains(&level),
        "{name}: mean level {level}"
    );
}

#[test]
fn a_picture_that_is_hdr_by_its_transfer_function_stays_hdr() {
    use peraz_engine::heif::{decode, read_properties};
    let dir = dir("pq");
    let Some(input) = sample("hdr-pq.avif") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    // The sample's values are PQ, with sRGB's primaries, and it says how bright it gets.
    let before = read_properties(&std::fs::read(&input).unwrap())
        .hdr
        .expect("the sample is PQ");
    assert_eq!((before.primaries, before.transfer), (1, 16));
    assert_eq!(before.light_level, Some((707, 165)));
    let source = decode(&tools.0, &input).unwrap();
    assert_eq!(source.hdr, Some(before));
    // Decoded at 16 bits per channel.
    assert_eq!(source.image.color(), image::ColorType::Rgb16);

    // As an AVIF and as a HEIC it is the same picture at 10 bits, and the file says the same
    // about its values.
    for (format, name) in [
        (ImageFormat::Keep, "out.avif"),
        (ImageFormat::Heic, "out.heic"),
    ] {
        if format == ImageFormat::Heic && !tools.1.has_encoder("libx265") {
            eprintln!("skipped: this FFmpeg has no libx265");
            continue;
        }
        let output = dir.0.join(name);
        let report = run_with(Some(&tools), &input, &output, format);
        assert_kept(&report, name);
        assert!(
            report.encoder.unwrap().contains("10-bit, HDR kept"),
            "{name}"
        );
        let file = std::fs::read(&output).unwrap();
        assert_eq!(read_properties(&file).hdr, Some(before), "{name}");
        // The stream itself says so too, which is what FFmpeg reads.
        let probed = peraz_engine::probe::probe(&tools.0, &output).unwrap();
        let picture = probed.video.unwrap();
        assert!(picture.color.is_hdr(), "{name}: {:?}", picture.color);
        assert!(
            picture.pixel_format.unwrap().contains("10"),
            "{name}: 10 bits"
        );
        let result = decode(&tools.0, &output).unwrap();
        assert_eq!(result.hdr, Some(before), "{name}");
        let difference = apart(&source.image, &result.image);
        assert!(difference < 0.012, "{name}: {difference} apart");
    }

    // Scaled down, it is still HDR.
    let mut recipe = Recipe::default();
    recipe.image.max_width = Some(200);
    let output = dir.0.join("small.avif");
    let job = Job {
        input: input.clone(),
        output: OutputTarget::File(output.clone()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let report = compress(Some(&tools.0), &tools.1, &job, &mut |_| {}, &NOT_CANCELLED);
    assert_kept(&report, "scaled");
    let result = decode(&tools.0, &output).unwrap();
    assert_eq!((result.image.width(), result.hdr), (200, Some(before)));
}

#[test]
fn an_hdr_picture_becomes_a_standard_one_where_the_format_holds_nothing_else() {
    let dir = dir("pq-standard");
    let Some(input) = sample("hdr-pq.avif") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    for (format, name) in [
        (ImageFormat::Jpeg, "out.jpg"),
        (ImageFormat::Webp, "out.webp"),
        (ImageFormat::Png, "out.png"),
    ] {
        let output = dir.0.join(name);
        let report = run_with(Some(&tools), &input, &output, format);
        assert_converted(&report, &output, name);
        assert!(!report.encoder.unwrap().contains("HDR"), "{name}");
        // macOS shows this picture in standard range with a mean level of 160. Read as it
        // is stored, the picture is dark and flat: a mean level of 130.
        let level = mean_level(&output);
        assert!(
            (152.0..=172.0).contains(&level),
            "{name}: mean level {level}"
        );
    }
    // The sky is bright but keeps its clouds: it is not one flat white.
    let jpeg = image::open(dir.0.join("out.jpg")).unwrap().to_luma8();
    let sky: Vec<u8> = (0..400).map(|x| jpeg.get_pixel(x, 40)[0]).collect();
    let (darkest, brightest) = (*sky.iter().min().unwrap(), *sky.iter().max().unwrap());
    assert!(
        darkest > 150 && brightest - darkest > 12,
        "{darkest}..{brightest}"
    );
}

#[test]
fn an_hlg_picture_is_kept_and_converted_like_a_pq_one() {
    use peraz_engine::hdr::{HLG, HdrColor};
    use peraz_engine::heif::{decode, read_properties};
    let dir = dir("hlg");
    let Some(pq) = sample("hdr-pq.avif") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    // No HLG sample is at hand: the PQ sample's values are written out as an HLG picture
    // with BT.2020 primaries. It is another picture, but a proper HLG one.
    let hlg = HdrColor {
        primaries: 9,
        transfer: HLG,
        light_level: None,
    };
    let pixels = decode(&tools.0, &pq).unwrap().image;
    let input = dir.0.join("hlg.avif");
    let file = peraz_engine::avif::encode_hdr(&pixels, hlg, 95, None).unwrap();
    std::fs::write(&input, file).unwrap();
    assert_eq!(
        read_properties(&std::fs::read(&input).unwrap()).hdr,
        Some(hlg)
    );
    let probed = peraz_engine::probe::probe(&tools.0, &input).unwrap();
    let color = probed.video.unwrap().color;
    assert_eq!(color.transfer.as_deref(), Some("arib-std-b67"));
    assert_eq!(color.primaries.as_deref(), Some("bt2020"));

    let output = dir.0.join("out.avif");
    let report = run_with(Some(&tools), &input, &output, ImageFormat::Keep);
    assert_kept(&report, "avif");
    let result = decode(&tools.0, &output).unwrap();
    assert_eq!(result.hdr, Some(hlg));
    let difference = apart(&decode(&tools.0, &input).unwrap().image, &result.image);
    assert!(difference < 0.012, "{difference} apart");

    let output = dir.0.join("out.jpg");
    let report = run_with(Some(&tools), &input, &output, ImageFormat::Jpeg);
    assert_converted(&report, &output, "jpeg");
}

#[test]
fn hdr_in_png_and_jpeg_xl_is_found_and_handled_the_same_way() {
    let dir = dir("pq-others");
    let Some(input) = sample("hdr-pq.avif") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    // A PNG says it with a `cICP` chunk: FFmpeg writes one for this picture.
    let png = dir.0.join("hdr.png");
    let made = std::process::Command::new(&tools.0.ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(&input)
        .args(["-frames:v", "1", "-pix_fmt", "rgb48be"])
        .arg(&png)
        .status()
        .unwrap();
    assert!(made.success());
    assert!(
        std::fs::read(&png)
            .unwrap()
            .windows(4)
            .any(|w| w == b"cICP")
    );
    // It stays a PNG with every shade it has: 16 bits, no palette, and the chunk.
    let output = dir.0.join("out.png");
    let report = run_with(Some(&tools), &png, &output, ImageFormat::Keep);
    assert_kept(&report, "png");
    let file = std::fs::read(&output).unwrap();
    assert_eq!((file[24], file[25]), (16, 2), "16 bits per channel, RGB");
    assert!(file.windows(8).any(|w| w == b"cICP\x01\x10\x00\x01"));
    // As a JPEG it is converted, with FFmpeg or without it.
    for with_ffmpeg in [true, false] {
        let output = dir.0.join("from-png.jpg");
        let tools = with_ffmpeg.then_some(&tools);
        let report = run_with(tools, &png, &output, ImageFormat::Jpeg);
        assert_converted(&report, &output, "png to jpeg");
    }

    if JxlTools::locate().is_none_or(|tools| tools.djxl.is_none()) {
        eprintln!("skipped: cjxl and djxl not found");
        return;
    }
    // A JPEG XL says it in its header. Made from the AVIF, it is PQ; it stays PQ as a JPEG
    // XL, and is converted as a JPEG.
    let jxl = dir.0.join("hdr.jxl");
    let report = run_with(Some(&tools), &input, &jxl, ImageFormat::Jxl);
    assert_kept(&report, "avif to jxl");
    let header = |path: &Path| peraz_engine::jxl_header::sniff(path).unwrap().unwrap();
    let hdr = header(&jxl).hdr.expect("a PQ JPEG XL");
    assert_eq!((hdr.primaries, hdr.transfer), (1, 16));
    let again = dir.0.join("again.jxl");
    let recipe = Recipe {
        quality: peraz_core::Quality::Smallest,
        ..Recipe::default()
    };
    let report = run_recipe(&jxl, &again, recipe);
    assert_kept(&report, "jxl");
    assert_eq!(header(&again).hdr, Some(hdr));
    let output = dir.0.join("from-jxl.jpg");
    let report = run(&jxl, &output, ImageFormat::Jpeg);
    assert_converted(&report, &output, "jxl to jpeg");
}

fn boxed(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(body.len() + 8).unwrap().to_be_bytes();
    [&size[..], &kind, body].concat()
}

/// A JPEG XL file in a container, with the boxes in `extra` after the picture.
fn jxl_container(codestream_or_container: &[u8], extra: &[Vec<u8>]) -> Vec<u8> {
    const SIGNATURE: &[u8] = b"\0\0\0\x0CJXL \r\n\x87\n";
    let mut file = if codestream_or_container.starts_with(SIGNATURE) {
        codestream_or_container.to_vec()
    } else {
        [
            SIGNATURE.to_vec(),
            boxed(*b"ftyp", b"jxl \0\0\0\0jxl "),
            boxed(*b"jxlc", codestream_or_container),
        ]
        .concat()
    };
    file.extend(extra.concat());
    file
}

#[test]
fn a_jpeg_xl_gain_map_that_is_left_out_is_reported() {
    if JxlTools::locate().is_none() {
        eprintln!("skipped: cjxl and djxl not found");
        return;
    }
    let dir = dir("jxl");
    let source = dir.0.join("photo.png");
    image::RgbImage::from_fn(320, 240, |x, y| {
        let grain = (x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 28) & 15;
        let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
        image::Rgb([
            channel(x * 220 / 320 + grain),
            channel(y * 220 / 240 + grain),
            channel(90 + grain),
        ])
    })
    .save(&source)
    .unwrap();
    let plain = dir.0.join("plain.jxl");
    let report = run(&source, &plain, ImageFormat::Jxl);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.warnings, Vec::<String>::new());

    // The same picture with a gain map box after it: a stand-in, since only its presence
    // counts here.
    let hdr = dir.0.join("hdr.jxl");
    let picture = std::fs::read(&plain).unwrap();
    std::fs::write(&hdr, jxl_container(&picture, &[boxed(*b"jhgm", &[0; 64])])).unwrap();
    // A container without one, to tell the box from the container.
    let contained = dir.0.join("contained.jxl");
    std::fs::write(&contained, jxl_container(&picture, &[])).unwrap();

    for (format, name) in [
        (ImageFormat::Png, "out.png"),
        (ImageFormat::Jpeg, "out.jpg"),
    ] {
        let report = run(&hdr, &dir.0.join(name), format);
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{name}: {:?}",
            report.outcome
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("HDR gain map")),
            "{name}: {:?}",
            report.warnings
        );
        let report = run(&contained, &dir.0.join(name), format);
        assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
    }
}

#[test]
fn a_jpeg_xl_keeps_its_gain_map() {
    use peraz_engine::jxl::JxlGainMap;
    use peraz_engine::jxl_header::{codestream, gain_map_box, with_box};
    if JxlTools::locate().is_none_or(|tools| tools.djxl.is_none()) {
        eprintln!("skipped: cjxl and djxl not found");
        return;
    }
    let dir = dir("jxl-kept");
    // No JPEG XL with a gain map was found that may be used here: one is put together from
    // a picture and a gain map that cjxl encodes.
    let picture = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(320, 240, |x, y| {
        let grain = (x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 28) & 15;
        let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
        image::Rgb([
            channel(x * 220 / 320 + grain),
            channel(y * 220 / 240 + grain),
            channel(90 + grain),
        ])
    }));
    let brighter = image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(160, 120, |x, _| {
        image::Luma([u8::try_from(x * 255 / 159).unwrap()])
    }));
    let encoded = |image: image::DynamicImage, quality: u8| {
        let path = dir.0.join("source.png");
        image.save(&path).unwrap();
        let decoded = peraz_engine::image::decode_native(&path).unwrap();
        peraz_engine::jxl::encode(&decoded, quality, quality, None)
            .unwrap()
            .0
    };
    let map = codestream(&encoded(brighter, 95)).unwrap();
    let description = [
        &[0][..],
        &27u16.to_be_bytes(),
        b"how the gain map is applied",
        &[0],
        &0u32.to_be_bytes(),
    ]
    .concat();
    let content = [&description[..], &map].concat();
    let input = dir.0.join("hdr.jxl");
    std::fs::write(
        &input,
        with_box(&encoded(picture, 98), *b"jhgm", &content).unwrap(),
    )
    .unwrap();

    let output = dir.0.join("out.jxl");
    let compressed = |max_width: Option<u32>| {
        let mut recipe = Recipe::default();
        recipe.image.format = ImageFormat::Jxl;
        recipe.image.max_width = max_width;
        let report = run_recipe(&input, &output, recipe);
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{:?}",
            report.outcome
        );
        assert_eq!(report.warnings, Vec::<String>::new());
        assert!(report.encoder.unwrap().contains("HDR gain map kept"));
        let file = std::fs::read(&output).unwrap();
        let kept = gain_map_box(&file).expect("the result's gain map");
        JxlGainMap::of_box(kept, (0, 0), true).expect("a gain map that can be read")
    };
    // The picture keeps its size: the gain map is the input's, byte for byte.
    let kept = compressed(None);
    assert_eq!(kept.description, description);
    assert_eq!(kept.codestream, map);
    // Scaled down, the gain map is scaled with the picture and says the same about itself.
    let kept = compressed(Some(160));
    assert_eq!(kept.description, description);
    let small = peraz_engine::jxl::decode(&kept.codestream).unwrap();
    assert_eq!((small.width(), small.height()), (80, 60));
    // Bright where the input's is bright.
    let small = small.to_luma8();
    assert!(small.get_pixel(2, 30)[0] < 40 && small.get_pixel(77, 30)[0] > 215);
    let result = peraz_engine::jxl::decode(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!((result.width(), result.height()), (160, 120));
}

/// An FFmpeg that also writes HEIC (libx265), or `None` with a note.
fn ffmpeg_with_x265() -> Option<(Toolchain, Capabilities)> {
    let tools = ffmpeg()?;
    if !tools.1.has_encoder("libx265") {
        eprintln!("skipped: this FFmpeg has no libx265");
        return None;
    }
    Some(tools)
}

/// The samples whose gain map is an ISO 21496-1 item. The HEIC is made on macOS only.
fn samples_with_a_gain_map_item() -> Vec<(&'static str, PathBuf)> {
    [("hdr-gainmap.heic", false), ("hdr-gainmap.avif", true)]
        .into_iter()
        .filter(|(_, everywhere)| *everywhere || cfg!(target_os = "macos"))
        .filter_map(|(name, _)| Some((name, sample(name)?)))
        .collect()
}

#[test]
fn a_heic_keeps_its_gain_map() {
    let dir = dir("heic");
    let Some(tools) = ffmpeg_with_x265() else {
        return;
    };
    // An AVIF becomes a HEIC with its gain map too.
    for (name, input) in samples_with_a_gain_map_item() {
        let output = dir.0.join("out.heic");
        keeps_its_gain_map(&tools, name, &input, &output, ImageFormat::Heic);
    }
}

#[test]
fn an_avif_keeps_its_gain_map() {
    let dir = dir("avif");
    let Some(tools) = ffmpeg() else {
        return;
    };
    // A HEIC becomes an AVIF with its gain map too.
    for (name, input) in samples_with_a_gain_map_item() {
        let output = dir.0.join("out.avif");
        keeps_its_gain_map(&tools, name, &input, &output, ImageFormat::Avif);
    }
}

/// Compresses a HEIC or AVIF that has a gain map into `format`, and checks that the result
/// has the same gain map, also when the picture is scaled down.
fn keeps_its_gain_map(
    tools: &(Toolchain, Capabilities),
    name: &str,
    input: &Path,
    output: &Path,
    format: ImageFormat,
) {
    use peraz_engine::heif::{decode, read_properties};
    let before = read_properties(&std::fs::read(input).unwrap())
        .tone_map
        .expect("the sample's gain map");
    let source = decode(&tools.0, input).unwrap();
    let map = source.heif_gain_map.expect("the gain map's picture");
    let picture = (source.image.width(), source.image.height());
    let map_size = (map.pixels.width(), map.pixels.height());

    let report = run_with(Some(tools), input, output, format);
    assert!(
        matches!(report.outcome, Outcome::Compressed { .. }),
        "{name}: {:?}",
        report.outcome
    );
    assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
    assert!(
        report.encoder.unwrap().contains("HDR gain map kept"),
        "{name}"
    );
    // The result says the same about its gain map: how it is applied, and what the HDR
    // rendition is.
    let after = read_properties(&std::fs::read(output).unwrap())
        .tone_map
        .expect("the result's gain map");
    assert_eq!(after.metadata, before.metadata, "{name}");
    assert_eq!(after.rendition, before.rendition, "{name}");
    // Read back, both pictures are there at their sizes.
    let result = decode(&tools.0, output).unwrap();
    assert_eq!(
        (result.image.width(), result.image.height()),
        picture,
        "{name}"
    );
    let kept = result.heif_gain_map.expect("the gain map's picture");
    assert_eq!(
        (kept.pixels.width(), kept.pixels.height()),
        map_size,
        "{name}"
    );
    // The gain map is the same picture: bright where the input's is bright.
    let (a, b) = (map.pixels.to_luma8(), kept.pixels.to_luma8());
    let apart: u64 = a
        .pixels()
        .zip(b.pixels())
        .map(|(p, q)| u64::from(p[0].abs_diff(q[0])))
        .sum::<u64>()
        / u64::from(map_size.0 * map_size.1);
    assert!(apart <= 6, "{name}: {apart} apart on average");

    // Scaled down, the gain map is scaled with the picture.
    let mut recipe = Recipe::default();
    recipe.image.format = format;
    recipe.image.max_width = Some(picture.0 / 2);
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe,
        existing: ExistingOutput::Replace,
        recompress: true,
    };
    let report = compress(Some(&tools.0), &tools.1, &job, &mut |_| {}, &NOT_CANCELLED);
    assert_eq!(report.warnings, Vec::<String>::new(), "{name}");
    let result = decode(&tools.0, output).unwrap();
    assert_eq!(result.image.width(), picture.0 / 2, "{name}");
    let kept = result.heif_gain_map.expect("the gain map's picture");
    assert_eq!(kept.pixels.width(), map_size.0 / 2, "{name}");
}

#[test]
fn a_heic_from_an_older_iphone_keeps_its_gain_map() {
    use peraz_engine::heif::{decode, read_metadata, read_properties};
    let dir = dir("heic-iphone");
    let Some(tools) = ffmpeg_with_x265() else {
        return;
    };
    let Some(input) = sample("hdr-iphone-gainmap.heic") else {
        return;
    };
    // The gain map is an auxiliary picture with an XMP packet of its own, and the picture's
    // maker note says how bright the HDR rendition goes.
    let before = read_properties(&std::fs::read(&input).unwrap())
        .auxiliary_map
        .expect("the sample's gain map");
    assert!(before.xmp.is_some() && before.headroom_note.is_some());
    let source = decode(&tools.0, &input).unwrap();
    assert_eq!((source.image.width(), source.image.height()), (1512, 850));
    let map = source.heif_gain_map.expect("the gain map's picture");
    // 426 rows are coded, and 425 of them are the gain map.
    assert_eq!((map.pixels.width(), map.pixels.height()), (756, 425));

    let output = dir.0.join("out.heic");
    let compressed = |keep_metadata: bool, max_width: Option<u32>| {
        let mut recipe = Recipe::default();
        recipe.image.format = ImageFormat::Heic;
        recipe.image.keep_metadata = keep_metadata;
        recipe.image.max_width = max_width;
        let job = Job {
            input: input.clone(),
            output: OutputTarget::File(output.clone()),
            recipe,
            existing: ExistingOutput::Replace,
            recompress: true,
        };
        let report = compress(Some(&tools.0), &tools.1, &job, &mut |_| {}, &NOT_CANCELLED);
        assert!(
            matches!(report.outcome, Outcome::Compressed { .. }),
            "{:?}",
            report.outcome
        );
        assert_eq!(report.warnings, Vec::<String>::new());
        assert!(report.encoder.unwrap().contains("HDR gain map kept"));
        let file = std::fs::read(&output).unwrap();
        // Stored the same way, with the same description.
        let properties = read_properties(&file);
        assert_eq!(properties.tone_map, None);
        let after = properties.auxiliary_map.expect("the result's gain map");
        assert_eq!(after.xmp, before.xmp);
        assert_eq!(after.headroom_note, before.headroom_note);
        (read_metadata(&file), decode(&tools.0, &output).unwrap())
    };

    // Without the metadata, the two values of the maker note are all the EXIF there is, and
    // the gain map's XMP is not taken for the picture's.
    let (metadata, result) = compressed(false, None);
    assert_eq!(metadata.exif, before.headroom_note);
    assert_eq!(metadata.xmp, None);
    assert_eq!((result.image.width(), result.image.height()), (1512, 850));
    // HEVC codes even sides.
    let kept = result.heif_gain_map.expect("the gain map's picture");
    assert_eq!((kept.pixels.width(), kept.pixels.height()), (756, 426));
    // The gain map is the same picture: bright where the input's is bright.
    let kept = kept
        .pixels
        .resize_exact(756, 425, image::imageops::FilterType::Triangle)
        .to_luma8();
    let apart: u64 = map
        .pixels
        .to_luma8()
        .pixels()
        .zip(kept.pixels())
        .map(|(p, q)| u64::from(p[0].abs_diff(q[0])))
        .sum::<u64>()
        / (756 * 425);
    assert!(apart <= 6, "{apart} apart on average");

    // With the metadata, the picture's EXIF is written whole.
    let (metadata, _) = compressed(true, None);
    assert_eq!(metadata.exif, source.metadata.exif);
    assert_eq!(metadata.xmp, source.metadata.xmp);

    // Scaled down, the gain map is scaled with the picture.
    let (_, result) = compressed(false, Some(756));
    assert_eq!(result.image.width(), 756);
    let kept = result.heif_gain_map.expect("the gain map's picture");
    assert_eq!(kept.pixels.width(), 378);

    // An AVIF has no place for a gain map stored this way: there it is rewritten the
    // standard's way.
    let avif = dir.0.join("out.avif");
    let report = run_with(Some(&tools), &input, &avif, ImageFormat::Avif);
    assert_eq!(report.warnings, Vec::<String>::new());
    let properties = read_properties(&std::fs::read(&avif).unwrap());
    assert!(properties.tone_map.is_some() && properties.auxiliary_map.is_none());
}

/// The gain map of a file in the common form, read the way its format needs.
fn common_of(
    tools: &(Toolchain, Capabilities),
    path: &Path,
) -> Option<peraz_engine::gain_params::CommonMap> {
    let extension = path.extension()?.to_str()?;
    match extension {
        "jpg" => {
            let decoded = peraz_engine::image::decode_native(path).unwrap();
            decoded
                .jpeg_gain_map?
                .common(decoded.metadata.exif.as_deref())
        }
        "jxl" => peraz_engine::jxl::decode_file(path)
            .unwrap()
            .jxl_gain_map?
            .common(),
        _ => peraz_engine::heif::decode(&tools.0, path)
            .unwrap()
            .heif_gain_map?
            .common(),
    }
}

/// How far apart two gain maps' pictures are on average (0 to 255), at the first one's size.
fn maps_apart(a: &image::DynamicImage, b: &image::DynamicImage) -> u64 {
    let a = a.to_rgb8();
    let b = b
        .resize_exact(a.width(), a.height(), image::imageops::FilterType::Triangle)
        .to_rgb8();
    let sum: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(p, q)| u64::from(p.abs_diff(*q)))
        .sum();
    sum / a.as_raw().len() as u64
}

#[test]
fn a_gain_map_goes_from_one_format_to_another() {
    let dir = dir("across");
    let Some(tools) = ffmpeg() else { return };
    let x265 = tools.1.has_encoder("libx265");
    let jxl = JxlTools::locate().is_some_and(|tools| tools.djxl.is_some());
    if !x265 {
        eprintln!("skipped: this FFmpeg has no libx265");
    }
    if !jxl {
        eprintln!("skipped: cjxl and djxl not found");
    }
    // The samples: Ultra HDR and an iPhone's JPEG, the standard's item in AVIF and HEIC, and
    // an older iPhone's HEIC. The HEIC with the standard's item is made on macOS only.
    for (name, everywhere) in [
        ("hdr-ultrahdr.jpg", true),
        ("hdr-apple-gainmap.jpg", true),
        ("hdr-gainmap.avif", true),
        ("hdr-gainmap.heic", false),
        ("hdr-iphone-gainmap.heic", true),
    ] {
        if !everywhere && !cfg!(target_os = "macos") {
            continue;
        }
        let Some(input) = sample(name) else { continue };
        let before = common_of(&tools, &input).expect("the sample's gain map");
        for (format, extension, possible) in [
            (ImageFormat::Jpeg, "jpg", true),
            (ImageFormat::Heic, "heic", x265),
            (ImageFormat::Avif, "avif", true),
            (ImageFormat::Jxl, "jxl", jxl),
        ] {
            if !possible {
                continue;
            }
            let case = format!("{name} to {extension}");
            let output = dir.0.join(format!("out.{extension}"));
            let mut recipe = Recipe::default();
            recipe.image.format = format;
            // Half the size, so that the gain map has to be scaled as well.
            recipe.image.max_width = Some(before.picture_size.0 / 2);
            let job = Job {
                input: input.clone(),
                output: OutputTarget::File(output.clone()),
                recipe,
                existing: ExistingOutput::Replace,
                recompress: true,
            };
            let report = compress(Some(&tools.0), &tools.1, &job, &mut |_| {}, &NOT_CANCELLED);
            assert!(
                matches!(report.outcome, Outcome::Compressed { .. }),
                "{case}: {:?}",
                report.outcome
            );
            assert_eq!(report.warnings, Vec::<String>::new(), "{case}");
            assert!(
                report.encoder.unwrap().contains("HDR gain map kept"),
                "{case}"
            );
            // Read back, the gain map is applied the same way, and it is the same picture
            // at half the size.
            let after = common_of(&tools, &output).unwrap_or_else(|| panic!("{case}: no map"));
            let same_layout = name.ends_with("iphone-gainmap.heic") && extension == "heic";
            if !same_layout {
                assert_eq!(after.parameters, before.parameters, "{case}");
            }
            let (was, now) = (before.pixels.width(), after.pixels.width());
            assert!(now.abs_diff(was / 2) <= 1, "{case}: {was} to {now}");
            let apart = maps_apart(&after.pixels, &before.pixels);
            assert!(apart <= 8, "{case}: {apart} apart on average");
        }
    }
}

#[test]
fn a_jpeg_kept_losslessly_in_jpeg_xl_takes_its_gain_map_along() {
    let dir = dir("lossless");
    if JxlTools::locate().is_none_or(|tools| tools.djxl.is_none()) {
        eprintln!("skipped: cjxl and djxl not found");
        return;
    }
    let Some(sample) = sample("hdr-ultrahdr.jpg") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    let at = |quality, format| {
        let mut recipe = Recipe {
            quality,
            ..Recipe::default()
        };
        recipe.image.format = format;
        recipe
    };
    // A JPEG that is small already: encoded anew at a high quality it would grow, so its
    // own data is what is kept.
    let input = dir.0.join("small.jpg");
    let smallest = at(peraz_core::Quality::Smallest, ImageFormat::Jpeg);
    let report = run_recipe(&sample, &input, smallest);
    assert_eq!(report.warnings, Vec::<String>::new());
    let before = common_of(&tools, &input).unwrap();
    // The JPEG's data is recompressed losslessly, and the gain map goes into the JPEG XL's
    // container.
    let highest = |format| at(peraz_core::Quality::Highest, format);
    let jxl = dir.0.join("out.jxl");
    let report = run_recipe(&input, &jxl, highest(ImageFormat::Jxl));
    assert_eq!(report.warnings, Vec::<String>::new());
    let encoder = report.encoder.unwrap();
    assert!(encoder.contains("lossless JPEG recompression"), "{encoder}");
    assert!(encoder.contains("HDR gain map kept"), "{encoder}");
    assert!(peraz_engine::jxl_header::has_jpeg_reconstruction(&jxl).unwrap());
    let stored = common_of(&tools, &jxl).expect("the gain map in the JPEG XL");
    assert_eq!(stored.parameters, before.parameters);
    // Back to a JPEG: the very JPEG data again, with the gain map after it.
    let jpeg = dir.0.join("back.jpg");
    let report = run_recipe(&jxl, &jpeg, highest(ImageFormat::Jpeg));
    assert_eq!(report.warnings, Vec::<String>::new());
    let encoder = report.encoder.unwrap();
    assert!(encoder.contains("rebuilt from JPEG XL"), "{encoder}");
    assert!(encoder.contains("HDR gain map kept"), "{encoder}");
    let back = common_of(&tools, &jpeg).expect("the gain map after the JPEG");
    assert_eq!(back.parameters, before.parameters);
    assert!(maps_apart(&back.pixels, &before.pixels) <= 8);
}

#[test]
fn a_gain_map_goes_with_a_heic_of_an_odd_size() {
    use peraz_engine::heif::{decode, read_properties};
    let dir = dir("heic-odd");
    let Some(tools) = ffmpeg_with_x265() else {
        return;
    };
    let Some(input) = sample("hdr-gainmap.avif") else {
        return;
    };
    // The picture cut to odd sides: HEVC codes even ones, so the file wraps the picture in a
    // grid of its true size, and the gain map belongs to that grid.
    let mut source = decode(&tools.0, &input).unwrap();
    source.image = source.image.crop_imm(0, 0, 399, 299);
    let map = source.heif_gain_map.as_mut().unwrap();
    map.pixels = map.pixels.crop_imm(0, 0, 399, 299);
    map.picture_size = (399, 299);
    let (file, kept) = peraz_engine::heic::encode(&tools.0, &source, 60, None).unwrap();
    assert!(kept);
    let output = dir.0.join("odd.heic");
    std::fs::write(&output, &file).unwrap();
    assert!(read_properties(&file).tone_map.is_some());
    let result = decode(&tools.0, &output).unwrap();
    assert_eq!((result.image.width(), result.image.height()), (399, 299));
    let kept = result.heif_gain_map.expect("the gain map's picture");
    assert_eq!((kept.pixels.width(), kept.pixels.height()), (400, 300));
}
