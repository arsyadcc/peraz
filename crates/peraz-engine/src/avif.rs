//! AVIF output with an HDR gain map.
//!
//! The AV1 encoder writes a file with one picture and, for transparency, its alpha plane. An
//! HDR photo needs more: the gain map as a second picture that is not shown by itself, and an
//! item that stands for the HDR rendition made from the two (ISO 21496-1, as in HEIC). So the
//! encoder's file is taken apart, the gain map is encoded as a picture of its own, and the
//! pieces are written again with the HEIF writer that HEIC output uses.

use crate::heic::{Format, Item, MapPicture, Parts, Reference, boxed, size_box};
use crate::heif::{GainMap, coded_pictures};
use crate::metadata::Metadata;

const PRIMARY_ITEM: u16 = 1;
const ALPHA_ITEM: u16 = 3;

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
    let (color, alpha) =
        coded_pictures(encoded).ok_or("the AV1 encoder's file cannot be taken apart")?;
    let map = map_picture(map, size, map_quality)?;

    // Properties, numbered from 1 in this order.
    let mut properties: Vec<Vec<u8>> = Vec::new();
    let mut add = |property: Vec<u8>| {
        let essential = needs_understanding(&property);
        (crate::heic::listed(&mut properties, property), essential)
    };
    let mut items = Vec::new();
    let mut references: Vec<Reference> = Vec::new();
    let (shown_size, _) = add(size_box(size));
    let mut own: Vec<(u8, bool)> = color.properties.into_iter().map(&mut add).collect();
    own.extend(icc.map(|icc| add(boxed(*b"colr", &[b"prof", icc].concat()))));
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
    let group = crate::heic::gain_map_items(
        &map,
        (shown_size, PRIMARY_ITEM),
        &mut properties,
        &mut items,
        &mut references,
    );
    crate::heic::metadata_items(metadata, PRIMARY_ITEM, &mut items, &mut references);
    let parts = Parts {
        items: &items,
        primary: PRIMARY_ITEM,
        references: &references,
        groups: &[group],
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
    use image::{DynamicImage, GrayImage, Luma, Rgba, RgbaImage};

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
            description: ToneMap {
                map_item: 2,
                metadata: b"how the gain map is applied".to_vec(),
                rendition: vec![crate::heic::full_box(*b"pixi", 0, 0, &[3, 10, 10, 10])],
            },
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
            assert_eq!(tone_map.metadata, map.description.metadata);
            assert_eq!(tone_map.rendition, map.description.rendition);
            assert_eq!(tone_map.map_item, 7);
            let kept = read_metadata(&file);
            assert_eq!(kept.exif, metadata.exif);
            assert_eq!(kept.xmp, metadata.xmp);
        }
    }

    #[test]
    fn a_file_that_is_not_the_encoders_is_refused() {
        assert!(with_gain_map(b"not an AVIF", &gain_map(), (48, 32), 85, None, None).is_err());
    }
}
