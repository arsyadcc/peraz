//! PDF: recompress the pictures, leave everything else as it is.
//!
//! Photos (`DCTDecode`) are re-encoded with mozjpeg. Uncompressed or Flate pictures become
//! indexed (palette) images when a palette holds their quality, and JPEG otherwise. A picture is
//! replaced only when the new version is smaller. Afterwards unused objects are removed, every
//! Flate-compressed stream (page contents, fonts, color profiles) is deflated again at the best
//! level, and the file is written with compressed object streams.
//!
//! Some documents are not rewritten at all (signed ones, and ones that need a password), and
//! some keep what they need: a PDF/A its plain structure, a PDF with permission restrictions
//! its encryption and restrictions.

use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Mutex;

use image::{DynamicImage, GrayImage, RgbImage, imageops::FilterType};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use peraz_core::plan::fit_within;
use peraz_core::{DocumentPlan, DocumentStats};

use crate::image::{encode_jpeg_plain, quantize};

/// Why a PDF was not rewritten.
#[derive(Debug)]
pub enum DocumentError {
    /// Encrypted, DRM-protected, or signed: Peraz never touches those.
    Protected(String),
    Failed(String),
}

/// The most a picture may take once decoded: 512 MB, which is far more than a page scanned for
/// print needs. A larger one is left as it is, so that a small file cannot ask for gigabytes.
const MAX_PICTURE_BYTES: u64 = 512_000_000;

/// The most a stream may hold, once inflated, to be deflated again.
const MAX_STREAM_BYTES: u64 = 256_000_000;

/// Recompresses the pictures of the PDF at `input`. Returns the new file, what happened to the
/// pictures, and what the report should warn about.
pub fn compress_pdf(
    input: &Path,
    plan: &DocumentPlan,
) -> Result<(Vec<u8>, DocumentStats, Vec<String>), DocumentError> {
    let mut doc = Document::load(input)
        .map_err(|e| DocumentError::Failed(format!("cannot read the PDF: {e}")))?;
    // A PDF that opens without a password is decrypted on loading; one that is still encrypted
    // here needs a password.
    if doc.is_encrypted() {
        return Err(DocumentError::Protected(
            "the PDF needs a password to open".to_owned(),
        ));
    }
    if is_signed(&doc) {
        return Err(DocumentError::Protected(
            "the PDF is digitally signed, and changing it would break the signature".to_owned(),
        ));
    }
    // Restrictions (on printing, copying, changing) come with encryption under an empty
    // password: the result gets the same encryption, with the same restrictions.
    let protection = doc.encryption_state.clone();
    let archival = is_pdf_a(&doc);
    let pages = doc.get_pages().len();
    let mut warnings = Vec::new();
    if doc.linearization.is_some() {
        warnings.push(
            "the PDF was made for fast web view (linearized); the result is not, so a browser \
             shows it only once the whole file has loaded"
                .to_owned(),
        );
    }
    let masks = mask_targets(&doc);
    let images: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, object)| {
            let stream = object.as_stream().ok()?;
            (stream.dict.get(b"Subtype").and_then(Object::as_name).ok()? == b"Image").then_some(*id)
        })
        .collect();

    let mut stats = DocumentStats::default();
    for id in images {
        stats.images += 1;
        if masks.contains(&id) {
            stats.skipped += 1; // soft masks and masks keep their exact edges
            continue;
        }
        let Some(Object::Stream(stream)) = doc.objects.get(&id) else {
            continue;
        };
        let color = resolve_color(&doc, &stream.dict);
        let keep_size = has_matte(&doc, &stream.dict);
        match recompress(stream, color, keep_size, plan) {
            Picture::Replace(new) => {
                stats.recompressed += 1;
                doc.objects.insert(id, Object::Stream(new));
            }
            Picture::Kept => stats.kept += 1,
            Picture::Skipped => stats.skipped += 1,
            Picture::Refused(why) => {
                stats.skipped += 1;
                warnings.push(why);
            }
        }
    }

    doc.prune_objects();
    if archival {
        // PDF/A wants its metadata readable without decompressing anything.
        for object in doc.objects.values_mut() {
            if let Object::Stream(stream) = object
                && stream.dict.has_type(b"Metadata")
            {
                stream.allows_compression = false;
            }
        }
    }
    doc.compress();
    deflate_again(&mut doc);
    if let Some(state) = &protection {
        doc.encrypt(state)
            .map_err(|e| DocumentError::Failed(format!("cannot encrypt the PDF again: {e}")))?;
    }
    let mut out = Vec::new();
    // PDF/A-1 is PDF 1.4: it has neither object streams nor cross-reference streams, so a
    // PDF/A is written in the plain structure every part of the standard allows.
    let saved = if archival {
        doc.save_to(&mut out)
    } else {
        doc.save_modern(&mut out)
    };
    saved.map_err(|e| DocumentError::Failed(format!("cannot write the PDF: {e}")))?;
    read_back(&out, pages)?;
    Ok((out, stats, warnings))
}

