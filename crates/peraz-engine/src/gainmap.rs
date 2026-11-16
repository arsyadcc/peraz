//! HDR gain maps in JPEG files.
//!
//! An HDR photo as phones store it in a JPEG is two pictures: the standard one, which every
//! program shows, and after its end a second JPEG, the gain map, which tells an HDR screen how
//! much brighter each part may be. They are tied together by an MPF segment in the first
//! picture, which says where the second one starts, and described by metadata in the gain map
//! itself: Ultra HDR's `hdrgm` XMP (Android, Adobe), Apple's `HDRGainMap` XMP, or an ISO
//! 21496-1 segment. The first picture of an Ultra HDR file also names the format in its XMP.
//!
//! Keeping the gain map means giving it the same turn and the same scaling as the picture,
//! encoding it again with its metadata, and writing the MPF segment for the new sizes.

use image::DynamicImage;
use image::metadata::Orientation;

use crate::gain_params::{CommonMap, Parameters};

const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const MPF_HEADER: &[u8] = b"MPF\0";
const ISO_HEADER: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";

/// The gain map of a JPEG, as it is stored after the main picture.
#[derive(Debug, Clone, PartialEq)]
pub struct JpegGainMap {
    /// The gain map's own JPEG file. For a gain map that came from another format, only its
    /// description: the segments that say how it is applied.
    pub jpeg: Vec<u8>,
    /// The gain map's pixels, upright, when it came from another format.
    pub pixels: Option<DynamicImage>,
    /// How the main picture is stored turned; the gain map is stored the same way.
    pub orientation: Orientation,
    /// The main picture's size, upright.
    pub picture_size: (u32, u32),
    /// The main picture's XMP names the Ultra HDR format (`hdrgm:Version`), where readers of
    /// that format look for it.
    pub names_ultra_hdr: bool,
    /// The main picture's ISO 21496-1 segment (the standard's version), if it has one.
    pub iso_version: Option<Vec<u8>>,
}

/// One segment before a JPEG's picture data: its marker and its content.
struct Segment<'a> {
    marker: u8,
    body: &'a [u8],
}

/// The segments of a JPEG up to its picture data, and where that data starts.
fn segments(jpeg: &[u8]) -> Option<(Vec<Segment<'_>>, usize)> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut found = Vec::new();
    let mut at = 2;
    loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        let marker = *jpeg.get(at + 1)?;
        // The picture data starts here, or the file ends.
        if marker == 0xDA || marker == 0xD9 {
            return Some((found, at));
        }
        let length = usize::from(u16::from_be_bytes([*jpeg.get(at + 2)?, *jpeg.get(at + 3)?]));
        let body = jpeg.get(at + 4..at + 2 + length)?;
        found.push(Segment { marker, body });
        at += 2 + length;
    }
}

fn contains(text: &[u8], mark: &[u8]) -> bool {
    text.windows(mark.len()).any(|window| window == mark)
}

/// The gain map stored after a JPEG's main picture, with what the main picture says about it.
/// `None` when there is none. The orientation and the picture's size are for the caller to
/// fill in: they come from decoding the main picture.
#[must_use]
pub fn of_jpeg(jpeg: &[u8]) -> Option<JpegGainMap> {
    let end = crate::metadata::jpeg_end(jpeg)?;
    let (own, _) = segments(jpeg)?;
    // The pictures after the main one: the gain map is the one whose metadata says so.
    let mut at = end;
    let map = loop {
        let rest = jpeg.get(at..)?;
        let start = rest.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])?;
        let candidate = &rest[start..];
        let length = crate::metadata::jpeg_end(candidate).unwrap_or(candidate.len());
        let candidate = &candidate[..length];
        let (described, _) = segments(candidate)?;
        let is_map = described.iter().any(|segment| match segment.marker {
            0xE1 => {
                segment.body.starts_with(XMP_HEADER)
                    && (contains(segment.body, b"hdrgm:") || contains(segment.body, b"HDRGainMap"))
            }
            0xE2 => segment.body.starts_with(ISO_HEADER),
            _ => false,
        });
        if is_map {
            break candidate;
        }
        at += start + length.max(3);
    };
    Some(JpegGainMap {
        jpeg: map.to_vec(),
        pixels: None,
        orientation: Orientation::NoTransforms,
        picture_size: (0, 0),
        names_ultra_hdr: own.iter().any(|segment| {
            segment.marker == 0xE1
                && segment.body.starts_with(XMP_HEADER)
                && contains(segment.body, b"hdrgm:Version")
        }),
        iso_version: own
            .iter()
            .find(|segment| segment.marker == 0xE2 && segment.body.starts_with(ISO_HEADER))
            .map(|segment| segment.body.to_vec()),
    })
}

