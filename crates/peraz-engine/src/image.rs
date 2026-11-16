//! Still-image encoding: decode, make upright, resize, then mozjpeg, imagequant + oxipng,
//! libwebp, rav1e (AVIF), cjxl (JPEG XL, run as a process), or Deflate (TIFF).
//!
//! Metadata policy: the EXIF orientation is applied to the pixels and the ICC color profile is
//! kept (in every output format, so wide-gamut pictures stay wide-gamut). Everything else (EXIF,
//! XMP, IPTC) is dropped by default, because it can hold location and device details, and
//! written to the output on request, with the orientation in it reset.

use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, RgbaImage, imageops::FilterType};
use peraz_core::plan::fit_within;
use peraz_core::{ImageEncoding, ImagePlan};

use crate::ffmpeg::Toolchain;
use crate::metadata::Metadata;

/// An encoded image, ready to be written.
#[derive(Debug)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// What did the work, for the report.
    pub encoder: String,
    /// What could not be written although it was asked for (metadata that did not fit).
    pub warnings: Vec<String>,
    /// The input's HDR gain map is in the result.
    pub kept_gain_map: bool,
}

/// A decoded picture, already upright, with its color profile and its metadata (which is
/// written to the output only on request).
#[derive(Debug, Clone)]
pub struct Decoded {
    pub image: DynamicImage,
    pub icc: Option<Vec<u8>>,
    pub metadata: Metadata,
    /// The file holds an HDR gain map next to this picture.
    pub gain_map: bool,
    /// The gain map of a JPEG, which a JPEG made from it can carry.
    pub jpeg_gain_map: Option<crate::gainmap::JpegGainMap>,
    /// The gain map of a HEIC or AVIF, which a HEIC made from it can carry.
    pub heif_gain_map: Option<crate::heif::GainMap>,
    /// What a PNG says about its colors without a profile (gamma, primaries, "this is sRGB").
    pub png_color: PngColor,
}

/// The chunks of a PNG that say what its colors mean: `gAMA`, `cHRM`, `sRGB`, and `cICP`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PngColor {
    /// The chunks as they are in the file, to be written into a PNG made from it.
    pub chunks: Vec<u8>,
    /// The picture's color profile was made from these chunks, for formats that cannot hold
    /// them: a PNG gets the chunks, not the profile.
    pub profile_from_chunks: bool,
}

/// Encodes a decoded picture according to `plan`. `tools` is the FFmpeg to use for the formats
/// that need one (HEIC).
pub fn encode_image(
    tools: Option<&Toolchain>,
    decoded: Decoded,
    plan: &ImagePlan,
) -> Result<EncodedImage, String> {
    encode_prepared(tools, &prepare(decoded, plan), plan)
}

/// Fits a decoded picture in the plan's box: the pixels the encoder gets.
pub fn prepare(decoded: Decoded, plan: &ImagePlan) -> Decoded {
    let Decoded {
        mut image,
        icc,
        metadata,
        gain_map,
        jpeg_gain_map,
        heif_gain_map,
        png_color,
    } = decoded;
    if let Some((w, h)) = fit_within(
        (image.width(), image.height()),
        plan.max_width,
        plan.max_height,
    ) {
        image = image.resize_exact(w, h, FilterType::Lanczos3);
    }
    Decoded {
        image,
        icc,
        metadata,
        gain_map,
        jpeg_gain_map,
        heif_gain_map,
        png_color,
    }
}

/// Encodes a picture that [`prepare`] made ready, according to `plan`.
pub fn encode_prepared(
    tools: Option<&Toolchain>,
    prepared: &Decoded,
    plan: &ImagePlan,
) -> Result<EncodedImage, String> {
    // mozjpeg and imagequant report some failures by panicking; keep them contained to this file.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        encode_image_inner(tools, prepared, plan)
    }))
    .unwrap_or_else(|_| Err("the image encoder crashed on this file".to_owned()))
}

fn encode_image_inner(
    tools: Option<&Toolchain>,
    prepared: &Decoded,
    plan: &ImagePlan,
) -> Result<EncodedImage, String> {
    let Decoded {
        image,
        icc,
        metadata,
        png_color,
        ..
    } = prepared;
    // The metadata to write: only on request, and only when there is any.
    let kept = Some(metadata).filter(|m| plan.keep_metadata && !m.is_empty());
    let (width, height) = (image.width(), image.height());
    let mut warnings = Vec::new();
    let mut kept_gain_map = false;
    let (bytes, encoder) = match plan.encoding {
        ImageEncoding::Jpeg {
            quality,
            progressive,
        } => {
            let (jpeg, settings) = jpeg_of(image, plan, quality, progressive, icc.as_deref())?;
            warnings.extend(kept.map(Metadata::too_large_for_jpeg).unwrap_or_default());
            let mut jpeg = kept.map_or_else(|| jpeg.clone(), |m| m.into_jpeg(&jpeg));
            // An HDR photo stays one: its gain map follows the picture, turned and scaled
            // like it. When that fails, the standard picture is still written.
            if let Some(map) = &prepared.jpeg_gain_map {
                match gain_map_jpeg(map, (width, height), settings) {
                    Ok(encoded) => {
                        jpeg = map.attach(&jpeg, &encoded);
                        kept_gain_map = true;
                    }
                    Err(why) => warnings.push(format!("the HDR gain map was left out: {why}")),
                }
            }
            let name = if kept_gain_map {
                format!("{}, HDR gain map kept", settings.name())
            } else {
                settings.name()
            };
            (jpeg, name)
        }
        ImageEncoding::Png {
            min_quality,
            max_quality,
        } => {
            // A PNG states its gamma and primaries in its own chunks, not in a profile made
            // from them.
            let profile = icc.as_deref().filter(|_| !png_color.profile_from_chunks);
            // A size target may have cut the palette down below what the quality range gives.
            let palette = plan.target.and_then(|target| target.palette);
            let (png, encoder, reduced) =
                encode_png(image, min_quality, max_quality, profile, palette)?;
            warnings.extend(reduced);
            let png = with_png_chunks(&png, &png_color.chunks);
            (
                kept.map_or_else(|| png.clone(), |m| m.into_png(&png)),
                encoder,
            )
        }
        ImageEncoding::Webp { quality } => (
            encode_webp(&image.to_rgba8(), quality, icc.as_deref(), kept)?,
            format!("libwebp q{quality}"),
        ),
        ImageEncoding::Jxl { quality, .. } => (
            crate::jxl::encode(prepared, quality, kept)?,
            format!("cjxl q{quality}"),
        ),
        ImageEncoding::Heic { quality } => {
            let tools = tools.ok_or("HEIC output needs FFmpeg, which was not found")?;
            let (heic, with_gain_map) = crate::heic::encode(tools, prepared, quality, kept)?;
            kept_gain_map = with_gain_map;
            let note = if with_gain_map {
                ", HDR gain map kept"
            } else {
                ""
            };
            (
                heic,
                format!("x265 crf {}{note}", crate::heic::crf(quality)),
            )
        }
        ImageEncoding::Tiff => {
            let mut tiff = crate::tiff::encode(image, icc.as_deref(), kept)?;
            // EXIF in a TIFF is not one block: its tags join the file's own directories.
            if let Some(exif) = kept.and_then(|m| m.exif.as_deref()) {
                match crate::exif::into_tiff(tiff.clone(), exif) {
                    Some(tagged) => tiff = tagged,
                    None => warnings.push(
                        "the EXIF data could not be written into the TIFF and was left out"
                            .to_owned(),
                    ),
                }
            }
            (tiff, crate::tiff::ENCODER.to_owned())
        }
        // SVGs and icon files are rewritten whole and never reach the pixel encoders.
        ImageEncoding::Svg { .. } | ImageEncoding::Icon { .. } => {
            return Err("internal error: this file is not encoded from pixels".to_owned());
        }
        ImageEncoding::Avif { quality } => {
            avif_of(image, quality, icc.as_deref(), kept, &mut warnings)?
        }
    };
    Ok(EncodedImage {
        bytes,
        width,
        height,
        encoder,
        warnings,
        kept_gain_map,
    })
}

