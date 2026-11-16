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

use image::{DynamicImage, GrayImage, ImageBuffer, RgbImage};

use crate::ffmpeg::{Toolchain, command};
use crate::hdr::HdrColor;
use crate::heif::MapDescription;
use crate::image::Decoded;
use crate::metadata::Metadata;

/// The FFmpeg encoder HEIC output needs.
pub const ENCODER: &str = "libx265";

/// The x265 CRF for a quality setting on the 0–100 scale the image encoders share: 100 is
/// CRF 0, and every two steps of quality are one step of CRF.
pub fn crf(quality: u8) -> u8 {
    (100 - quality.min(100)) / 2
}

/// Encodes a picture as HEIC at a quality setting (0–100), with its color profile, with its
/// transparency when it has any, and with the HDR gain map a HEIC or AVIF input had. Returns
/// the file, and whether a gain map is in it.
pub fn encode(
    tools: &Toolchain,
    prepared: &Decoded,
    quality: u8,
    metadata: Option<&Metadata>,
) -> Result<(Vec<u8>, bool), String> {
    let (width, height) = (prepared.image.width(), prepared.image.height());
    let rgba = prepared.image.to_rgba8();
    let has_alpha = rgba.pixels().any(|p| p[3] < 255);
    // HEVC with 4:2:0 color codes even sizes: repeat the last column and row to get there.
    let coded = (width + width % 2, height + height % 2);
    let color = padded(&DynamicImage::ImageRgba8(rgba.clone()).to_rgb8(), coded);
    // A picture that is HDR by its transfer function is coded at 10 bits, as it is.
    let deep = prepared
        .hdr
        .map(|hdr| (padded(&prepared.image.to_rgb16(), coded), hdr));
    let workdir = crate::compress::work_folder()?;
    let result = (|| {
        let plane = match &deep {
            Some((pixels, hdr)) => Plane::Deep(pixels, *hdr),
            None => Plane::Color(&color),
        };
        let color = hevc_picture(tools, &workdir, plane, crf(quality))?;
        let alpha = if has_alpha {
            let alpha =
                GrayImage::from_fn(width, height, |x, y| image::Luma([rgba.get_pixel(x, y)[3]]));
            let alpha = padded(&alpha, coded);
            // The shape of a picture shows every error: its plane gets a finer setting.
            let finer = crf(quality).saturating_sub(8);
            Some(hevc_picture(tools, &workdir, Plane::Alpha(&alpha), finer)?)
        } else {
            None
        };
        // The gain map: when it cannot be encoded, the standard picture is still written.
        let gain_map = prepared
            .heif_gain_map
            .as_ref()
            .and_then(|map| gain_map_picture(tools, &workdir, map, (width, height), quality).ok());
        let kept_gain_map = gain_map.is_some();
        let file = container(
            &Picture {
                size: (width, height),
                coded,
                color,
                alpha,
                gain_map,
                hdr: prepared.hdr,
            },
            prepared.icc.as_deref(),
            metadata,
        )?;
        Ok((file, kept_gain_map))
    })();
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

/// The gain map for a picture that is now `size`: scaled by the same factor as the picture
/// (to even sides, which HEVC codes), and coded like a plane of its own.
fn gain_map_picture<'a>(
    tools: &Toolchain,
    workdir: &Path,
    map: &'a crate::heif::GainMap,
    size: (u32, u32),
    quality: u8,
) -> Result<MapPicture<'a>, String> {
    let pixels = map.pixels_for(size, true);
    // A gain map is smooth, and its errors show as halos: a finer setting, as for alpha.
    let finer = crf(quality).saturating_sub(8);
    let gray = pixels.color().channel_count() == 1;
    let coded = if gray {
        hevc_picture(tools, workdir, Plane::Alpha(&pixels.to_luma8()), finer)?
    } else {
        hevc_picture(tools, workdir, Plane::Color(&pixels.to_rgb8()), finer)?
    };
    let channels: &[u8] = if gray { &[1, 8] } else { &[3, 8, 8, 8] };
    Ok(MapPicture {
        kind: *b"hvc1",
        data: coded.frame,
        properties: vec![
            (boxed(*b"hvcC", &coded.configuration), true),
            (size_box((pixels.width(), pixels.height())), false),
            (full_box(*b"pixi", 0, 0, channels), false),
            // As it was just coded, whatever the input said about its own gain map picture.
            (color_coding(true), false),
        ],
        description: &map.description,
    })
}

