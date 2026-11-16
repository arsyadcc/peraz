//! Picture metadata that can be carried from the input to the output on request: EXIF, XMP, and
//! IPTC. By default none of it is written.
//!
//! The pixels are always stored upright, so the orientation in a kept EXIF block is reset:
//! applying it again would turn the picture a second time.

/// The metadata of a picture, as the blocks the file formats store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// EXIF, as a TIFF structure (starting with `II*\0` or `MM\0*`).
    pub exif: Option<Vec<u8>>,
    /// The XMP packet (XML).
    pub xmp: Option<Vec<u8>>,
    /// Photoshop image resources, where IPTC data lives in JPEG and TIFF files.
    pub photoshop: Option<Vec<u8>>,
}

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const PHOTOSHOP_HEADER: &[u8] = b"Photoshop 3.0\0";

/// The most a JPEG segment holds after its two length bytes.
const JPEG_SEGMENT_MAX: usize = 65_533;

impl Metadata {
    pub fn is_empty(&self) -> bool {
        self.exif.is_none() && self.xmp.is_none() && self.photoshop.is_none()
    }

    /// Takes an EXIF block as files hold it (possibly after an `Exif\0\0` header), with its
    /// orientation reset; ignored when it is not a TIFF structure.
    pub fn set_exif(&mut self, block: &[u8]) {
        let block = block.strip_prefix(EXIF_HEADER).unwrap_or(block);
        self.exif = upright(block);
    }

    /// Takes an XMP packet, with the orientation it may state reset.
    pub fn set_xmp(&mut self, packet: Vec<u8>) {
        self.xmp = Some(upright_xmp(packet));
    }

    /// The blocks that a JPEG cannot hold because they are larger than one of its segments,
    /// as sentences for the report.
    pub fn too_large_for_jpeg(&self) -> Vec<String> {
        [
            ("EXIF", EXIF_HEADER, &self.exif),
            ("XMP", XMP_HEADER, &self.xmp),
        ]
        .into_iter()
        .filter_map(|(name, header, block)| {
            let size = block.as_ref()?.len();
            (header.len() + size > JPEG_SEGMENT_MAX).then(|| {
                format!(
                    "the {name} metadata ({}) is larger than a JPEG segment holds (64 kB) \
                         and was not kept",
                    peraz_core::units::format_bytes(size as u64)
                )
            })
        })
        .collect()
    }

    /// Reads the metadata segments of a JPEG, which come before its picture data.
    pub fn from_jpeg(jpeg: &[u8]) -> Self {
        let mut metadata = Self::default();
        let mut photoshop = Vec::new();
        let mut at = 2;
        while let (Some(0xFF), Some(&marker)) = (jpeg.get(at), jpeg.get(at + 1)) {
            if marker == 0xFF {
                at += 1; // fill byte
                continue;
            }
            // Start of scan: the rest is picture data.
            if marker == 0xDA {
                break;
            }
            let Some(length) = jpeg
                .get(at + 2..at + 4)
                .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            else {
                break;
            };
            let Some(body) = jpeg.get(at + 4..at + 2 + length) else {
                break;
            };
            match marker {
                0xE1 if body.starts_with(EXIF_HEADER) && metadata.exif.is_none() => {
                    metadata.set_exif(body);
                }
                0xE1 if body.starts_with(XMP_HEADER) && metadata.xmp.is_none() => {
                    metadata.set_xmp(body[XMP_HEADER.len()..].to_vec());
                }
                // Large resources continue in the next segment.
                0xED if body.starts_with(PHOTOSHOP_HEADER) => {
                    photoshop.extend_from_slice(&body[PHOTOSHOP_HEADER.len()..]);
                }
                _ => {}
            }
            at += 2 + length;
        }
        if !photoshop.is_empty() {
            metadata.photoshop = Some(photoshop);
        }
        metadata
    }

    /// The JPEG segments (`APP1` for EXIF and XMP, `APP13` for Photoshop resources) that carry
    /// this metadata, as marker number and body. A block too large for one segment is left
    /// out: only Photoshop resources may continue in a second one.
    pub fn jpeg_segments(&self) -> Vec<(u8, Vec<u8>)> {
        let mut segments = Vec::new();
        let mut one = |marker: u8, header: &[u8], block: &Option<Vec<u8>>| {
            if let Some(block) = block
                && header.len() + block.len() <= JPEG_SEGMENT_MAX
            {
                segments.push((marker, [header, block].concat()));
            }
        };
        one(1, EXIF_HEADER, &self.exif);
        one(1, XMP_HEADER, &self.xmp);
        if let Some(resources) = &self.photoshop {
            for part in resources.chunks(JPEG_SEGMENT_MAX - PHOTOSHOP_HEADER.len()) {
                segments.push((13, [PHOTOSHOP_HEADER, part].concat()));
            }
        }
        segments
    }

