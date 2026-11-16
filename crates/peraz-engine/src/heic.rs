//! HEIC output: an HEVC still picture in a HEIF container.
//!
//! FFmpeg encodes the pixels with libx265 (as a one-frame MP4, which carries the decoder
//! configuration ready-made); the HEIF container is written here, following ISO/IEC 23008-12:
//! a `meta` box that describes the items and their properties, and an `mdat` box with their
//! data. Transparency is a second, gray picture marked as the alpha plane of the first. HEVC
//! codes even sizes only, so a picture with an odd width or height is coded one pixel larger
//! and wrapped in a one-tile grid of its true size, the same construction phones use to tile
//! large photos.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use image::{DynamicImage, GrayImage, RgbImage};

use crate::ffmpeg::{Toolchain, command};
use crate::image::Decoded;
use crate::metadata::Metadata;

/// The FFmpeg encoder HEIC output needs.
pub const ENCODER: &str = "libx265";

/// The x265 CRF for a quality setting on the 0–100 scale the image encoders share: 100 is
/// CRF 0, and every two steps of quality are one step of CRF.
pub fn crf(quality: u8) -> u8 {
    (100 - quality.min(100)) / 2
}

/// Encodes a picture as HEIC at a quality setting (0–100), with its color profile, and with
/// its transparency when it has any.
pub fn encode(
    tools: &Toolchain,
    prepared: &Decoded,
    quality: u8,
    metadata: Option<&Metadata>,
) -> Result<Vec<u8>, String> {
    let (width, height) = (prepared.image.width(), prepared.image.height());
    let rgba = prepared.image.to_rgba8();
    let has_alpha = rgba.pixels().any(|p| p[3] < 255);
    // HEVC with 4:2:0 color codes even sizes: repeat the last column and row to get there.
    let coded = (width + width % 2, height + height % 2);
    let color = padded(&DynamicImage::ImageRgba8(rgba.clone()).to_rgb8(), coded);
    let workdir = crate::compress::work_folder()?;
    let result = (|| {
        let color = hevc_picture(tools, &workdir, Plane::Color(&color), crf(quality))?;
        let alpha = if has_alpha {
            let alpha =
                GrayImage::from_fn(width, height, |x, y| image::Luma([rgba.get_pixel(x, y)[3]]));
            let alpha = padded_gray(&alpha, coded);
            // The shape of a picture shows every error: its plane gets a finer setting.
            let finer = crf(quality).saturating_sub(8);
            Some(hevc_picture(tools, &workdir, Plane::Alpha(&alpha), finer)?)
        } else {
            None
        };
        container(
            &Picture {
                size: (width, height),
                coded,
                color,
                alpha,
            },
            prepared.icc.as_deref(),
            metadata,
        )
    })();
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

/// `image` enlarged to `size` by repeating its last column and row.
fn padded(image: &RgbImage, size: (u32, u32)) -> RgbImage {
    if image.dimensions() == size {
        return image.clone();
    }
    RgbImage::from_fn(size.0, size.1, |x, y| {
        *image.get_pixel(x.min(image.width() - 1), y.min(image.height() - 1))
    })
}

fn padded_gray(image: &GrayImage, size: (u32, u32)) -> GrayImage {
    if image.dimensions() == size {
        return image.clone();
    }
    GrayImage::from_fn(size.0, size.1, |x, y| {
        *image.get_pixel(x.min(image.width() - 1), y.min(image.height() - 1))
    })
}

/// The pixels handed to the HEVC encoder.
#[derive(Clone, Copy)]
enum Plane<'a> {
    Color(&'a RgbImage),
    Alpha(&'a GrayImage),
}

/// One HEVC-coded picture: the decoder configuration (`hvcC` payload) and the coded frame.
struct Coded {
    configuration: Vec<u8>,
    frame: Vec<u8>,
}

/// Encodes one picture with libx265 and takes the decoder configuration and the frame out of
/// the MP4 that FFmpeg writes.
fn hevc_picture(tools: &Toolchain, workdir: &Path, plane: Plane, crf: u8) -> Result<Coded, String> {
    let (input_format, size, pixels, output_format): (_, _, &[u8], _) = match plane {
        Plane::Color(image) => ("rgb24", image.dimensions(), image.as_raw(), "yuv420p"),
        Plane::Alpha(image) => ("gray", image.dimensions(), image.as_raw(), "gray"),
    };
    let output = workdir.join("picture.mp4");
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", input_format])
        .args(["-s", &format!("{}x{}", size.0, size.1), "-i", "pipe:0"])
        // Full-range BT.709 for the conversion from RGB, and the same written into the stream,
        // so every decoder converts back the same way.
        .args([
            "-vf",
            &format!("scale=out_color_matrix=bt709:out_range=full,format={output_format}"),
        ])
        .args(["-frames:v", "1", "-c:v", ENCODER, "-crf", &crf.to_string()])
        .args(["-preset", "slow", "-x265-params", "log-level=error:info=0"])
        .args(["-color_range", "pc", "-colorspace", "bt709"])
        .args(["-color_primaries", "bt709", "-color_trc", "iec61966-2-1"])
        .args(["-tag:v", "hvc1", "-an", "-f", "mp4"])
        .arg(crate::compress::file_url(&output)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", tools.ffmpeg.display()))?;
    // FFmpeg reads the whole frame before it writes anything to stderr worth waiting for.
    let written = child
        .stdin
        .take()
        .ok_or("ffmpeg stdin is unavailable".to_owned())
        .and_then(|mut stdin| {
            stdin
                .write_all(pixels)
                .map_err(|e| format!("cannot send the picture to ffmpeg: {e}"))
        });
    let finished = child
        .wait_with_output()
        .map_err(|e| format!("ffmpeg did not finish: {e}"))?;
    if !finished.status.success() {
        let stderr = String::from_utf8_lossy(&finished.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error message");
        return Err(format!("HEVC encoding failed: {}", last.trim()));
    }
    written?;
    let mp4 =
        std::fs::read(&output).map_err(|e| format!("cannot read the encoded picture: {e}"))?;
    let _ = std::fs::remove_file(&output);
    coded_picture(&mp4).ok_or_else(|| "ffmpeg wrote an MP4 that cannot be read back".to_owned())
}

/// Finds the decoder configuration and the single frame in a one-frame MP4.
fn coded_picture(mp4: &[u8]) -> Option<Coded> {
    let be32 = |at: usize| {
        let b = mp4.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let find = |kind: &[u8; 4]| mp4.windows(4).position(|w| w == kind);
    // `hvcC` sits inside the sample description: a box, with its size before its name.
    let at = find(b"hvcC")?;
    let size = be32(at.checked_sub(4)?)?;
    let configuration = mp4.get(at + 4..(at - 4).checked_add(size)?)?.to_vec();
    // One sample: its size from `stsz` (version and flags, a common size or else a table),
    // its place from `stco` (version and flags, a count, then offsets).
    let stsz = find(b"stsz")? + 4;
    let length = match be32(stsz + 4)? {
        0 => be32(stsz + 12)?,
        common => common,
    };
    let offset = be32(find(b"stco")? + 4 + 8)?;
    let frame = mp4.get(offset..offset.checked_add(length)?)?.to_vec();
    (!configuration.is_empty() && !frame.is_empty()).then_some(Coded {
        configuration,
        frame,
    })
}

/// What goes into the container.
struct Picture {
    /// The size shown.
    size: (u32, u32),
    /// The size coded: `size` rounded up to even numbers.
    coded: (u32, u32),
    color: Coded,
    alpha: Option<Coded>,
}

/// An item of the HEIF file.
struct Item {
    id: u16,
    kind: [u8; 4],
    /// Not a picture to show by itself (a tile of a grid).
    hidden: bool,
    data: Vec<u8>,
    /// Properties, by their place in the property list (from 1), and whether a reader must
    /// understand each.
    properties: Vec<(u8, bool)>,
    /// For a `mime` item: what its data is.
    content_type: Option<&'static str>,
}

fn boxed(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend(
        u32::try_from(payload.len() + 8)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&kind);
    out.extend_from_slice(payload);
    out
}

/// A box that starts with a version and flags.
fn full_box(kind: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![version];
    body.extend_from_slice(&flags.to_be_bytes()[1..]);
    body.extend_from_slice(payload);
    boxed(kind, &body)
}

/// The data of a grid item with one tile: the size shown.
fn grid_data((width, height): (u32, u32)) -> Vec<u8> {
    // Version, flags (bit 0: 32-bit sizes), rows minus one, columns minus one, then the size.
    if let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) {
        let mut out = vec![0, 0, 0, 0];
        out.extend(width.to_be_bytes());
        out.extend(height.to_be_bytes());
        return out;
    }
    let mut out = vec![0, 1, 0, 0];
    out.extend(width.to_be_bytes());
    out.extend(height.to_be_bytes());
    out
}

/// The property that gives a picture's size.
fn size_box((width, height): (u32, u32)) -> Vec<u8> {
    full_box(
        *b"ispe",
        0,
        0,
        &[width.to_be_bytes(), height.to_be_bytes()].concat(),
    )
}

/// The property that says how the colors are coded: BT.709 primaries and matrix, the sRGB
/// curve, full range. With a color profile, the profile says what the colors are, and this
/// only how they are stored.
fn color_coding(has_profile: bool) -> Vec<u8> {
    let (primaries, transfer) = if has_profile { (2u16, 2u16) } else { (1, 13) };
    let mut nclx = b"nclx".to_vec();
    nclx.extend(primaries.to_be_bytes());
    nclx.extend(transfer.to_be_bytes());
    nclx.extend(1u16.to_be_bytes());
    nclx.push(0x80);
    boxed(*b"colr", &nclx)
}

/// Writes the HEIF file.
fn container(
    picture: &Picture,
    icc: Option<&[u8]>,
    metadata: Option<&Metadata>,
) -> Result<Vec<u8>, String> {
    // Properties, numbered from 1 in this order.
    let mut properties: Vec<Vec<u8>> = Vec::new();
    let mut add = |property: Vec<u8>| {
        properties.push(property);
        u8::try_from(properties.len()).unwrap_or(u8::MAX)
    };
    let color_configuration = add(boxed(*b"hvcC", &picture.color.configuration));
    let shown_size = add(size_box(picture.size));
    let coded_size = if picture.coded == picture.size {
        shown_size
    } else {
        add(size_box(picture.coded))
    };
    let coding = add(color_coding(icc.is_some()));
    let profile = icc.map(|icc| add(boxed(*b"colr", &[b"prof", icc].concat())));
    let three_channels = add(full_box(*b"pixi", 0, 0, &[3, 8, 8, 8]));

    let odd = picture.coded != picture.size;
    let mut items = Vec::new();
    let mut references: Vec<([u8; 4], u16, u16)> = Vec::new();
    // What a picture's item carries, whether it is the coded picture or a grid over it.
    let mut shown = vec![(shown_size, false), (coding, false)];
    shown.extend(profile.map(|profile| (profile, false)));
    shown.push((three_channels, false));
    let primary = 1;
    if odd {
        items.push(Item {
            id: primary,
            kind: *b"grid",
            hidden: false,
            data: grid_data(picture.size),
            properties: shown,
            content_type: None,
        });
        items.push(Item {
            id: 2,
            kind: *b"hvc1",
            hidden: true,
            data: picture.color.frame.clone(),
            properties: vec![(color_configuration, true), (coded_size, false)],
            content_type: None,
        });
        references.push((*b"dimg", primary, 2));
    } else {
        let mut all = vec![(color_configuration, true)];
        all.extend(shown);
        items.push(Item {
            id: primary,
            kind: *b"hvc1",
            hidden: false,
            data: picture.color.frame.clone(),
            properties: all,
            content_type: None,
        });
    }
    if let Some(alpha) = &picture.alpha {
        let configuration = add(boxed(*b"hvcC", &alpha.configuration));
        let one_channel = add(full_box(*b"pixi", 0, 0, &[1, 8]));
        let is_alpha = add(full_box(*b"auxC", 0, 0, b"urn:mpeg:hevc:2015:auxid:1\0"));
        let marks = vec![(shown_size, false), (one_channel, false), (is_alpha, false)];
        if odd {
            items.push(Item {
                id: 3,
                kind: *b"grid",
                hidden: false,
                data: grid_data(picture.size),
                properties: marks,
                content_type: None,
            });
            items.push(Item {
                id: 4,
                kind: *b"hvc1",
                hidden: true,
                data: alpha.frame.clone(),
                properties: vec![(configuration, true), (coded_size, false)],
                content_type: None,
            });
            references.push((*b"dimg", 3, 4));
        } else {
            let mut all = vec![(configuration, true)];
            all.extend(marks);
            items.push(Item {
                id: 3,
                kind: *b"hvc1",
                hidden: false,
                data: alpha.frame.clone(),
                properties: all,
                content_type: None,
            });
        }
        // The alpha picture is an auxiliary picture of the primary one.
        references.push((*b"auxl", 3, primary));
    }
    metadata_items(metadata, primary, &mut items, &mut references);
    write_file(&items, primary, &references, &properties)
}

/// Adds the EXIF and XMP items, which describe the primary picture.
fn metadata_items(
    metadata: Option<&Metadata>,
    primary: u16,
    items: &mut Vec<Item>,
    references: &mut Vec<([u8; 4], u16, u16)>,
) {
    if let Some(exif) = metadata.and_then(|m| m.exif.as_deref()) {
        items.push(Item {
            id: 5,
            kind: *b"Exif",
            hidden: true,
            // The TIFF structure starts right after these four bytes.
            data: [&[0; 4][..], exif].concat(),
            properties: Vec::new(),
            content_type: None,
        });
        references.push((*b"cdsc", 5, primary));
    }
    if let Some(xmp) = metadata.and_then(|m| m.xmp.as_deref()) {
        items.push(Item {
            id: 6,
            kind: *b"mime",
            hidden: true,
            data: xmp.to_vec(),
            properties: Vec::new(),
            content_type: Some("application/rdf+xml"),
        });
        references.push((*b"cdsc", 6, primary));
    }
}

fn write_file(
    items: &[Item],
    primary: u16,
    references: &[([u8; 4], u16, u16)],
    properties: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    let ftyp = boxed(*b"ftyp", b"heic\0\0\0\0mif1heic");
    let mut handler = vec![0; 4];
    handler.extend_from_slice(b"pict");
    handler.extend([0; 13]);
    let mut infos = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .to_be_bytes()
        .to_vec();
    for item in items {
        let mut info = item.id.to_be_bytes().to_vec();
        info.extend([0, 0]);
        info.extend_from_slice(&item.kind);
        // An empty name, then the content type of a `mime` item.
        info.push(0);
        if let Some(content_type) = item.content_type {
            info.extend_from_slice(content_type.as_bytes());
            info.push(0);
        }
        infos.extend(full_box(*b"infe", 2, u32::from(item.hidden), &info));
    }
    let mut links = Vec::new();
    for (kind, from, to) in references {
        let mut link = from.to_be_bytes().to_vec();
        link.extend(1u16.to_be_bytes());
        link.extend(to.to_be_bytes());
        links.extend(boxed(*kind, &link));
    }
    let described: Vec<&Item> = items.iter().filter(|i| !i.properties.is_empty()).collect();
    let mut associations = u32::try_from(described.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    for item in described {
        associations.extend(item.id.to_be_bytes());
        associations.push(u8::try_from(item.properties.len()).unwrap_or(u8::MAX));
        for (index, essential) in &item.properties {
            associations.push(index | if *essential { 0x80 } else { 0 });
        }
    }
    let property_boxes = [
        boxed(*b"ipco", &properties.concat()),
        full_box(*b"ipma", 0, 0, &associations),
    ]
    .concat();

    // The item locations hold file offsets, which depend on the size of the box they are in:
    // that size does not change with the offsets, so it is measured with offsets of zero.
    let meta = |first_offset: usize| -> Result<Vec<u8>, String> {
        let mut locations = vec![0x44, 0x00];
        locations.extend(u16::try_from(items.len()).unwrap_or(u16::MAX).to_be_bytes());
        let mut offset = first_offset;
        for item in items {
            let too_large = |_| "the picture is too large for a HEIC file".to_owned();
            locations.extend(item.id.to_be_bytes());
            locations.extend([0, 0, 0, 1]);
            locations.extend(u32::try_from(offset).map_err(too_large)?.to_be_bytes());
            locations.extend(
                u32::try_from(item.data.len())
                    .map_err(too_large)?
                    .to_be_bytes(),
            );
            offset += item.data.len();
        }
        let mut body = full_box(*b"hdlr", 0, 0, &handler);
        body.extend(full_box(*b"pitm", 0, 0, &primary.to_be_bytes()));
        body.extend(full_box(*b"iloc", 0, 0, &locations));
        body.extend(full_box(*b"iinf", 0, 0, &infos));
        if !links.is_empty() {
            body.extend(full_box(*b"iref", 0, 0, &links));
        }
        body.extend(boxed(*b"iprp", &property_boxes));
        Ok(full_box(*b"meta", 0, 0, &body))
    };
    let data_start = ftyp.len() + meta(0)?.len() + 8;
    let data: Vec<u8> = items
        .iter()
        .flat_map(|item| item.data.iter().copied())
        .collect();
    let mut out = ftyp;
    out.extend(meta(data_start)?);
    out.extend(boxed(*b"mdat", &data));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coded(tag: u8) -> Coded {
        Coded {
            configuration: vec![1, tag, tag],
            frame: vec![tag; 40],
        }
    }

    #[test]
    fn quality_maps_to_crf() {
        assert_eq!(crf(100), 0);
        assert_eq!(crf(50), 25);
        assert_eq!(crf(10), 45);
        assert_eq!(crf(200), 0);
    }

    #[test]
    fn the_container_describes_what_it_holds() {
        // An even size without transparency: one picture item, read back by the HEIF reader
        // used for inputs.
        let plain = container(
            &Picture {
                size: (640, 480),
                coded: (640, 480),
                color: coded(7),
                alpha: None,
            },
            Some(b"a color profile"),
            Some(&Metadata {
                exif: Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec()),
                xmp: Some(b"<x:xmpmeta/>".to_vec()),
                photoshop: None,
            }),
        )
        .unwrap();
        assert_eq!(&plain[4..12], b"ftypheic");
        let properties = crate::heif::read_properties(&plain);
        assert_eq!(properties.primary, Some(1));
        assert_eq!(properties.alpha, None);
        assert_eq!(properties.icc.as_deref(), Some(&b"a color profile"[..]));
        assert_eq!(properties.transforms, []);
        // The metadata items are found again.
        let metadata = crate::heif::read_metadata(&plain);
        assert_eq!(
            metadata.exif.as_deref(),
            Some(&b"II*\0\x08\0\0\0\0\0\0\0\0\0"[..])
        );
        assert_eq!(metadata.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        // The frame is where the item location says.
        let frame = plain.windows(40).position(|w| w == [7; 40]).unwrap();
        let location = u32::try_from(frame).unwrap().to_be_bytes();
        assert!(plain.windows(4).any(|w| w == location), "offset {frame}");

        // An odd size with transparency: a grid over each coded picture, and the alpha grid
        // marked as the primary one's alpha plane.
        let odd = container(
            &Picture {
                size: (641, 479),
                coded: (642, 480),
                color: coded(7),
                alpha: Some(coded(9)),
            },
            None,
            None,
        )
        .unwrap();
        let properties = crate::heif::read_properties(&odd);
        assert_eq!(properties.primary, Some(1));
        assert_eq!(properties.alpha, Some(3));
        assert_eq!(properties.icc, None);
        assert!(crate::heif::read_metadata(&odd).is_empty());
        for kind in [&b"grid"[..], b"dimg", b"auxl", b"auxC", b"hvcC"] {
            assert!(odd.windows(4).any(|w| w == kind), "{kind:?}");
        }
        // Every box size adds up to the file.
        for file in [&plain, &odd] {
            let mut at = 0;
            while at < file.len() {
                let size = u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]);
                at += size as usize;
            }
            assert_eq!(at, file.len());
        }
    }

    #[test]
    fn padding_repeats_the_last_column_and_row() {
        let image = RgbImage::from_fn(3, 3, |x, y| {
            image::Rgb([u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 0])
        });
        let even = padded(&image, (4, 4));
        assert_eq!(even.get_pixel(3, 1).0, [2, 1, 0]);
        assert_eq!(even.get_pixel(1, 3).0, [1, 2, 0]);
        assert_eq!(padded(&image, (3, 3)), image);
    }

    #[test]
    fn the_frame_and_its_configuration_are_found_in_an_mp4() {
        // Just the boxes that are read: the configuration inside a sample entry, the sample
        // size table, and the chunk offset.
        let frame = [0xAB; 25];
        let mut mp4 = boxed(*b"ftyp", b"isom");
        let frame_at = mp4.len() + 8;
        mp4.extend(boxed(*b"mdat", &frame));
        mp4.extend(boxed(*b"hvcC", &[1, 2, 3, 4]));
        let mut sizes = vec![0; 4];
        sizes.extend(0u32.to_be_bytes());
        sizes.extend(1u32.to_be_bytes());
        sizes.extend(25u32.to_be_bytes());
        mp4.extend(boxed(*b"stsz", &sizes));
        let mut offsets = vec![0; 4];
        offsets.extend(1u32.to_be_bytes());
        offsets.extend(u32::try_from(frame_at).unwrap().to_be_bytes());
        mp4.extend(boxed(*b"stco", &offsets));
        let coded = coded_picture(&mp4).unwrap();
        assert_eq!(coded.configuration, [1, 2, 3, 4]);
        assert_eq!(coded.frame, frame);
        assert!(coded_picture(b"not an mp4").is_none());
    }
}