/// `image` enlarged to `size` by repeating its last column and row.
fn padded<P: image::Pixel>(
    image: &ImageBuffer<P, Vec<P::Subpixel>>,
    size: (u32, u32),
) -> ImageBuffer<P, Vec<P::Subpixel>> {
    if image.dimensions() == size {
        return image.clone();
    }
    ImageBuffer::from_fn(size.0, size.1, |x, y| {
        *image.get_pixel(x.min(image.width() - 1), y.min(image.height() - 1))
    })
}

/// The pixels handed to the HEVC encoder.
#[derive(Clone, Copy)]
enum Plane<'a> {
    Color(&'a RgbImage),
    Alpha(&'a GrayImage),
    /// An HDR picture at 16 bits per channel, coded at 10 bits with what its values mean.
    Deep(&'a ImageBuffer<image::Rgb<u16>, Vec<u16>>, HdrColor),
}

/// One HEVC-coded picture: the decoder configuration (`hvcC` payload) and the coded frame.
struct Coded {
    configuration: Vec<u8>,
    frame: Vec<u8>,
}

/// Encodes one picture with libx265 and takes the decoder configuration and the frame out of
/// the MP4 that FFmpeg writes.
fn hevc_picture(tools: &Toolchain, workdir: &Path, plane: Plane, crf: u8) -> Result<Coded, String> {
    let deep_bytes: Vec<u8>;
    let (input_format, size, pixels, output_format): (_, _, &[u8], _) = match plane {
        Plane::Color(image) => ("rgb24", image.dimensions(), image.as_raw(), "yuv420p"),
        Plane::Alpha(image) => ("gray", image.dimensions(), image.as_raw(), "gray"),
        Plane::Deep(image, _) => {
            deep_bytes = image
                .as_raw()
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            ("rgb48le", image.dimensions(), &deep_bytes, "yuv420p10le")
        }
    };
    // How the stream says what its values mean: BT.709 with the sRGB curve, or what an HDR
    // picture's values are (the matrix follows its primaries).
    let (matrix, colorspace, primaries, transfer) = match plane {
        Plane::Deep(_, hdr) => {
            let (primaries, transfer) = hdr.names();
            if hdr.matrix().0 == 9 {
                ("bt2020", "bt2020nc", primaries, transfer)
            } else {
                ("bt709", "bt709", primaries, transfer)
            }
        }
        _ => ("bt709", "bt709", "bt709", "iec61966-2-1"),
    };
    let output = workdir.join("picture.mp4");
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", input_format])
        .args(["-s", &format!("{}x{}", size.0, size.1), "-i", "pipe:0"])
        // Full range for the conversion from RGB, and the same matrix written into the
        // stream, so every decoder converts back the same way.
        .args([
            "-vf",
            &format!("scale=out_color_matrix={matrix}:out_range=full,format={output_format}"),
        ])
        .args(["-frames:v", "1", "-c:v", ENCODER, "-crf", &crf.to_string()])
        .args(["-preset", "slow", "-x265-params", "log-level=error:info=0"])
        .args(["-color_range", "pc", "-colorspace", colorspace])
        .args(["-color_primaries", primaries, "-color_trc", transfer])
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
struct Picture<'a> {
    /// The size shown.
    size: (u32, u32),
    /// The size coded: `size` rounded up to even numbers.
    coded: (u32, u32),
    color: Coded,
    alpha: Option<Coded>,
    gain_map: Option<MapPicture<'a>>,
    /// The picture is HDR by its transfer function, coded at 10 bits.
    hdr: Option<HdrColor>,
}

/// An HDR gain map, coded, with what the input said about it.
pub(crate) struct MapPicture<'a> {
    /// The item type of the coded picture.
    pub kind: [u8; 4],
    pub data: Vec<u8>,
    /// The picture's properties as whole boxes, and whether a reader must understand each.
    pub properties: Vec<(Vec<u8>, bool)>,
    pub description: &'a MapDescription,
}

/// A reference from one item to others: its type, the item it is from, and the items it is to.
pub(crate) type Reference = ([u8; 4], u16, Vec<u16>);
/// A group of items: its type, its own ID, and its members.
pub(crate) type Group = ([u8; 4], u32, Vec<u16>);

/// An item of the HEIF file.
pub(crate) struct Item {
    pub id: u16,
    pub kind: [u8; 4],
    /// Not a picture to show by itself (a tile of a grid, a gain map).
    pub hidden: bool,
    pub data: Vec<u8>,
    /// Properties, by their place in the property list (from 1), and whether a reader must
    /// understand each.
    pub properties: Vec<(u8, bool)>,
    /// For a `mime` item: what its data is.
    pub content_type: Option<&'static str>,
}

/// The item IDs of what goes with a picture, the same in every file written here.
pub(crate) const GAIN_MAP_ITEM: u16 = 7;
pub(crate) const TONE_MAP_ITEM: u16 = 8;
/// The XMP packet of a gain map that is an auxiliary picture (which has no `tmap` item).
const MAP_NOTE_ITEM: u16 = 8;
const ALTERNATIVES_GROUP: u32 = 9;

/// The kind of HEIF file to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Heic,
    /// A HEIC whose picture has 10 bits per channel: another HEVC profile, with a brand of
    /// its own.
    HeicTenBit,
    Avif,
}