    /// Fills in what is missing from the "raw profile" text chunks of a PNG: the form in which
    /// some tools (libjxl's `djxl` among them) store EXIF and XMP, as hexadecimal text.
    pub fn add_png_raw_profiles(&mut self, png: &[u8]) {
        use std::io::Read as _;
        let mut at = 8;
        while let Some(head) = png.get(at..at + 8) {
            let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
            let Some(body) = png.get(at + 8..at + 8 + length) else {
                break;
            };
            at += length + 12;
            let compressed = match &head[4..] {
                b"zTXt" => true,
                b"tEXt" => false,
                b"IDAT" | b"IEND" => break,
                _ => continue,
            };
            let Some(end) = body.iter().position(|b| *b == 0) else {
                continue;
            };
            let Some(kind) = body[..end].strip_prefix(b"Raw profile type ") else {
                continue;
            };
            let text = if compressed {
                // The keyword, its end, a compression method, then zlib data.
                let mut text = Vec::new();
                let data = body.get(end + 2..).unwrap_or_default();
                if flate2::read::ZlibDecoder::new(data)
                    .read_to_end(&mut text)
                    .is_err()
                {
                    continue;
                }
                text
            } else {
                body[end + 1..].to_vec()
            };
            // A name line, a length line, then the bytes as hexadecimal digits.
            let digits: Vec<u8> = text
                .split(|b| *b == b'\n')
                .skip(3)
                .flatten()
                .copied()
                .filter(u8::is_ascii_hexdigit)
                .collect();
            let bytes: Vec<u8> = digits
                .as_chunks::<2>()
                .0
                .iter()
                .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                .collect();
            match kind {
                b"exif" | b"APP1" if self.exif.is_none() => self.set_exif(&bytes),
                b"xmp" if self.xmp.is_none() && !bytes.is_empty() => self.set_xmp(bytes),
                _ => {}
            }
        }
    }

    /// Adds this metadata to a JPEG: its segments go right after the start of the file and the
    /// JFIF header, before everything else.
    pub fn into_jpeg(&self, jpeg: &[u8]) -> Vec<u8> {
        let segments = self.jpeg_segments();
        if segments.is_empty() || jpeg.get(..2) != Some(&[0xFF, 0xD8]) {
            return jpeg.to_vec();
        }
        let mut at = 2;
        if jpeg.get(2..4) == Some(&[0xFF, 0xE0])
            && let Some(length) = jpeg.get(4..6)
        {
            at += 2 + usize::from(u16::from_be_bytes([length[0], length[1]]));
        }
        let at = at.min(jpeg.len());
        let mut out = jpeg[..at].to_vec();
        for (marker, body) in segments {
            out.extend([0xFF, 0xE0 + marker]);
            // The length counts its own two bytes; `jpeg_segments` keeps bodies within it.
            out.extend(
                u16::try_from(body.len() + 2)
                    .unwrap_or(u16::MAX)
                    .to_be_bytes(),
            );
            out.extend(body);
        }
        out.extend_from_slice(&jpeg[at..]);
        out
    }

    /// Adds this metadata to a PNG: an `eXIf` chunk and the XMP packet as an `iTXt` chunk,
    /// before the picture data.
    pub fn into_png(&self, png: &[u8]) -> Vec<u8> {
        let mut chunks = Vec::new();
        if let Some(exif) = &self.exif {
            chunks.extend(png_chunk(*b"eXIf", exif));
        }
        if let Some(xmp) = &self.xmp {
            // Keyword, then: not compressed, no language, no translated keyword.
            let body = [&b"XML:com.adobe.xmp\0\0\0\0\0"[..], xmp].concat();
            chunks.extend(png_chunk(*b"iTXt", &body));
        }
        // After the header chunk (8 bytes of signature, then IHDR: 25 bytes with its frame).
        let Some(insert_at) = first_chunk(png, *b"IDAT").filter(|_| !chunks.is_empty()) else {
            return png.to_vec();
        };
        [&png[..insert_at], &chunks, &png[insert_at..]].concat()
    }
}

