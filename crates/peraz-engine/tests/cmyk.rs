//! CMYK JPEGs (from print workflows) become RGB pictures with the right colors: converted
//! through their color profile when they have one, and never tagged with a profile that does
//! not describe the pixels written. The inputs are made here, with mozjpeg.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image::ImageDecoder as _;
use peraz_core::{ExistingOutput, ImageFormat, Job, Outcome, OutputTarget, Quality, Recipe};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-cmyk-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Stores a (small) color profile in a JPEG being written, as one numbered part.
macro_rules! write_profile_segment {
    ($jpeg:expr, $icc:expr) => {
        $jpeg.write_marker(
            mozjpeg::Marker::APP(2),
            &[&b"ICC_PROFILE\0\x01\x01"[..], $icc].concat(),
        )
    };
}

/// Compresses `input` into a PNG (lossless at the highest level for these flat pictures) and
/// returns its pixels and whether it carries a color profile.
fn to_png(input: &Path, output: &Path) -> (image::RgbImage, Option<Vec<u8>>) {
    let mut recipe = Recipe {
        quality: Quality::Highest,
        ..Recipe::default()
    };
    recipe.image.format = ImageFormat::Png;
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
        "{:?}",
        report.outcome
    );
    let file = std::io::BufReader::new(std::fs::File::open(output).unwrap());
    let mut decoder = image::codecs::png::PngDecoder::new(file).unwrap();
    let icc = decoder.icc_profile().unwrap();
    (
        image::DynamicImage::from_decoder(decoder)
            .unwrap()
            .to_rgb8(),
        icc,
    )
}

const SIZE: u32 = 64;

/// A CMYK JPEG as print tools write it: four quarters of plain ink (cyan, magenta, yellow,
/// black), stored inverted with an Adobe marker, as CMYK or as YCCK, with a profile if given.
fn cmyk_jpeg(ycck: bool, icc: Option<&[u8]>) -> Vec<u8> {
    let mut inks = Vec::new();
    for y in 0..SIZE {
        for x in 0..SIZE {
            let ink: [u8; 4] = match (x < SIZE / 2, y < SIZE / 2) {
                (true, true) => [255, 0, 0, 0],
                (false, true) => [0, 255, 0, 0],
                (true, false) => [0, 0, 255, 0],
                (false, false) => [0, 0, 0, 255],
            };
            // Adobe's convention: 255 means no ink.
            inks.extend(ink.map(|amount| 255 - amount));
        }
    }
    let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_CMYK);
    compress.set_size(SIZE as usize, SIZE as usize);
    compress.set_quality(95.0);
    compress.set_color_space(if ycck {
        mozjpeg::ColorSpace::JCS_YCCK
    } else {
        mozjpeg::ColorSpace::JCS_CMYK
    });
    let mut started = compress.start_compress(Vec::new()).unwrap();
    if let Some(icc) = icc {
        write_profile_segment!(started, icc);
    }
    started.write_scanlines(&inks).unwrap();
    started.finish().unwrap()
}

/// A CMYK profile unlike the plain formula, so that using it shows: only black ink darkens
/// the paper, and cyan, magenta, and yellow ink print nothing at all.
fn black_ink_only_profile() -> Vec<u8> {
    use moxcms::{
        ColorProfile, DataColorSpace, LutDataType, LutStore, LutType, LutWarehouse, Matrix3d,
        ProfileClass,
    };
    // Tables of two entries are straight lines: the inks and the result pass unchanged.
    let straight: Vec<u16> = vec![0, 0xFFFF];
    // One entry per corner of the ink cube, black ink changing fastest: paper white (L 100)
    // without black ink, black (L 0) with it. In 16-bit Lab, 0xFF00 is L 100 and 0x8000 is
    // no color.
    let mut corners = Vec::new();
    for corner in 0..16 {
        let black = corner & 1 == 1;
        corners.extend([if black { 0 } else { 0xFF00 }, 0x8000, 0x8000]);
    }
    let lut = LutDataType {
        num_input_channels: 4,
        num_output_channels: 3,
        num_clut_grid_points: 2,
        matrix: Matrix3d::IDENTITY,
        num_input_table_entries: 2,
        num_output_table_entries: 2,
        input_table: LutStore::Store16(straight.repeat(4)),
        clut_table: LutStore::Store16(corners),
        output_table: LutStore::Store16(straight.repeat(3)),
        lut_type: LutType::Lut16,
    };
    let mut profile = ColorProfile::default();
    profile.pcs = DataColorSpace::Lab;
    profile.color_space = DataColorSpace::Cmyk;
    profile.profile_class = ProfileClass::OutputDevice;
    profile.lut_a_to_b_perceptual = Some(LutWarehouse::Lut(lut.clone()));
    profile.lut_a_to_b_colorimetric = Some(LutWarehouse::Lut(lut));
    profile.encode().unwrap()
}

