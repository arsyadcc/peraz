//! TIFF output: lossless Deflate compression that keeps the bit depth, transparency, and color
//! profile; and counting the pages of a TIFF input, because only single pictures are compressed.

use std::borrow::Cow;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use image::DynamicImage;
use tiff::encoder::colortype::{self, ColorType};
use tiff::encoder::{Compression, DeflateLevel, TiffEncoder, TiffValue};
use tiff::tags::{ExtraSamples, Predictor, Tag, Type};

/// What the report names as the encoder of a TIFF.
pub const ENCODER: &str = "TIFF Deflate (lossless)";

/// Encodes a picture as a Deflate-compressed TIFF with the same samples: 8-bit, 16-bit, or
/// floating point; gray or RGB; with or without transparency. Gray with transparency is stored
/// as RGB with transparency, which every TIFF reader understands.
pub fn encode(image: &DynamicImage, icc: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let (width, height) = (image.width(), image.height());
    let page = Page { width, height, icc };
    let mut out = Cursor::new(Vec::new());
    match image {
        DynamicImage::ImageLuma8(pixels) => {
            page.write::<colortype::Gray8>(&mut out, pixels.as_raw(), false)
        }
        DynamicImage::ImageLumaA8(_) => {
            let pixels = image.to_rgba8();
            page.write::<colortype::RGBA8>(&mut out, pixels.as_raw(), true)
        }
        DynamicImage::ImageRgb8(pixels) => {
            page.write::<colortype::RGB8>(&mut out, pixels.as_raw(), false)
        }
        DynamicImage::ImageRgba8(pixels) => {
            page.write::<colortype::RGBA8>(&mut out, pixels.as_raw(), true)
        }
        DynamicImage::ImageLuma16(pixels) => {
            page.write::<colortype::Gray16>(&mut out, pixels.as_raw(), false)
        }
        DynamicImage::ImageLumaA16(_) => {
            let pixels = image.to_rgba16();
            page.write::<colortype::RGBA16>(&mut out, pixels.as_raw(), true)
        }
        DynamicImage::ImageRgb16(pixels) => {
            page.write::<colortype::RGB16>(&mut out, pixels.as_raw(), false)
        }
        DynamicImage::ImageRgba16(pixels) => {
            page.write::<colortype::RGBA16>(&mut out, pixels.as_raw(), true)
        }
        DynamicImage::ImageRgb32F(pixels) => {
            page.write::<colortype::RGB32Float>(&mut out, pixels.as_raw(), false)
        }
        DynamicImage::ImageRgba32F(pixels) => {
            page.write::<colortype::RGBA32Float>(&mut out, pixels.as_raw(), true)
        }
        // `DynamicImage` may grow new sample types: 8-bit RGBA holds any of them.
        other => {
            let pixels = other.to_rgba8();
            page.write::<colortype::RGBA8>(&mut out, pixels.as_raw(), true)
        }
    }
    .map_err(|e| format!("cannot write the TIFF: {e}"))?;
    Ok(out.into_inner())
}

struct Page<'a> {
    width: u32,
    height: u32,
    icc: Option<&'a [u8]>,
}

impl Page<'_> {
    fn write<C: ColorType>(
        &self,
        out: &mut Cursor<Vec<u8>>,
        samples: &[C::Inner],
        alpha: bool,
    ) -> tiff::TiffResult<()>
    where
        [C::Inner]: TiffValue,
    {
        // Horizontal differencing makes smooth pictures compress much better; the encoder
        // offers it for whole-number samples only.
        let predictor = if C::SAMPLE_FORMAT.contains(&tiff::tags::SampleFormat::IEEEFP) {
            Predictor::None
        } else {
            Predictor::Horizontal
        };
        let mut encoder = TiffEncoder::new(out)?
            .with_compression(Compression::Deflate(DeflateLevel::Best))
            .with_predictor(predictor);
        let mut image = encoder.new_image::<C>(self.width, self.height)?;
        // Transparency is stored unassociated (not premultiplied), as the decoders give it.
        if alpha {
            image.encoder().write_tag(
                Tag::ExtraSamples,
                &[ExtraSamples::UnassociatedAlpha.to_u16()][..],
            )?;
        }
        if let Some(icc) = self.icc {
            image.encoder().write_tag(Tag::IccProfile, Undefined(icc))?;
        }
        image.write_data(samples)
    }
}

/// Bytes stored with the TIFF type `UNDEFINED`, which the ICC profile tag is specified to use.
struct Undefined<'a>(&'a [u8]);

impl TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: Type = Type::UNDEFINED;

    fn count(&self) -> usize {
        self.0.len()
    }

    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.0)
    }
}