/// The quality a gain map is encoded at, at least: it is smooth, so this costs little, and
/// its errors would show as halos in the bright parts of the picture.
const GAIN_MAP_QUALITY: u8 = 85;

/// A JPEG's gain map for the picture at its new `size`: encoded again as a JPEG (gray if it
/// was gray), with the description the stored gain map had.
fn gain_map_jpeg(
    map: &crate::gainmap::JpegGainMap,
    size: (u32, u32),
    picture: JpegSettings,
) -> Result<Vec<u8>, String> {
    let pixels = map.pixels_for(size)?;
    let settings = JpegSettings {
        quality: picture.quality.max(GAIN_MAP_QUALITY),
        full_color: false,
        ..picture
    };
    let (width, height) = (pixels.width(), pixels.height());
    let encoded = if pixels.color().channel_count() == 1 {
        let space = mozjpeg::ColorSpace::JCS_GRAYSCALE;
        jpeg_from_pixels(
            pixels.to_luma8().as_raw(),
            width,
            height,
            space,
            settings,
            None,
        )?
    } else {
        let space = mozjpeg::ColorSpace::JCS_RGB;
        jpeg_from_pixels(
            pixels.to_rgb8().as_raw(),
            width,
            height,
            space,
            settings,
            None,
        )?
    };
    Ok(map.described(&encoded))
}

/// Decodes a JPEG/PNG/WebP/BMP/TIFF image with its EXIF orientation applied and its ICC profile.
pub fn decode_native(input: &Path) -> Result<Decoded, String> {
    let bytes =
        std::fs::read(input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;
    decode_native_bytes(&bytes, &input.display().to_string())
}

/// Like [`decode_native`], for an image held in memory (e.g. inside an EPUB). `name` is used in
/// error messages.
pub fn decode_native_bytes(bytes: &[u8], name: &str) -> Result<Decoded, String> {
    if !jpeg_is_complete(bytes) {
        return Err(format!(
            "{name} is incomplete: the file ends before the picture does (it may have been cut \
             off while copying or downloading)"
        ));
    }
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("cannot read {name}: {e}"))?;
    let mut decoded = decode_reader(reader, name)?;
    let mut converted_from_cmyk = false;
    // A CMYK JPEG's inks mean what its profile says: converted through it, the pixels are
    // sRGB. Without one, the decoder's plain conversion stands.
    if let Some(profile) = crate::metadata::jpeg_profile(bytes)
        .filter(|profile| profile_color_space(profile) == Some(*b"CMYK"))
        && let Some(mut pixels) = cmyk_jpeg_to_srgb(bytes, &profile)
    {
        // The picture was already turned upright once: do the same to the new pixels.
        let orientation = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(bytes))
            .and_then(|mut decoder| decoder.orientation())
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        pixels.apply_orientation(orientation);
        decoded.image = pixels;
        decoded.icc = None;
        converted_from_cmyk = true;
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // A JPEG's segments are read here, to get the Photoshop resources (IPTC) too.
        decoded.metadata = Metadata::from_jpeg(bytes);
        // The decoder finds no profile when its parts are numbered from 0.
        if decoded.icc.is_none() && !converted_from_cmyk {
            decoded.icc = crate::metadata::jpeg_profile(bytes)
                .filter(|profile| profile_fits(profile, decoded.image.color()));
        }
        decoded.gain_map = crate::metadata::has_gain_map(bytes);
        // The gain map itself, with how the picture was stored: it shares the picture's turn
        // and is scaled with it.
        decoded.jpeg_gain_map = crate::gainmap::of_jpeg(bytes).map(|mut map| {
            map.orientation = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(bytes))
                .and_then(|mut decoder| decoder.orientation())
                .unwrap_or(image::metadata::Orientation::NoTransforms);
            map.picture_size = (decoded.image.width(), decoded.image.height());
            map
        });
    } else if bytes.starts_with(b"\x89PNG") {
        let (color, profile) = png_color(bytes, decoded.icc.is_some());
        decoded.png_color = color;
        if let Some(profile) = profile {
            decoded.icc = Some(profile);
        }
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        decoded.metadata.photoshop = crate::tiff::photoshop_resources(bytes);
        // A TIFF keeps its EXIF among its own directories: gathered into one block here.
        if decoded.metadata.exif.is_none()
            && let Some(block) = crate::exif::of_tiff(bytes)
        {
            decoded.metadata.set_exif(&block);
        }
    }
    Ok(decoded)
}

fn decode_reader<R: std::io::BufRead + std::io::Seek>(
    reader: ImageReader<R>,
    name: &str,
) -> Result<Decoded, String> {
    let error = |e: image::ImageError| format!("cannot decode {name}: {e}");
    let mut decoder = reader.into_decoder().map_err(error)?;
    let orientation = decoder.orientation().map_err(error)?;
    let icc = decoder.icc_profile().map_err(error)?;
    let metadata = decoder_metadata(&mut decoder);
    let mut image = DynamicImage::from_decoder(decoder).map_err(error)?;
    image.apply_orientation(orientation);
    Ok(Decoded {
        // A profile for other kinds of pixels (CMYK on RGB) would give wrong colors.
        icc: icc.filter(|profile| profile_fits(profile, image.color())),
        image,
        metadata,
        gain_map: false,
        jpeg_gain_map: None,
        heif_gain_map: None,
        png_color: PngColor::default(),
    })
}

/// The color space an ICC profile is for (`RGB `, `GRAY`, `CMYK`, …), from its header.
fn profile_color_space(icc: &[u8]) -> Option<[u8; 4]> {
    icc.get(16..20)?.try_into().ok()
}

/// Whether a color profile can describe pixels of this kind: an RGB profile for color
/// pictures, a gray one for gray pictures. A profile of a kind this cannot tell is kept.
fn profile_fits(icc: &[u8], pixels: image::ColorType) -> bool {
    let gray = pixels.channel_count() <= 2;
    match profile_color_space(icc).as_ref() {
        Some(b"RGB ") => !gray,
        Some(b"GRAY") => gray,
        Some(b"CMYK") => false,
        _ => true,
    }
}