impl JpegGainMap {
    /// The gain map's pixels for a picture that is now `size` (upright): turned the way the
    /// picture was, and scaled by the same factor. A gain map is often stored smaller than its
    /// picture; it keeps that proportion.
    pub fn pixels_for(&self, size: (u32, u32)) -> Result<DynamicImage, String> {
        let mut map = match &self.pixels {
            Some(pixels) => pixels.clone(),
            None => image::load_from_memory_with_format(&self.jpeg, image::ImageFormat::Jpeg)
                .map_err(|e| format!("cannot read the HDR gain map: {e}"))?,
        };
        map.apply_orientation(self.orientation);
        let scaled = |length: u32, now: u32, was: u32| {
            let pixels = u64::from(length) * u64::from(now) / u64::from(was.max(1));
            u32::try_from(pixels).unwrap_or(u32::MAX).max(1)
        };
        let wanted = (
            scaled(map.width(), size.0, self.picture_size.0),
            scaled(map.height(), size.1, self.picture_size.1),
        );
        if wanted != (map.width(), map.height()) {
            map = map.resize_exact(wanted.0, wanted.1, image::imageops::FilterType::Lanczos3);
        }
        Ok(map)
    }

    /// The gain map in the common form, for another format. `exif` is the main picture's
    /// EXIF, where an older iPhone photo says how bright its HDR rendition goes. `None` when
    /// the file does not say how the gain map is applied in a way that is understood.
    #[must_use]
    pub fn common(&self, exif: Option<&[u8]>) -> Option<CommonMap> {
        let pixels = self.pixels_for(self.picture_size).ok()?;
        let described = self.description();
        let body_of = |marker: u8, header: &[u8]| {
            described
                .iter()
                .find(|(found, body)| *found == marker && body.starts_with(header))
                .map(|(_, body)| &body[header.len()..])
        };
        // The standard's own segment first, then the XMP attributes.
        let xmp = body_of(0xE1, XMP_HEADER);
        let parameters = body_of(0xE2, ISO_HEADER)
            .and_then(Parameters::of_iso)
            .or_else(|| xmp.and_then(Parameters::of_xmp));
        if let Some(parameters) = parameters {
            return Some(CommonMap {
                pixels: match pixels {
                    DynamicImage::ImageLuma8(_) => pixels,
                    other => DynamicImage::ImageRgb8(other.to_rgb8()),
                },
                picture_size: self.picture_size,
                parameters,
            });
        }
        // An older iPhone photo: only a headroom, in the maker note.
        let headroom = xmp
            .filter(|xmp| contains(xmp, b"HDRGainMap"))
            .and_then(|_| crate::exif::apple_headroom(exif?))?;
        Some(CommonMap::of_apple(&pixels, self.picture_size, headroom))
    }

    /// A gain map from another format, as a JPEG's: described by XMP attributes (Ultra HDR)
    /// and by the standard's segment, for readers of either.
    #[must_use]
    pub fn of_common(map: &CommonMap) -> Self {
        let xmp = [XMP_HEADER, map.parameters.xmp().as_bytes()].concat();
        let iso = [ISO_HEADER, &map.parameters.iso()[..]].concat();
        // The description alone, in the shape of a JPEG: its segments between the markers
        // for the start and the end of a picture.
        let mut jpeg = vec![0xFF, 0xD8];
        for (marker, body) in [(0xE1, &xmp), (0xE2, &iso)] {
            jpeg.extend([0xFF, marker]);
            jpeg.extend(
                u16::try_from(body.len() + 2)
                    .unwrap_or(u16::MAX)
                    .to_be_bytes(),
            );
            jpeg.extend_from_slice(body);
        }
        jpeg.extend([0xFF, 0xD9]);
        Self {
            jpeg,
            pixels: Some(map.pixels.clone()),
            orientation: Orientation::NoTransforms,
            picture_size: map.picture_size,
            names_ultra_hdr: true,
            // In the main picture, the segment holds the standard's version alone.
            iso_version: Some([ISO_HEADER, &[0; 4]].concat()),
        }
    }