pub(crate) fn boxed(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
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
pub(crate) fn full_box(kind: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
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
pub(crate) fn size_box((width, height): (u32, u32)) -> Vec<u8> {
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

/// The property that says what the values of an HDR picture mean: its primaries, PQ or HLG,
/// the matrix that goes with the primaries, full range.
fn hdr_coding(hdr: HdrColor) -> Vec<u8> {
    let mut nclx = b"nclx".to_vec();
    nclx.extend(u16::from(hdr.primaries).to_be_bytes());
    nclx.extend(u16::from(hdr.transfer).to_be_bytes());
    nclx.extend(u16::from(hdr.matrix().0).to_be_bytes());
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
    let coding = add(match picture.hdr {
        Some(hdr) => hdr_coding(hdr),
        None => color_coding(icc.is_some()),
    });
    let profile = icc.map(|icc| add(boxed(*b"colr", &[b"prof", icc].concat())));
    let depth = if picture.hdr.is_some() { 10 } else { 8 };
    let three_channels = add(full_box(*b"pixi", 0, 0, &[3, depth, depth, depth]));
    // The brightest pixel and the brightest frame average of an HDR picture, when known.
    let light_level = picture
        .hdr
        .and_then(|hdr| hdr.light_level)
        .map(|(brightest, average)| {
            let levels = [brightest.to_be_bytes(), average.to_be_bytes()].concat();
            add(boxed(*b"clli", &levels))
        });

    let odd = picture.coded != picture.size;
    let mut items = Vec::new();
    let mut references: Vec<Reference> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    // What a picture's item carries, whether it is the coded picture or a grid over it.
    let mut shown = vec![(shown_size, false), (coding, false)];
    shown.extend(profile.map(|profile| (profile, false)));
    shown.push((three_channels, false));
    shown.extend(light_level.map(|levels| (levels, false)));
    let primary = 1;
    let grid = odd.then_some(picture.size);
    let coded = [(color_configuration, true), (coded_size, false)];
    picture_items(
        primary,
        &picture.color.frame,
        grid,
        shown,
        coded,
        &mut items,
    );
    if odd {
        references.push((*b"dimg", primary, vec![2]));
    }
    if let Some(alpha) = &picture.alpha {
        let sizes = (shown_size, coded_size);
        alpha_items(
            alpha,
            odd.then_some(picture.size),
            sizes,
            &mut properties,
            &mut items,
        );
        if odd {
            references.push((*b"dimg", 3, vec![4]));
        }
        // The alpha picture is an auxiliary picture of the primary one.
        references.push((*b"auxl", 3, vec![primary]));
    }
    if let Some(map) = &picture.gain_map {
        let size = (shown_size, primary);
        groups.extend(gain_map_items(
            map,
            size,
            &mut properties,
            &mut items,
            &mut references,
        ));
    }
    let with_note = headroom_note(picture.gain_map.as_ref(), metadata);
    let metadata = with_note.as_ref().or(metadata);
    metadata_items(metadata, primary, &mut items, &mut references);
    let parts = Parts {
        items: &items,
        primary,
        references: &references,
        groups: &groups,
        properties: &properties,
    };
    let format = if picture.hdr.is_some() {
        Format::HeicTenBit
    } else {
        Format::Heic
    };
    write_file(&parts, format)
}

/// An iPhone's gain map needs two values of the picture's maker note. When the picture's EXIF
/// is not written, they are written by themselves: the metadata to write then, with them as
/// its EXIF.
fn headroom_note(map: Option<&MapPicture>, metadata: Option<&Metadata>) -> Option<Metadata> {
    let MapDescription::Auxiliary(auxiliary) = map?.description else {
        return None;
    };
    if metadata.is_some_and(|kept| kept.exif.is_some()) {
        return None;
    }
    Some(Metadata {
        exif: Some(auxiliary.headroom_note.clone()?),
        xmp: metadata.and_then(|kept| kept.xmp.clone()),
        photoshop: None,
    })
}

/// Adds a picture as item `id`: the coded picture itself, or for a picture of an odd size
/// (`grid`) a grid of that size over the coded picture, which is then item `id + 1`. `shown`
/// are the properties of the picture as it is shown, and `coded` those of the coded picture
/// alone: its decoder configuration and its size.
fn picture_items(
    id: u16,
    frame: &[u8],
    grid: Option<(u32, u32)>,
    shown: Vec<(u8, bool)>,
    [configuration, coded_size]: [(u8, bool); 2],
    items: &mut Vec<Item>,
) {
    if let Some(size) = grid {
        items.push(Item {
            id,
            kind: *b"grid",
            hidden: false,
            data: grid_data(size),
            properties: shown,
            content_type: None,
        });
        items.push(Item {
            id: id + 1,
            kind: *b"hvc1",
            hidden: true,
            data: frame.to_vec(),
            properties: vec![configuration, coded_size],
            content_type: None,
        });
    } else {
        let mut all = vec![configuration];
        all.extend(shown);
        items.push(Item {
            id,
            kind: *b"hvc1",
            hidden: false,
            data: frame.to_vec(),
            properties: all,
            content_type: None,
        });
    }
}

/// Adds the alpha picture (item 3, and item 4 under a grid). `sizes` are the size properties
/// of the picture shown and of the picture coded.
fn alpha_items(
    alpha: &Coded,
    grid: Option<(u32, u32)>,
    (shown_size, coded_size): (u8, u8),
    properties: &mut Vec<Vec<u8>>,
    items: &mut Vec<Item>,
) {
    let mut add = |property: Vec<u8>| {
        properties.push(property);
        u8::try_from(properties.len()).unwrap_or(u8::MAX)
    };
    let configuration = add(boxed(*b"hvcC", &alpha.configuration));
    let one_channel = add(full_box(*b"pixi", 0, 0, &[1, 8]));
    let is_alpha = add(full_box(*b"auxC", 0, 0, b"urn:mpeg:hevc:2015:auxid:1\0"));
    let marks = vec![(shown_size, false), (one_channel, false), (is_alpha, false)];
    let coded = [(configuration, true), (coded_size, false)];
    picture_items(3, &alpha.frame, grid, marks, coded, items);
}

/// Adds the items of an HDR gain map: the gain map picture, and what ties it to the primary
/// picture, the way the input did. `shown` is the primary picture's size property and its
/// item. Returns the group, if any, that the file needs for it.
pub(crate) fn gain_map_items(
    map: &MapPicture,
    (shown_size, primary): (u8, u16),
    properties: &mut Vec<Vec<u8>>,
    items: &mut Vec<Item>,
    references: &mut Vec<Reference>,
) -> Option<Group> {
    let mut add = |property: Vec<u8>| listed(properties, property);
    // The gain map picture, not to be shown by itself.
    let mut own: Vec<(u8, bool)> = map
        .properties
        .iter()
        .map(|(property, essential)| (add(property.clone()), *essential))
        .collect();
    let picture = |own| Item {
        id: GAIN_MAP_ITEM,
        kind: map.kind,
        hidden: true,
        data: map.data.clone(),
        properties: own,
        content_type: None,
    };
    match map.description {
        // ISO 21496-1: an item that stands for the HDR rendition, derived from the primary
        // picture and the gain map. How the gain map is applied is its data, and it is
        // described as the input described it (the rendition's color coding and bit depth).
        MapDescription::Iso(tone_map) => {
            items.push(picture(own));
            let mut rendition = vec![(shown_size, false)];
            for property in &tone_map.rendition {
                rendition.push((add(property.clone()), false));
            }
            items.push(Item {
                id: TONE_MAP_ITEM,
                kind: *b"tmap",
                hidden: false,
                data: tone_map.metadata.clone(),
                properties: rendition,
                content_type: None,
            });
            references.push((*b"dimg", TONE_MAP_ITEM, vec![primary, GAIN_MAP_ITEM]));
            // A reader that knows gain maps shows the HDR rendition in the picture's place.
            Some((*b"altr", ALTERNATIVES_GROUP, vec![TONE_MAP_ITEM, primary]))
        }
        // As iPhones stored it before: an auxiliary picture of the primary one, with an XMP
        // packet of its own.
        MapDescription::Auxiliary(auxiliary) => {
            let kind = [crate::heif::APPLE_GAIN_MAP, &[0]].concat();
            own.push((add(full_box(*b"auxC", 0, 0, &kind)), false));
            items.push(picture(own));
            references.push((*b"auxl", GAIN_MAP_ITEM, vec![primary]));
            if let Some(xmp) = &auxiliary.xmp {
                items.push(Item {
                    id: MAP_NOTE_ITEM,
                    kind: *b"mime",
                    hidden: true,
                    data: xmp.clone(),
                    properties: Vec::new(),
                    content_type: Some("application/rdf+xml"),
                });
                references.push((*b"cdsc", MAP_NOTE_ITEM, vec![GAIN_MAP_ITEM]));
            }
            None
        }
    }
}

/// The place of a property in the property list (from 1), where it is added unless it is
/// there already.
pub(crate) fn listed(properties: &mut Vec<Vec<u8>>, property: Vec<u8>) -> u8 {
    let place = properties
        .iter()
        .position(|listed| *listed == property)
        .unwrap_or_else(|| {
            properties.push(property);
            properties.len() - 1
        });
    u8::try_from(place + 1).unwrap_or(u8::MAX)
}

/// Adds the EXIF and XMP items, which describe the primary picture.
pub(crate) fn metadata_items(
    metadata: Option<&Metadata>,
    primary: u16,
    items: &mut Vec<Item>,
    references: &mut Vec<Reference>,
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
        references.push((*b"cdsc", 5, vec![primary]));
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
        references.push((*b"cdsc", 6, vec![primary]));
    }
}

/// The contents of the item list (`iinf`): a count, then what each item is.
fn item_infos(items: &[Item]) -> Vec<u8> {
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
    infos
}

/// The boxes of the item references (`iref`), one for each.
fn reference_boxes(references: &[Reference]) -> Vec<u8> {
    let mut links = Vec::new();
    for (kind, from, to) in references {
        let mut link = from.to_be_bytes().to_vec();
        link.extend(u16::try_from(to.len()).unwrap_or(u16::MAX).to_be_bytes());
        for item in to {
            link.extend(item.to_be_bytes());
        }
        links.extend(boxed(*kind, &link));
    }
    links
}

/// The boxes of the item groups (`grpl`), one for each.
fn group_boxes(groups: &[Group]) -> Vec<u8> {
    let mut grouped = Vec::new();
    for (kind, id, members) in groups {
        let mut group = id.to_be_bytes().to_vec();
        group.extend(
            u32::try_from(members.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for member in members {
            group.extend(u32::from(*member).to_be_bytes());
        }
        grouped.extend(full_box(*kind, 0, 0, &group));
    }
    grouped
}

/// What a HEIF file is made of.
pub(crate) struct Parts<'a> {
    pub items: &'a [Item],
    pub primary: u16,
    pub references: &'a [Reference],
    pub groups: &'a [Group],
    /// The property list, which the items point into.
    pub properties: &'a [Vec<u8>],
}

pub(crate) fn write_file(parts: &Parts, format: Format) -> Result<Vec<u8>, String> {
    let Parts {
        items,
        primary,
        references,
        groups,
        properties,
    } = *parts;
    let (mut brands, name) = match format {
        Format::Heic => (b"heic\0\0\0\0mif1heic".to_vec(), "a HEIC"),
        Format::HeicTenBit => (b"heix\0\0\0\0mif1heix".to_vec(), "a HEIC"),
        Format::Avif => (b"avif\0\0\0\0avifmif1miaf".to_vec(), "an AVIF"),
    };
    // The `tmap` brand says that the file may hold a gain map.
    if !groups.is_empty() {
        brands.extend_from_slice(b"tmap");
    }
    let ftyp = boxed(*b"ftyp", &brands);
    let mut handler = vec![0; 4];
    handler.extend_from_slice(b"pict");
    handler.extend([0; 13]);
    let infos = item_infos(items);
    let (links, grouped) = (reference_boxes(references), group_boxes(groups));
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
            let too_large = |_| format!("the picture is too large for {name} file");
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
        if !grouped.is_empty() {
            body.extend(boxed(*b"grpl", &grouped));
        }
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
                gain_map: None,
                hdr: None,
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
                gain_map: None,
                hdr: None,
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
    fn an_hdr_picture_gets_its_own_brand_and_color_description() {
        let hdr = HdrColor {
            primaries: 9,
            transfer: crate::hdr::PQ,
            light_level: Some((1000, 180)),
        };
        let file = container(
            &Picture {
                size: (641, 480),
                coded: (642, 480),
                color: coded(7),
                alpha: None,
                gain_map: None,
                hdr: Some(hdr),
            },
            None,
            None,
        )
        .unwrap();
        // A HEIC with 10 bits per channel is not the profile every reader of `heic` knows.
        assert_eq!(&file[4..24], b"ftypheix\0\0\0\0mif1heix");
        // Read back: PQ with BT.2020 primaries and the light levels, also under a grid.
        assert_eq!(crate::heif::read_properties(&file).hdr, Some(hdr));
        // The matrix that goes with the primaries, full range; ten bits in three channels.
        let coding = [&b"colrnclx"[..], &[0, 9, 0, 16, 0, 9, 0x80]].concat();
        assert!(file.windows(coding.len()).any(|w| w == coding));
        let depth = [&b"pixi"[..], &[0, 0, 0, 0, 3, 10, 10, 10]].concat();
        assert!(file.windows(depth.len()).any(|w| w == depth));
    }

    #[test]
    fn a_gain_map_stored_as_an_auxiliary_picture_is_written_that_way() {
        use crate::heif::AuxiliaryMap;
        let note = crate::exif::apple_hdr_note(&apple_exif()).unwrap();
        let description = MapDescription::Auxiliary(AuxiliaryMap {
            map_item: 10,
            xmp: Some(b"<x:xmpmeta>HDRGainMap</x:xmpmeta>".to_vec()),
            shown: None,
            headroom_note: Some(note.clone()),
        });
        let with = |metadata: Option<&Metadata>| {
            let picture = Picture {
                size: (640, 480),
                coded: (640, 480),
                color: coded(7),
                alpha: None,
                gain_map: Some(MapPicture {
                    kind: *b"hvc1",
                    data: vec![5; 30],
                    properties: vec![(boxed(*b"hvcC", &[1, 5, 5]), true)],
                    description: &description,
                }),
                hdr: None,
            };
            container(&picture, None, metadata).unwrap()
        };
        let file = with(None);
        // No `tmap` item and no brand for one: the gain map is an auxiliary picture.
        assert_eq!(&file[4..24], b"ftypheic\0\0\0\0mif1heic");
        let properties = crate::heif::read_properties(&file);
        assert!(properties.gain_map);
        assert_eq!((properties.alpha, &properties.tone_map), (None, &None));
        let MapDescription::Auxiliary(written) = &description else {
            unreachable!()
        };
        assert_eq!(
            properties.auxiliary_map,
            Some(AuxiliaryMap {
                map_item: 7,
                ..written.clone()
            })
        );
        // The picture's EXIF is only the two values the gain map needs, and the gain map's
        // XMP is not the picture's.
        let metadata = crate::heif::read_metadata(&file);
        assert_eq!(metadata.exif.as_deref(), Some(&note[..]));
        assert_eq!(metadata.xmp, None);

        // With the picture's own EXIF and XMP, those are written, whole.
        let kept = Metadata {
            exif: Some(apple_exif()),
            xmp: Some(b"<x:xmpmeta>the picture's</x:xmpmeta>".to_vec()),
            photoshop: None,
        };
        let file = with(Some(&kept));
        let metadata = crate::heif::read_metadata(&file);
        assert_eq!(metadata.exif, kept.exif);
        assert_eq!(metadata.xmp, kept.xmp);
        let properties = crate::heif::read_properties(&file).auxiliary_map.unwrap();
        assert_eq!(properties.headroom_note, Some(note));
        assert_eq!(properties.xmp, written.xmp);
    }

    /// An EXIF block with a camera name and an Apple maker note that has the two HDR values.
    fn apple_exif() -> Vec<u8> {
        // The maker note: its name, then a directory of two signed fractions whose values
        // follow it (offsets count from the note's start).
        let mut note = b"Apple iOS\0\0\x01MM".to_vec();
        note.extend([0, 2]);
        note.extend([0, 0x21, 0, 10, 0, 0, 0, 1, 0, 0, 0, 44]);
        note.extend([0, 0x30, 0, 10, 0, 0, 0, 1, 0, 0, 0, 52]);
        note.extend([0; 4]);
        note.extend([0, 1, 0xCC, 0xA6, 0, 1, 0xC1, 0x31]);
        note.extend([0, 0, 3, 1, 0, 6, 0xF6, 0]);
        // The block: a first directory with the camera's maker and the pointer to the EXIF
        // directory, which holds the maker note.
        let mut exif = b"MM\0*\0\0\0\x08".to_vec();
        exif.extend([0, 2]);
        exif.extend([0x01, 0x0F, 0, 2, 0, 0, 0, 6, 0, 0, 0, 38]);
        exif.extend([0x87, 0x69, 0, 4, 0, 0, 0, 1, 0, 0, 0, 44]);
        exif.extend([0; 4]);
        exif.extend(b"Apple\0");
        exif.extend([0, 1]);
        exif.extend([0x92, 0x7C, 0, 7]);
        exif.extend(u32::try_from(note.len()).unwrap().to_be_bytes());
        exif.extend(62u32.to_be_bytes());
        exif.extend([0; 4]);
        exif.extend(note);
        exif
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