/// Decodes a CMYK (or YCCK) JPEG to its ink values and converts them to sRGB through the
/// JPEG's CMYK profile. `None` when the file is not CMYK or cannot be converted.
fn cmyk_jpeg_to_srgb(jpeg: &[u8], profile: &[u8]) -> Option<DynamicImage> {
    // Files with an Adobe segment store the inks inverted (255 is no ink).
    let inverted = jpeg
        .windows(9)
        .take(64 * 1024)
        .any(|w| w == b"\xFF\xEE\x00\x0EAdobe");
    // mozjpeg reports some failures by panicking.
    let decoded = std::panic::catch_unwind(|| {
        let decoder = mozjpeg::Decompress::new_mem(jpeg).ok()?;
        if !matches!(
            decoder.color_space(),
            mozjpeg::ColorSpace::JCS_CMYK | mozjpeg::ColorSpace::JCS_YCCK
        ) {
            return None;
        }
        let mut started = decoder.to_colorspace(mozjpeg::ColorSpace::JCS_CMYK).ok()?;
        let size = (
            u32::try_from(started.width()).ok()?,
            u32::try_from(started.height()).ok()?,
        );
        let inks: Vec<u8> = started.read_scanlines::<u8>().ok()?;
        started.finish().ok()?;
        Some((size, inks))
    })
    .ok()??;
    let ((width, height), mut inks) = decoded;
    if inverted {
        for ink in &mut inks {
            *ink = 255 - *ink;
        }
    }
    let source = moxcms::ColorProfile::new_from_slice(profile).ok()?;
    let transform = source
        .create_transform_8bit(
            // Four channels: cyan, magenta, yellow, black.
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgb,
            moxcms::TransformOptions::default(),
        )
        .ok()?;
    let mut rgb = vec![0u8; inks.len() / 4 * 3];
    transform.transform(&inks, &mut rgb).ok()?;
    RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
}

/// The EXIF and XMP a decoder found in its file. Metadata that cannot be read is left out:
/// the picture itself is what matters.
pub(crate) fn decoder_metadata(decoder: &mut impl ImageDecoder) -> Metadata {
    let mut metadata = Metadata::default();
    if let Ok(Some(exif)) = decoder.exif_metadata() {
        metadata.set_exif(&exif);
    }
    if let Ok(Some(xmp)) = decoder.xmp_metadata() {
        metadata.set_xmp(xmp);
    }
    metadata
}

/// Why `path` is a cut-off JPEG or GIF, or `None` when it is complete (or another format).
pub fn incomplete(path: &Path) -> Option<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 6];
    file.read_exact(&mut head).ok()?;
    let complete = if head.starts_with(b"GIF8") {
        // A GIF ends with its trailer byte `;` (sometimes followed by zero padding).
        let mut tail = Vec::new();
        let len = file.seek(SeekFrom::End(0)).ok()?;
        file.seek(SeekFrom::Start(len.saturating_sub(64))).ok()?;
        file.read_to_end(&mut tail).ok()?;
        tail.iter().rev().find(|b| **b != 0) == Some(&b';')
    } else if head.starts_with(&[0xFF, 0xD8]) {
        jpeg_is_complete(&std::fs::read(path).ok()?)
    } else {
        true
    };
    (!complete).then(|| {
        format!(
            "{} is incomplete: the file ends before the picture does (it may have been cut \
             off while copying or downloading)",
            path.display()
        )
    })
}

/// False for a JPEG that was cut off: its last scan (`FFDA`) has no end marker (`FFD9`) after
/// it. Decoders fill the missing part with gray instead of failing, so this is checked first.
/// Marker bytes cannot occur inside the compressed data (`FF` is escaped there), so an embedded
/// thumbnail's markers come earlier and do not count. Other formats are always "complete" here.
fn jpeg_is_complete(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return true;
    }
    let last_scan = bytes.windows(2).rposition(|w| w == [0xFF, 0xDA]);
    let Some(scan) = last_scan else {
        return false;
    };
    bytes[scan..].windows(2).any(|w| w == [0xFF, 0xD9])
}

/// The warning for metadata that the AVIF container could not take.
const AVIF_ITEM_LEFT_OUT: &str = "the {} could not be written into the AVIF and was left out";

/// The picture as an AVIF, with the encoder's name: with its color profile, and with the EXIF
/// block and the XMP packet of `kept`.
fn avif_of(
    image: &DynamicImage,
    quality: u8,
    icc: Option<&[u8]>,
    kept: Option<&Metadata>,
    warnings: &mut Vec<String>,
) -> Result<(Vec<u8>, String), String> {
    let mut rgba = image.to_rgba8();
    let avif = encode_avif(&rgba, quality)?;
    let tagged = match icc {
        Some(profile) => crate::heif::embed_icc(&avif, profile).ok_or(profile),
        None => Ok(avif),
    };
    let (mut avif, encoder) = match tagged {
        Ok(avif) => (avif, format!("rav1e q{quality}")),
        // The container could not take the profile: keep the colors by converting.
        Err(profile) => {
            let note = if to_srgb(&mut rgba, profile)? {
                " (converted to sRGB)"
            } else {
                ""
            };
            (
                encode_avif(&rgba, quality)?,
                format!("rav1e q{quality}{note}"),
            )
        }
    };
    // The EXIF block and the XMP packet are added to the encoder's file as items of their own,
    // each in one piece: the encoder has no place for XMP, and stores EXIF in two pieces, which
    // FFmpeg does not read.
    if let Some(exif) = kept.and_then(|m| m.exif.as_deref()) {
        match crate::heif::embed_exif(&avif, exif) {
            Some(with_item) => avif = with_item,
            None => warnings.push(AVIF_ITEM_LEFT_OUT.replace("{}", "EXIF data")),
        }
    }
    if let Some(xmp) = kept.and_then(|m| m.xmp.as_deref()) {
        match crate::heif::embed_xmp(&avif, xmp) {
            Some(with_item) => avif = with_item,
            None => warnings.push(AVIF_ITEM_LEFT_OUT.replace("{}", "XMP packet")),
        }
    }
    Ok((avif, encoder))
}

/// The picture as a JPEG, with the settings it was written with: gray stays one channel,
/// transparency goes on the plan's background, and sharp colored edges keep full color.
fn jpeg_of(
    image: &DynamicImage,
    plan: &ImagePlan,
    quality: u8,
    progressive: bool,
    icc: Option<&[u8]>,
) -> Result<(Vec<u8>, JpegSettings), String> {
    let (width, height) = (image.width(), image.height());
    let mut settings = JpegSettings {
        quality,
        progressive,
        full_color: false,
    };
    let jpeg = if image.color().channel_count() == 1 {
        // A gray picture stays one channel: smaller, and nothing to get wrong.
        let gray = image.to_luma8();
        let space = mozjpeg::ColorSpace::JCS_GRAYSCALE;
        jpeg_from_pixels(gray.as_raw(), width, height, space, settings, icc)?
    } else {
        let rgb = match plan.background {
            Some(color) if plan.flatten_alpha => flatten_on(&image.to_rgba8(), color),
            _ => image.to_rgb8(),
        };
        settings.full_color = has_sharp_color_edges(&rgb);
        let space = mozjpeg::ColorSpace::JCS_RGB;
        jpeg_from_pixels(rgb.as_raw(), width, height, space, settings, icc)?
    };
    Ok((jpeg, settings))
}