/// Deflates every Flate-compressed stream again at the best level, and keeps the result where
/// it is smaller. Many PDF writers compress fast, not well. Only the compression changes: what
/// a stream inflates to stays byte for byte, so a predictor (`DecodeParms`) still applies as
/// before. Streams with another filter, or more than one, are left alone, and so is data that
/// does not inflate cleanly.
fn deflate_again(doc: &mut Document) {
    let streams: Vec<&mut Stream> = doc
        .objects
        .values_mut()
        .filter_map(|object| match object {
            Object::Stream(stream) if is_only_flate(&stream.dict) => Some(stream),
            _ => None,
        })
        .collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(streams.len());
    // The best level is slow: the streams are shared out between the processor's cores.
    let queue = Mutex::new(streams);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut compressor =
                    libdeflater::Compressor::new(libdeflater::CompressionLvl::best());
                while let Some(stream) = queue.lock().ok().and_then(|mut queue| queue.pop()) {
                    let Some(data) = inflate(&stream.content) else {
                        continue;
                    };
                    let mut packed = vec![0; compressor.zlib_compress_bound(data.len())];
                    if let Ok(length) = compressor.zlib_compress(&data, &mut packed)
                        && length < stream.content.len()
                    {
                        packed.truncate(length);
                        stream.set_content(packed);
                    }
                }
            });
        }
    });
}

/// The stream's only filter is `FlateDecode`.
fn is_only_flate(dict: &Dictionary) -> bool {
    let flate = |name: &Object| name.as_name().is_ok_and(|name| name == b"FlateDecode");
    match dict.get(b"Filter") {
        Ok(Object::Array(filters)) => filters.len() == 1 && flate(&filters[0]),
        Ok(filter) => flate(filter),
        Err(_) => false,
    }
}

/// What a zlib stream holds, or `None` when it is damaged or holds more than
/// [`MAX_STREAM_BYTES`].
fn inflate(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .take(MAX_STREAM_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() as u64 <= MAX_STREAM_BYTES).then_some(data)
}

/// Checks the new file before it takes anyone's place: it must load, with as many pages as
/// the original.
fn read_back(bytes: &[u8], pages: usize) -> Result<(), DocumentError> {
    let failed = |what: String| {
        DocumentError::Failed(format!(
            "the new PDF {what}; nothing was written, and the original is as it was"
        ))
    };
    let doc = Document::load_mem(bytes).map_err(|e| failed(format!("does not read back: {e}")))?;
    let found = doc.get_pages().len();
    if found == pages {
        Ok(())
    } else {
        Err(failed(format!(
            "has {found} pages where the original has {pages}"
        )))
    }
}

/// The document carries a digital signature: its form says so (`SigFlags`), or a signature
/// with the byte range it covers is somewhere in it. Any rewrite moves those bytes.
fn is_signed(doc: &Document) -> bool {
    let flags = doc
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"AcroForm").ok())
        .and_then(|form| match form {
            Object::Reference(id) => doc.get_dictionary(*id).ok(),
            other => other.as_dict().ok(),
        })
        .and_then(|form| form.get(b"SigFlags").and_then(Object::as_i64).ok())
        .unwrap_or(0);
    // Bit 1: signatures exist. Bit 2: the file may only be appended to, because of them.
    flags & 3 != 0 || doc.objects.values().any(holds_signature)
}

/// A signature dictionary (one with a `ByteRange`) is in this object, at any depth.
fn holds_signature(object: &Object) -> bool {
    let in_dict = |dict: &Dictionary| {
        dict.has(b"ByteRange") || dict.iter().any(|(_, value)| holds_signature(value))
    };
    match object {
        Object::Dictionary(dict) => in_dict(dict),
        Object::Stream(stream) => in_dict(&stream.dict),
        Object::Array(items) => items.iter().any(holds_signature),
        _ => false,
    }
}

