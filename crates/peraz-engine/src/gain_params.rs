//! How a gain map is applied, in the one form every layout can be written from.
//!
//! A gain map says, pixel by pixel, how much brighter the HDR rendition of a photo is than
//! its standard picture. How its stored values become gains is a handful of numbers, which
//! ISO 21496-1 defines: the smallest and the largest gain (as powers of two), a gamma, an
//! offset for each of the two renditions, and how much brighter than standard white each
//! rendition may go (its headroom, also a power of two). HEIC, AVIF, and JPEG XL store them
//! in the standard's binary form. A JPEG stores them as XMP attributes (Ultra HDR, with
//! names from Adobe's gain map specification) and sometimes in the binary form too. An
//! iPhone's older photos store a single number, the headroom, and apply the gain map by a
//! rule of their own.

use std::fmt::Write as _;

use image::DynamicImage;

/// The XML namespace of the gain map attributes in XMP.
const XMP_NAMESPACE: &str = "http://ns.adobe.com/hdr-gain-map/1.0/";

/// What a fraction's value is counted in when it is made from a decimal number.
const PARTS: u32 = 1_000_000;

/// The numbers for one channel of a gain map, as fractions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    /// The gain where the gain map is 0, as a power of two.
    pub min: (i32, u32),
    /// The gain where the gain map is 1, as a power of two.
    pub max: (i32, u32),
    /// The gain map's values are raised to the inverse of this before they are used.
    pub gamma: (u32, u32),
    /// Added to the standard picture's light before the gain is applied.
    pub base_offset: (i32, u32),
    /// Taken from the result.
    pub alternate_offset: (i32, u32),
}

/// How a gain map is applied (ISO 21496-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameters {
    /// The headroom of the picture the file shows by itself, as a power of two: 0 for a
    /// standard picture.
    pub base_headroom: (u32, u32),
    /// The headroom of the rendition the gain map leads to.
    pub alternate_headroom: (u32, u32),
    /// One channel for a gray gain map, or three.
    pub channels: Vec<Channel>,
    /// The gain is applied in the color space of the picture, not of the other rendition.
    pub use_base_color_space: bool,
}

fn fraction(value: f64) -> (i32, u32) {
    #[allow(clippy::cast_possible_truncation)]
    let parts = (value * f64::from(PARTS)).round() as i32;
    (parts, PARTS)
}

fn positive(value: f64) -> (u32, u32) {
    let (parts, of) = fraction(value.max(0.0));
    (parts.unsigned_abs(), of)
}

fn decimal((parts, of): (i32, u32)) -> f64 {
    f64::from(parts) / f64::from(of.max(1))
}