/// Composites transparent pixels onto `background`, for formats without alpha.
fn flatten_on(rgba: &RgbaImage, background: peraz_core::Color) -> RgbImage {
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
        let blend = |channel: u8, under: u8| {
            let (c, u, a) = (u32::from(channel), u32::from(under), u32::from(alpha));
            // Rounded (c·a + u·(255 − a)) / 255; always fits in u8.
            u8::try_from((c * a + u * (255 - a) + 127) / 255).unwrap_or(u8::MAX)
        };
        let [under_red, under_green, under_blue] = background.0;
        image::Rgb([
            blend(red, under_red),
            blend(green, under_green),
            blend(blue, under_blue),
        ])
    })
}

/// How a JPEG is written.
#[derive(Debug, Clone, Copy)]
struct JpegSettings {
    quality: u8,
    /// Several passes from coarse to fine; otherwise a baseline JPEG, in one pass.
    progressive: bool,
    /// Color at every pixel (4:4:4) instead of one color value for each two-by-two block
    /// (4:2:0).
    full_color: bool,
}

impl JpegSettings {
    /// The encoder's name for the report.
    fn name(self) -> String {
        format!(
            "mozjpeg q{}{}{}",
            self.quality,
            if self.full_color { " 4:4:4" } else { "" },
            if self.progressive { "" } else { " baseline" }
        )
    }
}

/// How far a color value must be from the average of its two-by-two block to count as a sharp
/// colored edge (of 255).
const SHARP_COLOR_STEP: i32 = 32;
/// The share of such pixels, in a hundred thousand, above which a picture keeps its color at
/// every pixel: 0.1%.
const SHARP_COLOR_SHARE: usize = 100;

/// Whether the picture has sharp colored edges: colored text and thin lines, as in screenshots
/// and charts. A JPEG usually stores one color value for each two-by-two block (4:2:0), which
/// photos do not show, but which smears such edges.
///
/// Measured with SSIMULACRA2 on 24 photos and on screenshots with colored text. Photos reach
/// the same score in about 13% less space with 4:2:0, and at most 0.06% of their pixels count
/// here. Screenshots with colored text do not get past a score of 80 with 4:2:0 at any
/// quality, and reach it in half the space with 4:4:4; 0.13% of their pixels and more count.
fn has_sharp_color_edges(rgb: &RgbImage) -> bool {
    // The two color differences of YCbCr, scaled by 256.
    let color = |pixel: &image::Rgb<u8>| {
        let [red, green, blue] = pixel.0.map(i32::from);
        (
            -43 * red - 85 * green + 128 * blue,
            128 * red - 107 * green - 21 * blue,
        )
    };
    let (width, height) = (rgb.width() / 2 * 2, rgb.height() / 2 * 2);
    let mut sharp = 0usize;
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let block = [(0, 0), (1, 0), (0, 1), (1, 1)]
                .map(|(right, down)| color(rgb.get_pixel(x + right, y + down)));
            let average = (
                block.iter().map(|c| c.0).sum::<i32>() / 4,
                block.iter().map(|c| c.1).sum::<i32>() / 4,
            );
            sharp += block
                .iter()
                .filter(|c| {
                    (c.0 - average.0).abs().max((c.1 - average.1).abs()) > SHARP_COLOR_STEP * 256
                })
                .count();
        }
    }
    let pixels = width as usize * height as usize;
    pixels > 0 && sharp * 100_000 > pixels * SHARP_COLOR_SHARE
}

/// Encodes a grayscale or RGB picture as JPEG with mozjpeg (no ICC profile), for documents
/// that keep their color space elsewhere. Panics inside mozjpeg are contained.
pub(crate) fn encode_jpeg_plain(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let (pixels, space) = if image.color().channel_count() == 1 {
        (
            image.to_luma8().into_raw(),
            mozjpeg::ColorSpace::JCS_GRAYSCALE,
        )
    } else {
        (image.to_rgb8().into_raw(), mozjpeg::ColorSpace::JCS_RGB)
    };
    let (width, height) = (image.width(), image.height());
    let settings = JpegSettings {
        quality,
        progressive: true,
        full_color: false,
    };
    std::panic::catch_unwind(move || {
        jpeg_from_pixels(&pixels, width, height, space, settings, None)
    })
    .unwrap_or_else(|_| Err("mozjpeg crashed on this picture".to_owned()))
}

fn jpeg_from_pixels(
    pixels: &[u8],
    width: u32,
    height: u32,
    space: mozjpeg::ColorSpace,
    settings: JpegSettings,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut compress = mozjpeg::Compress::new(space);
    compress.set_size(width as usize, height as usize);
    compress.set_quality(f32::from(settings.quality));
    if settings.full_color && space == mozjpeg::ColorSpace::JCS_RGB {
        compress.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
    }
    // mozjpeg's defaults already use progressive scans and optimized Huffman tables. Without
    // its scans, the picture is written in one pass, which makes a baseline JPEG; trellis
    // quantization and the optimized tables stay.
    if !settings.progressive {
        compress.set_optimize_scans(false);
        // A baseline JPEG has 8-bit quantization tables. mozjpeg's table has values that go
        // past 255 below quality 70, so the same table is set here with its values held at 255.
        let quality = f32::from(settings.quality.clamp(1, 100));
        let table = mozjpeg::qtable::NRobidoux.scaled(quality, quality);
        compress.set_luma_qtable(&table);
        compress.set_chroma_qtable(&table);
    }
    let mut started = compress
        .start_compress(Vec::new())
        .map_err(|e| format!("mozjpeg: {e}"))?;
    if let Some(icc) = icc {
        for segment in jpeg_profile_segments(icc) {
            started.write_marker(mozjpeg::Marker::APP(2), &segment);
        }
    }
    started
        .write_scanlines(pixels)
        .map_err(|e| format!("mozjpeg: {e}"))?;
    started.finish().map_err(|e| format!("mozjpeg: {e}"))
}

/// A color profile as the `APP2` segments of a JPEG: split into parts, each with its number
/// (from 1) and the number of parts, as the ICC specification asks. Readers that check the
/// numbering ignore a profile whose parts are counted from 0.
fn jpeg_profile_segments(icc: &[u8]) -> Vec<Vec<u8>> {
    const HEADER: &[u8] = b"ICC_PROFILE\0";
    // A segment holds 65533 bytes after its length: the header, the two counts, then data.
    const PART: usize = 65_533 - HEADER.len() - 2;
    let Ok(parts) = u8::try_from(icc.len().div_ceil(PART)) else {
        // More than 255 parts (over 16 MB) cannot be numbered: no real profile is that large.
        return Vec::new();
    };
    (1..=parts)
        .zip(icc.chunks(PART))
        .map(|(number, part)| [HEADER, &[number, parts], part].concat())
        .collect()
}