/// The pixel in the middle of each quarter: cyan, magenta, yellow, and black ink.
fn quarters(image: &image::RgbImage) -> [[u8; 3]; 4] {
    let (near, far) = (SIZE / 4, SIZE * 3 / 4);
    [(near, near), (far, near), (near, far), (far, far)].map(|(x, y)| image.get_pixel(x, y).0)
}

#[test]
fn without_a_profile_inks_become_their_plain_colors() {
    let dir = dir("plain");
    for (ycck, name) in [(false, "cmyk"), (true, "ycck")] {
        let input = dir.0.join(format!("{name}.jpg"));
        std::fs::write(&input, cmyk_jpeg(ycck, None)).unwrap();
        let (pixels, icc) = to_png(&input, &dir.0.join(format!("{name}.png")));
        assert_eq!(icc, None, "{name}: plain sRGB carries no profile");
        let [cyan, magenta, yellow, black] = quarters(&pixels);
        let near =
            |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) < 40);
        assert!(near(cyan, [0, 255, 255]), "{name}: cyan is {cyan:?}");
        assert!(
            near(magenta, [255, 0, 255]),
            "{name}: magenta is {magenta:?}"
        );
        assert!(near(yellow, [255, 255, 0]), "{name}: yellow is {yellow:?}");
        assert!(near(black, [0, 0, 0]), "{name}: black is {black:?}");
    }
}

#[test]
fn with_a_profile_inks_become_the_colors_it_gives_them() {
    let dir = dir("profile");
    let profile = black_ink_only_profile();
    for (ycck, name) in [(false, "cmyk"), (true, "ycck")] {
        let input = dir.0.join(format!("{name}.jpg"));
        std::fs::write(&input, cmyk_jpeg(ycck, Some(&profile))).unwrap();
        let (pixels, icc) = to_png(&input, &dir.0.join(format!("{name}.png")));
        // The pixels are sRGB now: a CMYK profile would be wrong for them.
        assert_eq!(
            icc, None,
            "{name}: the CMYK profile is not written to an RGB picture"
        );
        let [cyan, magenta, yellow, black] = quarters(&pixels);
        for (ink, color) in [("cyan", cyan), ("magenta", magenta), ("yellow", yellow)] {
            assert!(
                color.iter().all(|c| *c > 235),
                "{name}: this profile prints {ink} as paper white, not {color:?}"
            );
        }
        assert!(black.iter().all(|c| *c < 20), "{name}: black is {black:?}");
    }
}

#[test]
fn a_profile_for_other_pixels_is_never_written() {
    let dir = dir("mismatch");
    // An RGB JPEG that wrongly carries a CMYK profile.
    let rgb = image::RgbImage::from_fn(SIZE, SIZE, |x, y| {
        image::Rgb([
            u8::try_from(x * 4 % 256).unwrap(),
            u8::try_from(y * 4 % 256).unwrap(),
            90,
        ])
    });
    let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    compress.set_size(SIZE as usize, SIZE as usize);
    compress.set_quality(95.0);
    let mut started = compress.start_compress(Vec::new()).unwrap();
    write_profile_segment!(started, &black_ink_only_profile());
    started.write_scanlines(rgb.as_raw()).unwrap();
    let input = dir.0.join("tagged.jpg");
    std::fs::write(&input, started.finish().unwrap()).unwrap();
    let (pixels, icc) = to_png(&input, &dir.0.join("tagged.png"));
    assert_eq!(icc, None);
    let (got, want) = (pixels.get_pixel(40, 20).0, rgb.get_pixel(40, 20).0);
    assert!(
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) < 12),
        "the colors are left alone: {got:?} for {want:?}"
    );

    // A matching profile is still kept.
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    compress.set_size(SIZE as usize, SIZE as usize);
    let mut started = compress.start_compress(Vec::new()).unwrap();
    write_profile_segment!(started, &p3);
    started.write_scanlines(rgb.as_raw()).unwrap();
    let input = dir.0.join("p3.jpg");
    std::fs::write(&input, started.finish().unwrap()).unwrap();
    let (_, icc) = to_png(&input, &dir.0.join("p3.png"));
    assert_eq!(icc, Some(p3));
}
