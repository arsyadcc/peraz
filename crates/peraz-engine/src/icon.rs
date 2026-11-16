//! Icon files: ICO and CUR (Windows) and ICNS (macOS). Each holds several sizes of one picture.
//!
//! The file is written back in its own format with the same pictures at the same sizes. What
//! gets smaller is the PNG data inside: every PNG picture is recompressed, and a 256-pixel BMP
//! picture in an ICO becomes a PNG, which Windows reads at that size since Vista. Everything
//! else is kept byte for byte: smaller BMP pictures (older programs expect them), JPEG 2000 and
//! run-length encoded pictures and masks in an ICNS, and data that is not a picture.
//!
//! PNG pictures stay 8-bit RGBA, also when their colors are reduced: Windows asks for exactly
//! that inside an ICO, and it is what macOS's own tools write into an ICNS.

use std::io::{Cursor, Read};
use std::path::Path;

use image::{DynamicImage, ImageDecoder as _, RgbaImage};

use crate::image::{PngPixels, quantize, write_png};

/// Which icon format a file is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Ico,
    /// A Windows cursor: an ICO whose pictures carry a hotspot.
    Cur,
    Icns,
}

impl Container {
    /// The name used for the container in probe results, and the file extension.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ico => "ico",
            Self::Cur => "cur",
            Self::Icns => "icns",
        }
    }
}

/// What an icon file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub container: Container,
    /// The size of the largest picture, in pixels; 0 when no picture says its size.
    pub width: u32,
    pub height: u32,
}

/// What happened to the pictures inside an icon file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Pictures in the file.
    pub pictures: u32,
    /// Pictures replaced by a smaller version.
    pub recompressed: u32,
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1A\n";
const ICNS_MAGIC: &[u8] = b"icns";

/// Largest icon file read into memory: far more than any real one.
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Recognizes an icon file from its content. An ICO has no signature of its own, so its whole
/// directory is checked.
pub fn sniff(path: &Path) -> std::io::Result<Option<Header>> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let maybe_ico = matches!(head, [0, 0, 1 | 2, 0]);
    if head != ICNS_MAGIC && !maybe_ico {
        return Ok(None);
    }
    let mut bytes = head.to_vec();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes)?;
    if maybe_ico {
        let Some((container, entries)) = parse_ico(&bytes) else {
            return Ok(None);
        };
        let (width, height) = entries
            .iter()
            .map(IcoEntry::size)
            .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
            .unwrap_or((0, 0));
        return Ok(Some(Header {
            container,
            width,
            height,
        }));
    }
    let Some(entries) = parse_icns(&bytes) else {
        return Ok(None);
    };
    let size = entries
        .iter()
        .filter_map(|entry| png_size(entry.data).or_else(|| icns_type_size(entry.kind)))
        .max()
        .unwrap_or(0);
    Ok(Some(Header {
        container: Container::Icns,
        width: size,
        height: size,
    }))
}

/// Compresses an icon file held in memory. `palette_quality` is the quality range (0–100,
/// pngquant-style) for reducing the colors of PNG pictures; `None` keeps every pixel.
pub fn compress(
    bytes: &[u8],
    palette_quality: Option<(u8, u8)>,
) -> Result<(Vec<u8>, Stats), String> {
    let mut stats = Stats::default();
    let out = if bytes.starts_with(ICNS_MAGIC) {
        compress_icns(bytes, palette_quality, &mut stats, 0)?
    } else {
        compress_ico(bytes, palette_quality, &mut stats)?
    };
    Ok((out, stats))
}

/// One picture of an ICO or CUR file, with its directory entry.
struct IcoEntry<'a> {
    width: u8,
    height: u8,
    colors: u8,
    /// Color planes in an ICO; the hotspot's x in a CUR.
    planes: u16,
    /// Bits per pixel in an ICO; the hotspot's y in a CUR.
    bits: u16,
    data: &'a [u8],
}