impl Parameters {
    /// Reads the standard's binary form: two version numbers, flags, the two headrooms,
    /// then five fractions for each channel.
    #[must_use]
    pub fn of_iso(bytes: &[u8]) -> Option<Self> {
        let number = |at: usize| -> Option<u32> {
            let b = bytes.get(at..at + 4)?;
            Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        // The version a reader must know: only the first is defined.
        if bytes.get(..2)? != [0, 0] {
            return None;
        }
        let flags = *bytes.get(4)?;
        let channels = if flags & 0x80 == 0 { 1 } else { 3 };
        let signed = |at: usize| Some((number(at)?.cast_signed(), number(at + 4)?));
        let unsigned = |at: usize| Some((number(at)?, number(at + 4)?));
        let channel = |index: usize| {
            let at = 21 + index * 40;
            Some(Channel {
                min: signed(at)?,
                max: signed(at + 8)?,
                gamma: unsigned(at + 16)?,
                base_offset: signed(at + 24)?,
                alternate_offset: signed(at + 32)?,
            })
        };
        Some(Self {
            base_headroom: unsigned(5)?,
            alternate_headroom: unsigned(13)?,
            channels: (0..channels).map(channel).collect::<Option<_>>()?,
            use_base_color_space: flags & 0x40 != 0,
        })
    }

    /// The standard's binary form.
    #[must_use]
    pub fn iso(&self) -> Vec<u8> {
        let mut flags = 0u8;
        if self.channels.len() > 1 {
            flags |= 0x80;
        }
        if self.use_base_color_space {
            flags |= 0x40;
        }
        let mut out = vec![0, 0, 0, 0, flags];
        let mut put = |(count, parts): (u32, u32)| {
            out.extend(count.to_be_bytes());
            out.extend(parts.to_be_bytes());
        };
        put(self.base_headroom);
        put(self.alternate_headroom);
        for channel in &self.channels {
            let signed = |(count, parts): (i32, u32)| (count.cast_unsigned(), parts);
            put(signed(channel.min));
            put(signed(channel.max));
            put(channel.gamma);
            put(signed(channel.base_offset));
            put(signed(channel.alternate_offset));
        }
        out
    }

    /// Reads the gain map attributes of an XMP packet (Ultra HDR, and what iPhones write
    /// with the same names). `None` when the packet has none, or when the picture the file
    /// shows is the HDR one, which is not handled.
    #[must_use]
    pub fn of_xmp(xmp: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(xmp).ok()?;
        // The prefix the packet uses for the namespace.
        let declared = text.find(&format!("=\"{XMP_NAMESPACE}\""))?;
        let prefix = text[..declared].rsplit("xmlns:").next()?;
        let values = |name: &str| values_of(text, prefix, name);
        let one = |names: &[&str]| names.iter().find_map(|name| values(name)?.first().copied());
        let base_is_hdr = text_of(text, prefix, "BaseRenditionIsHDR")
            .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1");
        if base_is_hdr {
            return None;
        }
        let max = values("GainMapMax")?;
        let count = max.len().min(3);
        // What is not given has a value the specification names.
        let or = |name: &str, default: f64| {
            let given = values(name).unwrap_or_default();
            (0..count)
                .map(|index| *given.get(index).or(given.first()).unwrap_or(&default))
                .collect::<Vec<f64>>()
        };
        let (min, gamma) = (or("GainMapMin", 0.0), or("Gamma", 1.0));
        let (base, alternate) = (or("OffsetSDR", 1.0 / 64.0), or("OffsetHDR", 1.0 / 64.0));
        let channels = (0..count)
            .map(|index| Channel {
                min: fraction(min[index]),
                max: fraction(max[index]),
                gamma: positive(gamma[index]),
                base_offset: fraction(base[index]),
                alternate_offset: fraction(alternate[index]),
            })
            .collect();
        Some(Self {
            base_headroom: positive(one(&["HDRCapacityMin", "BaseHeadroom"]).unwrap_or(0.0)),
            alternate_headroom: positive(one(&["HDRCapacityMax", "AlternateHeadroom"])?),
            channels,
            use_base_color_space: true,
        })
    }

    /// An XMP packet for a gain map picture that says the same, with the names of Ultra HDR.
    #[must_use]
    pub fn xmp(&self) -> String {
        let number = |value: f64| {
            let text = format!("{value:.6}");
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        };
        let unsigned = |(parts, of): (u32, u32)| f64::from(parts) / f64::from(of.max(1));
        let fields: [(&str, Vec<f64>); 5] = [
            ("GainMapMin", self.per_channel(|c| decimal(c.min))),
            ("GainMapMax", self.per_channel(|c| decimal(c.max))),
            ("Gamma", self.per_channel(|c| unsigned(c.gamma))),
            ("OffsetSDR", self.per_channel(|c| decimal(c.base_offset))),
            (
                "OffsetHDR",
                self.per_channel(|c| decimal(c.alternate_offset)),
            ),
        ];
        let mut attributes = format!(
            " hdrgm:Version=\"1.0\" hdrgm:BaseRenditionIsHDR=\"False\" \
             hdrgm:HDRCapacityMin=\"{}\" hdrgm:HDRCapacityMax=\"{}\"",
            number(unsigned(self.base_headroom)),
            number(unsigned(self.alternate_headroom)),
        );
        let mut elements = String::new();
        for (name, values) in fields {
            // One value for every channel is an attribute; three are a list.
            let written: Vec<String> = values.iter().map(|value| number(*value)).collect();
            if written.windows(2).all(|pair| pair[0] == pair[1]) {
                let _ = write!(attributes, " hdrgm:{name}=\"{}\"", written[0]);
            } else {
                let _ = write!(elements, "<hdrgm:{name}><rdf:Seq>");
                for value in written {
                    let _ = write!(elements, "<rdf:li>{value}</rdf:li>");
                }
                let _ = write!(elements, "</rdf:Seq></hdrgm:{name}>");
            }
        }
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" xmlns:hdrgm=\"{XMP_NAMESPACE}\"{attributes}>\
             {elements}</rdf:Description></rdf:RDF></x:xmpmeta>"
        )
    }