    /// The metadata segments of the stored gain map that describe it: its XMP, and its
    /// segments for a color profile and for ISO 21496-1. Each as marker and content.
    fn description(&self) -> Vec<(u8, &[u8])> {
        let Some((found, _)) = segments(&self.jpeg) else {
            return Vec::new();
        };
        found
            .into_iter()
            .filter(|segment| match segment.marker {
                0xE1 => segment.body.starts_with(XMP_HEADER),
                0xE2 => !segment.body.starts_with(MPF_HEADER),
                _ => false,
            })
            .map(|segment| (segment.marker, segment.body))
            .collect()
    }

    /// A gain map picture encoded again (`encoded`), with the description of the stored one.
    #[must_use]
    pub fn described(&self, encoded: &[u8]) -> Vec<u8> {
        with_segments(encoded, &self.description())
    }

    /// The main picture's JPEG with the gain map after it, tied together the way readers
    /// expect: the format's name in the picture's XMP (added when the picture has no XMP that
    /// says it), the ISO 21496-1 segment it had, and an MPF segment with where the gain map
    /// starts.
    #[must_use]
    pub fn attach(&self, picture: &[u8], map: &[u8]) -> Vec<u8> {
        let mut added: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut picture = picture.to_vec();
        if self.names_ultra_hdr {
            match length_in_directory(&picture) {
                // The picture kept its own XMP: its directory gets the gain map's new size.
                Some(place) => {
                    picture = with_length(&picture, &place, map.len());
                }
                None if !has_xmp_with(&picture, b"hdrgm:Version") => {
                    added.push((
                        0xE1,
                        [XMP_HEADER, ultra_hdr_xmp(map.len()).as_bytes()].concat(),
                    ));
                }
                None => {}
            }
        }
        if let Some(version) = &self.iso_version {
            added.push((0xE2, version.clone()));
        }
        // The MPF segment last: it is 90 bytes long whatever it holds, so the sizes it names
        // are known before it is written.
        let lead = leading_length(&picture);
        let before: usize = added.iter().map(|(_, body)| body.len() + 4).sum();
        let mpf_at = lead + before;
        let total = picture.len() + before + MPF_SEGMENT;
        // Offsets count from the byte order mark inside the segment.
        added.push((0xE2, mpf(total, map.len(), total - (mpf_at + 8))));
        let borrowed: Vec<(u8, &[u8])> = added
            .iter()
            .map(|(marker, body)| (*marker, body.as_slice()))
            .collect();
        let mut out = with_segments(&picture, &borrowed);
        out.extend_from_slice(map);
        out
    }
}

/// The length of a whole MPF segment for two pictures, with its marker.
const MPF_SEGMENT: usize = 90;

/// The content of an MPF segment for two pictures: the first is `first` bytes long, the second
/// `second` bytes and starts `offset` bytes after the segment's byte order mark.
fn mpf(first: usize, second: usize, offset: usize) -> Vec<u8> {
    let long = |value: usize| u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes();
    let mut out = MPF_HEADER.to_vec();
    out.extend(b"MM\0\x2A\0\0\0\x08"); // big-endian, the directory follows
    out.extend([0, 3]); // three entries
    out.extend([0xB0, 0x00, 0, 7, 0, 0, 0, 4]); // the MPF version: four bytes,
    out.extend(b"0100");
    out.extend([0xB0, 0x01, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2]); // two pictures,
    out.extend([0xB0, 0x02, 0, 7, 0, 0, 0, 32, 0, 0, 0, 50]); // their entries, at 50
    out.extend([0; 4]); // no further directory
    // The first picture: the one to show, a "baseline MP primary image".
    out.extend([0x20, 0x03, 0, 0]);
    out.extend(long(first));
    out.extend([0; 8]);
    // The second: no type of its own.
    out.extend([0; 4]);
    out.extend(long(second));
    out.extend(long(offset));
    out.extend([0; 4]);
    out
}

/// How many bytes at the start of a JPEG are its start marker and its JFIF header: new
/// segments go after them.
fn leading_length(jpeg: &[u8]) -> usize {
    let mut at = 2;
    if jpeg.get(2..4) == Some(&[0xFF, 0xE0])
        && let Some(length) = jpeg.get(4..6)
    {
        at += 2 + usize::from(u16::from_be_bytes([length[0], length[1]]));
    }
    at.min(jpeg.len())
}