impl IcoEntry<'_> {
    fn is_png(&self) -> bool {
        self.data.starts_with(PNG_SIGNATURE)
    }

    /// The picture's size in pixels, from its own data.
    fn size(&self) -> (u32, u32) {
        if let Some(size) = png_dimensions(self.data) {
            return size;
        }
        // A bitmap: width and height follow the header's size; the height counts the picture
        // and its mask.
        let word = |at: usize| {
            self.data
                .get(at..at + 4)
                .map_or(0, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        (word(4).unsigned_abs(), word(8).unsigned_abs() / 2)
    }
}

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([pair[0], pair[1]]))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    let four = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([four[0], four[1], four[2], four[3]]))
}

fn u32_be(bytes: &[u8], at: usize) -> Option<u32> {
    let four = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([four[0], four[1], four[2], four[3]]))
}

/// Reads the directory of an ICO or CUR file; `None` when the bytes are not one. Every picture
/// must lie inside the file and start like a PNG or a bitmap.
fn parse_ico(bytes: &[u8]) -> Option<(Container, Vec<IcoEntry<'_>>)> {
    if u16_le(bytes, 0)? != 0 {
        return None;
    }
    let container = match u16_le(bytes, 2)? {
        1 => Container::Ico,
        2 => Container::Cur,
        _ => return None,
    };
    let count = usize::from(u16_le(bytes, 4)?);
    if count == 0 {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let at = 6 + index * 16;
        let entry = bytes.get(at..at + 16)?;
        let size = usize::try_from(u32_le(entry, 8)?).ok()?;
        let offset = usize::try_from(u32_le(entry, 12)?).ok()?;
        if offset < 6 + count * 16 {
            return None;
        }
        let data = bytes.get(offset..offset.checked_add(size)?)?;
        // A bitmap starts with the size of its header.
        let is_bitmap = matches!(u32_le(data, 0), Some(40 | 108 | 124)) && data.len() > 40;
        if !data.starts_with(PNG_SIGNATURE) && !is_bitmap {
            return None;
        }
        entries.push(IcoEntry {
            width: entry[0],
            height: entry[1],
            colors: entry[2],
            planes: u16_le(entry, 4)?,
            bits: u16_le(entry, 6)?,
            data,
        });
    }
    Some((container, entries))
}

fn compress_ico(
    bytes: &[u8],
    palette_quality: Option<(u8, u8)>,
    stats: &mut Stats,
) -> Result<Vec<u8>, String> {
    let (container, entries) =
        parse_ico(bytes).ok_or("this icon file is damaged: its directory cannot be read")?;
    let mut pictures: Vec<(IcoEntry, Option<Vec<u8>>)> = Vec::with_capacity(entries.len());
    for mut entry in entries {
        stats.pictures += 1;
        let smaller = if entry.is_png() {
            recompress_png(entry.data, palette_quality)
        } else if entry.size() == (256, 256) {
            // A bitmap this large is where the bytes are, and Windows reads a PNG here.
            let png = bitmap_as_png(&entry, container, palette_quality);
            if png.is_some() && container == Container::Ico {
                (entry.colors, entry.planes, entry.bits) = (0, 1, 32);
            }
            png
        } else {
            None
        };
        stats.recompressed += u32::from(smaller.is_some());
        pictures.push((entry, smaller));
    }

    let count = u16::try_from(pictures.len()).map_err(|_| "too many pictures".to_owned())?;
    let mut out = vec![0, 0];
    out.extend(if container == Container::Cur { 2u16 } else { 1 }.to_le_bytes());
    out.extend(count.to_le_bytes());
    let mut offset = 6 + pictures.len() * 16;
    for (entry, smaller) in &pictures {
        let data = smaller.as_deref().unwrap_or(entry.data);
        out.extend([entry.width, entry.height, entry.colors, 0]);
        out.extend(entry.planes.to_le_bytes());
        out.extend(entry.bits.to_le_bytes());
        let too_large = |_| "this icon file is too large".to_owned();
        out.extend(u32::try_from(data.len()).map_err(too_large)?.to_le_bytes());
        out.extend(u32::try_from(offset).map_err(too_large)?.to_le_bytes());
        offset += data.len();
    }
    for (entry, smaller) in &pictures {
        out.extend_from_slice(smaller.as_deref().unwrap_or(entry.data));
    }
    Ok(out)
}

