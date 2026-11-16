//! HDR photos: until Peraz carries a gain map over, the report says when one was left out.
//! The JPEG XL part needs libjxl's tools and is skipped (with a note) without them.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use peraz_core::{
    ExistingOutput, ImageFormat, Job, Outcome, OutputTarget, Recipe, Report, SkipReason,
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

#[test]
fn a_jpeg_gain_map_that_is_left_out_is_reported() {
    let dir = dir("jpeg");
    // As Android phones and Photoshop store it (Ultra HDR), and as iPhones do.
    for name in ["hdr-ultrahdr.jpg", "hdr-apple-gainmap.jpg"] {
        let Some(input) = sample(name) else { continue };
        assert!(
            peraz_engine::metadata::has_gain_map(&std::fs::read(&input).unwrap()),
            "{name}"
        );
        for (format, output) in [
            (ImageFormat::Keep, "out.jpg"),
            (ImageFormat::Webp, "out.webp"),
        ] {
            let report = run(&input, &dir.0.join(output), format);
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
        }
        // The result is the standard picture alone: nothing of the gain map is left in it.
        let result = std::fs::read(dir.0.join("out.jpg")).unwrap();
        assert!(!peraz_engine::metadata::has_gain_map(&result), "{name}");
        assert!(image::load_from_memory(&result).is_ok(), "{name}");
    }
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
            (ImageFormat::Jpeg, "out.jpg"),
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

#[test]
fn a_picture_that_is_hdr_by_its_transfer_function_is_left_alone() {
    let dir = dir("pq");
    let Some(input) = sample("hdr-pq.avif") else {
        return;
    };
    let Some(tools) = ffmpeg() else { return };
    // Read as a standard picture, its values come out dark and flat: until Peraz can keep or
    // convert them, the file is skipped, in every format.
    for format in [ImageFormat::Keep, ImageFormat::Jpeg, ImageFormat::Avif] {
        let output = dir.0.join("out.bin");
        let report = run_with(Some(&tools), &input, &output, format);
        let Outcome::Skipped { reason, message } = &report.outcome else {
            panic!("{format}: unexpected outcome: {:?}", report.outcome)
        };
        assert_eq!(*reason, SkipReason::Unsupported, "{format}");
        assert!(message.contains("HDR (PQ or HLG)"), "{format}: {message}");
        assert!(!output.exists(), "{format}");
    }
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
