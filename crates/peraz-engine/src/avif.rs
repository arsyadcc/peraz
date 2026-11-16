//! AVIF output for HDR pictures.
//!
//! The AV1 encoder writes a file with one picture and, for transparency, its alpha plane. An
//! HDR photo needs more: the gain map as a second picture that is not shown by itself, and an
//! item that stands for the HDR rendition made from the two (ISO 21496-1, as in HEIC). A
//! picture that is HDR by its transfer function needs its color description (PQ or HLG, and
//! its primaries), which the encoder cannot write. So the encoder's file is taken apart and
//! the pieces are written again, with what is missing, by the HEIF writer that HEIC output
//! uses.

use crate::heic::{Format, Item, MapPicture, Parts, Reference, boxed, size_box};
use image::DynamicImage;

use crate::hdr::HdrColor;
use crate::heif::{GainMap, MapDescription, coded_pictures};
use crate::metadata::Metadata;

const PRIMARY_ITEM: u16 = 1;
const ALPHA_ITEM: u16 = 3;

/// What goes into the file besides what the encoder wrote.
#[derive(Default)]
struct Extras<'a> {
    icc: Option<&'a [u8]>,
    metadata: Option<&'a Metadata>,
    gain_map: Option<MapPicture<'a>>,
    /// More properties of the primary picture, as whole boxes (its color description).
    described: Vec<Vec<u8>>,
}

/// The AVIF for a picture that the encoder wrote as `encoded`, with the gain map of its input:
/// scaled like the picture, which is now `size`. The color profile and the metadata are
/// written too. `map_quality` is the encoder's setting for the gain map.
pub fn with_gain_map(
    encoded: &[u8],
    map: &GainMap,
    size: (u32, u32),
    map_quality: u8,
    icc: Option<&[u8]>,
    metadata: Option<&Metadata>,
) -> Result<Vec<u8>, String> {
    // Only the standard's gain map has a place in an AVIF.
    if !matches!(map.description, MapDescription::Iso(_)) {
        return Err("an AVIF cannot hold a gain map stored as an auxiliary picture".to_owned());
    }
    let extras = Extras {
        icc,
        metadata,
        gain_map: Some(map_picture(map, size, map_quality)?),
        described: Vec::new(),
    };
    assemble(encoded, size, extras)
}

/// A picture that is HDR by its transfer function, as an AVIF with 10 bits per channel: its
/// values are stored as they are, as brightness and two color differences, and the file says
/// what they mean (the primaries, PQ or HLG, and the light levels when they are known).
pub fn encode_hdr(
    image: &DynamicImage,
    color: HdrColor,
    quality: u8,
    metadata: Option<&Metadata>,
) -> Result<Vec<u8>, String> {
    let (matrix, red, blue) = color.matrix();
    let green = 1.0 - red - blue;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ten_bits = |value: f32| (value * 1023.0).round().clamp(0.0, 1023.0) as u16;
    let pixels = image.to_rgba16();
    let planes = pixels.pixels().map(|pixel| {
        let [r, g, b] = [0, 1, 2].map(|c| f32::from(pixel[c]) / f32::from(u16::MAX));
        let brightness = red * r + green * g + blue * b;
        [
            ten_bits(brightness),
            ten_bits((b - brightness) / (2.0 * (1.0 - blue)) + 0.5),
            ten_bits((r - brightness) / (2.0 * (1.0 - red)) + 0.5),
        ]
    });
    let has_alpha = pixels.pixels().any(|pixel| pixel[3] < u16::MAX);
    let alpha = has_alpha.then(|| pixels.pixels().map(|pixel| pixel[3] >> 6));
    let matrix_code = if matrix == 9 {
        ravif::MatrixCoefficients::BT2020NCL
    } else {
        ravif::MatrixCoefficients::BT709
    };
    let encoded = ravif::Encoder::new()
        .with_quality(f32::from(quality))
        .with_alpha_quality(f32::from(quality.saturating_add(10).min(100)))
        .with_speed(6)
        .encode_raw_planes_10_bit(
            pixels.width() as usize,
            pixels.height() as usize,
            planes,
            alpha,
            ravif::PixelRange::Full,
            matrix_code,
        )
        .map_err(|e| format!("rav1e: {e}"))?;
    let mut coding = b"nclx".to_vec();
    coding.extend(u16::from(color.primaries).to_be_bytes());
    coding.extend(u16::from(color.transfer).to_be_bytes());
    coding.extend(u16::from(matrix).to_be_bytes());
    coding.push(0x80); // full range
    let mut described = vec![boxed(*b"colr", &coding)];
    if let Some((brightest, average)) = color.light_level {
        let levels = [brightest.to_be_bytes(), average.to_be_bytes()].concat();
        described.push(boxed(*b"clli", &levels));
    }
    let extras = Extras {
        metadata,
        described,
        ..Extras::default()
    };
    let size = (pixels.width(), pixels.height());
    let mut file = assemble(&encoded.avif_file, size, extras)?;
    // The stream itself says sRGB: set the same code points there.
    let layout = coded_pictures(&file).ok_or("the AVIF just written cannot be read back")?;
    let at = find(&file, &layout.0.data).ok_or("the picture is not where the file says")?;
    crate::av1::describe_color(
        &mut file[at..at + layout.0.data.len()],
        color.primaries,
        color.transfer,
    );
    Ok(file)
}

