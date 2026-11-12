//! EPUB: recompress the pictures inside the book, keep everything else byte for byte.
//!
//! Pictures keep their names and formats, so every reference in the book still works. Other
//! entries are copied without recompression. The archive is rewritten with `mimetype` first and
//! stored uncompressed, as the EPUB container format requires.

use std::io::{Cursor, Read as _, Write as _};
use std::path::Path;

use peraz_core::{DocumentPlan, DocumentStats, ImageFormat};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::image::{decode_native_bytes, encode_image};
use crate::pdf::DocumentError;

/// Font obfuscation algorithms: these only scramble embedded fonts and are not DRM.
const FONT_OBFUSCATION: &[&str] = &[
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

/// Recompresses the pictures of the EPUB at `input`. Returns the new file and what happened.
pub fn compress_epub(
    input: &Path,
    plan: &DocumentPlan,
) -> Result<(Vec<u8>, DocumentStats), DocumentError> {
    let failed = |message: String| DocumentError::Failed(message);
    let file = std::fs::File::open(input)
        .map_err(|e| failed(format!("cannot read {}: {e}", input.display())))?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| failed(format!("not a valid EPUB archive: {e}")))?;

    if archive.by_name("META-INF/rights.xml").is_ok() {
        return Err(DocumentError::Protected(
            "the EPUB is DRM-protected (META-INF/rights.xml)".to_owned(),
        ));
    }
    let encrypted = match archive.by_name("META-INF/encryption.xml") {
        Ok(mut entry) => {
            let mut xml = String::new();
            entry
                .read_to_string(&mut xml)
                .map_err(|e| failed(format!("cannot read encryption.xml: {e}")))?;
            let entries = parse_encryption(&xml);
            if entries
                .iter()
                .any(|(algorithm, _)| !FONT_OBFUSCATION.contains(&algorithm.as_str()))
            {
                return Err(DocumentError::Protected(
                    "the EPUB is DRM-protected (encrypted content)".to_owned(),
                ));
            }
            entries.into_iter().map(|(_, uri)| uri).collect()
        }
        Err(_) => Vec::new(),
    };

    let mut out = ZipWriter::new(Cursor::new(Vec::new()));
    let mut stats = DocumentStats::default();
    // `mimetype` must come first and be stored uncompressed.
    if let Ok(mut entry) = archive.by_name("mimetype") {
        let mut content = Vec::new();
        entry
            .read_to_end(&mut content)
            .map_err(|e| failed(format!("cannot read mimetype: {e}")))?;
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        out.start_file("mimetype", stored)
            .map_err(|e| failed(e.to_string()))?;
        out.write_all(&content).map_err(|e| failed(e.to_string()))?;
    }
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|e| failed(format!("damaged EPUB archive: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| failed(e.to_string()))?
            .into_owned();
        if name == "mimetype" {
            continue;
        }
        let format = picture_format(&name)
            .filter(|_| !encrypted.iter().any(|uri| name.ends_with(uri.as_str())));
        let Some(format) = format else {
            out.raw_copy_file(entry)
                .map_err(|e| failed(format!("cannot copy {name}: {e}")))?;
            continue;
        };
        drop(entry);
        stats.images += 1;
        let mut original = Vec::new();
        archive
            .by_index(index)
            .and_then(|mut e| Ok(e.read_to_end(&mut original)?))
            .map_err(|e| failed(format!("cannot read {name}: {e}")))?;
        let smaller = recompress_picture(&original, &name, format, plan);
        match &smaller {
            Some(bytes) if bytes.len() < original.len() => stats.recompressed += 1,
            Some(_) => stats.kept += 1,
            None => stats.skipped += 1,
        }
        let bytes = smaller
            .filter(|b| b.len() < original.len())
            .unwrap_or(original);
        // Pictures are already compressed: store them as they are.
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        out.start_file(name.as_str(), stored)
            .map_err(|e| failed(e.to_string()))?;
        out.write_all(&bytes).map_err(|e| failed(e.to_string()))?;
    }
    let bytes = out
        .finish()
        .map_err(|e| failed(format!("cannot write the EPUB: {e}")))?
        .into_inner();
    Ok((bytes, stats))
}

fn picture_format(name: &str) -> Option<ImageFormat> {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.')?.1;
    match ext {
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::Webp),
        _ => None,
    }
}