/// Where the main picture of a JPEG ends: just after its end marker. Anything after that is
/// not part of the picture (phones append an HDR gain map or other pictures there). `None` when
/// the file is not a JPEG or ends before its picture does.
pub fn jpeg_end(jpeg: &[u8]) -> Option<usize> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        match *jpeg.get(at + 1)? {
            // A fill byte before the marker.
            0xFF => at += 1,
            0xD9 => return Some(at + 2),
            // Markers without a body.
            0x01 | 0xD0..=0xD7 => at += 2,
            marker => {
                let length = jpeg.get(at + 2..at + 4)?;
                at += 2 + usize::from(u16::from_be_bytes([length[0], length[1]]));
                if marker != 0xDA {
                    continue;
                }
                // A scan: compressed data follows its header, up to the next marker. In that
                // data a 0xFF byte is followed by a zero or by a restart marker.
                loop {
                    if *jpeg.get(at)? != 0xFF {
                        at += 1;
                        continue;
                    }
                    match *jpeg.get(at + 1)? {
                        0x00 | 0xD0..=0xD7 => at += 2,
                        0xFF => at += 1,
                        _ => break,
                    }
                }
            }
        }
    }
}

/// The color profile of a JPEG, put together from its `APP2` parts in the order of their
/// numbers. Parts are numbered from 1, but files exist whose parts count from 0 (Peraz wrote
/// them up to version 0.5.0): those are read too, where stricter readers find no profile.
pub fn jpeg_profile(jpeg: &[u8]) -> Option<Vec<u8>> {
    const HEADER: &[u8] = b"ICC_PROFILE\0";
    let mut parts: Vec<(u8, &[u8])> = Vec::new();
    let mut at = 2;
    while let (Some(0xFF), Some(&marker)) = (jpeg.get(at), jpeg.get(at + 1)) {
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if marker == 0xDA {
            break;
        }
        let length = jpeg.get(at + 2..at + 4)?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        let body = jpeg.get(at + 4..at + 2 + length)?;
        if marker == 0xE2
            && let Some(rest) = body.strip_prefix(HEADER)
            && rest.len() >= 2
        {
            // The part's number, the number of parts, then its data.
            parts.push((rest[0], &rest[2..]));
        }
        at += 2 + length;
    }
    parts.sort_by_key(|(number, _)| *number);
    let profile: Vec<u8> = parts
        .iter()
        .flat_map(|(_, data)| data.iter().copied())
        .collect();
    (!profile.is_empty()).then_some(profile)
}

/// What marks a picture as an HDR gain map, or as having one: the Ultra HDR and Apple names
/// in its XMP, and the name of the standard for gain maps (ISO 21496-1).
const GAIN_MAP_MARKS: [&[u8]; 3] = [b"hdrgm:", b"HDRGainMap", b"urn:iso:std:iso:ts:21496"];

/// Whether a JPEG carries an HDR gain map: a second picture stored after the main one, which
/// tells HDR screens how to brighten it.
pub fn has_gain_map(jpeg: &[u8]) -> bool {
    let Some(end) = jpeg_end(jpeg) else {
        return false;
    };
    // The marks are in the metadata at the start of the gain map picture.
    let after = &jpeg[end..jpeg.len().min(end + 256 * 1024)];
    after.windows(2).any(|w| w == [0xFF, 0xD8])
        && GAIN_MAP_MARKS
            .iter()
            .any(|mark| after.windows(mark.len()).any(|w| w == *mark))
}

/// An XMP packet with its `tiff:Orientation` set to "upright" (1), in either way of writing it.
fn upright_xmp(xmp: Vec<u8>) -> Vec<u8> {
    let Ok(mut text) = String::from_utf8(xmp.clone()) else {
        return xmp;
    };
    for (before, after) in [
        ("tiff:Orientation=\"", "\""),
        ("tiff:Orientation='", "'"),
        ("<tiff:Orientation>", "<"),
    ] {
        let mut from = 0;
        while let Some(found) = text[from..].find(before) {
            let start = from + found + before.len();
            let Some(length) = text[start..].find(after) else {
                break;
            };
            text.replace_range(start..start + length, "1");
            from = start + 1;
        }
    }
    text.into_bytes()
}

