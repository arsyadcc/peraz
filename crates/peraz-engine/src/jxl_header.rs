//! Reads what a JPEG XL file holds from its header (ISO/IEC 18181-1 `SizeHeader` and the start
//! of `ImageMetadata`), without decoding it: the picture size, whether it has transparency, and
//! whether it is an animation.
//!
//! A file is either a bare codestream (starting `FF 0A`) or an ISO-BMFF container (ISO/IEC
//! 18181-2) whose codestream is in a `jxlc` box or split across `jxlp` boxes. A container can
//! also hold a `jbrd` box: the data that rebuilds the JPEG a file was losslessly made from.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use peraz_core::Alpha;

/// What the header says about a JPEG XL file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The stored size, before the orientation is applied.
    pub width: u32,
    pub height: u32,
    pub alpha: Alpha,
    pub animated: bool,
}

const CODESTREAM: &[u8] = &[0xFF, 0x0A];
const CONTAINER: &[u8] = b"\0\0\0\x0CJXL \r\n\x87\n";
/// Enough codestream for the header fields read here.
const HEADER_BYTES: usize = 256;

/// The file's header when it is JPEG XL; `None` when it is another format.
pub fn sniff(path: &Path) -> std::io::Result<Option<Header>> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 12];
    let filled = read_up_to(&mut file, &mut head)?;
    let head = &head[..filled];
    let codestream = if head.starts_with(CODESTREAM) {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = vec![0; HEADER_BYTES];
        let filled = read_up_to(&mut file, &mut bytes)?;
        bytes.truncate(filled);
        bytes
    } else if head == CONTAINER {
        container_codestream(&mut file)?
    } else {
        return Ok(None);
    };
    parse(&codestream).map(Some).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the JPEG XL header is damaged or incomplete",
        )
    })
}

/// One box of a container: its type and where its content lies in the file.
struct BoxSpan {
    kind: [u8; 4],
    start: u64,
    end: u64,
}

/// The boxes of a container, read from their headers only.
fn container_boxes(file: &mut std::fs::File) -> std::io::Result<Vec<BoxSpan>> {
    let mut boxes = Vec::new();
    let mut offset = CONTAINER.len() as u64;
    let end = file.seek(SeekFrom::End(0))?;
    while offset + 8 <= end {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let mut size = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let mut start = offset + 8;
        if size == 1 {
            let mut large = [0u8; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            start += 8;
        } else if size == 0 {
            size = end - offset;
        }
        let box_end = offset.saturating_add(size).min(end);
        if box_end < start {
            break;
        }
        boxes.push(BoxSpan {
            kind: [header[4], header[5], header[6], header[7]],
            start,
            end: box_end,
        });
        offset = box_end;
    }
    Ok(boxes)
}

/// The start of the codestream inside a container, from its `jxlc` box or `jxlp` boxes.
fn container_codestream(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
    let mut codestream = Vec::new();
    for span in container_boxes(file)? {
        let skip = match &span.kind {
            b"jxlc" => 0,
            // Each part starts with its 4-byte index.
            b"jxlp" => 4,
            _ => continue,
        };
        let content_start = span.start + skip;
        let wanted = (HEADER_BYTES - codestream.len()) as u64;
        let take = span.end.saturating_sub(content_start).min(wanted);
        file.seek(SeekFrom::Start(content_start))?;
        let mut part = vec![0; usize::try_from(take).unwrap_or(0)];
        file.read_exact(&mut part)?;
        codestream.extend_from_slice(&part);
        if codestream.len() >= HEADER_BYTES {
            break;
        }
    }
    Ok(codestream)
}

/// The file is a JPEG recompressed losslessly: its container has a `jbrd` box, from which
/// `djxl` rebuilds the original JPEG file exactly.
pub fn has_jpeg_reconstruction(path: &Path) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 12];
    if read_up_to(&mut file, &mut head)? < head.len() || head != CONTAINER {
        return Ok(false);
    }
    Ok(container_boxes(&mut file)?
        .iter()
        .any(|span| &span.kind == b"jbrd"))
}

fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Parses the header at the start of a codestream.
pub fn parse(codestream: &[u8]) -> Option<Header> {
    let rest = codestream.strip_prefix(CODESTREAM)?;
    let mut bits = Bits::new(rest);
    let (width, height) = size_header(&mut bits)?;
    let mut header = Header {
        width,
        height,
        alpha: Alpha::Absent,
        animated: false,
    };
    // ImageMetadata.
    if bits.flag()? {
        // All defaults: 8-bit, no extra channels, no animation.
        return Some(header);
    }
    if bits.flag()? {
        let _orientation = bits.read(3)?;
        if bits.flag()? {
            size_header(&mut bits)?; // intrinsic size
        }
        if bits.flag()? {
            preview_header(&mut bits)?;
        }
        if bits.flag()? {
            header.animated = true;
            return Some(header);
        }
    }
    // BitDepth.
    if bits.flag()? {
        bits.u32([Val(32), Val(16), Val(24), Offset(6, 1)])?;
        bits.read(4)?;
    } else {
        bits.u32([Val(8), Val(10), Val(12), Offset(6, 1)])?;
    }
    let _modular_16_bit_buffers = bits.flag()?;
    let extra_channels = bits.u32([Val(0), Val(1), Offset(4, 2), Offset(12, 1)])?;
    if extra_channels > 0 {
        // The first extra channel is alpha when it has the default settings (an alpha channel)
        // or its type is 0 (alpha). Other kinds (depth, spot colors) come first only rarely.
        header.alpha =
            if bits.flag()? || bits.u32([Val(0), Val(1), Offset(4, 2), Offset(6, 18)])? == 0 {
                Alpha::Present
            } else {
                Alpha::Unknown
            };
    }
    Some(header)
}

/// `SizeHeader`: the picture's height, and its width directly or as a ratio of the height.
fn size_header(bits: &mut Bits) -> Option<(u32, u32)> {
    const DIMENSION: [Dist; 4] = [Offset(9, 1), Offset(13, 1), Offset(18, 1), Offset(30, 1)];
    let small = bits.flag()?;
    let dimension = |bits: &mut Bits| {
        if small {
            Some((bits.read(5)? + 1) * 8)
        } else {
            bits.u32(DIMENSION)
        }
    };
    let height = dimension(bits)?;
    let width = match bits.read(3)? {
        0 => dimension(bits)?,
        ratio => from_ratio(height, ratio),
    };
    Some((width, height))
}

/// `PreviewHeader`, read only to get past it.
fn preview_header(bits: &mut Bits) -> Option<()> {
    let div8 = bits.flag()?;
    let dimension = |bits: &mut Bits| {
        if div8 {
            bits.u32([Val(16), Val(32), Offset(5, 1), Offset(9, 33)])
        } else {
            bits.u32([
                Offset(6, 1),
                Offset(8, 65),
                Offset(10, 321),
                Offset(12, 1345),
            ])
        }
    };
    dimension(bits)?;
    if bits.read(3)? == 0 {
        dimension(bits)?;
    }
    Some(())
}

/// The width for one of the fixed aspect ratios (1–7) a `SizeHeader` can name.
fn from_ratio(height: u32, ratio: u32) -> u32 {
    let (num, den) = match ratio {
        1 => (1, 1),
        2 => (12, 10),
        3 => (4, 3),
        4 => (3, 2),
        5 => (16, 9),
        6 => (5, 4),
        _ => (2, 1),
    };
    u32::try_from(u64::from(height) * num / den).unwrap_or(u32::MAX)
}

/// One of the four choices of a `U32` field.
#[derive(Clone, Copy)]
enum Dist {
    Val(u32),
    /// `n` bits plus an offset.
    Offset(u32, u32),
}
use Dist::{Offset, Val};