/// Quantizes to a palette when the quality range can be met, otherwise keeps true color; then
/// optimizes losslessly. Returns the PNG and a description of the encoder.
fn encode_png(
    image: &DynamicImage,
    min_quality: u8,
    max_quality: u8,
    icc: Option<&[u8]>,
    cut: Option<peraz_core::PngPalette>,
) -> Result<(Vec<u8>, String, Option<String>), String> {
    let (width, height) = (image.width(), image.height());
    // 16 bits per channel: kept on the lossless path, reduced by a palette (which holds 8-bit
    // colors), and then the report says so.
    let deep = image.color().bytes_per_pixel() / image.color().channel_count() == 2;
    let rgba = image.to_rgba8();
    let quantized = match cut {
        Some(cut) => Some(quantize_cut(&rgba, cut)?),
        None => quantize(&rgba, min_quality, max_quality)?,
    };
    let mut reduced = None;
    let (png, encoder) = if let Some((palette, indexes, achieved)) = quantized {
        let rgb: Vec<u8> = palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect();
        let mut alpha: Vec<u8> = palette.iter().map(|c| c.a).collect();
        while alpha.last() == Some(&255) {
            alpha.pop();
        }
        let pixels = PngPixels::Indexed {
            palette: &rgb,
            alpha: &alpha,
            indexes: &indexes,
        };
        if deep {
            reduced = Some(
                "the 16-bit picture was reduced to a palette of 8-bit colors; use -q highest \
                 to keep its depth where a palette is not good enough"
                    .to_owned(),
            );
        }
        let name = match cut {
            Some(cut) => format!(
                "imagequant {} colors{} + oxipng",
                palette.len(),
                if cut.dithering { "" } else { ", no dithering" }
            ),
            None => format!("imagequant q{achieved} + oxipng"),
        };
        (write_png(width, height, &pixels, icc)?, name)
    } else if deep {
        let (color, samples): (png::ColorType, Vec<u16>) = match image {
            DynamicImage::ImageLuma16(gray) => (png::ColorType::Grayscale, gray.as_raw().clone()),
            DynamicImage::ImageLumaA16(gray) => {
                (png::ColorType::GrayscaleAlpha, gray.as_raw().clone())
            }
            DynamicImage::ImageRgb16(rgb) => (png::ColorType::Rgb, rgb.as_raw().clone()),
            other => (png::ColorType::Rgba, other.to_rgba16().into_raw()),
        };
        // PNG stores the high byte first.
        let data = samples
            .iter()
            .flat_map(|sample| sample.to_be_bytes())
            .collect();
        (
            write_png(width, height, &PngPixels::Deep { color, data }, icc)?,
            "oxipng (lossless)".to_owned(),
        )
    } else {
        let opaque = rgba.pixels().all(|p| p[3] == 255);
        let pixels = if opaque {
            PngPixels::Rgb(DynamicImage::ImageRgba8(rgba.clone()).to_rgb8().into_raw())
        } else {
            PngPixels::Rgba(rgba.as_raw())
        };
        (
            write_png(width, height, &pixels, icc)?,
            "oxipng (lossless)".to_owned(),
        )
    };
    let options = oxipng::Options {
        strip: oxipng::StripChunks::Safe,
        ..oxipng::Options::from_preset(2)
    };
    let optimized =
        oxipng::optimize_from_memory(&png, &options).map_err(|e| format!("oxipng: {e}"))?;
    Ok((optimized, encoder, reduced))
}

/// The names of the PNG chunks that say what the colors mean when there is no profile.
const PNG_COLOR_CHUNKS: [&[u8; 4]; 4] = [b"gAMA", b"cHRM", b"sRGB", b"cICP"];

/// A PNG with color chunks (as taken from another PNG) added right after its header, where
/// they belong: before the palette and the picture data.
fn with_png_chunks(png: &[u8], chunks: &[u8]) -> Vec<u8> {
    // The signature (8 bytes), then the header chunk (25 bytes with its frame).
    const AFTER_HEADER: usize = 33;
    if chunks.is_empty() || png.len() < AFTER_HEADER {
        return png.to_vec();
    }
    [&png[..AFTER_HEADER], chunks, &png[AFTER_HEADER..]].concat()
}

/// Reads what a PNG says about its colors: the chunks themselves, and, when they describe
/// something other than sRGB and the file has no profile, a color profile that says the same
/// for the formats that cannot hold the chunks.
fn png_color(png: &[u8], has_profile: bool) -> (PngColor, Option<Vec<u8>>) {
    let mut color = PngColor::default();
    let (mut gamma, mut primaries, mut is_srgb, mut cicp) = (None, None, false, None);
    let mut at = 8;
    while let Some(head) = png.get(at..at + 8) {
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(whole) = png.get(at..at + length + 12) else {
            break;
        };
        let data = &whole[8..8 + length];
        let word = |index: usize| {
            data.get(index * 4..index * 4 + 4)
                .map(|b| f64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) / 100_000.0)
        };
        match &head[4..] {
            b"IDAT" | b"IEND" => break,
            b"gAMA" => gamma = word(0),
            b"cHRM" => primaries = (0..8).map(word).collect::<Option<Vec<f64>>>(),
            b"sRGB" => is_srgb = true,
            b"cICP" => cicp = data.get(..4).map(|c| [c[0], c[1], c[2], c[3]]),
            _ => {}
        }
        if PNG_COLOR_CHUNKS.iter().any(|name| head[4..] == **name) {
            color.chunks.extend_from_slice(whole);
        }
        at += length + 12;
    }
    // In order of precedence: `cICP`, a profile, `sRGB`, then gamma and primaries.
    let profile = match (cicp, gamma) {
        (Some(cicp), _) => profile_from_cicp(cicp),
        (None, Some(gamma)) if !has_profile && !is_srgb && gamma > 0.0 => {
            profile_from_gamma(gamma, primaries.as_deref())
        }
        _ => None,
    };
    color.profile_from_chunks = profile.is_some();
    (color, profile)
}

/// A color profile for what a `cICP` chunk says (primaries, transfer function, matrix, range).
/// `None` for sRGB, which an unmarked picture is taken for anyway, and for what a profile
/// cannot say (HDR transfer functions).
fn profile_from_cicp([primaries, transfer, _, _]: [u8; 4]) -> Option<Vec<u8>> {
    // BT.709 primaries with the sRGB curve; and PQ (16) and HLG (18), which are HDR.
    if (primaries, transfer) == (1, 13) || matches!(transfer, 16 | 18) {
        return None;
    }
    let cicp = moxcms::CicpProfile {
        color_primaries: primaries.try_into().ok()?,
        transfer_characteristics: transfer.try_into().ok()?,
        matrix_coefficients: moxcms::MatrixCoefficients::Identity,
        full_range: true,
    };
    let mut profile = moxcms::ColorProfile::new_from_cicp(cicp);
    // Code points without known primaries or curve leave the profile empty.
    if profile.red_trc.is_none() || profile.red_colorant.x == 0.0 {
        return None;
    }
    // A plain profile, for every reader: the code points are in the PNG chunk already.
    profile.cicp = None;
    profile.encode().ok()
}