/// A bitmap picture of an ICO as a PNG, when that is smaller. The bitmap and its mask are
/// decoded as a one-picture ICO.
fn bitmap_as_png(
    entry: &IcoEntry,
    container: Container,
    palette_quality: Option<(u8, u8)>,
) -> Option<Vec<u8>> {
    let mut single = vec![0, 0, 1, 0, 1, 0];
    single.extend([entry.width, entry.height, entry.colors, 0]);
    // A cursor's hotspot is not a bit depth: let the bitmap's own header speak.
    let (planes, bits) = if container == Container::Ico {
        (entry.planes, entry.bits)
    } else {
        (1, 0)
    };
    single.extend(planes.to_le_bytes());
    single.extend(bits.to_le_bytes());
    single.extend(u32::try_from(entry.data.len()).ok()?.to_le_bytes());
    single.extend(22u32.to_le_bytes());
    single.extend_from_slice(entry.data);
    let decoder = image::codecs::ico::IcoDecoder::new(Cursor::new(single)).ok()?;
    let rgba = DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
    if rgba.dimensions() != (256, 256) {
        return None;
    }
    let png = icon_png(&rgba, None, palette_quality)?;
    (png.len() < entry.data.len()).then_some(png)
}

/// One element of an ICNS file.
struct IcnsEntry<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

/// Reads the elements of an ICNS file; `None` when the bytes are not one.
fn parse_icns(bytes: &[u8]) -> Option<Vec<IcnsEntry<'_>>> {
    if !bytes.starts_with(ICNS_MAGIC) {
        return None;
    }
    let total = usize::try_from(u32_be(bytes, 4)?).ok()?;
    let bytes = bytes.get(..total)?;
    let mut entries = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let kind = bytes.get(at..at + 4)?;
        let length = usize::try_from(u32_be(bytes, at + 4)?).ok()?;
        if length < 8 {
            return None;
        }
        entries.push(IcnsEntry {
            kind: [kind[0], kind[1], kind[2], kind[3]],
            data: bytes.get(at + 8..at.checked_add(length)?)?,
        });
        at += length;
    }
    Some(entries)
}

/// The pixel size of an ICNS picture type, for the types whose data does not say it.
fn icns_type_size(kind: [u8; 4]) -> Option<u32> {
    Some(match &kind {
        b"is32" | b"ics#" | b"ics4" | b"ics8" | b"icp4" | b"ic04" => 16,
        b"icsb" => 18,
        b"sb24" => 24,
        b"il32" | b"ICON" | b"ICN#" | b"icl4" | b"icl8" | b"icp5" | b"ic05" | b"ic11" => 32,
        b"icsB" => 36,
        b"ih32" | b"ich#" | b"ich4" | b"ich8" | b"SB24" => 48,
        b"icp6" | b"ic12" => 64,
        b"it32" | b"ic07" => 128,
        b"ic08" | b"ic13" => 256,
        b"ic09" | b"ic14" => 512,
        b"ic10" => 1024,
        _ => return None,
    })
}

/// Elements of an ICNS file that are not pictures.
const ICNS_NOT_PICTURES: &[&[u8; 4]] = &[b"TOC ", b"icnV", b"info", b"name"];

/// How deep ICNS files inside ICNS files (the dark and selected variants) are followed.
const MAX_ICNS_DEPTH: u8 = 4;