/// The document says it is a PDF/A (an archival PDF): its metadata has a `pdfaid:part`.
fn is_pdf_a(doc: &Document) -> bool {
    let Some(metadata) = doc
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"Metadata").ok())
        .and_then(|metadata| match metadata {
            Object::Reference(id) => doc.get_object(*id).ok(),
            other => Some(other),
        })
        .and_then(|metadata| metadata.as_stream().ok())
    else {
        return false;
    };
    let text = if metadata.dict.has(b"Filter") {
        // Metadata is small; a limit keeps a crafted one from growing without end.
        match metadata.decompressed_content_with_limit(16_000_000) {
            Ok(text) => text,
            Err(_) => return false,
        }
    } else {
        metadata.content.clone()
    };
    let mark = b"pdfaid:part";
    text.windows(mark.len()).any(|window| window == mark)
}

/// The picture was blended with a matte color, which its soft mask names (`Matte`): then the
/// picture and the mask belong together pixel by pixel, and must keep the same size.
fn has_matte(doc: &Document, dict: &Dictionary) -> bool {
    let mask = match dict.get(b"SMask") {
        Ok(Object::Reference(id)) => doc.get_object(*id).ok(),
        Ok(other) => Some(other),
        Err(_) => None,
    };
    mask.and_then(|mask| mask.as_stream().ok())
        .is_some_and(|mask| mask.dict.has(b"Matte"))
}

/// Pictures used as another picture's `SMask` or `Mask`.
fn mask_targets(doc: &Document) -> HashSet<ObjectId> {
    doc.objects
        .values()
        .filter_map(|object| object.as_stream().ok())
        .flat_map(|stream| {
            [b"SMask".as_slice(), b"Mask".as_slice()]
                .into_iter()
                .filter_map(|key| stream.dict.get(key).and_then(Object::as_reference).ok())
        })
        .collect()
}

/// How many color components a picture has, if Peraz can rewrite it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Components {
    Gray,
    Rgb,
}

/// The picture's color space, resolved through references and ICC streams. `None` for color
/// spaces Peraz leaves alone (CMYK, Lab, Indexed, Separation, …).
fn resolve_color(doc: &Document, dict: &Dictionary) -> Option<Components> {
    let space = dict.get(b"ColorSpace").ok()?;
    let space = match space {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    match space {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"CalGray" => Some(Components::Gray),
            b"DeviceRGB" | b"CalRGB" => Some(Components::Rgb),
            _ => None,
        },
        Object::Array(parts) => {
            let family = parts.first()?.as_name().ok()?;
            match family {
                b"ICCBased" => {
                    let profile = match parts.get(1)? {
                        Object::Reference(id) => doc.get_object(*id).ok()?.as_stream().ok()?,
                        Object::Stream(stream) => stream,
                        _ => return None,
                    };
                    match profile.dict.get(b"N").and_then(Object::as_i64).ok()? {
                        1 => Some(Components::Gray),
                        3 => Some(Components::Rgb),
                        _ => None,
                    }
                }
                b"CalGray" => Some(Components::Gray),
                b"CalRGB" => Some(Components::Rgb),
                _ => None,
            }
        }
        _ => None,
    }
}

enum Picture {
    Replace(Stream),
    /// Could be rewritten, but the new version was not smaller.
    Kept,
    /// Left alone.
    Skipped,
    /// Left alone for a reason worth telling: a warning for the report.
    Refused(String),
}

/// `keep_size`: the picture must stay the size it is.
fn recompress(
    stream: &Stream,
    color: Option<Components>,
    keep_size: bool,
    plan: &DocumentPlan,
) -> Picture {
    let dict = &stream.dict;
    let Some(components) = color else {
        return Picture::Skipped;
    };
    let flag = |key: &[u8]| dict.get(key).and_then(Object::as_bool).unwrap_or(false);
    let bits = dict
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .unwrap_or(0);
    if flag(b"ImageMask") || bits != 8 || dict.has(b"Decode") {
        return Picture::Skipped;
    }
    // A color key (`Mask` as an array of values) makes exactly those values see-through: a
    // palette or a JPEG moves them, so the picture is kept exact.
    if dict.get(b"Mask").is_ok_and(|mask| mask.as_array().is_ok()) {
        return Picture::Skipped;
    }
    let filters: Vec<Vec<u8>> = stream
        .filters()
        .map(|f| f.into_iter().map(<[u8]>::to_vec).collect())
        .unwrap_or_default();
    let decoded = match filters
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [b"DCTDecode"] => decode_jpeg(&stream.content, components),
        [] | [b"FlateDecode"] => match decode_raw(stream, components) {
            Ok(picture) => picture,
            Err(why) => return Picture::Refused(why),
        },
        _ => None, // JPEG 2000, JBIG2, CCITT, chained filters
    };
    let Some(mut picture) = decoded else {
        return Picture::Skipped;
    };
    if !keep_size
        && let Some((w, h)) = fit_within(
            (picture.width(), picture.height()),
            plan.max_image_side,
            plan.max_image_side,
        )
    {
        picture = picture.resize_exact(w, h, FilterType::Lanczos3);
    }
    let photo = filters.first().is_some_and(|f| f == b"DCTDecode");
    let candidate = if photo {
        jpeg_stream(dict, &picture, plan)
    } else {
        graphic_stream(dict, &picture, components, plan)
    };
    match candidate {
        Some(new) if new.content.len() < stream.content.len() => Picture::Replace(new),
        Some(_) => Picture::Kept,
        None => Picture::Skipped,
    }
}

