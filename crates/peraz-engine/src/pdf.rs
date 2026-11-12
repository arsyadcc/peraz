//! PDF: recompress the pictures, leave everything else as it is.
//!
//! Photos (`DCTDecode`) are re-encoded with mozjpeg. Uncompressed or Flate pictures become
//! indexed (palette) images when a palette holds their quality, and JPEG otherwise. A picture is
//! replaced only when the new version is smaller. Afterwards unused objects are removed and the
//! file is written with compressed object streams.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;

use image::{DynamicImage, GrayImage, RgbImage, imageops::FilterType};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use peraz_core::plan::fit_within;
use peraz_core::{DocumentPlan, DocumentStats};

use crate::image::{encode_jpeg_plain, quantize};

/// Why a PDF was not rewritten.
#[derive(Debug)]
pub enum DocumentError {
    /// Encrypted or DRM-protected: Peraz never touches those.
    Protected(String),
    Failed(String),
}

/// Recompresses the pictures of the PDF at `input`. Returns the new file and what happened.
pub fn compress_pdf(
    input: &Path,
    plan: &DocumentPlan,
) -> Result<(Vec<u8>, DocumentStats), DocumentError> {
    let mut doc = Document::load(input)
        .map_err(|e| DocumentError::Failed(format!("cannot read the PDF: {e}")))?;
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(DocumentError::Protected("the PDF is encrypted".to_owned()));
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
        match recompress(stream, color, plan) {
            Picture::Replace(new) => {
                stats.recompressed += 1;
                doc.objects.insert(id, Object::Stream(new));
            }
            Picture::Kept => stats.kept += 1,
            Picture::Skipped => stats.skipped += 1,
        }
    }

    doc.prune_objects();
    doc.compress();
    let mut out = Vec::new();
    doc.save_modern(&mut out)
        .map_err(|e| DocumentError::Failed(format!("cannot write the PDF: {e}")))?;
    Ok((out, stats))
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
}

fn recompress(stream: &Stream, color: Option<Components>, plan: &DocumentPlan) -> Picture {
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
        [] | [b"FlateDecode"] => decode_raw(stream, components),
        _ => None, // JPEG 2000, JBIG2, CCITT, chained filters
    };
    let Some(mut picture) = decoded else {
        return Picture::Skipped;
    };
    if let Some((w, h)) = fit_within(
        (picture.width(), picture.height()),
        plan.max_image_side,
        plan.max_image_side,
    ) {
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

fn decode_raw(stream: &Stream, components: Components) -> Option<DynamicImage> {
    let width = u32::try_from(stream.dict.get(b"Width").and_then(Object::as_i64).ok()?).ok()?;
    let height = u32::try_from(stream.dict.get(b"Height").and_then(Object::as_i64).ok()?).ok()?;
    let pixels = if stream.filters().map_or(true, |f| f.is_empty()) {
        stream.content.clone()
    } else {
        stream.decompressed_content().ok()?
    };
    match components {
        Components::Gray => {
            GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        Components::Rgb => RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8),
    }
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
        let (out, stats) = compress_pdf(&path, &plan()).unwrap();
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
    fn encrypted_pdfs_are_left_alone() {
        let dir = std::env::temp_dir().join(format!("peraz-pdf-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("locked.pdf");
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
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
}