/// Reads bits least significant first, as JPEG XL stores them.
struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for i in 0..count {
            let byte = self.bytes.get(self.position / 8)?;
            let bit = (byte >> (self.position % 8)) & 1;
            value |= u32::from(bit) << i;
            self.position += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        self.read(1).map(|bit| bit == 1)
    }

    fn u32(&mut self, choices: [Dist; 4]) -> Option<u32> {
        match choices[self.read(2)? as usize] {
            Val(value) => Some(value),
            Offset(count, offset) => self.read(count)?.checked_add(offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits least significant first, to build headers.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        position: usize,
    }

    impl Writer {
        fn put(&mut self, count: u32, value: u32) -> &mut Self {
            for i in 0..count {
                if self.position.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = u8::from((value >> i) & 1 == 1);
                *self.bytes.last_mut().unwrap() |= bit << (self.position % 8);
                self.position += 1;
            }
            self
        }

        fn codestream(&self) -> Vec<u8> {
            [CODESTREAM, &self.bytes].concat()
        }
    }

    // The first bytes of files made by cjxl 0.11.1 from the test fixtures.
    const OPAQUE_1600X1200: &[u8] = &[0xff, 0x0a, 0x7a, 0x25, 0x1b, 0x0c, 0x46, 0x02, 0x89, 0x03];
    const ALPHA_1200X900: &[u8] = &[0xff, 0x0a, 0x1a, 0x1c, 0x03, 0x3b, 0x01, 0x0c, 0x18, 0x09];
    const ANIMATION_480X270: &[u8] = &[0xff, 0x0a, 0x68, 0x58, 0x41, 0x00, 0xd6, 0x04, 0x0c, 0x30];

    #[test]
    fn reads_files_made_by_cjxl() {
        let header = |width, height, alpha, animated| Header {
            width,
            height,
            alpha,
            animated,
        };
        assert_eq!(
            parse(OPAQUE_1600X1200),
            Some(header(1600, 1200, Alpha::Absent, false))
        );
        assert_eq!(
            parse(ALPHA_1200X900),
            Some(header(1200, 900, Alpha::Present, false))
        );
        assert_eq!(
            parse(ANIMATION_480X270).map(|h| (h.width, h.height, h.animated)),
            Some((480, 270, true))
        );
    }

    #[test]
    fn reads_small_sizes_ratios_and_defaults() {
        // Small (multiple of 8) height 64, ratio 16:9, all-default metadata.
        let mut w = Writer::default();
        w.put(1, 1).put(5, 7).put(3, 5).put(1, 1);
        assert_eq!(
            parse(&w.codestream()),
            Some(Header {
                width: 113,
                height: 64,
                alpha: Alpha::Absent,
                animated: false
            })
        );
        // Explicit 300x7000 (9 and 13 bits), a preview, then 10-bit with a non-alpha channel.
        let mut w = Writer::default();
        w.put(1, 0)
            .put(2, 1)
            .put(13, 6999)
            .put(3, 0)
            .put(2, 0)
            .put(9, 299);
        w.put(1, 0).put(1, 1).put(3, 0).put(1, 0); // extra fields, orientation 1, no intrinsic
        w.put(1, 1).put(1, 1).put(2, 0).put(3, 1); // preview: div8, 16, ratio 1
        w.put(1, 0); // no animation
        w.put(1, 0).put(2, 1); // 10-bit integers
        w.put(1, 1).put(2, 1); // modular 16-bit buffers, one extra channel
        w.put(1, 0).put(2, 1); // not default, type 1 (depth)
        assert_eq!(
            parse(&w.codestream()),
            Some(Header {
                width: 300,
                height: 7000,
                alpha: Alpha::Unknown,
                animated: false
            })
        );
    }

    #[test]
    fn rejects_other_and_cut_off_data() {
        assert_eq!(parse(b"\xFF\xD8\xFF"), None);
        assert_eq!(parse(&OPAQUE_1600X1200[..3]), None);
    }

    #[test]
    fn finds_the_codestream_in_a_container() {
        let boxed = |kind: &[u8], body: &[u8]| {
            let size = u32::try_from(body.len() + 8).unwrap().to_be_bytes();
            [&size[..], kind, body].concat()
        };
        let dir = tempdir();
        let parts = [
            CONTAINER.to_vec(),
            boxed(b"ftyp", b"jxl \0\0\0\0jxl "),
            boxed(b"Exif", &[0; 300]),
            boxed(b"jxlp", &[&[0, 0, 0, 0][..], &ALPHA_1200X900[..5]].concat()),
            boxed(
                b"jxlp",
                &[&[0x80, 0, 0, 1][..], &ALPHA_1200X900[5..]].concat(),
            ),
        ];
        let path = dir.join("boxed.jxl");
        std::fs::write(&path, parts.concat()).unwrap();
        assert_eq!(sniff(&path).unwrap(), parse(ALPHA_1200X900));
        assert!(!has_jpeg_reconstruction(&path).unwrap());
        let path = dir.join("from-jpeg.jxl");
        std::fs::write(&path, [parts.concat(), boxed(b"jbrd", &[0; 20])].concat()).unwrap();
        assert!(has_jpeg_reconstruction(&path).unwrap());

        let path = dir.join("single.jxl");
        let single = [CONTAINER.to_vec(), boxed(b"jxlc", OPAQUE_1600X1200)].concat();
        std::fs::write(&path, single).unwrap();
        assert_eq!(sniff(&path).unwrap(), parse(OPAQUE_1600X1200));

        let path = dir.join("other.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1A\n").unwrap();
        assert_eq!(sniff(&path).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("peraz-jxl-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