fn decode_jpeg(bytes: &[u8], components: Components) -> Option<DynamicImage> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let channels = image.color().channel_count();
    // A CMYK JPEG would decode with 4 channels; the color space check above rules those out.
    match (components, channels) {
        (Components::Gray, 1) | (Components::Rgb, 3) => Some(image),
        _ => None,
    }
}

/// Decodes an uncompressed or Flate picture. `Ok(None)` when its data does not make a picture;
/// `Err` with a warning when it asks for more memory than a picture should.
fn decode_raw(stream: &Stream, components: Components) -> Result<Option<DynamicImage>, String> {
    let side = |key: &[u8]| {
        stream
            .dict
            .get(key)
            .and_then(Object::as_i64)
            .ok()
            .and_then(|value| u32::try_from(value).ok())
    };
    let (Some(width), Some(height)) = (side(b"Width"), side(b"Height")) else {
        return Ok(None);
    };
    let channels = match components {
        Components::Gray => 1,
        Components::Rgb => 3,
    };
    let needed = u64::from(width) * u64::from(height) * channels;
    if needed > MAX_PICTURE_BYTES {
        return Err(format!(
            "a {width}x{height} picture was left as it is: decoding it takes more than {} MB",
            MAX_PICTURE_BYTES / 1_000_000
        ));
    }
    let pixels = if stream.filters().map_or(true, |f| f.is_empty()) {
        stream.content.clone()
    } else {
        // No more than the picture holds, with room for the tag byte a predictor puts before
        // each row and for padding at the end.
        let limit = usize::try_from(needed + u64::from(height) + 65_536).unwrap_or(usize::MAX);
        match stream.decompressed_content_with_limit(limit) {
            Ok(pixels) => pixels,
            Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                ..
            })) => {
                return Err(format!(
                    "a {width}x{height} picture was left as it is: its data expands to more \
                     data than a picture of that size holds"
                ));
            }
            Err(_) => return Ok(None),
        }
    };
    Ok(match components {
        Components::Gray => {
            GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        Components::Rgb => RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8),
    })
}

/// The picture's dictionary with new dimensions, ready for new content.
fn base_dict(dict: &Dictionary, picture: &DynamicImage) -> Dictionary {
    let mut dict = dict.clone();
    dict.set("Width", i64::from(picture.width()));
    dict.set("Height", i64::from(picture.height()));
    dict.set("BitsPerComponent", 8);
    dict.remove(b"DecodeParms");
    dict.remove(b"Length");
    dict
}

fn jpeg_stream(dict: &Dictionary, picture: &DynamicImage, plan: &DocumentPlan) -> Option<Stream> {
    let bytes = encode_jpeg_plain(picture, plan.jpeg_quality).ok()?;
    let mut dict = base_dict(dict, picture);
    dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    Some(Stream::new(dict, bytes).with_compression(false))
}