/// Where `part` starts in `whole`.
fn find(whole: &[u8], part: &[u8]) -> Option<usize> {
    whole.windows(part.len().max(1)).position(|w| w == part)
}

/// Writes the file: the encoder's picture and alpha plane, and what `extras` adds.
fn assemble(encoded: &[u8], size: (u32, u32), extras: Extras) -> Result<Vec<u8>, String> {
    let (color, alpha) =
        coded_pictures(encoded).ok_or("the AV1 encoder's file cannot be taken apart")?;

    // Properties, numbered from 1 in this order.
    let mut properties: Vec<Vec<u8>> = Vec::new();
    let mut add = |property: Vec<u8>| {
        let essential = needs_understanding(&property);
        (crate::heic::listed(&mut properties, property), essential)
    };
    let mut items = Vec::new();
    let mut references: Vec<Reference> = Vec::new();
    let (shown_size, _) = add(size_box(size));
    // A color description given here takes the place of the encoder's, which says sRGB.
    let is_coding = |property: &Vec<u8>| property.get(4..12) == Some(b"colrnclx");
    let replaced = extras.described.iter().any(is_coding);
    let mut own: Vec<(u8, bool)> = color
        .properties
        .into_iter()
        .filter(|property| !(replaced && is_coding(property)))
        .map(&mut add)
        .collect();
    own.extend(
        extras
            .icc
            .map(|icc| add(boxed(*b"colr", &[b"prof", icc].concat()))),
    );
    own.extend(extras.described.into_iter().map(&mut add));
    items.push(Item {
        id: PRIMARY_ITEM,
        kind: *b"av01",
        hidden: false,
        data: color.data,
        properties: own,
        content_type: None,
    });
    if let Some(alpha) = alpha {
        items.push(Item {
            id: ALPHA_ITEM,
            kind: *b"av01",
            hidden: false,
            data: alpha.data,
            properties: alpha.properties.into_iter().map(&mut add).collect(),
            content_type: None,
        });
        // The alpha picture is an auxiliary picture of the primary one.
        references.push((*b"auxl", ALPHA_ITEM, vec![PRIMARY_ITEM]));
    }
    let groups: Vec<_> = extras
        .gain_map
        .and_then(|map| {
            crate::heic::gain_map_items(
                &map,
                (shown_size, PRIMARY_ITEM),
                &mut properties,
                &mut items,
                &mut references,
            )
        })
        .into_iter()
        .collect();
    crate::heic::metadata_items(extras.metadata, PRIMARY_ITEM, &mut items, &mut references);
    let parts = Parts {
        items: &items,
        primary: PRIMARY_ITEM,
        references: &references,
        groups: &groups,
        properties: &properties,
    };
    crate::heic::write_file(&parts, Format::Avif)
}

/// A reader that does not know this property must not show the item: the decoder's
/// configuration.
fn needs_understanding(property: &[u8]) -> bool {
    property.get(4..8) == Some(b"av1C")
}