/// Most directories followed in one file: far beyond any real document, and it ends a chain
/// that loops.
const MAX_DIRECTORIES: usize = 100_000;

/// How many pages a TIFF file holds: its pictures, not counting reduced-size previews and
/// transparency masks stored next to them. `None` when the file is not a readable TIFF.
pub fn page_count(path: &Path) -> Option<u32> {
    let mut file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    pages(&mut file)
}

fn pages<R: Read + Seek>(file: &mut R) -> Option<u32> {
    let mut head = [0u8; 16];
    file.read_exact(&mut head[..8]).ok()?;
    let little = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |b: &[u8]| {
        let pair = [b[0], b[1]];
        if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let u32_at = |b: &[u8]| {
        let four = [b[0], b[1], b[2], b[3]];
        if little {
            u32::from_le_bytes(four)
        } else {
            u32::from_be_bytes(four)
        }
    };
    let u64_at = |b: &[u8]| {
        let eight = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        if little {
            u64::from_le_bytes(eight)
        } else {
            u64::from_be_bytes(eight)
        }
    };
    // Classic TIFF (42) has 32-bit offsets and 12-byte entries; BigTIFF (43) 64-bit and 20-byte.
    let big = match u16_at(&head[2..4]) {
        42 => false,
        43 => true,
        _ => return None,
    };
    let mut offset = if big {
        file.read_exact(&mut head[8..16]).ok()?;
        u64_at(&head[8..16])
    } else {
        u64::from(u32_at(&head[4..8]))
    };
    let entry_len: usize = if big { 20 } else { 12 };
    let mut count = 0u32;
    let mut seen = std::collections::HashSet::new();
    while offset != 0 && seen.len() < MAX_DIRECTORIES && seen.insert(offset) {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut number = [0u8; 8];
        let entries = if big {
            file.read_exact(&mut number).ok()?;
            u64_at(&number)
        } else {
            file.read_exact(&mut number[..2]).ok()?;
            u64::from(u16_at(&number[..2]))
        };
        let mut table = vec![0u8; usize::try_from(entries).ok()?.checked_mul(entry_len)?];
        file.read_exact(&mut table).ok()?;
        let mut is_page = true;
        for entry in table.chunks_exact(entry_len) {
            let value = u32_at(&entry[if big { 12 } else { 8 }..]);
            match u16_at(&entry[..2]) {
                // NewSubfileType: bit 0 marks a reduced-size version, bit 2 a transparency mask.
                254 if value & 0b101 != 0 => is_page = false,
                // SubfileType (the older tag), stored as a 16-bit value: 2 is a reduced-size
                // version.
                255 if u16_at(&entry[if big { 12 } else { 8 }..]) == 2 => is_page = false,
                _ => {}
            }
        }
        count += u32::from(is_page);
        offset = if big {
            file.read_exact(&mut number).ok()?;
            u64_at(&number)
        } else {
            file.read_exact(&mut number[..4]).ok()?;
            u64::from(u32_at(&number[..4]))
        };
    }
    Some(count)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small synthetic images
mod tests {
    use image::ImageDecoder as _;

    use super::*;

    fn read_back(tiff: &[u8]) -> (DynamicImage, Option<Vec<u8>>) {
        let mut decoder = image::codecs::tiff::TiffDecoder::new(Cursor::new(tiff)).unwrap();
        let icc = decoder.icc_profile().unwrap();
        (DynamicImage::from_decoder(decoder).unwrap(), icc)
    }

    #[test]
    fn every_sample_type_survives_with_its_depth_alpha_and_profile() {
        let (w, h) = (40, 24);
        let v = |x: u32, y: u32| (x * 6 + y * 3) as u8;
        let wide = |x: u32, y: u32| (x * 1500 + y * 700) as u16;
        let pictures = [
            DynamicImage::ImageLuma8(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Luma([v(x, y)])
            })),
            DynamicImage::ImageLumaA8(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::LumaA([v(x, y), if x < 8 { 0 } else { 255 }])
            })),
            DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgb([v(x, y), v(y, x), 90])
            })),
            DynamicImage::ImageRgba8(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgba([v(x, y), v(y, x), 90, if x < 8 { 40 } else { 255 }])
            })),
            DynamicImage::ImageLuma16(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Luma([wide(x, y)])
            })),
            DynamicImage::ImageLumaA16(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::LumaA([wide(x, y), if x < 8 { 0 } else { u16::MAX }])
            })),
            DynamicImage::ImageRgb16(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgb([wide(x, y), wide(y, x), 257])
            })),
            DynamicImage::ImageRgba16(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgba([
                    wide(x, y),
                    wide(y, x),
                    257,
                    if x < 8 { 999 } else { u16::MAX },
                ])
            })),
            DynamicImage::ImageRgb32F(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgb([f32::from(v(x, y)) / 255.0, 0.25, f32::from(v(y, x)) / 300.0])
            })),
            DynamicImage::ImageRgba32F(image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgba([
                    f32::from(v(x, y)) / 255.0,
                    0.25,
                    1.5,
                    if x < 8 { 0.5 } else { 1.0 },
                ])
            })),
        ];
        let icc = b"not a real profile, only bytes to carry".to_vec();
        for picture in pictures {
            let tiff = encode(&picture, Some(&icc)).unwrap();
            let (back, profile) = read_back(&tiff);
            // Gray with transparency is stored as RGB with transparency, at the same depth.
            let expected = match picture {
                DynamicImage::ImageLumaA8(_) => DynamicImage::ImageRgba8(picture.to_rgba8()),
                DynamicImage::ImageLumaA16(_) => DynamicImage::ImageRgba16(picture.to_rgba16()),
                other => other,
            };
            assert_eq!(back.color(), expected.color(), "the sample type is kept");
            assert_eq!(
                back.as_bytes(),
                expected.as_bytes(),
                "{:?}",
                expected.color()
            );
            assert_eq!(profile.as_deref(), Some(icc.as_slice()));
            assert_eq!(pages(&mut Cursor::new(&tiff)), Some(1));
        }
    }

    #[test]
    fn smooth_pictures_get_much_smaller_than_their_samples() {
        let picture = DynamicImage::ImageRgb16(image::ImageBuffer::from_fn(256, 256, |x, y| {
            image::Rgb([(x * 250) as u16, (y * 250) as u16, ((x + y) * 120) as u16])
        }));
        let tiff = encode(&picture, None).unwrap();
        assert!(
            tiff.len() * 10 < picture.as_bytes().len(),
            "{} bytes for {} bytes of samples",
            tiff.len(),
            picture.as_bytes().len()
        );
        assert_eq!(read_back(&tiff).1, None, "no profile is invented");
    }

    /// A TIFF of 1x1 gray pictures, one per `subfile` value (the `NewSubfileType` tag).
    fn chain(subfiles: &[u32], little: bool, big: bool) -> Vec<u8> {
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
        let n64 = |v: u64| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut out = if little {
            b"II".to_vec()
        } else {
            b"MM".to_vec()
        };
        if big {
            out.extend(n16(43));
            out.extend(n16(8));
            out.extend(n16(0));
            out.extend(n64(16));
        } else {
            out.extend(n16(42));
            out.extend(n32(8));
        }
        for (i, subfile) in subfiles.iter().enumerate() {
            let last = i + 1 == subfiles.len();
            // Two entries: NewSubfileType (254, LONG) and ImageWidth (256, LONG).
            if big {
                out.extend(n64(2));
                for (tag, value) in [(254u16, *subfile), (256, 1)] {
                    out.extend(n16(tag));
                    out.extend(n16(4));
                    out.extend(n64(1));
                    out.extend(n32(value));
                    out.extend(n32(0));
                }
                let next = out.len() as u64 + 8;
                out.extend(n64(if last { 0 } else { next }));
            } else {
                out.extend(n16(2));
                for (tag, value) in [(254u16, *subfile), (256, 1)] {
                    out.extend(n16(tag));
                    out.extend(n16(4));
                    out.extend(n32(1));
                    out.extend(n32(value));
                }
                let next = out.len() as u32 + 4;
                out.extend(n32(if last { 0 } else { next }));
            }
        }
        out
    }

    #[test]
    fn pages_are_counted_without_previews_and_masks() {
        for (little, big) in [(true, false), (false, false), (true, true), (false, true)] {
            let count = |subfiles: &[u32]| pages(&mut Cursor::new(chain(subfiles, little, big)));
            assert_eq!(count(&[0]), Some(1));
            assert_eq!(count(&[0, 0, 0]), Some(3));
            // Pages of a multi-page document are marked with bit 1.
            assert_eq!(count(&[2, 2]), Some(2));
            // A picture with its preview (bit 0) and its mask (bit 2) is one page.
            assert_eq!(count(&[0, 1, 4]), Some(1));
        }
        assert_eq!(pages(&mut Cursor::new(b"\x89PNG\r\n\x1a\n".to_vec())), None);
        // A directory chain that points back at itself ends.
        let mut looping = chain(&[0], true, false);
        let end = looping.len() - 4;
        looping[end..].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(pages(&mut Cursor::new(looping)), Some(1));
    }
}