fn recompress_picture(
    bytes: &[u8],
    name: &str,
    format: ImageFormat,
    plan: &DocumentPlan,
) -> Option<Vec<u8>> {
    let decoded = decode_native_bytes(bytes, name).ok()?;
    encode_image(decoded, &plan.image_plan(format))
        .ok()
        .map(|encoded| encoded.bytes)
}

/// `(algorithm, uri)` for every encrypted entry listed in `META-INF/encryption.xml`.
fn parse_encryption(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut entries = Vec::new();
    let mut algorithm = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag) | Event::Empty(tag)) => {
                let local = tag.local_name();
                let attr = |key: &str| {
                    tag.attributes()
                        .filter_map(Result::ok)
                        .find(|a| a.key.local_name().as_ref() == key)
                        .map(|a| a.value.to_string())
                };
                match local.as_ref() {
                    "EncryptionMethod" => algorithm = attr("Algorithm"),
                    "CipherReference" => {
                        if let Some(uri) = attr("URI") {
                            entries.push((algorithm.take().unwrap_or_default(), uri));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epub(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        out.start_file("mimetype", stored).unwrap();
        out.write_all(b"application/epub+zip").unwrap();
        for (name, bytes) in entries {
            out.start_file(*name, SimpleFileOptions::default()).unwrap();
            out.write_all(bytes).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    /// A flat-color illustration, the typical picture in a book.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let picture = image::RgbImage::from_fn(width, height, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            let band = ((x / 50 + y / 50) % 6) as u8;
            image::Rgb([band * 40, 200 - band * 30, 90 + band * 20])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(picture)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
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

    fn run(bytes: &[u8]) -> Result<(Vec<u8>, DocumentStats), DocumentError> {
        let dir =
            std::env::temp_dir().join(format!("peraz-epub-{}-{}", std::process::id(), bytes.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.epub");
        std::fs::write(&path, bytes).unwrap();
        let result = compress_epub(&path, &plan());
        std::fs::remove_dir_all(&dir).unwrap();
        result
    }

    #[test]
    fn pictures_shrink_and_the_archive_stays_valid() {
        let chapter = b"<html><body><img src=\"images/a.png\"/></body></html>".to_vec();
        let book = epub(&[
            ("OEBPS/chapter.xhtml", chapter.clone()),
            ("OEBPS/images/a.png", png(600, 400)),
        ]);
        let (out, stats) = run(&book).unwrap();
        assert_eq!((stats.images, stats.recompressed), (1, 1), "{stats:?}");
        let mut archive = ZipArchive::new(Cursor::new(&out)).unwrap();
        let first = archive.by_index(0).unwrap();
        assert_eq!(
            (first.name().unwrap().as_ref(), first.compression()),
            ("mimetype", CompressionMethod::Stored)
        );
        drop(first);
        let mut text = Vec::new();
        archive
            .by_name("OEBPS/chapter.xhtml")
            .unwrap()
            .read_to_end(&mut text)
            .unwrap();
        assert_eq!(text, chapter, "other entries are copied unchanged");
        let mut picture = Vec::new();
        archive
            .by_name("OEBPS/images/a.png")
            .unwrap()
            .read_to_end(&mut picture)
            .unwrap();
        assert_eq!(image::load_from_memory(&picture).unwrap().width(), 600);
    }

    #[test]
    fn drm_is_detected_but_font_obfuscation_is_not_drm() {
        let fonts = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
          <enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>
          <enc:CipherData><enc:CipherReference URI="OEBPS/fonts/a.otf"/></enc:CipherData></enc:EncryptedData></encryption>"#;
        let book = epub(&[
            ("META-INF/encryption.xml", fonts.as_bytes().to_vec()),
            ("OEBPS/a.png", png(64, 64)),
        ]);
        assert!(run(&book).is_ok());

        let drm = fonts.replace(
            "http://www.idpf.org/2008/embedding",
            "http://www.w3.org/2001/04/xmlenc#aes128-cbc",
        );
        let book = epub(&[
            ("META-INF/encryption.xml", drm.into_bytes()),
            ("OEBPS/a.png", png(64, 64)),
        ]);
        assert!(matches!(run(&book), Err(DocumentError::Protected(_))));

        let book = epub(&[("META-INF/rights.xml", b"<rights/>".to_vec())]);
        assert!(matches!(run(&book), Err(DocumentError::Protected(_))));
    }
}