    fn per_channel(&self, value: impl Fn(&Channel) -> f64) -> Vec<f64> {
        self.channels.iter().map(value).collect()
    }

    /// The numbers for a gain map whose values are the gain itself, from no gain at 0 to
    /// `headroom` times the light at 1, spread evenly as powers of two: what an iPhone's
    /// older gain map becomes once its values are rewritten that way.
    #[must_use]
    pub fn up_to(headroom: f64) -> Self {
        let stops = headroom.max(1.0).log2();
        Self {
            base_headroom: (0, PARTS),
            alternate_headroom: positive(stops),
            channels: vec![Channel {
                min: (0, PARTS),
                max: fraction(stops),
                gamma: (PARTS, PARTS),
                base_offset: (0, PARTS),
                alternate_offset: (0, PARTS),
            }],
            use_base_color_space: true,
        }
    }
}

/// The power an older iPhone's gain map values are raised to before they are used. Measured:
/// with it, macOS shows a photo rewritten to the standard's form as it shows the original;
/// with the sRGB curve nearly so, and with the Rec. 709 curve clearly not.
const APPLE_GAMMA: f64 = 2.2;

/// A gain map in the one form every output format is written from: its picture, upright
/// and with 8 bits per channel (gray, or three channels), and how it is applied.
#[derive(Debug, Clone)]
pub struct CommonMap {
    pub pixels: DynamicImage,
    /// The main picture's size, upright.
    pub picture_size: (u32, u32),
    pub parameters: Parameters,
}

impl CommonMap {
    /// The gain map's pixels for a picture that is now `size`: scaled by the same factor as
    /// the picture, so it keeps its proportion to it.
    #[must_use]
    pub fn pixels_for(&self, size: (u32, u32)) -> DynamicImage {
        let scaled = |length: u32, now: u32, was: u32| {
            let pixels = u64::from(length) * u64::from(now) / u64::from(was.max(1));
            u32::try_from(pixels).unwrap_or(u32::MAX).max(1)
        };
        let wanted = (
            scaled(self.pixels.width(), size.0, self.picture_size.0),
            scaled(self.pixels.height(), size.1, self.picture_size.1),
        );
        if wanted == (self.pixels.width(), self.pixels.height()) {
            self.pixels.clone()
        } else {
            self.pixels
                .resize_exact(wanted.0, wanted.1, image::imageops::FilterType::Lanczos3)
        }
    }

    /// The gain map of an older iPhone photo in the common form. Its stored values are not
    /// powers of two between a smallest and a largest gain: raised to the power 2.2, a value
    /// is the part of the way from no gain to `headroom` times the light. So the values are
    /// rewritten: each becomes its gain's place between 1 and `headroom`, counted in powers
    /// of two.
    #[must_use]
    pub fn of_apple(pixels: &DynamicImage, picture_size: (u32, u32), headroom: f64) -> Self {
        let headroom = headroom.max(1.0);
        let rewritten: Vec<u8> = (0..=u8::MAX)
            .map(|stored| {
                let value = f64::from(stored) / 255.0;
                let part = value.powf(APPLE_GAMMA);
                let gain = 1.0 + (headroom - 1.0) * part;
                let place = if headroom > 1.0 {
                    gain.log2() / headroom.log2()
                } else {
                    0.0
                };
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let stored = (place.clamp(0.0, 1.0) * 255.0).round() as u8;
                stored
            })
            .collect();
        let mut gray = pixels.to_luma8();
        for pixel in gray.pixels_mut() {
            pixel[0] = rewritten[usize::from(pixel[0])];
        }
        Self {
            pixels: DynamicImage::ImageLuma8(gray),
            picture_size,
            parameters: Parameters::up_to(headroom),
        }
    }
}

/// The text of one gain map field in an XMP packet: an attribute's value, or an element's
/// content.
fn text_of<'a>(xmp: &'a str, prefix: &str, name: &str) -> Option<&'a str> {
    let attribute = format!("{prefix}:{name}=\"");
    if let Some(at) = xmp.find(&attribute) {
        let rest = &xmp[at + attribute.len()..];
        return Some(&rest[..rest.find('"')?]);
    }
    let open = format!("<{prefix}:{name}>");
    let at = xmp.find(&open)?;
    let rest = &xmp[at + open.len()..];
    Some(rest[..rest.find(&format!("</{prefix}:{name}>"))?].trim())
}