/// A color profile for pixels with this PNG gamma (the encoding exponent: 0.45455 for the
/// usual 2.2) and these chromaticities (white, red, green, blue, as x and y), or sRGB's when
/// none are given. `None` when that is what an unmarked picture is taken for anyway: a gamma
/// of 2.2 with sRGB's primaries.
fn profile_from_gamma(gamma: f64, primaries: Option<&[f64]>) -> Option<Vec<u8>> {
    const SRGB: [f64; 8] = [0.3127, 0.3290, 0.64, 0.33, 0.30, 0.60, 0.15, 0.06];
    let usual = (gamma - 1.0 / 2.2).abs() < 0.001
        && primaries.is_none_or(|p| p.iter().zip(SRGB).all(|(a, b)| (a - b).abs() < 0.001));
    if usual {
        return None;
    }
    let p = primaries.unwrap_or(&SRGB);
    let mut profile = moxcms::ColorProfile::new_srgb();
    #[allow(clippy::cast_possible_truncation)]
    let point = |index: usize| moxcms::Chromaticity {
        x: p[index * 2] as f32,
        y: p[index * 2 + 1] as f32,
    };
    profile.update_rgb_colorimetry(
        moxcms::XyY {
            x: p[0],
            y: p[1],
            yb: 1.0,
        },
        moxcms::ColorPrimaries {
            red: point(1),
            green: point(2),
            blue: point(3),
        },
    );
    // Decoding raises the stored values to the inverse of the encoding exponent.
    #[allow(clippy::cast_possible_truncation)]
    let curve = moxcms::ToneReprCurve::Parametric(vec![(1.0 / gamma) as f32]);
    profile.red_trc = Some(curve.clone());
    profile.green_trc = Some(curve.clone());
    profile.blue_trc = Some(curve);
    profile.cicp = None;
    profile.encode().ok()
}

/// Palette, per-pixel indexes, and achieved quality; `None` when the minimum quality is out of reach.
pub(crate) type Quantized = (Vec<imagequant::RGBA>, Vec<u8>, u8);

pub(crate) fn quantize(
    rgba: &RgbaImage,
    min_quality: u8,
    max_quality: u8,
) -> Result<Option<Quantized>, String> {
    let liq_error = |e: imagequant::Error| format!("imagequant: {e}");
    let mut attributes = imagequant::new();
    attributes
        .set_quality(min_quality, max_quality)
        .map_err(liq_error)?;
    quantize_with(rgba, attributes, 1.0)
}

/// The best palette of at most `cut.colors` colors, whatever quality that is: for a picture
/// that has to fit a size.
fn quantize_cut(rgba: &RgbaImage, cut: peraz_core::PngPalette) -> Result<Quantized, String> {
    let liq_error = |e: imagequant::Error| format!("imagequant: {e}");
    let mut attributes = imagequant::new();
    attributes
        .set_max_colors(u32::from(cut.colors.clamp(2, 256)))
        .map_err(liq_error)?;
    let dithering = if cut.dithering { 1.0 } else { 0.0 };
    quantize_with(rgba, attributes, dithering)?
        .ok_or_else(|| "imagequant: no palette for this picture".to_owned())
}

fn quantize_with(
    rgba: &RgbaImage,
    mut attributes: imagequant::Attributes,
    dithering: f32,
) -> Result<Option<Quantized>, String> {
    let liq_error = |e: imagequant::Error| format!("imagequant: {e}");
    let max_quality = attributes.quality().1;
    attributes.set_speed(4).map_err(liq_error)?;
    let pixels: Vec<imagequant::RGBA> = rgba
        .pixels()
        .map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3]))
        .collect();
    let mut image = attributes
        .new_image(pixels, rgba.width() as usize, rgba.height() as usize, 0.0)
        .map_err(liq_error)?;
    let mut result = match attributes.quantize(&mut image) {
        Ok(result) => result,
        Err(imagequant::Error::QualityTooLow) => return Ok(None),
        Err(e) => return Err(liq_error(e)),
    };
    result.set_dithering_level(dithering).map_err(liq_error)?;
    let achieved = result.quantization_quality().unwrap_or(max_quality);
    let (palette, indexes) = result.remapped(&mut image).map_err(liq_error)?;
    Ok(Some((palette, indexes, achieved)))
}

pub(crate) enum PngPixels<'a> {
    Indexed {
        palette: &'a [u8],
        alpha: &'a [u8],
        indexes: &'a [u8],
    },
    Rgb(Vec<u8>),
    Rgba(&'a [u8]),
    /// 16 bits per sample, high byte first, in this color type.
    Deep {
        color: png::ColorType,
        data: Vec<u8>,
    },
}

pub(crate) fn write_png(
    width: u32,
    height: u32,
    pixels: &PngPixels,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let png_error = |e: png::EncodingError| format!("png: {e}");
    let mut info = png::Info::with_size(width, height);
    info.bit_depth = png::BitDepth::Eight;
    info.icc_profile = icc.map(|p| p.to_vec().into());
    let data: &[u8] = match pixels {
        PngPixels::Indexed {
            palette,
            alpha,
            indexes,
        } => {
            info.color_type = png::ColorType::Indexed;
            info.palette = Some(palette.to_vec().into());
            if !alpha.is_empty() {
                info.trns = Some(alpha.to_vec().into());
            }
            indexes
        }
        PngPixels::Rgb(data) => {
            info.color_type = png::ColorType::Rgb;
            data
        }
        PngPixels::Rgba(data) => {
            info.color_type = png::ColorType::Rgba;
            data
        }
        PngPixels::Deep { color, data } => {
            info.color_type = *color;
            info.bit_depth = png::BitDepth::Sixteen;
            data
        }
    };
    let mut out = Vec::new();
    let mut writer = png::Encoder::with_info(&mut out, info)
        .map_err(png_error)?
        .write_header()
        .map_err(png_error)?;
    writer.write_image_data(data).map_err(png_error)?;
    writer.finish().map_err(png_error)?;
    Ok(out)
}

/// Converts pixels from `icc` to sRGB in place. Returns false when the profile already is sRGB.
fn to_srgb(rgba: &mut RgbaImage, icc: &[u8]) -> Result<bool, String> {
    let source = moxcms::ColorProfile::new_from_slice(icc)
        .map_err(|e| format!("cannot read the embedded color profile: {e}"))?;
    let srgb = moxcms::ColorProfile::new_srgb();
    let transform = source
        .create_transform_8bit(
            moxcms::Layout::Rgba,
            &srgb,
            moxcms::Layout::Rgba,
            moxcms::TransformOptions::default(),
        )
        .map_err(|e| format!("cannot convert colors to sRGB: {e}"))?;
    // Profiles that already describe sRGB (under many names and encodings) map colors onto
    // themselves; detect that on a sample grid instead of comparing profile internals.
    let samples: Vec<u8> = (0..=255u8)
        .step_by(17)
        .flat_map(|v| [v, 255 - v, v / 2, 255, 255, v, 0, 255, 0, 0, v, 255])
        .collect();
    let mut mapped = vec![0u8; samples.len()];
    transform
        .transform(&samples, &mut mapped)
        .map_err(|e| format!("cannot convert colors to sRGB: {e}"))?;
    if samples
        .iter()
        .zip(&mapped)
        .all(|(a, b)| a.abs_diff(*b) <= 1)
    {
        return Ok(false);
    }
    let mut converted = vec![0u8; rgba.as_raw().len()];
    transform
        .transform(rgba.as_raw(), &mut converted)
        .map_err(|e| format!("cannot convert colors to sRGB: {e}"))?;
    rgba.copy_from_slice(&converted);
    Ok(true)
}