/// A JPEG with more segments after its start marker and JFIF header.
fn with_segments(jpeg: &[u8], segments: &[(u8, &[u8])]) -> Vec<u8> {
    let at = leading_length(jpeg);
    let mut out = jpeg[..at].to_vec();
    for (marker, body) in segments {
        // A segment holds at most 65533 bytes; a larger one is left out rather than cut.
        let Ok(length) = u16::try_from(body.len() + 2) else {
            continue;
        };
        out.extend([0xFF, *marker]);
        out.extend(length.to_be_bytes());
        out.extend_from_slice(body);
    }
    out.extend_from_slice(&jpeg[at..]);
    out
}

fn has_xmp_with(jpeg: &[u8], mark: &[u8]) -> bool {
    segments(jpeg).is_some_and(|(found, _)| {
        found.iter().any(|segment| {
            segment.marker == 0xE1
                && segment.body.starts_with(XMP_HEADER)
                && contains(segment.body, mark)
        })
    })
}

/// Where a JPEG's XMP states the length of its gain map: the digits of `Item:Length` in the
/// directory entry whose `Item:Semantic` is `GainMap`, as a range of the file.
fn length_in_directory(jpeg: &[u8]) -> Option<std::ops::Range<usize>> {
    let mark = b"Item:Semantic=\"GainMap\"";
    let at = jpeg.windows(mark.len()).position(|w| w == mark)?;
    // The entry is one XML tag: from the `<` before to the `>` after.
    let start = jpeg[..at].iter().rposition(|b| *b == b'<')?;
    let end = at + jpeg[at..].iter().position(|b| *b == b'>')?;
    let name = b"Item:Length=\"";
    let digits = start
        + jpeg[start..end]
            .windows(name.len())
            .position(|w| w == name)?
        + name.len();
    let length = jpeg[digits..end].iter().position(|b| *b == b'"')?;
    Some(digits..digits + length)
}

/// The JPEG with another number at `place` (found by [`length_in_directory`]), and the length
/// of the XMP segment around it put right.
fn with_length(jpeg: &[u8], place: &std::ops::Range<usize>, length: usize) -> Vec<u8> {
    let digits = length.to_string();
    let Some((start, old)) = segment_around(jpeg, place.start) else {
        return jpeg.to_vec();
    };
    let new = old + digits.len() - place.len();
    let Ok(new) = u16::try_from(new) else {
        return jpeg.to_vec();
    };
    let mut out = jpeg[..place.start].to_vec();
    out.extend(digits.as_bytes());
    out.extend_from_slice(&jpeg[place.end..]);
    out[start + 2..start + 4].copy_from_slice(&new.to_be_bytes());
    out
}

/// The segment that holds the byte at `at`: where its marker is, and the length it states.
fn segment_around(jpeg: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut start = 2;
    loop {
        if *jpeg.get(start)? != 0xFF || matches!(*jpeg.get(start + 1)?, 0xDA | 0xD9) {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([
            *jpeg.get(start + 2)?,
            *jpeg.get(start + 3)?,
        ]));
        if (start..start + 2 + length).contains(&at) {
            return Some((start, length));
        }
        start += 2 + length;
    }
}