/// The numbers of one gain map field: one, or a list with one for each channel.
fn values_of(xmp: &str, prefix: &str, name: &str) -> Option<Vec<f64>> {
    let text = text_of(xmp, prefix, name)?;
    let numbers: Vec<f64> = if text.contains("<rdf:li>") {
        text.split("<rdf:li>")
            .skip(1)
            .filter_map(|item| item.split('<').next()?.trim().parse().ok())
            .collect()
    } else {
        vec![text.parse().ok()?]
    };
    (!numbers.is_empty()).then_some(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (i32, u32), b: f64) -> bool {
        (decimal(a) - b).abs() < 1e-6
    }

    #[test]
    fn the_binary_form_is_read_and_written_back() {
        // One channel: a standard picture, an HDR rendition up to 2^1.5 times brighter.
        let one = Parameters {
            base_headroom: (0, 1),
            alternate_headroom: (3, 2),
            channels: vec![Channel {
                min: (-1, 4),
                max: (3, 2),
                gamma: (1, 1),
                base_offset: (1, 64),
                alternate_offset: (1, 64),
            }],
            use_base_color_space: true,
        };
        let bytes = one.iso();
        assert_eq!(bytes.len(), 61);
        assert_eq!(&bytes[..5], [0, 0, 0, 0, 0x40]);
        assert_eq!(Parameters::of_iso(&bytes), Some(one.clone()));
        // Three channels.
        let mut three = one.clone();
        three.channels = vec![one.channels[0]; 3];
        three.channels[2].max = (7, 4);
        three.use_base_color_space = false;
        let bytes = three.iso();
        assert_eq!((bytes.len(), bytes[4]), (141, 0x80));
        assert_eq!(Parameters::of_iso(&bytes), Some(three));
        // Cut off, or a version that is not known.
        assert_eq!(Parameters::of_iso(&bytes[..100]), None);
        assert_eq!(Parameters::of_iso(&[0, 1, 0, 0, 0]), None);
        assert_eq!(Parameters::of_iso(&[]), None);
    }

    #[test]
    fn ultra_hdr_attributes_are_read() {
        // As Android phones write it: attributes, one value each.
        let xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description
            xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" hdrgm:Version="1.0"
            hdrgm:GainMapMin="-0.5" hdrgm:GainMapMax="2.25" hdrgm:Gamma="0.9"
            hdrgm:OffsetSDR="0.015625" hdrgm:OffsetHDR="0.02" hdrgm:HDRCapacityMin="0"
            hdrgm:HDRCapacityMax="2.25" hdrgm:BaseRenditionIsHDR="False"/></rdf:RDF></x:xmpmeta>"#;
        let read = Parameters::of_xmp(xmp).unwrap();
        assert_eq!(read.channels.len(), 1);
        let channel = read.channels[0];
        assert!(close(channel.min, -0.5) && close(channel.max, 2.25));
        assert!(close(channel.base_offset, 0.015_625) && close(channel.alternate_offset, 0.02));
        assert_eq!(channel.gamma, (900_000, PARTS));
        assert_eq!(read.base_headroom, (0, PARTS));
        assert_eq!(read.alternate_headroom, (2_250_000, PARTS));

        // As Photoshop writes it: lists for the values that differ by channel, and defaults
        // for what is left out.
        let xmp = br#"<rdf:Description xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/"
            hdrgm:Version="1.0" hdrgm:HDRCapacityMax="1.3">
            <hdrgm:GainMapMax><rdf:Seq><rdf:li>1.2</rdf:li><rdf:li>1.25</rdf:li>
            <rdf:li>1.3</rdf:li></rdf:Seq></hdrgm:GainMapMax>
            <hdrgm:Gamma> <rdf:Seq> <rdf:li>0.95</rdf:li> <rdf:li>0.94</rdf:li>
            <rdf:li>0.92</rdf:li> </rdf:Seq> </hdrgm:Gamma></rdf:Description>"#;
        let read = Parameters::of_xmp(xmp).unwrap();
        assert_eq!(read.channels.len(), 3);
        assert!(close(read.channels[1].max, 1.25) && close(read.channels[2].max, 1.3));
        assert_eq!(read.channels[2].gamma, (920_000, PARTS));
        assert!(read.channels.iter().all(|c| close(c.min, 0.0)));
        assert!(
            read.channels
                .iter()
                .all(|c| close(c.base_offset, 1.0 / 64.0))
        );

        // As iPhones write it: elements, under another prefix, with other names for the
        // headrooms.
        let xmp = br#"<rdf:Description xmlns:HDRToneMap="http://ns.adobe.com/hdr-gain-map/1.0/">
            <HDRToneMap:GainMapMax>1.757545</HDRToneMap:GainMapMax>
            <HDRToneMap:Gamma>0.873047</HDRToneMap:Gamma>
            <HDRToneMap:BaseRenditionIsHDR>0</HDRToneMap:BaseRenditionIsHDR>
            <HDRToneMap:BaseHeadroom>0.000000</HDRToneMap:BaseHeadroom>
            <HDRToneMap:AlternateHeadroom>1.757545</HDRToneMap:AlternateHeadroom>
            <HDRToneMap:GainMapMin>-0.332520</HDRToneMap:GainMapMin></rdf:Description>"#;
        let read = Parameters::of_xmp(xmp).unwrap();
        assert!(close(read.channels[0].min, -0.332_52));
        assert_eq!(read.alternate_headroom, (1_757_545, PARTS));

        // No gain map attributes, or a file whose own picture is the HDR one.
        assert_eq!(Parameters::of_xmp(b"<x:xmpmeta/>"), None);
        let hdr_base = br#"<rdf:Description xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/"
            hdrgm:GainMapMax="2" hdrgm:HDRCapacityMax="2" hdrgm:BaseRenditionIsHDR="True"/>"#;
        assert_eq!(Parameters::of_xmp(hdr_base), None);
    }

    #[test]
    fn an_older_iphone_gain_map_is_rewritten_as_powers_of_two() {
        // Stored values from none to full, in one row.
        let stored =
            image::GrayImage::from_fn(256, 1, |x, _| image::Luma([u8::try_from(x).unwrap()]));
        let map = CommonMap::of_apple(&DynamicImage::ImageLuma8(stored), (512, 2), 8.0);
        assert_eq!(map.picture_size, (512, 2));
        // Up to eight times the light: three powers of two.
        assert_eq!(map.parameters, Parameters::up_to(8.0));
        assert_eq!(map.parameters.channels[0].max, (3_000_000, PARTS));
        let rewritten = map.pixels.to_luma8();
        let at = |x: u32| rewritten.get_pixel(x, 0)[0];
        // No gain stays none, and the full gain stays full.
        assert_eq!((at(0), at(255)), (0, 255));
        // In between, the stored value 186 is half the way in light (0.5 to the power of
        // 1/2.2 is 0.73): a gain of 4.5, which is 2.17 of the three powers of two.
        assert!(at(186).abs_diff(184) <= 1, "{}", at(186));
        // Never darker toward the right.
        assert!((1..256).all(|x| at(x) >= at(x - 1)));
        // A headroom of 1 is no gain at all.
        let flat = CommonMap::of_apple(&map.pixels, (512, 2), 1.0);
        assert!(flat.pixels.to_luma8().pixels().all(|p| p[0] == 0));
    }

    #[test]
    fn the_attributes_written_are_read_back_the_same() {
        let mut parameters = Parameters::up_to(4.0);
        assert_eq!(parameters.alternate_headroom, (2_000_000, PARTS));
        assert_eq!(parameters.channels[0].max, (2_000_000, PARTS));
        let xmp = parameters.xmp();
        assert!(xmp.contains("hdrgm:GainMapMax=\"2\""), "{xmp}");
        assert!(xmp.contains("hdrgm:OffsetSDR=\"0\""), "{xmp}");
        assert_eq!(Parameters::of_xmp(xmp.as_bytes()), Some(parameters.clone()));
        // Three channels that differ are written as lists.
        parameters.channels = vec![parameters.channels[0]; 3];
        parameters.channels[1].max = fraction(1.75);
        parameters.channels[2].min = fraction(-0.25);
        let xmp = parameters.xmp();
        assert!(xmp.contains("<hdrgm:GainMapMax><rdf:Seq><rdf:li>2</rdf:li><rdf:li>1.75</rdf:li>"));
        assert_eq!(Parameters::of_xmp(xmp.as_bytes()), Some(parameters));
    }
}