fn encode_webp(
    rgba: &RgbaImage,
    quality: u8,
    icc: Option<&[u8]>,
    metadata: Option<&Metadata>,
) -> Result<Vec<u8>, String> {
    let mut config = webp::WebPConfig::new()
        .map_err(|()| "libwebp: cannot create a configuration".to_owned())?;
    config.quality = f32::from(quality);
    config.alpha_quality = 100;
    // Method 0–6 trades speed for size; 5 is close to the smallest at a reasonable speed.
    config.method = 5;
    // Sharper color edges (e.g. red text) at a small speed cost.
    config.use_sharp_yuv = 1;
    let has_alpha = rgba.pixels().any(|p| p[3] < 255);
    let encoded = if has_alpha {
        webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
            .encode_advanced(&config)
    } else {
        let rgb = DynamicImage::ImageRgba8(rgba.clone()).to_rgb8();
        webp::Encoder::from_rgb(rgb.as_raw(), rgba.width(), rgba.height()).encode_advanced(&config)
    }
    .map_err(|e| format!("libwebp: {e:?}"))?;
    let exif = metadata.and_then(|m| m.exif.as_deref());
    let xmp = metadata.and_then(|m| m.xmp.as_deref());
    if icc.is_none() && exif.is_none() && xmp.is_none() {
        return Ok(encoded.to_vec());
    }
    let extras = WebpExtras { icc, exif, xmp };
    webp_extended(&encoded, &extras, rgba.width(), rgba.height(), has_alpha)
}

fn encode_avif(rgba: &RgbaImage, quality: u8) -> Result<Vec<u8>, String> {
    let av1 = ravif::Encoder::new()
        .with_quality(f32::from(quality))
        .with_alpha_quality(f32::from(quality.saturating_add(10).min(100)))
        // rav1e speed 1–10; 6 keeps encoding interactive without much size loss.
        .with_speed(6);
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let pixels: Vec<ravif::RGBA8> = rgba
        .pixels()
        .map(|p| ravif::RGBA8::new(p[0], p[1], p[2], p[3]))
        .collect();
    // ravif drops the alpha plane by itself when every pixel is opaque.
    let encoded = av1
        .encode_rgba(ravif::Img::new(pixels.as_slice(), width, height))
        .map_err(|e| format!("rav1e: {e}"))?;
    Ok(encoded.avif_file)
}

/// What an extended WebP file carries besides the picture.
struct WebpExtras<'a> {
    icc: Option<&'a [u8]>,
    exif: Option<&'a [u8]>,
    xmp: Option<&'a [u8]>,
}

/// Rewrites a simple WebP file as an extended (VP8X) file carrying a color profile and
/// metadata.
///
/// Follows the WebP container specification: `RIFF` header, a `VP8X` chunk with a flag per
/// extra, the `ICCP` chunk, the image chunks (`ALPH`, `VP8 `/`VP8L`) in their original order,
/// then the `EXIF` and `XMP ` chunks.
fn webp_extended(
    webp: &[u8],
    extras: &WebpExtras,
    width: u32,
    height: u32,
    has_alpha: bool,
) -> Result<Vec<u8>, String> {
    const ICC_FLAG: u8 = 0x20;
    const ALPHA_FLAG: u8 = 0x10;
    const EXIF_FLAG: u8 = 0x08;
    const XMP_FLAG: u8 = 0x04;
    let invalid = || "libwebp produced an unexpected file layout".to_owned();
    if webp.len() < 12 || &webp[0..4] != b"RIFF" || &webp[8..12] != b"WEBP" {
        return Err(invalid());
    }
    let mut flags = if has_alpha { ALPHA_FLAG } else { 0 };
    let mut image_chunks = Vec::new();
    let mut offset = 12;
    while offset + 8 <= webp.len() {
        let fourcc = &webp[offset..offset + 4];
        let size = u32::from_le_bytes(
            webp[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| invalid())?,
        ) as usize;
        let end = offset + 8 + size + (size & 1);
        let chunk = webp.get(offset..end.min(webp.len())).ok_or_else(invalid)?;
        if fourcc == b"VP8X" {
            flags |= chunk.get(8).copied().unwrap_or(0) & ALPHA_FLAG;
        } else {
            image_chunks.extend_from_slice(chunk);
        }
        offset = end;
    }
    if image_chunks.is_empty() {
        return Err(invalid());
    }
    for (extra, flag) in [
        (extras.icc, ICC_FLAG),
        (extras.exif, EXIF_FLAG),
        (extras.xmp, XMP_FLAG),
    ] {
        if extra.is_some() {
            flags |= flag;
        }
    }
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);

    let mut body = b"WEBP".to_vec();
    push_chunk(&mut body, *b"VP8X", &vp8x);
    if let Some(icc) = extras.icc {
        push_chunk(&mut body, *b"ICCP", icc);
    }
    body.extend_from_slice(&image_chunks);
    if let Some(exif) = extras.exif {
        push_chunk(&mut body, *b"EXIF", exif);
    }
    if let Some(xmp) = extras.xmp {
        push_chunk(&mut body, *b"XMP ", xmp);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(
        &u32::try_from(body.len())
            .map_err(|_| "image too large for WebP".to_owned())?
            .to_le_bytes(),
    );
    out.extend_from_slice(&body);
    Ok(out)
}