fn png_chunk(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(&kind);
    crc.update(body);
    let mut out = u32::try_from(body.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    out.extend(crc.sum().to_be_bytes());
    out
}

/// Where the first chunk of `kind` starts in a PNG.
fn first_chunk(png: &[u8], kind: [u8; 4]) -> Option<usize> {
    let mut at = 8;
    loop {
        let head = png.get(at..at + 8)?;
        if head[4..] == kind {
            return Some(at);
        }
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        at = at.checked_add(usize::try_from(length).ok()? + 12)?;
    }
}

/// An EXIF block with its orientation set to "upright" (1). `None` when the block is not a
/// TIFF structure.
fn upright(exif: &[u8]) -> Option<Vec<u8>> {
    let little = match exif.get(..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |at: usize| {
        let pair = exif.get(at..at + 2)?;
        Some(if little {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        })
    };
    let u32_at = |at: usize| {
        let four = exif.get(at..at + 4)?;
        let four = [four[0], four[1], four[2], four[3]];
        Some(if little {
            u32::from_le_bytes(four)
        } else {
            u32::from_be_bytes(four)
        })
    };
    let mut out = exif.to_vec();
    // The first directory: a count, then 12-byte entries (tag, type, count, value).
    let directory = usize::try_from(u32_at(4)?).ok()?;
    let entries = usize::from(u16_at(directory)?);
    for index in 0..entries {
        let entry = directory + 2 + index * 12;
        // Orientation: one 16-bit value, stored in the entry itself.
        if u16_at(entry)? == 0x0112 && u16_at(entry + 2)? == 3 && u32_at(entry + 4)? == 1 {
            let one = if little { [1, 0] } else { [0, 1] };
            out.get_mut(entry + 8..entry + 10)?.copy_from_slice(&one);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EXIF block with an orientation and a camera maker, in either byte order.
    pub(crate) fn exif(little: bool, orientation: u16) -> Vec<u8> {
        let n16 = |v: u16| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let n32 = |v: u32| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut out = if little {
            b"II*\0".to_vec()
        } else {
            b"MM\0*".to_vec()
        };
        out.extend(n32(8));
        out.extend(n16(2));
        // Make (0x010F), ASCII, 6 bytes, stored after the directory.
        out.extend(n16(0x010F));
        out.extend(n16(2));
        out.extend(n32(6));
        out.extend(n32(38));
        // Orientation (0x0112), one SHORT.
        out.extend(n16(0x0112));
        out.extend(n16(3));
        out.extend(n32(1));
        out.extend(n16(orientation));
        out.extend(n16(0));
        out.extend(n32(0));
        out.extend_from_slice(b"Peraz\0");
        out
    }

    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let length = u16::try_from(body.len() + 2).unwrap().to_be_bytes();
        [&[0xFF, marker], &length[..], body].concat()
    }

    #[test]
    fn the_orientation_is_reset_and_the_rest_stays() {
        for little in [true, false] {
            let mut metadata = Metadata::default();
            metadata.set_exif(&[EXIF_HEADER, &exif(little, 6)].concat());
            assert_eq!(metadata.exif, Some(exif(little, 1)), "little: {little}");
            // Without the header, as PNG and WebP store it.
            metadata.set_exif(&exif(little, 8));
            assert_eq!(metadata.exif, Some(exif(little, 1)));
        }
        let mut metadata = Metadata::default();
        metadata.set_exif(b"not a TIFF structure");
        assert_eq!(metadata.exif, None);
        // A cut-off block is not kept either.
        metadata.set_exif(&exif(true, 6)[..20]);
        assert_eq!(metadata.exif, None);
    }

    #[test]
    fn jpeg_segments_are_read_and_written_back() {
        let xmp = b"<x:xmpmeta>a caption</x:xmpmeta>".to_vec();
        let resources = b"8BIM\x04\x04\0\0\0\0\0\x08IPTC data".to_vec();
        let jpeg = [
            &[0xFF, 0xD8][..],
            &segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"),
            &segment(0xE1, &[EXIF_HEADER, &exif(false, 3)].concat()),
            &segment(0xE1, &[XMP_HEADER, &xmp].concat()),
            &segment(0xED, &[PHOTOSHOP_HEADER, &resources].concat()),
            &segment(0xDB, &[0; 65]),
            &[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9],
        ]
        .concat();
        let metadata = Metadata::from_jpeg(&jpeg);
        assert_eq!(
            metadata,
            Metadata {
                exif: Some(exif(false, 1)),
                xmp: Some(xmp.clone()),
                photoshop: Some(resources.clone()),
            }
        );
        assert!(!metadata.is_empty());
        let segments = metadata.jpeg_segments();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0], (1, [EXIF_HEADER, &exif(false, 1)].concat()));
        assert_eq!(segments[1], (1, [XMP_HEADER, &xmp].concat()));
        assert_eq!(segments[2], (13, [PHOTOSHOP_HEADER, &resources].concat()));

        // A block that does not fit a segment is left out; resources are split.
        let large = Metadata {
            exif: None,
            xmp: Some(vec![b'x'; 70_000]),
            photoshop: Some(vec![7; 70_000]),
        };
        let segments = large.jpeg_segments();
        assert_eq!(segments.len(), 2);
        assert!(
            segments
                .iter()
                .all(|(marker, body)| *marker == 13 && body.len() <= JPEG_SEGMENT_MAX)
        );
        assert!(Metadata::from_jpeg(b"\xFF\xD8\xFF\xD9").is_empty());
        assert!(Metadata::from_jpeg(b"").is_empty());
    }

    #[test]
    fn a_jpeg_profile_is_read_whichever_way_its_parts_are_numbered() {
        let part = |number: u8, count: u8, data: &[u8]| {
            segment(
                0xE2,
                &[b"ICC_PROFILE\0", &[number, count][..], data].concat(),
            )
        };
        let end = [0xFF, 0xDA, 0, 2, 0xFF, 0xD9];
        // Numbered from 1, as the specification says, stored out of order.
        let jpeg = [
            &[0xFF, 0xD8][..],
            &part(2, 2, b"profile"),
            &part(1, 2, b"a color "),
            &end,
        ]
        .concat();
        assert_eq!(
            jpeg_profile(&jpeg).as_deref(),
            Some(&b"a color profile"[..])
        );
        // Numbered from 0, as some writers did.
        let jpeg = [&[0xFF, 0xD8][..], &part(0, 1, b"counted from zero"), &end].concat();
        assert_eq!(
            jpeg_profile(&jpeg).as_deref(),
            Some(&b"counted from zero"[..])
        );
        // Other APP2 data (the index of pictures in a phone photo) is not a profile.
        let jpeg = [&[0xFF, 0xD8][..], &segment(0xE2, b"MPF\0 an index"), &end].concat();
        assert_eq!(jpeg_profile(&jpeg), None);
        assert_eq!(jpeg_profile(b""), None);
    }

    #[test]
    fn png_chunks_go_before_the_picture_data() {
        use image::ImageDecoder as _;
        let mut png = Vec::new();
        image::RgbImage::new(4, 4)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let metadata = Metadata {
            exif: Some(exif(true, 1)),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            photoshop: None,
        };
        let tagged = metadata.into_png(&png);
        assert!(tagged.len() > png.len());
        let mut decoder =
            image::codecs::png::PngDecoder::new(std::io::Cursor::new(&tagged)).unwrap();
        assert_eq!(decoder.exif_metadata().unwrap(), metadata.exif);
        assert_eq!(decoder.xmp_metadata().unwrap(), metadata.xmp);
        assert_eq!(Metadata::default().into_png(&png), png);
    }

    #[test]
    fn raw_profile_text_chunks_are_read() {
        use std::io::Write as _;
        let mut png = Vec::new();
        image::RgbImage::new(4, 4)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let hex = |bytes: &[u8]| {
            let digits = bytes.iter().fold(String::new(), |mut digits, b| {
                use std::fmt::Write as _;
                let _ = write!(digits, "{b:02x}");
                digits
            });
            format!("\nname\n{:8}\n{digits}\n", bytes.len())
        };
        // The EXIF profile compressed, the XMP one as plain text.
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zlib.write_all(hex(&[EXIF_HEADER, &exif(true, 6)].concat()).as_bytes())
            .unwrap();
        let exif_chunk = [&b"Raw profile type exif\0\0"[..], &zlib.finish().unwrap()].concat();
        let xmp_chunk = [b"Raw profile type xmp\0", hex(b"<x:xmpmeta/>").as_bytes()].concat();
        let insert_at = first_chunk(&png, *b"IDAT").unwrap();
        let tagged = [
            &png[..insert_at],
            &png_chunk(*b"zTXt", &exif_chunk),
            &png_chunk(*b"tEXt", &xmp_chunk),
            &png[insert_at..],
        ]
        .concat();
        let mut metadata = Metadata::default();
        metadata.add_png_raw_profiles(&tagged);
        assert_eq!(metadata.exif, Some(exif(true, 1)));
        assert_eq!(metadata.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        // What is already known stays.
        let mut known = Metadata {
            xmp: Some(b"first".to_vec()),
            ..Metadata::default()
        };
        known.add_png_raw_profiles(&tagged);
        assert_eq!(known.xmp.as_deref(), Some(&b"first"[..]));
        let mut none = Metadata::default();
        none.add_png_raw_profiles(&png);
        assert!(none.is_empty());
    }
}
