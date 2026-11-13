//! Still-image encoding: decode, make upright, resize, then mozjpeg, imagequant + oxipng,
//! libwebp, or rav1e (AVIF).
//!
//! Metadata policy: the EXIF orientation is applied to the pixels, the ICC color profile is kept
//! (in every output format, so wide-gamut pictures stay wide-gamut), and everything else (EXIF,
//! XMP, text chunks) is dropped because it can hold location and device details.

use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, RgbaImage, imageops::FilterType};
use peraz_core::plan::fit_within;
use peraz_core::{ImageEncoding, ImagePlan};

/// An encoded image, ready to be written.
#[derive(Debug)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// What did the work, for the report.
    pub encoder: String,
}

/// A decoded picture, already upright, with its color profile.
#[derive(Debug)]
pub struct Decoded {
    pub image: DynamicImage,
    pub icc: Option<Vec<u8>>,
}

/// Encodes a decoded picture according to `plan`.
pub fn encode_image(decoded: Decoded, plan: &ImagePlan) -> Result<EncodedImage, String> {
    encode_prepared(&prepare(decoded, plan), plan)
}

/// Fits a decoded picture in the plan's box: the pixels the encoder gets.
pub fn prepare(decoded: Decoded, plan: &ImagePlan) -> Decoded {
    let Decoded { mut image, icc } = decoded;
    if let Some((w, h)) = fit_within(
        (image.width(), image.height()),
        plan.max_width,
        plan.max_height,
    ) {
        image = image.resize_exact(w, h, FilterType::Lanczos3);
    }
    Decoded { image, icc }
}

/// Encodes a picture that [`prepare`] made ready, according to `plan`.
pub fn encode_prepared(prepared: &Decoded, plan: &ImagePlan) -> Result<EncodedImage, String> {
    // mozjpeg and imagequant report some failures by panicking; keep them contained to this file.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        encode_image_inner(prepared, plan)
    }))
    .unwrap_or_else(|_| Err("the image encoder crashed on this file".to_owned()))
}

fn encode_image_inner(prepared: &Decoded, plan: &ImagePlan) -> Result<EncodedImage, String> {
    let Decoded { image, icc } = prepared;
    let (width, height) = (image.width(), image.height());
    let (bytes, encoder) = match plan.encoding {
        ImageEncoding::Jpeg { quality } => {
            let rgb = if plan.flatten_alpha {
                flatten_on_white(&image.to_rgba8())
            } else {
                image.to_rgb8()
            };
            (
                encode_jpeg(&rgb, quality, icc.as_deref())?,
                format!("mozjpeg q{quality}"),
            )
        }
        ImageEncoding::Png {
            min_quality,
            max_quality,
        } => encode_png(&image.to_rgba8(), min_quality, max_quality, icc.as_deref())?,
        ImageEncoding::Webp { quality } => (
            encode_webp(&image.to_rgba8(), quality, icc.as_deref())?,
            format!("libwebp q{quality}"),
        ),
        ImageEncoding::Avif { quality } => {
            let mut rgba = image.to_rgba8();
            let avif = encode_avif(&rgba, quality)?;
            let tagged = match icc.as_deref() {
                Some(profile) => crate::heif::embed_icc(&avif, profile).ok_or(profile),
                None => Ok(avif),
            };
            match tagged {
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
            }
        }
    };
    Ok(EncodedImage {
        bytes,
        width,
        height,
        encoder,
    })
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
    decode_reader(reader, name)
}

fn decode_reader<R: std::io::BufRead + std::io::Seek>(
    reader: ImageReader<R>,
    name: &str,
) -> Result<Decoded, String> {
    let error = |e: image::ImageError| format!("cannot decode {name}: {e}");
    let mut decoder = reader.into_decoder().map_err(error)?;
    let orientation = decoder.orientation().map_err(error)?;
    let icc = decoder.icc_profile().map_err(error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(error)?;
    image.apply_orientation(orientation);
    Ok(Decoded { image, icc })
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

/// Composites transparent pixels onto white, for formats without alpha.
fn flatten_on_white(rgba: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
        let blend = |channel: u8| {
            let (c, a) = (u32::from(channel), u32::from(alpha));
            // Rounded (c·a + 255·(255 − a)) / 255; always fits in u8.
            u8::try_from((c * a + 255 * (255 - a) + 127) / 255).unwrap_or(u8::MAX)
        };
        image::Rgb([blend(red), blend(green), blend(blue)])
    })
}