/// Screenshots and graphics: an indexed palette image when it holds the quality range (sharp
/// edges stay sharp), JPEG otherwise.
fn graphic_stream(
    dict: &Dictionary,
    picture: &DynamicImage,
    components: Components,
    plan: &DocumentPlan,
) -> Option<Stream> {
    let rgba = picture.to_rgba8();
    match quantize(&rgba, plan.png_min_quality, plan.png_max_quality) {
        Ok(Some((palette, indexes, _))) => {
            let (base, lookup): (Object, Vec<u8>) = match components {
                Components::Rgb => (
                    dict.get(b"ColorSpace")
                        .cloned()
                        .unwrap_or(Object::Name(b"DeviceRGB".to_vec())),
                    palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect(),
                ),
                Components::Gray => (
                    dict.get(b"ColorSpace")
                        .cloned()
                        .unwrap_or(Object::Name(b"DeviceGray".to_vec())),
                    palette.iter().map(|c| c.r).collect(),
                ),
            };
            let high = i64::try_from(palette.len().saturating_sub(1)).ok()?;
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            encoder.write_all(&indexes).ok()?;
            let content = encoder.finish().ok()?;
            let mut dict = base_dict(dict, picture);
            dict.set(
                "ColorSpace",
                Object::Array(vec![
                    Object::Name(b"Indexed".to_vec()),
                    base,
                    Object::Integer(high),
                    Object::String(lookup, lopdf::StringFormat::Hexadecimal),
                ]),
            );
            dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            Some(Stream::new(dict, content).with_compression(false))
        }
        _ => jpeg_stream(dict, picture, plan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{content::Content, content::Operation, dictionary};

    /// A one-page PDF with an uncompressed RGB gradient picture, a flat graphic, and a JPEG.
    pub(crate) fn sample_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let (w, h) = (400u32, 300u32);
        let photo = RgbImage::from_fn(w, h, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            image::Rgb([
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        let raw = |pixels: Vec<u8>, space: &str| {
            Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image", "Width" => i64::from(w), "Height" => i64::from(h),
                    "ColorSpace" => space, "BitsPerComponent" => 8,
                },
                pixels,
            )
        };
        let gradient = doc.add_object(raw(photo.clone().into_raw(), "DeviceRGB"));
        let flat = RgbImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgb([200, 30, 30])
            } else {
                image::Rgb([20, 20, 160])
            }
        });
        let graphic = doc.add_object(raw(flat.into_raw(), "DeviceRGB"));
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(photo)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let mut jpeg_stream = raw(jpeg, "DeviceRGB");
        jpeg_stream.dict.set("Filter", "DCTDecode");
        let photo_jpeg = doc.add_object(jpeg_stream);
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        200.into(),
                        0.into(),
                        0.into(),
                        150.into(),
                        0.into(),
                        0.into(),
                    ],
                ),
                Operation::new("Do", vec!["Im1".into()]),
                Operation::new("Do", vec!["Im2".into()]),
                Operation::new("Do", vec!["Im3".into()]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let resources = dictionary! { "XObject" => dictionary! { "Im1" => gradient, "Im2" => graphic, "Im3" => photo_jpeg } };
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn plan() -> DocumentPlan {
        DocumentPlan {
            jpeg_quality: 78,
            png_min_quality: 65,
            png_max_quality: 85,
            webp_quality: 76,
            max_image_side: Some(2400),
        }
    }

    #[test]
    fn recompresses_pictures_and_keeps_the_document_valid() {
        let dir = std::env::temp_dir().join(format!("peraz-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.pdf");
        let original = sample_pdf();
        std::fs::write(&path, &original).unwrap();
        let (out, stats, warnings) = compress_pdf(&path, &plan()).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(stats.images, 3);
        assert_eq!(stats.recompressed, 3, "{stats:?}");
        assert!(
            out.len() < original.len() / 4,
            "{} → {}",
            original.len(),
            out.len()
        );
        let doc = Document::load_mem(&out).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
        let kinds: Vec<String> = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter(|s| s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image"))
            .map(|s| {
                let space = s.dict.get(b"ColorSpace").unwrap();
                let filter = s
                    .dict
                    .get(b"Filter")
                    .and_then(Object::as_name)
                    .unwrap_or(b"none");
                format!(
                    "{}:{}",
                    String::from_utf8_lossy(filter),
                    if space.as_array().is_ok() {
                        "Indexed"
                    } else {
                        "direct"
                    }
                )
            })
            .collect();
        assert!(
            kinds.contains(&"FlateDecode:Indexed".to_owned()),
            "flat graphic becomes a palette image: {kinds:?}"
        );
        assert!(kinds.contains(&"DCTDecode:direct".to_owned()), "{kinds:?}");
    }

    #[test]
    fn a_pdf_with_broken_encryption_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("peraz-pdf-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("locked.pdf");
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        // An encryption dictionary without the values a password is checked against.
        let encrypt = doc.add_object(dictionary! { "Filter" => "Standard", "V" => 1, "R" => 2 });
        doc.trailer.set("Encrypt", encrypt);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let result = compress_pdf(&path, &plan());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(
                result,
                Err(DocumentError::Protected(_) | DocumentError::Failed(_))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_result_that_does_not_read_back_is_refused() {
        let good = sample_pdf();
        assert!(read_back(&good, 1).is_ok());
        // Another number of pages, or a file that does not load: nothing may be written.
        let Err(DocumentError::Failed(message)) = read_back(&good, 2) else {
            panic!("a page went missing unnoticed")
        };
        assert!(
            message.contains("1 pages where the original has 2") && message.contains("original"),
            "{message}"
        );
        assert!(matches!(
            read_back(&good[..good.len() / 2], 1),
            Err(DocumentError::Failed(_))
        ));
        assert!(matches!(
            read_back(b"not a PDF", 1),
            Err(DocumentError::Failed(_))
        ));
    }
}