/// The gain map for a picture that is now `size`: scaled by the same factor as the picture,
/// and encoded as an AV1 picture of its own with 8 bits per channel.
fn map_picture(map: &GainMap, size: (u32, u32), quality: u8) -> Result<MapPicture<'_>, String> {
    // A gray gain map is coded as a color picture whose channels are equal: its color planes
    // are flat and cost next to nothing.
    let pixels = map.pixels_for(size, false).to_rgb8();
    let (width, height) = (pixels.width() as usize, pixels.height() as usize);
    let buffer: Vec<ravif::RGB8> = pixels
        .pixels()
        .map(|p| ravif::RGB8::new(p[0], p[1], p[2]))
        .collect();
    let encoded = ravif::Encoder::new()
        .with_quality(f32::from(quality))
        .with_bit_depth(ravif::BitDepth::Eight)
        .with_speed(6)
        .encode_rgb(ravif::Img::new(buffer.as_slice(), width, height))
        .map_err(|e| format!("rav1e: {e}"))?;
    let (coded, _) = coded_pictures(&encoded.avif_file)
        .ok_or("the AV1 encoder's file for the gain map cannot be taken apart")?;
    Ok(MapPicture {
        kind: *b"av01",
        data: coded.data,
        properties: coded
            .properties
            .into_iter()
            .map(|property| {
                let essential = needs_understanding(&property);
                (property, essential)
            })
            .collect(),
        description: &map.description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heif::{ToneMap, read_metadata, read_properties};
    use image::{GrayImage, Luma, Rgba, RgbaImage};

    fn picture(transparent: bool) -> RgbaImage {
        RgbaImage::from_fn(48, 32, |x, y| {
            let alpha = if transparent && x < 8 { 0 } else { 255 };
            let (red, green) = (u8::try_from(x * 5).unwrap(), u8::try_from(y * 7).unwrap());
            Rgba([red, green, 128, alpha])
        })
    }

    fn gain_map() -> GainMap {
        GainMap {
            pixels: DynamicImage::ImageLuma8(GrayImage::from_fn(24, 16, |x, _| {
                Luma([u8::try_from(x * 10).unwrap()])
            })),
            picture_size: (48, 32),
            description: MapDescription::Iso(ToneMap {
                map_item: 2,
                metadata: b"how the gain map is applied".to_vec(),
                rendition: vec![crate::heic::full_box(*b"pixi", 0, 0, &[3, 10, 10, 10])],
            }),
        }
    }

    #[test]
    fn the_file_holds_the_picture_its_alpha_plane_and_the_gain_map() {
        let map = gain_map();
        let metadata = Metadata {
            exif: Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec()),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            photoshop: None,
        };
        for transparent in [false, true] {
            let encoded = crate::image::encode_avif(&picture(transparent), 60).unwrap();
            let (color, alpha) = coded_pictures(&encoded).unwrap();
            assert_eq!(alpha.is_some(), transparent);
            let file = with_gain_map(
                &encoded,
                &map,
                (48, 32),
                85,
                Some(b"a color profile"),
                Some(&metadata),
            )
            .unwrap();
            assert_eq!(&file[4..12], b"ftypavif");
            assert!(file[..40].windows(4).any(|w| w == b"tmap"));
            // Every box size adds up to the file.
            let mut at = 0;
            while at < file.len() {
                let size = u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]);
                at += size as usize;
            }
            assert_eq!(at, file.len());
            // The picture and its alpha plane are the encoder's, unchanged.
            let (written, written_alpha) = coded_pictures(&file).unwrap();
            assert_eq!(written.data, color.data);
            assert_eq!(written_alpha.map(|a| a.data), alpha.map(|a| a.data));
            let properties = read_properties(&file);
            assert_eq!(properties.primary, Some(1));
            assert_eq!(properties.alpha, transparent.then_some(3));
            assert_eq!(properties.icc.as_deref(), Some(&b"a color profile"[..]));
            // The gain map is described as it was, and points at its picture.
            let tone_map = properties.tone_map.expect("the gain map");
            assert_eq!(tone_map.map_item, 7);
            let written = ToneMap {
                map_item: 2,
                ..tone_map
            };
            assert_eq!(MapDescription::Iso(written), map.description);
            let kept = read_metadata(&file);
            assert_eq!(kept.exif, metadata.exif);
            assert_eq!(kept.xmp, metadata.xmp);
        }
    }

    #[test]
    fn an_hdr_picture_is_written_at_ten_bits_with_what_its_values_mean() {
        use crate::hdr::{HLG, HdrColor, PQ};
        let deep = |transparent: bool| {
            DynamicImage::ImageRgba16(image::ImageBuffer::from_fn(48, 32, |x, y| {
                let value = |v: u32| u16::try_from(v * 1000).unwrap();
                let alpha = if transparent && x < 8 { 0 } else { u16::MAX };
                image::Rgba([value(x), value(y), 30_000, alpha])
            }))
        };
        let pq = HdrColor {
            primaries: 9,
            transfer: PQ,
            light_level: Some((1000, 180)),
        };
        let hlg = HdrColor {
            primaries: 1,
            transfer: HLG,
            light_level: None,
        };
        for (color, matrix, transparent) in [(pq, 9, false), (hlg, 1, true)] {
            let file = encode_hdr(&deep(transparent), color, 80, None).unwrap();
            assert_eq!(&file[4..12], b"ftypavif");
            // The container says what the values mean, once, and the stream says the same.
            let properties = read_properties(&file);
            assert_eq!(properties.hdr, Some(color));
            assert_eq!(properties.alpha, transparent.then_some(3));
            let (coded, _) = coded_pictures(&file).unwrap();
            let codings = coded
                .properties
                .iter()
                .filter(|property| property.get(4..12) == Some(b"colrnclx"))
                .count();
            assert_eq!(codings, 1);
            // Ten bits in each of three channels.
            let depth = boxed(*b"pixi", &[0, 0, 0, 0, 3, 10, 10, 10]);
            assert!(coded.properties.contains(&depth));
            assert_eq!(
                crate::av1::color_description(&coded.data),
                Some([color.primaries, color.transfer, matrix])
            );
        }
    }

    #[test]
    fn a_file_that_is_not_the_encoders_is_refused() {
        assert!(with_gain_map(b"not an AVIF", &gain_map(), (48, 32), 85, None, None).is_err());
    }
}
