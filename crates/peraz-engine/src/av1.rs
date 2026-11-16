//! The color description inside an AV1 picture.
//!
//! An AV1 stream says in its sequence header what its pixel values mean: the color
//! primaries, the transfer function, and the matrix. The AV1 encoder used for AVIF always
//! writes sRGB there. For an HDR picture the container says the right thing, but programs
//! that read the stream alone (FFmpeg among them) would take the picture for a standard one,
//! so the two code points are set in the stream too. They are whole bytes at a place that
//! depends on what comes before them, which is why the header is read up to there.

/// Reads a sequence of bits, the first bit of each byte first.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self.bytes.get(self.at / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.read(1)? == 1)
    }

    fn skip(&mut self, count: u32) -> Option<()> {
        self.at = self.at.checked_add(count as usize)?;
        (self.at <= self.bytes.len() * 8).then_some(())
    }

    /// A number with a unary length before it (`uvlc`).
    fn skip_unary_coded(&mut self) -> Option<()> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        self.skip(zeros)
    }
}

/// The payload of the first sequence header in a series of OBUs: where it starts and ends.
fn sequence_header(obus: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    while at < obus.len() {
        let header = *obus.get(at)?;
        let kind = (header >> 3) & 0xF;
        let has_extension = header & 0b100 != 0;
        let has_size = header & 0b10 != 0;
        at += 1 + usize::from(has_extension);
        let size = if has_size {
            // A size in groups of seven bits, the lowest group first.
            let mut size = 0usize;
            for group in 0..8 {
                let byte = *obus.get(at)?;
                at += 1;
                size |= usize::from(byte & 0x7F) << (7 * group);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            size
        } else {
            obus.len().checked_sub(at)?
        };
        let end = at.checked_add(size)?;
        if kind == 1 {
            return (end <= obus.len()).then_some(at..end);
        }
        at = end;
    }
    None
}

/// The bit in a sequence header where the color primaries start: they, the transfer
/// function, and the matrix follow each other as one byte each. `None` when the header does
/// not describe its colors, or cannot be followed.
fn color_description_at(header: &[u8]) -> Option<usize> {
    let mut bits = Bits {
        bytes: header,
        at: 0,
    };
    let profile = bits.read(3)?;
    let _still_picture = bits.flag()?;
    let reduced = bits.flag()?;
    if reduced {
        bits.skip(5)?; // the level
    } else {
        let mut decoder_model = None;
        if bits.flag()? {
            // Timing: two 32-bit numbers, then an optional tick count.
            bits.skip(64)?;
            if bits.flag()? {
                bits.skip_unary_coded()?;
            }
            if bits.flag()? {
                let delay_bits = bits.read(5)? + 1;
                bits.skip(32 + 5 + 5)?;
                decoder_model = Some(delay_bits);
            }
        }
        let display_delay = bits.flag()?;
        let points = bits.read(5)? + 1;
        for _ in 0..points {
            bits.skip(12)?;
            if bits.read(5)? > 7 {
                bits.skip(1)?; // the tier
            }
            if let Some(delay_bits) = decoder_model
                && bits.flag()?
            {
                bits.skip(2 * delay_bits + 1)?;
            }
            if display_delay && bits.flag()? {
                bits.skip(4)?;
            }
        }
    }
    let (width_bits, height_bits) = (bits.read(4)? + 1, bits.read(4)? + 1);
    bits.skip(width_bits + height_bits)?;
    if !reduced && bits.flag()? {
        bits.skip(4 + 3)?; // frame ID lengths
    }
    bits.skip(3)?; // superblock size, filter intra, intra edge filter
    if !reduced {
        bits.skip(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = bits.flag()?;
        if order_hint {
            bits.skip(2)?;
        }
        // Screen content tools: chosen per frame, or forced on or off for all.
        let screen_content = if bits.flag()? { 2 } else { bits.read(1)? };
        if screen_content > 0 && !bits.flag()? {
            bits.skip(1)?; // integer motion vectors forced on or off
        }
        if order_hint {
            bits.skip(3)?;
        }
    }
    bits.skip(3)?; // superres, CDEF, loop restoration
    // The color configuration.
    let high_bit_depth = bits.flag()?;
    if profile == 2 && high_bit_depth {
        bits.skip(1)?; // twelve bits
    }
    if profile != 1 {
        bits.skip(1)?; // monochrome
    }
    let described = bits.flag()?;
    (described && bits.at + 24 <= header.len() * 8).then_some(bits.at)
}

/// Sets one byte that starts at any bit.
fn set_byte(bytes: &mut [u8], at: usize, value: u8) {
    for bit in 0..8 {
        let (byte, mask) = ((at + bit) / 8, 0x80 >> ((at + bit) % 8));
        if value & (0x80 >> bit) == 0 {
            bytes[byte] &= !mask;
        } else {
            bytes[byte] |= mask;
        }
    }
}

/// Sets the color primaries and the transfer function in the sequence header of an AV1
/// picture (its OBUs, as an AVIF item holds them). Returns whether they could be set: the
/// header must describe its colors already.
pub(crate) fn describe_color(obus: &mut [u8], primaries: u8, transfer: u8) -> bool {
    let Some(header) = sequence_header(obus) else {
        return false;
    };
    let Some(at) = color_description_at(&obus[header.clone()]) else {
        return false;
    };
    let header = &mut obus[header];
    set_byte(header, at, primaries);
    set_byte(header, at + 8, transfer);
    true
}

/// The color primaries, the transfer function, and the matrix that the sequence header of an
/// AV1 picture gives, when it describes its colors.
#[cfg(test)]
pub(crate) fn color_description(obus: &[u8]) -> Option<[u8; 3]> {
    let header = &obus[sequence_header(obus)?];
    let mut bits = Bits {
        bytes: header,
        at: color_description_at(header)?,
    };
    let mut byte = || u8::try_from(bits.read(8)?).ok();
    Some([byte()?, byte()?, byte()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits, the first bit of each byte first.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        at: usize,
    }

    impl Writer {
        fn put(&mut self, count: u32, value: u32) -> &mut Self {
            for bit in (0..count).rev() {
                if self.at.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> bit) & 1 == 1 {
                    *self.bytes.last_mut().unwrap() |= 0x80 >> (self.at % 8);
                }
                self.at += 1;
            }
            self
        }
    }

    /// OBUs with a temporal delimiter, then a sequence header with this payload.
    fn obus(header: &[u8]) -> Vec<u8> {
        let mut obus = vec![0b0001_0010, 0];
        obus.extend([0b0000_1010, u8::try_from(header.len()).unwrap()]);
        obus.extend_from_slice(header);
        obus.extend([0b0011_0010, 2, 0xAA, 0xBB]);
        obus
    }

    #[test]
    fn the_colors_are_found_in_a_short_and_in_a_full_header() {
        // A still picture with the short header: profile 1, level, sizes, tools, then the
        // colors.
        let mut short = Writer::default();
        short.put(3, 1).put(1, 1).put(1, 1).put(5, 31);
        short.put(4, 8).put(4, 8).put(9, 399).put(9, 299);
        short.put(3, 0).put(3, 0b101);
        short.put(1, 1).put(1, 1).put(8, 1).put(8, 13).put(8, 6);
        short.put(8, 0xFF);
        let mut stream = obus(&short.bytes);
        assert_eq!(color_description(&stream), Some([1, 13, 6]));
        assert!(describe_color(&mut stream, 9, 16));
        assert_eq!(color_description(&stream), Some([9, 16, 6]));
        // Nothing around the two bytes has changed.
        let mut expected = Writer::default();
        expected.put(3, 1).put(1, 1).put(1, 1).put(5, 31);
        expected.put(4, 8).put(4, 8).put(9, 399).put(9, 299);
        expected.put(3, 0).put(3, 0b101);
        expected.put(1, 1).put(1, 1).put(8, 9).put(8, 16).put(8, 6);
        expected.put(8, 0xFF);
        assert_eq!(stream, obus(&expected.bytes));

        // The full header of a video stream: profile 0 with timing, one operating point of
        // a high level, frame IDs, and every tool flag.
        let mut full = Writer::default();
        full.put(3, 0).put(1, 0).put(1, 0);
        full.put(1, 1)
            .put(32, 1)
            .put(32, 30)
            .put(1, 1)
            .put(3, 0b010)
            .put(1, 0);
        full.put(1, 0).put(5, 0);
        full.put(12, 0).put(5, 9).put(1, 0);
        full.put(4, 10).put(4, 10).put(11, 1919).put(11, 1079);
        full.put(1, 1).put(4, 3).put(3, 2);
        full.put(3, 0b111);
        full.put(4, 0b1010).put(1, 1).put(2, 0b11);
        full.put(1, 0).put(1, 1).put(1, 0).put(1, 1);
        full.put(3, 6);
        full.put(3, 0b010);
        full.put(1, 1)
            .put(1, 0)
            .put(1, 1)
            .put(8, 9)
            .put(8, 18)
            .put(8, 9);
        let mut stream = obus(&full.bytes);
        assert_eq!(color_description(&stream), Some([9, 18, 9]));
        assert!(describe_color(&mut stream, 1, 16));
        assert_eq!(color_description(&stream), Some([1, 16, 9]));
    }

    #[test]
    fn a_header_without_a_color_description_is_left_alone() {
        let mut plain = Writer::default();
        plain.put(3, 1).put(1, 1).put(1, 1).put(5, 31);
        plain.put(4, 8).put(4, 8).put(9, 399).put(9, 299);
        plain.put(3, 0).put(3, 0);
        plain.put(1, 0).put(1, 0).put(8, 0);
        let mut stream = obus(&plain.bytes);
        let before = stream.clone();
        assert!(!describe_color(&mut stream, 9, 16));
        assert_eq!(stream, before);
        // Cut off, or without a sequence header at all.
        assert!(!describe_color(&mut stream[..6], 9, 16));
        assert!(!describe_color(&mut [0b0001_0010, 0], 9, 16));
        assert!(!describe_color(&mut [], 9, 16));
    }
}