fn encode_jpeg(rgb: &RgbImage, quality: u8, icc: Option<&[u8]>) -> Result<Vec<u8>, String> {
    jpeg_from_pixels(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        mozjpeg::ColorSpace::JCS_RGB,
        quality,
        icc,
    )
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
    std::panic::catch_unwind(move || jpeg_from_pixels(&pixels, width, height, space, quality, None))
        .unwrap_or_else(|_| Err("mozjpeg crashed on this picture".to_owned()))
}

fn jpeg_from_pixels(
    pixels: &[u8],
    width: u32,
    height: u32,
    space: mozjpeg::ColorSpace,
    quality: u8,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut compress = mozjpeg::Compress::new(space);
    compress.set_size(width as usize, height as usize);
    compress.set_quality(f32::from(quality));
    // mozjpeg's defaults already use progressive scans and optimized Huffman tables.
    let mut started = compress
        .start_compress(Vec::new())
        .map_err(|e| format!("mozjpeg: {e}"))?;
    if let Some(icc) = icc {
        started.write_icc_profile(icc);
    }
    started
        .write_scanlines(pixels)
        .map_err(|e| format!("mozjpeg: {e}"))?;
    started.finish().map_err(|e| format!("mozjpeg: {e}"))
}

/// Quantizes to a palette when the quality range can be met, otherwise keeps true color; then
/// optimizes losslessly. Returns the PNG and a description of the encoder.
fn encode_png(
    rgba: &RgbaImage,
    min_quality: u8,
    max_quality: u8,
    icc: Option<&[u8]>,
) -> Result<(Vec<u8>, String), String> {
    let (width, height) = rgba.dimensions();
    let quantized = quantize(rgba, min_quality, max_quality)?;
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
        (
            write_png(width, height, &pixels, icc)?,
            format!("imagequant q{achieved} + oxipng"),
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
    Ok((optimized, encoder))
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
    result.set_dithering_level(1.0).map_err(liq_error)?;
    let achieved = result.quantization_quality().unwrap_or(max_quality);
    let (palette, indexes) = result.remapped(&mut image).map_err(liq_error)?;
    Ok(Some((palette, indexes, achieved)))
}

enum PngPixels<'a> {
    Indexed {
        palette: &'a [u8],
        alpha: &'a [u8],
        indexes: &'a [u8],
    },
    Rgb(Vec<u8>),
    Rgba(&'a [u8]),
}

fn write_png(
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

fn encode_webp(rgba: &RgbaImage, quality: u8, icc: Option<&[u8]>) -> Result<Vec<u8>, String> {
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
    match icc {
        Some(icc) => webp_with_icc(&encoded, icc, rgba.width(), rgba.height(), has_alpha),
        None => Ok(encoded.to_vec()),
    }
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

/// Rewrites a simple WebP file as an extended (VP8X) file carrying an ICC profile.
///
/// Follows the WebP container specification: `RIFF` header, a `VP8X` chunk with the ICC flag,
/// an `ICCP` chunk, then the image chunks (`ALPH`, `VP8 `/`VP8L`) in their original order.
fn webp_with_icc(
    webp: &[u8],
    icc: &[u8],
    width: u32,
    height: u32,
    has_alpha: bool,
) -> Result<Vec<u8>, String> {
    const ICC_FLAG: u8 = 0x20;
    const ALPHA_FLAG: u8 = 0x10;
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
            flags |= chunk.get(8).copied().unwrap_or(0) & !ICC_FLAG;
        } else {
            image_chunks.extend_from_slice(chunk);
        }
        offset = end;
    }
    if image_chunks.is_empty() {
        return Err(invalid());
    }
    let mut vp8x = vec![flags | ICC_FLAG, 0, 0, 0];
    vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);

    let mut body = b"WEBP".to_vec();
    push_chunk(&mut body, *b"VP8X", &vp8x);
    push_chunk(&mut body, *b"ICCP", icc);
    body.extend_from_slice(&image_chunks);
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

    #[test]
    fn flattening_blends_onto_white() {
        let mut rgba = RgbaImage::new(3, 1);
        rgba.put_pixel(0, 0, image::Rgba([0, 0, 0, 255])); // opaque black stays black
        rgba.put_pixel(1, 0, image::Rgba([0, 0, 0, 0])); // fully transparent becomes white
        rgba.put_pixel(2, 0, image::Rgba([0, 0, 0, 128])); // half transparent becomes mid gray
        let rgb = flatten_on_white(&rgba);
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
        assert_eq!(rgb.get_pixel(2, 0).0, [127, 127, 127]);
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
        let (png, encoder) = encode_png(&rgba, 0, 100, Some(&icc)).unwrap();
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
            let webp = encode_webp(&gradient(64, 48, transparent), 80, Some(&icc)).unwrap();
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
}