fn compress_icns(
    bytes: &[u8],
    palette_quality: Option<(u8, u8)>,
    stats: &mut Stats,
    depth: u8,
) -> Result<Vec<u8>, String> {
    let entries =
        parse_icns(bytes).ok_or("this icon file is damaged: its elements cannot be read")?;
    let mut elements: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let data = if entry.data.starts_with(ICNS_MAGIC) && depth < MAX_ICNS_DEPTH {
            // A variant of the icon (dark, selected) is an ICNS file of its own.
            match compress_icns(entry.data, palette_quality, stats, depth + 1) {
                Ok(inner) if inner.len() < entry.data.len() => inner,
                _ => entry.data.to_vec(),
            }
        } else {
            if !ICNS_NOT_PICTURES.contains(&&entry.kind) {
                stats.pictures += 1;
            }
            let smaller = entry
                .data
                .starts_with(PNG_SIGNATURE)
                .then(|| recompress_png(entry.data, palette_quality))
                .flatten();
            stats.recompressed += u32::from(smaller.is_some());
            smaller.unwrap_or_else(|| entry.data.to_vec())
        };
        elements.push((entry.kind, data));
    }
    let too_large = |_| "this icon file is too large".to_owned();
    // The table of contents lists the type and length of every other element.
    let mut contents = Vec::new();
    for (kind, data) in elements.iter().filter(|(kind, _)| kind != b"TOC ") {
        contents.extend_from_slice(kind);
        contents.extend(
            u32::try_from(data.len() + 8)
                .map_err(too_large)?
                .to_be_bytes(),
        );
    }
    let mut out = ICNS_MAGIC.to_vec();
    out.extend([0; 4]);
    for (kind, data) in &elements {
        let data = if kind == b"TOC " { &contents } else { data };
        out.extend_from_slice(kind);
        out.extend(
            u32::try_from(data.len() + 8)
                .map_err(too_large)?
                .to_be_bytes(),
        );
        out.extend_from_slice(data);
    }
    let total = u32::try_from(out.len()).map_err(too_large)?;
    out[4..8].copy_from_slice(&total.to_be_bytes());
    Ok(out)
}

/// The width and height in a PNG's header.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(PNG_SIGNATURE) || data.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((u32_be(data, 16)?, u32_be(data, 20)?))
}

/// The longer side of a PNG picture.
fn png_size(data: &[u8]) -> Option<u32> {
    png_dimensions(data).map(|(width, height)| width.max(height))
}

/// A PNG picture of an icon compressed again, when that is smaller. Pictures that are not
/// plain 8-bit PNGs (16-bit, animated) are left alone.
fn recompress_png(data: &[u8], palette_quality: Option<(u8, u8)>) -> Option<Vec<u8>> {
    let mut decoder = image::codecs::png::PngDecoder::new(Cursor::new(data)).ok()?;
    if decoder.is_apng().ok()?
        || decoder.color_type().bits_per_pixel()
            != u16::from(decoder.color_type().channel_count()) * 8
    {
        return None;
    }
    let icc = decoder.icc_profile().ok()?;
    let rgba = DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
    let png = icon_png(&rgba, icc.as_deref(), palette_quality)?;
    (png.len() < data.len()).then_some(png)
}

/// Encodes a picture as an 8-bit RGBA PNG, the form icon files take: losslessly, and with its
/// colors reduced to a palette's when `palette_quality` can be met; the smaller one is used.
fn icon_png(
    rgba: &RgbaImage,
    icc: Option<&[u8]>,
    palette_quality: Option<(u8, u8)>,
) -> Option<Vec<u8>> {
    // Recompression only: a reduction would change the PNG's color type.
    let options = oxipng::Options {
        strip: oxipng::StripChunks::Safe,
        bit_depth_reduction: false,
        color_type_reduction: false,
        palette_reduction: false,
        grayscale_reduction: false,
        interlace: Some(false),
        ..oxipng::Options::from_preset(2)
    };
    let encode = |pixels: &[u8]| {
        let png = write_png(rgba.width(), rgba.height(), &PngPixels::Rgba(pixels), icc).ok()?;
        oxipng::optimize_from_memory(&png, &options).ok()
    };
    let lossless = encode(rgba.as_raw())?;
    // imagequant reports some failures by panicking.
    let reduced = palette_quality.and_then(|(min, max)| {
        let quantized = std::panic::catch_unwind(|| quantize(rgba, min, max))
            .ok()?
            .ok()??;
        let (palette, indexes, _) = quantized;
        let pixels: Vec<u8> = indexes
            .iter()
            .flat_map(|index| {
                let color = palette[usize::from(*index)];
                [color.r, color.g, color.b, color.a]
            })
            .collect();
        encode(&pixels)
    });
    Some(match reduced {
        Some(reduced) if reduced.len() < lossless.len() => reduced,
        _ => lossless,
    })
}