/// The XMP of the main picture of an Ultra HDR file: the format's version, and a directory of
/// the two pictures with the length of the gain map.
fn ultra_hdr_xmp(map_length: usize) -> String {
    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"\" \
         xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" \
         xmlns:Container=\"http://ns.google.com/photos/1.0/container/\" \
         xmlns:Item=\"http://ns.google.com/photos/1.0/container/item/\" \
         hdrgm:Version=\"1.0\">\
         <Container:Directory><rdf:Seq>\
         <rdf:li rdf:parseType=\"Resource\">\
         <Container:Item Item:Semantic=\"Primary\" Item:Mime=\"image/jpeg\"/></rdf:li>\
         <rdf:li rdf:parseType=\"Resource\">\
         <Container:Item Item:Semantic=\"GainMap\" Item:Mime=\"image/jpeg\" \
         Item:Length=\"{map_length}\"/></rdf:li>\
         </rdf:Seq></Container:Directory>\
         </rdf:Description></rdf:RDF></x:xmpmeta>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gain_map_from_another_format_is_described_both_ways() {
        let common = CommonMap {
            pixels: DynamicImage::ImageLuma8(image::GrayImage::from_fn(40, 30, |x, _| {
                image::Luma([u8::try_from(x * 6).unwrap()])
            })),
            picture_size: (80, 60),
            parameters: Parameters::up_to(4.0),
        };
        let map = JpegGainMap::of_common(&common);
        // Its description: the XMP attributes, and the standard's segment.
        let described: Vec<u8> = map
            .description()
            .iter()
            .map(|(marker, _)| *marker)
            .collect();
        assert_eq!(described, [0xE1, 0xE2]);
        assert!(map.names_ultra_hdr);
        assert_eq!(map.iso_version, Some([ISO_HEADER, &[0; 4]].concat()));
        // The pixels are the ones given, scaled with the picture.
        let half = map.pixels_for((40, 30)).unwrap();
        assert_eq!((half.width(), half.height()), (20, 15));
        // Read back, it is the same gain map; from either description alone, too.
        let back = map.common(None).unwrap();
        assert_eq!(back.parameters, common.parameters);
        assert_eq!(back.picture_size, (80, 60));
        assert_eq!(back.pixels.to_luma8(), common.pixels.to_luma8());
        let only = |kept: u8| {
            let mut jpeg = vec![0xFF, 0xD8];
            for (marker, body) in map.description() {
                if marker == kept {
                    jpeg.extend([0xFF, marker]);
                    jpeg.extend(u16::try_from(body.len() + 2).unwrap().to_be_bytes());
                    jpeg.extend_from_slice(body);
                }
            }
            jpeg.extend([0xFF, 0xD9]);
            JpegGainMap {
                jpeg,
                ..map.clone()
            }
        };
        assert_eq!(
            only(0xE1).common(None).unwrap().parameters,
            common.parameters
        );
        assert_eq!(
            only(0xE2).common(None).unwrap().parameters,
            common.parameters
        );
        // Without either, and without an iPhone's headroom, there is nothing to go by.
        assert!(only(0).common(None).is_none());
    }

    /// A small JPEG with these segments (marker, content) before its picture.
    fn jpeg(gray: bool, segments: &[(u8, &[u8])]) -> Vec<u8> {
        let picture = if gray {
            DynamicImage::ImageLuma8(image::GrayImage::from_fn(24, 16, |x, y| {
                image::Luma([u8::try_from(x * 9 + y * 3).unwrap()])
            }))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_fn(48, 32, |x, y| {
                image::Rgb([
                    u8::try_from(x * 5).unwrap(),
                    u8::try_from(y * 7).unwrap(),
                    90,
                ])
            }))
        };
        let mut plain = Vec::new();
        picture
            .write_to(
                &mut std::io::Cursor::new(&mut plain),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        with_segments(&plain, segments)
    }

    fn map_xmp() -> Vec<u8> {
        [
            XMP_HEADER,
            b"<x:xmpmeta hdrgm:Version=\"1.0\" hdrgm:GainMapMax=\"2.3\"/>",
        ]
        .concat()
    }

    /// The second picture's entry in a file's MPF segment: where it starts and how long it is.
    fn second_picture(file: &[u8]) -> (usize, usize) {
        let (found, _) = segments(file).unwrap();
        let mpf = found
            .iter()
            .find(|segment| segment.marker == 0xE2 && segment.body.starts_with(MPF_HEADER))
            .expect("an MPF segment");
        let base = mpf.body.as_ptr() as usize - file.as_ptr() as usize + 4;
        let number =
            |at: usize| u32::from_be_bytes(mpf.body[at..at + 4].try_into().unwrap()) as usize;
        // Entries start 50 bytes after the byte order mark; each is 16 bytes long.
        assert_eq!(
            number(4 + 50 + 4),
            file.len() - number(4 + 66 + 4),
            "the first size"
        );
        (base + number(4 + 66 + 8), number(4 + 66 + 4))
    }

    #[test]
    fn a_gain_map_is_found_with_what_the_picture_says_about_it() {
        let map = jpeg(true, &[(0xE1, &map_xmp())]);
        let version = [ISO_HEADER, &[0, 0, 0, 0]].concat();
        let own_xmp = [XMP_HEADER, b"<x:xmpmeta hdrgm:Version=\"1.0\"/>"].concat();
        let picture = jpeg(false, &[(0xE1, &own_xmp), (0xE2, &version)]);
        let found = of_jpeg(&[picture.clone(), map.clone()].concat()).unwrap();
        assert_eq!(found.jpeg, map);
        assert!(found.names_ultra_hdr);
        assert_eq!(found.iso_version, Some(version));
        // Another picture between the two (a preview) is passed over.
        let preview = jpeg(false, &[]);
        let found = of_jpeg(&[picture.clone(), preview.clone(), map.clone()].concat()).unwrap();
        assert_eq!(found.jpeg, map);
        // Without a gain map: a picture alone, and one followed by a picture that is none.
        assert_eq!(of_jpeg(&picture), None);
        assert_eq!(of_jpeg(&[picture, preview].concat()), None);
        assert_eq!(of_jpeg(b"not a JPEG"), None);
    }

    #[test]
    fn the_gain_map_is_turned_and_scaled_with_the_picture() {
        let map = JpegGainMap {
            jpeg: jpeg(true, &[]),
            pixels: None,
            orientation: Orientation::Rotate90,
            picture_size: (64, 96),
            names_ultra_hdr: false,
            iso_version: None,
        };
        // Stored 24x16 for a picture stored 96x64: upright it is 16x24 for 64x96.
        let same = map.pixels_for((64, 96)).unwrap();
        assert_eq!((same.width(), same.height()), (16, 24));
        let half = map.pixels_for((32, 48)).unwrap();
        assert_eq!((half.width(), half.height()), (8, 12));
        assert_eq!(same.color(), image::ColorType::L8, "a gray map stays gray");
    }

    #[test]
    fn the_two_pictures_are_tied_together() {
        let stored = jpeg(true, &[(0xE1, &map_xmp()), (0xE0, b"JFIF\0")]);
        let map = JpegGainMap {
            jpeg: stored,
            pixels: None,
            orientation: Orientation::NoTransforms,
            picture_size: (48, 32),
            names_ultra_hdr: true,
            iso_version: Some([ISO_HEADER, &[0, 0, 0, 0]].concat()),
        };
        // The new gain map carries the stored one's description.
        let new_map = map.described(&jpeg(true, &[]));
        assert!(has_xmp_with(&new_map, b"hdrgm:GainMapMax=\"2.3\""));
        let picture = jpeg(false, &[]);
        let file = map.attach(&picture, &new_map);
        // The picture names the format and the gain map's length, and carries the ISO
        // segment; the MPF segment says where the gain map starts.
        let length = format!("Item:Length=\"{}\"", new_map.len());
        assert!(has_xmp_with(&file, b"hdrgm:Version=\"1.0\""));
        assert!(has_xmp_with(&file, length.as_bytes()));
        let (start, size) = second_picture(&file);
        assert_eq!(&file[start..], new_map.as_slice());
        assert_eq!(size, new_map.len());
        // Read back, it is the same gain map with the same notes.
        let again = of_jpeg(&file).unwrap();
        assert_eq!(again.jpeg, new_map);
        assert!(again.names_ultra_hdr && again.iso_version == map.iso_version);
        // The main picture still decodes, and ends where the gain map starts.
        assert_eq!(crate::metadata::jpeg_end(&file), Some(start));
        assert!(image::load_from_memory(&file).is_ok());
    }

    #[test]
    fn a_kept_directory_gets_the_new_length() {
        let own_xmp = [
            XMP_HEADER,
            b"<x:xmpmeta hdrgm:Version=\"1.0\"><Container:Item Item:Mime=\"image/jpeg\" \
              Item:Semantic=\"GainMap\" Item:Length=\"7\"/></x:xmpmeta>",
        ]
        .concat();
        let picture = jpeg(false, &[(0xE1, &own_xmp)]);
        let map = JpegGainMap {
            jpeg: jpeg(true, &[(0xE1, &map_xmp())]),
            pixels: None,
            orientation: Orientation::NoTransforms,
            picture_size: (48, 32),
            names_ultra_hdr: true,
            iso_version: None,
        };
        let new_map = map.described(&jpeg(true, &[]));
        let file = map.attach(&picture, &new_map);
        let length = format!("Item:Length=\"{}\"", new_map.len());
        assert!(
            has_xmp_with(&file, length.as_bytes()),
            "the directory is put right"
        );
        // One XMP segment still, and the file still reads.
        let (found, _) = segments(&file).unwrap();
        let packets = found
            .iter()
            .filter(|s| s.marker == 0xE1 && s.body.starts_with(XMP_HEADER));
        assert_eq!(packets.count(), 1);
        let (start, _) = second_picture(&file);
        assert_eq!(&file[start..], new_map.as_slice());
        assert!(image::load_from_memory(&file).is_ok());
    }
}