fn push_chunk(out: &mut Vec<u8>, fourcc: [u8; 4], payload: &[u8]) {
    out.extend_from_slice(&fourcc);
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small synthetic images
mod tests {
    use super::*;

    #[test]
    fn cut_off_jpegs_are_refused() {
        let picture = DynamicImage::ImageRgb8(RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 4) as u8, 90])
        }));
        let jpeg = encode_jpeg_plain(&picture, 80).unwrap();
        assert!(jpeg_is_complete(&jpeg));
        assert!(decode_native_bytes(&jpeg, "whole.jpg").is_ok());
        // Bytes after the end marker (some cameras add them) are fine.
        assert!(jpeg_is_complete(&[jpeg.as_slice(), b"trailer"].concat()));
        let cut = &jpeg[..jpeg.len() * 2 / 3];
        assert!(!jpeg_is_complete(cut));
        let error = decode_native_bytes(cut, "cut.jpg").unwrap_err();
        assert!(error.contains("cut.jpg is incomplete"), "{error}");
        // Only JPEGs are checked here.
        assert!(jpeg_is_complete(b"\x89PNG"));

        // A camera JPEG carries a thumbnail (a whole JPEG, with its own scan and end marker)
        // inside its EXIF segment: that must not make a cut-off photo look complete.
        let thumbnail = encode_jpeg_plain(&picture.thumbnail(8, 8), 60).unwrap();
        let app1_len = u16::try_from(thumbnail.len() + 8).unwrap().to_be_bytes();
        let with_thumbnail = [
            &jpeg[..2],
            &[0xFF, 0xE1][..],
            &app1_len,
            b"Exif\0\0",
            &thumbnail,
            &jpeg[2..],
        ]
        .concat();
        assert!(jpeg_is_complete(&with_thumbnail));
        assert!(decode_native_bytes(&with_thumbnail, "camera.jpg").is_ok());
        let cut = &with_thumbnail[..with_thumbnail.len() - 200];
        assert!(!jpeg_is_complete(cut));
    }

    /// A JPEG of an RGB picture, as the tests need it: progressive, with the usual color
    /// sampling.
    fn encode_jpeg(rgb: &RgbImage, quality: u8, icc: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let settings = JpegSettings {
            quality,
            progressive: true,
            full_color: false,
        };
        let space = mozjpeg::ColorSpace::JCS_RGB;
        jpeg_from_pixels(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            space,
            settings,
            icc,
        )
    }

    #[test]
    fn flattening_blends_onto_the_background() {
        let mut rgba = RgbaImage::new(3, 1);
        rgba.put_pixel(0, 0, image::Rgba([0, 0, 0, 255])); // opaque black stays black
        rgba.put_pixel(1, 0, image::Rgba([0, 0, 0, 0])); // fully transparent becomes white
        rgba.put_pixel(2, 0, image::Rgba([0, 0, 0, 128])); // half transparent becomes mid gray
        let rgb = flatten_on(&rgba, peraz_core::Color::WHITE);
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
        assert_eq!(rgb.get_pixel(2, 0).0, [127, 127, 127]);
        // On another color: that color shows through, channel by channel.
        let rgb = flatten_on(&rgba, peraz_core::Color([30, 60, 200]));
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [30, 60, 200]);
        assert_eq!(rgb.get_pixel(2, 0).0, [15, 30, 100]);
    }

    #[test]
    fn sharp_colored_edges_are_told_from_photos() {
        // A photo: colors change slowly, with grain in the brightness.
        let photo = RgbImage::from_fn(256, 192, |x, y| {
            let grain = (x.wrapping_mul(2_654_435_761).wrapping_add(y * 40_503) >> 28) & 15;
            let channel = |v: u32| u8::try_from(v.min(255)).unwrap();
            image::Rgb([
                channel(x * 200 / 256 + grain),
                channel(y * 200 / 192 + grain),
                channel(90 + grain),
            ])
        });
        assert!(!has_sharp_color_edges(&photo));
        // Thin red and blue strokes on a dark background, as colored text has them.
        let text = RgbImage::from_fn(256, 192, |x, y| {
            if y % 16 < 9 && x % 7 == 3 {
                image::Rgb(if (x / 40) % 2 == 0 {
                    [240, 70, 80]
                } else {
                    [90, 160, 250]
                })
            } else {
                image::Rgb([30, 30, 46])
            }
        });
        assert!(has_sharp_color_edges(&text));
        // Black text on white has sharp edges, but no color in them.
        let plain = RgbImage::from_fn(256, 192, |x, y| {
            image::Rgb(if y % 16 < 9 && x % 7 == 3 {
                [0; 3]
            } else {
                [255; 3]
            })
        });
        assert!(!has_sharp_color_edges(&plain));
        assert!(!has_sharp_color_edges(&RgbImage::new(1, 1)));
    }

    #[test]
    fn png_round_trip_keeps_pixels_size_and_icc() {
        let mut rgba = RgbaImage::new(64, 32);
        for (x, y, p) in rgba.enumerate_pixels_mut() {
            #[allow(clippy::cast_possible_truncation)]
            let v = (x * 4) as u8;
            *p = image::Rgba([v, 255 - v, (y * 8) as u8, if x < 8 { 0 } else { 255 }]);
        }
        let icc = b"fake icc profile bytes".to_vec();
        let picture = DynamicImage::ImageRgba8(rgba.clone());
        let (png, encoder, _) = encode_png(&picture, 0, 100, Some(&icc), None).unwrap();
        assert!(encoder.starts_with("imagequant"), "{encoder}");
        let mut decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&png)).unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), Some(icc));
        let pixels = DynamicImage::from_decoder(decoder).unwrap().to_rgba8();
        assert_eq!(pixels.dimensions(), (64, 32));
        assert_eq!(pixels.get_pixel(0, 0)[3], 0, "transparency survives");
    }

    fn gradient(width: u32, height: u32, transparent_left: bool) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            let alpha = if transparent_left && x < width / 4 {
                0
            } else {
                255
            };
            image::Rgba([(x * 255 / width) as u8, (y * 255 / height) as u8, 90, alpha])
        })
    }

    #[test]
    fn webp_keeps_alpha_and_icc() {
        let icc = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        for transparent in [false, true] {
            let webp = encode_webp(&gradient(64, 48, transparent), 80, Some(&icc), None).unwrap();
            let mut decoder =
                image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&webp)).unwrap();
            assert_eq!(
                decoder.icc_profile().unwrap().as_deref(),
                Some(icc.as_slice())
            );
            let pixels = DynamicImage::from_decoder(decoder).unwrap().to_rgba8();
            assert_eq!(pixels.dimensions(), (64, 48));
            assert_eq!(
                pixels.get_pixel(0, 0)[3] < 128,
                transparent,
                "alpha survives"
            );
        }
    }

    #[test]
    fn avif_is_written() {
        let avif = encode_avif(&gradient(32, 32, true), 70).unwrap();
        assert_eq!(&avif[4..12], b"ftypavif");
    }

    #[test]
    fn display_p3_is_converted_to_srgb_and_srgb_is_left_alone() {
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let mut red = RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        assert!(to_srgb(&mut red, &p3).unwrap());
        // Display P3 red is outside sRGB: it clips to the most saturated sRGB red.
        assert!(
            red.get_pixel(0, 0)[0] > 250 && red.get_pixel(0, 0)[1] < 10,
            "{:?}",
            red.get_pixel(0, 0)
        );
        let mut gray = RgbaImage::from_pixel(1, 1, image::Rgba([128, 128, 128, 255]));
        let srgb = moxcms::ColorProfile::new_srgb().encode().unwrap();
        assert!(!to_srgb(&mut gray, &srgb).unwrap());
        assert_eq!(gray.get_pixel(0, 0).0, [128, 128, 128, 255]);
    }

    #[test]
    fn jpeg_encodes_and_decodes() {
        let rgb = RgbImage::from_fn(40, 30, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            image::Rgb([(x * 6) as u8, (y * 8) as u8, 128])
        });
        let jpeg = encode_jpeg(&rgb, 80, None).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 30));
    }

    #[test]
    fn a_jpeg_carries_its_profile_the_way_readers_expect() {
        let rgb = RgbImage::from_fn(40, 30, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            image::Rgb([(x * 6) as u8, (y * 8) as u8, 128])
        });
        // A small profile in one segment, and one that needs three.
        let small = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let large: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        for icc in [small, large] {
            let jpeg = encode_jpeg(&rgb, 80, Some(&icc)).unwrap();
            // The parts are numbered from 1, each with the total count.
            let headers: Vec<(u8, u8)> = jpeg
                .windows(14)
                .filter(|w| w.starts_with(b"ICC_PROFILE\0"))
                .map(|w| (w[12], w[13]))
                .collect();
            let count = u8::try_from(icc.len().div_ceil(65_519)).unwrap();
            let expected: Vec<(u8, u8)> = (1..=count).map(|part| (part, count)).collect();
            assert_eq!(headers, expected);
            // A reader that checks the numbering finds the profile.
            let mut decoder =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
            assert_eq!(decoder.icc_profile().unwrap(), Some(icc));
        }
    }
}
