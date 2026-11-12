//! HEIC/HEIF and AVIF input.
//!
//! FFmpeg decodes the pixels: a photo is usually stored as a grid of tiles, which are stitched
//! with `xstack` and cropped to the image size. FFmpeg does not apply the HEIF transformations
//! or export the color profile with the pixels, so this module reads them from the file's
//! ISO-BMFF boxes (`pitm`, `ipco`, `ipma`, `colr`, `irot`, `imir`) and applies them itself.
//! Transparency is a separate auxiliary image (`iref` type `auxl`, `auxC` alpha type), decoded
//! like the main picture and merged in as its alpha channel.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;

use image::DynamicImage;
use serde::Deserialize;

use crate::ffmpeg::{Toolchain, check, command};
use crate::image::Decoded;

/// Decodes a HEIF/AVIF still image with its orientation applied and its ICC profile.
pub fn decode(tools: &Toolchain, input: &Path) -> Result<Decoded, String> {
    let url = file_url(input)?;
    let bytes =
        std::fs::read(input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;
    let properties = read_properties(&bytes);
    let (filter, has_alpha) = picture_filter(tools, &url, &properties)?;
    let mut cmd = command(&tools.ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-noautorotate",
        "-i",
    ])
    .arg(&url)
    .args([
        "-filter_complex",
        &filter,
        "-map",
        "[out]",
        "-frames:v",
        "1",
    ])
    .args([
        "-c:v",
        "png",
        "-pix_fmt",
        if has_alpha { "rgba" } else { "rgb24" },
        "-f",
        "image2pipe",
        "pipe:1",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let output = cmd
        .output()
        .map_err(|e| format!("could not run {}: {e}", tools.ffmpeg.display()))?;
    if !output.status.success() {
        let failed = check("ffmpeg", output).unwrap_err();
        return Err(format!("cannot decode {}: {failed}", input.display()));
    }
    let mut image = image::load_from_memory_with_format(&output.stdout, image::ImageFormat::Png)
        .map_err(|e| format!("cannot read the decoded picture: {e}"))?;

    for transform in &properties.transforms {
        image = transform.apply(image);
    }
    Ok(Decoded {
        image,
        icc: properties.icc,
    })
}

fn file_url(path: &Path) -> Result<std::ffi::OsString, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut url = std::ffi::OsString::from("file:");
    url.push(absolute.as_os_str());
    Ok(url)
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    stream_groups: Vec<ProbeGroup>,
}

#[derive(Deserialize)]
struct ProbeStream {
    index: u32,
    /// The HEIF item ID, in hex (`0x7`).
    id: Option<String>,
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    disposition: ProbeDisposition,
}

#[derive(Deserialize, Default)]
struct ProbeDisposition {
    #[serde(default)]
    dependent: u8,
}

#[derive(Deserialize)]
struct ProbeGroup {
    /// The HEIF item ID of the grid, in hex.
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    components: Vec<TileGrid>,
    /// The group's streams: tiles refer to them by position in this list.
    #[serde(default)]
    streams: Vec<GroupStream>,
}

#[derive(Deserialize)]
struct GroupStream {
    index: u32,
}

impl ProbeGroup {
    /// The tile grid, with tiles pointing at the file's stream indexes.
    fn grid(&self) -> Option<TileGrid> {
        if self.kind.as_deref() != Some("Tile Grid") {
            return None;
        }
        let grid = self.components.first()?;
        let subcomponents = grid
            .subcomponents
            .iter()
            .map(|tile| {
                let stream = self.streams.get(usize::try_from(tile.stream_index).ok()?)?;
                Some(Tile {
                    stream_index: stream.index,
                    ..*tile
                })
            })
            .collect::<Option<Vec<_>>>()?;
        (!subcomponents.is_empty()).then_some(TileGrid {
            subcomponents,
            ..*grid
        })
    }
}

#[derive(Deserialize)]
struct TileGrid {
    width: u32,
    height: u32,
    #[serde(default)]
    horizontal_offset: u32,
    #[serde(default)]
    vertical_offset: u32,
    #[serde(default)]
    subcomponents: Vec<Tile>,
}

#[derive(Deserialize, Clone, Copy)]
struct Tile {
    stream_index: u32,
    #[serde(rename = "tile_horizontal_offset")]
    x: u32,
    #[serde(rename = "tile_vertical_offset")]
    y: u32,
}

/// Parses the `0x7`-style item IDs that `ffprobe` prints.
fn item_id(id: Option<&String>) -> Option<u32> {
    u32::from_str_radix(id?.strip_prefix("0x")?, 16).ok()
}

/// Builds the filter graph that produces the full picture as `[out]`: the primary item, with
/// its alpha item merged in when there is one. Returns whether the picture has alpha.
fn picture_filter(
    tools: &Toolchain,
    url: &std::ffi::OsStr,
    properties: &Properties,
) -> Result<(String, bool), String> {
    let mut cmd = command(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        "-show_stream_groups",
        "-i",
    ])
    .arg(url);
    let output = cmd
        .output()
        .map_err(|e| format!("could not run {}: {e}", tools.ffprobe.display()))?;
    let json = check("ffprobe", output).map_err(|e| e.to_string())?;
    let probe: Probe =
        serde_json::from_str(&json).map_err(|e| format!("unexpected ffprobe output: {e}"))?;
    let (color, size) = item_filter(&probe, properties.primary, "color")
        .or_else(|| fallback_filter(&probe, "color"))
        .ok_or("the file contains no picture")?;
    let alpha = properties
        .alpha
        .and_then(|item| item_filter(&probe, Some(item), "alpha"));
    Ok(match alpha {
        // The alpha plane may be stored at another size, and with limited-range values.
        Some((alpha, _)) => (
            format!(
                "{color};{alpha};[alpha]scale=w={}:h={}:in_range=auto:out_range=full,format=gray[a];\
                 [color]format=rgb24[c];[c][a]alphamerge,format=rgba[out]",
                size.0, size.1
            ),
            true,
        ),
        None => (format!("{color};[color]null[out]"), false),
    })
}

/// The filter that produces `item` (a tile grid or a single picture) as `[label]`, and its size.
fn item_filter(probe: &Probe, item: Option<u32>, label: &str) -> Option<(String, (u32, u32))> {
    let item = item?;
    if let Some(grid) = probe
        .stream_groups
        .iter()
        .find(|g| item_id(g.id.as_ref()) == Some(item))
        .and_then(ProbeGroup::grid)
    {
        return Some((grid_filter(&grid, label), (grid.width, grid.height)));
    }
    let stream = probe.streams.iter().find(|s| {
        s.codec_type.as_deref() == Some("video") && item_id(s.id.as_ref()) == Some(item)
    })?;
    Some((
        format!("[0:{}]null[{label}]", stream.index),
        (stream.width?, stream.height?),
    ))
}

/// When the primary item is unknown: the first tile grid, else the first independent picture.
fn fallback_filter(probe: &Probe, label: &str) -> Option<(String, (u32, u32))> {
    if let Some(grid) = probe.stream_groups.iter().find_map(ProbeGroup::grid) {
        return Some((grid_filter(&grid, label), (grid.width, grid.height)));
    }
    let videos = || {
        probe
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("video"))
    };
    let stream = videos()
        .find(|s| s.disposition.dependent == 0)
        .or_else(|| videos().next())?;
    Some((
        format!("[0:{}]null[{label}]", stream.index),
        (stream.width.unwrap_or(0), stream.height.unwrap_or(0)),
    ))
}

fn grid_filter(grid: &TileGrid, label: &str) -> String {
    let inputs = grid.subcomponents.iter().fold(String::new(), |mut acc, t| {
        let _ = write!(acc, "[0:{}]", t.stream_index);
        acc
    });
    let layout: Vec<String> = grid
        .subcomponents
        .iter()
        .map(|t| format!("{}_{}", t.x, t.y))
        .collect();
    let crop = format!(
        "crop={}:{}:{}:{}",
        grid.width, grid.height, grid.horizontal_offset, grid.vertical_offset
    );
    if grid.subcomponents.len() == 1 {
        return format!("{inputs}{crop}[{label}]");
    }
    format!(
        "{inputs}xstack=inputs={}:layout={}:fill=black,{crop}[{label}]",
        grid.subcomponents.len(),
        layout.join("|")
    )
}

/// A HEIF transformative property, applied to the decoded picture in file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// `irot`: rotate counterclockwise by this many quarter turns.
    Rotate { quarter_turns: u8 },
    /// `imir` axis 0: mirror about the horizontal axis (top and bottom swap).
    FlipVertical,
    /// `imir` axis 1: mirror about the vertical axis (left and right swap).
    FlipHorizontal,
}

impl Transform {
    fn apply(self, image: DynamicImage) -> DynamicImage {
        match self {
            // `image` rotates clockwise, HEIF counterclockwise.
            Self::Rotate { quarter_turns: 1 } => image.rotate270(),
            Self::Rotate { quarter_turns: 2 } => image.rotate180(),
            Self::Rotate { quarter_turns: 3 } => image.rotate90(),
            Self::Rotate { .. } => image,
            Self::FlipVertical => image.flipv(),
            Self::FlipHorizontal => image.fliph(),
        }
    }
}

/// What Peraz needs from a HEIF file's item properties.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Properties {
    /// The primary item's ID.
    pub primary: Option<u32>,
    /// The item holding the primary item's transparency, if any.
    pub alpha: Option<u32>,
    pub icc: Option<Vec<u8>>,
    pub transforms: Vec<Transform>,
}

/// `auxC` types that mark an auxiliary image as an alpha plane (HEVC and AV1 conventions).
const ALPHA_TYPES: &[&[u8]] = &[
    b"urn:mpeg:hevc:2015:auxid:1",
    b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
];

/// Reads the primary item, its alpha item, ICC profile, and transformations. Malformed or missing boxes yield
/// empty properties: the picture still decodes, just without them.
pub fn read_properties(file: &[u8]) -> Properties {
    parse_properties(file).unwrap_or_default()
}

fn parse_properties(file: &[u8]) -> Option<Properties> {
    let meta = boxes(file).find(|b| &b.kind == b"meta")?;
    let meta_children = meta.payload.get(4..)?; // full box: version + flags
    let mut primary = None;
    let mut properties: Vec<Bx> = Vec::new();
    let mut associations: Vec<(u32, Vec<u16>)> = Vec::new();
    let mut auxiliary: Vec<(u32, u32)> = Vec::new();
    for child in boxes(meta_children) {
        match &child.kind {
            b"iref" => auxiliary = parse_auxl(child.payload)?,
            b"pitm" => {
                let version = *child.payload.first()?;
                primary = Some(if version == 0 {
                    u32::from(be16(child.payload, 4)?)
                } else {
                    be32(child.payload, 4)?
                });
            }
            b"iprp" => {
                for part in boxes(child.payload) {
                    match &part.kind {
                        b"ipco" => properties = boxes(part.payload).collect(),
                        b"ipma" => associations.extend(parse_ipma(part.payload)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let primary = primary?;
    let properties_of = |id: u32| -> Vec<&Bx> {
        associations
            .iter()
            .filter(|(item, _)| *item == id)
            .flat_map(|(_, indexes)| indexes.iter())
            .filter_map(|&i| properties.get(usize::from(i).checked_sub(1)?))
            .collect()
    };
    let own = properties_of(primary);
    // Auxiliary images of the primary item that are alpha planes (not depth maps or gain maps).
    let alpha = auxiliary
        .iter()
        .filter(|(_, to)| *to == primary)
        .map(|(from, _)| *from)
        .find(|&item| {
            properties_of(item).iter().any(|b| {
                &b.kind == b"auxC"
                    && b.payload.get(4..).is_some_and(|urn| {
                        let urn = urn.split(|c| *c == 0).next().unwrap_or_default();
                        ALPHA_TYPES.contains(&urn)
                    })
            })
        });

    let icc_of = |b: &Bx| {
        (&b.kind == b"colr")
            .then(|| icc_payload(b.payload))
            .flatten()
    };
    // Grid images sometimes attach the profile only to their tiles: fall back to any profile.
    let icc = own
        .iter()
        .find_map(|b| icc_of(b))
        .or_else(|| properties.iter().find_map(icc_of));
    let transforms = own
        .iter()
        .filter_map(|b| match &b.kind {
            b"irot" => Some(Transform::Rotate {
                quarter_turns: b.payload.first()? & 3,
            }),
            b"imir" => Some(if b.payload.first()? & 1 == 0 {
                Transform::FlipVertical
            } else {
                Transform::FlipHorizontal
            }),
            _ => None,
        })
        .collect();
    Some(Properties {
        primary: Some(primary),
        alpha,
        icc,
        transforms,
    })
}

fn icc_payload(colr: &[u8]) -> Option<Vec<u8>> {
    let kind = colr.get(0..4)?;
    (kind == b"prof" || kind == b"rICC").then(|| colr[4..].to_vec())
}

/// `iref`: the (auxiliary item, main item) pairs of its `auxl` references.
fn parse_auxl(payload: &[u8]) -> Option<Vec<(u32, u32)>> {
    let wide = *payload.first()? != 0; // version 1: 32-bit item IDs
    let id = |at: usize| {
        if wide {
            be32(payload, at)
        } else {
            be16(payload, at).map(u32::from)
        }
    };
    let width = if wide { 4 } else { 2 };
    let mut pairs = Vec::new();
    for reference in boxes(payload.get(4..)?) {
        if &reference.kind != b"auxl" {
            continue;
        }
        let at = offset_in(payload, reference.payload)?;
        let from = id(at)?;
        let count = be16(payload, at + width)?;
        for i in 0..usize::from(count) {
            pairs.push((from, id(at + width + 2 + i * width)?));
        }
    }
    Some(pairs)
}

/// `ipma`: for each item, the 1-based indexes of its properties in `ipco`.
fn parse_ipma(payload: &[u8]) -> Option<Vec<(u32, Vec<u16>)>> {
    let version = *payload.first()?;
    let wide_index = payload.get(3)? & 1 == 1; // flags bit 0
    let count = be32(payload, 4)?;
    let mut at = 8;
    let mut items = Vec::new();
    for _ in 0..count {
        let item = if version < 1 {
            let id = u32::from(be16(payload, at)?);
            at += 2;
            id
        } else {
            let id = be32(payload, at)?;
            at += 4;
            id
        };
        let n = *payload.get(at)?;
        at += 1;
        let mut indexes = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            if wide_index {
                indexes.push(be16(payload, at)? & 0x7fff);
                at += 2;
            } else {
                indexes.push(u16::from(*payload.get(at)? & 0x7f));
                at += 1;
            }
        }
        items.push((item, indexes));
    }
    Some(items)
}

/// One ISO-BMFF box: its type and payload (after the header).
#[derive(Debug, Clone, Copy)]
struct Bx<'a> {
    kind: [u8; 4],
    /// Size of the box header: 8, or 16 with a 64-bit size.
    header: usize,
    payload: &'a [u8],
}

/// Adds an ICC profile to a HEIF/AVIF file: a `colr` box of type `prof` in `ipco`, associated
/// with the primary item, and `iloc` offsets moved by the bytes this adds before the image data.
/// Returns `None` when the file's structure is not one this understands.
pub fn embed_icc(file: &[u8], icc: &[u8]) -> Option<Vec<u8>> {
    let top: Vec<Bx> = boxes(file).collect();
    if top
        .iter()
        .map(|b| b.header + b.payload.len())
        .sum::<usize>()
        != file.len()
    {
        return None;
    }
    let meta = top.iter().find(|b| &b.kind == b"meta")?;
    let meta_end = offset_in(file, meta.payload)? + meta.payload.len();
    let (meta_head, meta_children) = meta.payload.split_at_checked(4)?;
    let children: Vec<Bx> = boxes(meta_children).collect();
    let iprp = children.iter().find(|b| &b.kind == b"iprp")?;
    let parts: Vec<Bx> = boxes(iprp.payload).collect();
    // Rewritten boxes get 8-byte headers, so only the added bytes move the image data.
    if [meta, iprp]
        .into_iter()
        .chain(&children)
        .chain(&parts)
        .any(|b| b.header != 8)
    {
        return None;
    }

    let pitm = children.iter().find(|b| &b.kind == b"pitm")?;
    let primary = match *pitm.payload.first()? {
        0 => u32::from(be16(pitm.payload, 4)?),
        _ => be32(pitm.payload, 4)?,
    };

    // New iprp: the profile appended to ipco, and the primary item associated with it.
    let ipco = parts.iter().find(|b| &b.kind == b"ipco")?;
    let index = u16::try_from(boxes(ipco.payload).count() + 1).ok()?;
    let colr = make_box(*b"colr", &[&b"prof"[..], icc].concat())?;
    let mut new_iprp = Vec::new();
    for part in &parts {
        let payload = match &part.kind {
            b"ipco" => [part.payload, &colr].concat(),
            b"ipma" => associate(part.payload, primary, index)?,
            _ => part.payload.to_vec(),
        };
        new_iprp.extend(make_box(part.kind, &payload)?);
    }
    let new_iprp = make_box(*b"iprp", &new_iprp)?;
    let shift = new_iprp
        .len()
        .checked_sub(iprp.header + iprp.payload.len())?;

    let mut new_meta = meta_head.to_vec();
    for child in &children {
        match &child.kind {
            b"iprp" => new_meta.extend_from_slice(&new_iprp),
            b"iloc" => new_meta.extend(make_box(
                *b"iloc",
                &shift_iloc(child.payload, meta_end, shift)?,
            )?),
            _ => new_meta.extend(make_box(child.kind, child.payload)?),
        }
    }
    let new_meta = make_box(*b"meta", &new_meta)?;

    let mut out = Vec::with_capacity(file.len() + shift);
    for b in &top {
        if &b.kind == b"meta" {
            out.extend_from_slice(&new_meta);
        } else {
            let start = offset_in(file, b.payload)? - b.header;
            out.extend_from_slice(&file[start..start + b.header + b.payload.len()]);
        }
    }
    Some(out)
}

/// Where `part` (a subslice of `file`) starts in `file`.
fn offset_in(file: &[u8], part: &[u8]) -> Option<usize> {
    (part.as_ptr() as usize).checked_sub(file.as_ptr() as usize)
}

/// A box with a 32-bit size header.
fn make_box(kind: [u8; 4], payload: &[u8]) -> Option<Vec<u8>> {
    let size = u32::try_from(payload.len() + 8).ok()?;
    Some([&size.to_be_bytes()[..], &kind, payload].concat())
}

/// `ipma` with one more (non-essential) property index for `item`, everything else unchanged.
fn associate(payload: &[u8], item: u32, index: u16) -> Option<Vec<u8>> {
    let version = *payload.first()?;
    let wide_index = payload.get(3)? & 1 == 1;
    if (!wide_index && index > 0x7f) || index > 0x7fff {
        return None;
    }
    let count = be32(payload, 4)?;
    let mut out = payload.get(..8)?.to_vec();
    let mut at = 8;
    let mut found = false;
    for _ in 0..count {
        let id_len = if version < 1 { 2 } else { 4 };
        let id = if version < 1 {
            u32::from(be16(payload, at)?)
        } else {
            be32(payload, at)?
        };
        let n = *payload.get(at + id_len)?;
        let entries_len = usize::from(n) * if wide_index { 2 } else { 1 };
        let entries = payload.get(at + id_len + 1..at + id_len + 1 + entries_len)?;
        out.extend_from_slice(&payload[at..at + id_len]);
        if id == item {
            found = true;
            out.push(n.checked_add(1)?);
            out.extend_from_slice(entries);
            if wide_index {
                out.extend_from_slice(&index.to_be_bytes());
            } else {
                out.push(u8::try_from(index).ok()?);
            }
        } else {
            out.push(n);
            out.extend_from_slice(entries);
        }
        at += id_len + 1 + entries_len;
    }
    (found && at == payload.len()).then_some(out)
}

/// `iloc` with every file offset at or past `from` moved forward by `shift` bytes.
fn shift_iloc(payload: &[u8], from: usize, shift: usize) -> Option<Vec<u8>> {
    let mut out = payload.to_vec();
    let version = *payload.first()?;
    let sizes = be16(payload, 4)?;
    let offset_size = usize::from(sizes >> 12);
    let length_size = usize::from((sizes >> 8) & 0xf);
    let base_offset_size = usize::from((sizes >> 4) & 0xf);
    let index_size = if version >= 1 {
        usize::from(sizes & 0xf)
    } else {
        0
    };
    let (count, mut at) = if version < 2 {
        (u32::from(be16(payload, 6)?), 8)
    } else {
        (be32(payload, 6)?, 10)
    };
    // Rewrites the `width`-byte offset at `at` in place when it points past the shifted bytes.
    let bump = |out: &mut Vec<u8>, at: usize, width: usize| -> Option<()> {
        if width == 0 {
            return Some(());
        }
        let field = out.get_mut(at..at + width)?;
        let value = field.iter().fold(0u64, |v, b| v << 8 | u64::from(*b));
        if value >= u64::try_from(from).ok()? {
            let moved = value.checked_add(u64::try_from(shift).ok()?)?;
            if width < 8 && moved >> (8 * width) != 0 {
                return None;
            }
            field.copy_from_slice(&moved.to_be_bytes()[8 - width..]);
        }
        Some(())
    };
    for _ in 0..count {
        at += if version < 2 { 2 } else { 4 }; // item ID
        let mut file_offsets = true;
        if version >= 1 {
            // Construction method 0 (the low 4 bits): offsets into the file.
            file_offsets = be16(payload, at)?.trailing_zeros() >= 4;
            at += 2;
        }
        at += 2; // data reference index
        let base_offset = at;
        at += base_offset_size;
        let extents = be16(payload, at)?;
        at += 2;
        let base = payload.get(base_offset..at - 2)?;
        let has_base = base.iter().any(|b| *b != 0);
        if file_offsets && has_base {
            bump(&mut out, base_offset, base_offset_size)?;
        }
        for _ in 0..extents {
            at += index_size;
            if file_offsets && !has_base {
                bump(&mut out, at, offset_size)?;
            }
            at += offset_size + length_size;
        }
    }
    (at == payload.len()).then_some(out)
}

/// Iterates over the boxes in `data`, stopping at the first malformed one.
fn boxes(data: &[u8]) -> impl Iterator<Item = Bx<'_>> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let size = usize::try_from(be32(data, at)?).ok()?;
        let kind: [u8; 4] = data.get(at + 4..at + 8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len() - at),
            1 => (16, usize::try_from(be64(data, at + 8)?).ok()?),
            n => (8, n),
        };
        let payload = data.get(at + header..at.checked_add(size)?)?;
        at += size;
        Some(Bx {
            kind,
            header,
            payload,
        })
    })
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
#[allow(clippy::trivially_copy_pass_by_ref)] // `bx(b"ftyp", …)` reads like the spec
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// A minimal HEIF `meta` box: primary item 1 with properties 1 (colr), 2 (irot), 3 (imir);
    /// item 2 (a tile) with property 4 (another irot that must be ignored).
    fn sample() -> Vec<u8> {
        let mut colr = b"prof".to_vec();
        colr.extend_from_slice(b"ICC-DATA");
        let ipco = [
            bx(b"colr", &colr),
            bx(b"irot", &[1]),
            bx(b"imir", &[1]),
            bx(b"irot", &[2]),
        ]
        .concat();
        let ipma = [
            vec![0, 0, 0, 0], // version 0, flags 0
            2u32.to_be_bytes().to_vec(),
            vec![0, 1, 3, 0x81, 2, 3], // item 1: colr (essential), irot, imir
            vec![0, 2, 1, 4],          // item 2: irot
        ]
        .concat();
        let iprp = bx(b"iprp", &[bx(b"ipco", &ipco), bx(b"ipma", &ipma)].concat());
        let pitm = bx(b"pitm", &[0, 0, 0, 0, 0, 1]);
        let meta = bx(b"meta", &[vec![0, 0, 0, 0], pitm, iprp].concat());
        [bx(b"ftyp", b"heic"), meta, bx(b"mdat", &[0; 16])].concat()
    }

    #[test]
    fn reads_primary_item_properties_in_order() {
        let properties = read_properties(&sample());
        assert_eq!(properties.icc.as_deref(), Some(&b"ICC-DATA"[..]));
        assert_eq!(
            properties.transforms,
            [
                Transform::Rotate { quarter_turns: 1 },
                Transform::FlipHorizontal
            ]
        );
    }

    /// The (offset, length) extents of every item in a version 0 or 1 `iloc` without base offsets,
    /// as ravif writes it.
    fn extents(file: &[u8]) -> Vec<(usize, usize)> {
        let meta = boxes(file).find(|b| &b.kind == b"meta").unwrap();
        let iloc = boxes(&meta.payload[4..])
            .find(|b| &b.kind == b"iloc")
            .unwrap()
            .payload;
        let sizes = be16(iloc, 4).unwrap();
        let (offset_size, length_size) =
            (usize::from(sizes >> 12), usize::from((sizes >> 8) & 0xf));
        assert_eq!(sizes & 0xf0, 0, "no base offsets");
        let read = |at: usize, width: usize| {
            iloc[at..at + width]
                .iter()
                .fold(0usize, |v, b| v << 8 | usize::from(*b))
        };
        let mut at = 8;
        let mut out = Vec::new();
        for _ in 0..be16(iloc, 6).unwrap() {
            at += 2 + if iloc[0] == 1 { 2 } else { 0 } + 2; // item ID, method, data reference
            let count = be16(iloc, at).unwrap();
            at += 2;
            for _ in 0..count {
                out.push((read(at, offset_size), read(at + offset_size, length_size)));
                at += offset_size + length_size;
            }
        }
        out
    }

    #[test]
    fn embeds_an_icc_profile_into_an_avif_file() {
        // A real encoder output, with an alpha item, so both images must still be found.
        let pixels: Vec<ravif::RGBA8> = (0..64u8)
            .map(|i| ravif::RGBA8::new(i * 4, 255 - i * 4, 128, i * 4))
            .collect();
        let plain = ravif::Encoder::new()
            .with_speed(10)
            .encode_rgba(ravif::Img::new(pixels.as_slice(), 8, 8))
            .unwrap()
            .avif_file;
        assert_eq!(read_properties(&plain).icc, None);
        let profile = b"ICC-PROFILE-BYTES".repeat(40);
        let tagged = embed_icc(&plain, &profile).unwrap();
        assert_eq!(read_properties(&tagged).icc.as_deref(), Some(&profile[..]));

        let (before, after) = (extents(&plain), extents(&tagged));
        assert_eq!(before.len(), 2, "color and alpha");
        for ((old, len), (new, new_len)) in before.into_iter().zip(after) {
            assert_eq!(len, new_len);
            assert_eq!(plain[old..old + len], tagged[new..new + len]);
        }
    }

    #[test]
    fn rejects_files_it_cannot_tag() {
        assert_eq!(embed_icc(b"", b"icc"), None);
        assert_eq!(embed_icc(&sample()[..40], b"icc"), None);
        // No iloc is fine (nothing to move), but the primary item must have an association.
        let tagged = embed_icc(&sample(), b"NEW").unwrap();
        // The original profile is property 1 and comes first, so it still wins.
        assert_eq!(
            read_properties(&tagged).icc.as_deref(),
            Some(&b"ICC-DATA"[..])
        );
    }

    #[test]
    fn moves_base_offsets_and_extents_past_the_insertion() {
        // Version 1, offset/length/base sizes 4/4/4, index size 0; two items.
        let mut iloc = vec![1, 0, 0, 0, 0x44, 0x40, 0, 2];
        // Item 1: file offsets, base 1000, one extent (offset 0, length 10).
        iloc.extend([
            0, 1, 0, 0, 0, 0, 0, 0, 0x03, 0xe8, 0, 1, 0, 0, 0, 0, 0, 0, 0, 10,
        ]);
        // Item 2: idat (construction method 1) offset 5: never moved.
        iloc.extend([0, 2, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 3]);
        let moved = shift_iloc(&iloc, 900, 7).unwrap();
        assert_eq!(&moved[14..18], 1007u32.to_be_bytes());
        assert_eq!(moved[18..], iloc[18..]);
        // Offsets before the insertion point stay.
        assert_eq!(shift_iloc(&iloc, 2000, 7).unwrap(), iloc);
        // An offset that would no longer fit its field is an error, not a wraparound.
        assert_eq!(shift_iloc(&iloc, 0, u32::MAX as usize), None);
    }

    /// Primary item 1 with an alpha plane (item 2) and an HDR gain map (item 3).
    fn sample_with_auxiliary(alpha_linked: bool) -> Vec<u8> {
        let aux_c = |urn: &[u8]| bx(b"auxC", &[&[0, 0, 0, 0][..], urn, &[0]].concat());
        let ipco = [
            bx(b"colr", b"profICC"),
            aux_c(b"urn:mpeg:hevc:2015:auxid:1"),
            aux_c(b"urn:com:apple:photo:2020:aux:hdrgainmap"),
        ]
        .concat();
        let ipma = [
            vec![0, 0, 0, 0],
            3u32.to_be_bytes().to_vec(),
            vec![0, 1, 1, 1],
            vec![0, 2, 1, 2],
            vec![0, 3, 1, 3],
        ]
        .concat();
        let iprp = bx(b"iprp", &[bx(b"ipco", &ipco), bx(b"ipma", &ipma)].concat());
        let mut references = vec![bx(b"auxl", &[0, 3, 0, 1, 0, 1])];
        if alpha_linked {
            references.push(bx(b"auxl", &[0, 2, 0, 1, 0, 1]));
        }
        references.push(bx(b"dimg", &[0, 1, 0, 1, 0, 4]));
        let iref = bx(b"iref", &[vec![0, 0, 0, 0], references.concat()].concat());
        let pitm = bx(b"pitm", &[0, 0, 0, 0, 0, 1]);
        let meta = bx(b"meta", &[vec![0, 0, 0, 0], pitm, iref, iprp].concat());
        [bx(b"ftyp", b"heic"), meta].concat()
    }

    #[test]
    fn finds_the_alpha_plane_but_not_other_auxiliary_images() {
        let properties = read_properties(&sample_with_auxiliary(true));
        assert_eq!((properties.primary, properties.alpha), (Some(1), Some(2)));
        let gain_map_only = read_properties(&sample_with_auxiliary(false));
        assert_eq!(gain_map_only.alpha, None);
        assert_eq!(read_properties(&sample()).alpha, None);
    }

    #[test]
    fn tiles_refer_to_streams_by_position_in_their_group() {
        // FFprobe numbers a grid's tiles within the group: the alpha grid's tile 0 is stream 2.
        let group: ProbeGroup = serde_json::from_str(
            r#"{"id": "0xe", "type": "Tile Grid", "streams": [{"index": 2}, {"index": 3}],
                "components": [{"width": 900, "height": 400, "subcomponents": [
                  {"stream_index": 0, "tile_horizontal_offset": 0, "tile_vertical_offset": 0},
                  {"stream_index": 1, "tile_horizontal_offset": 512, "tile_vertical_offset": 0}]}]}"#,
        )
        .unwrap();
        assert_eq!(item_id(group.id.as_ref()), Some(14));
        assert_eq!(
            grid_filter(&group.grid().unwrap(), "alpha"),
            "[0:2][0:3]xstack=inputs=2:layout=0_0|512_0:fill=black,crop=900:400:0:0[alpha]"
        );
    }

    #[test]
    fn malformed_files_have_no_properties() {
        assert_eq!(read_properties(b""), Properties::default());
        assert_eq!(read_properties(&sample()[..40]), Properties::default());
        let mut lying = sample();
        lying[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read_properties(&lying), Properties::default());
    }

    #[test]
    fn transforms_match_heif_semantics() {
        // 2×1 image: left pixel red, right pixel blue.
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let img = DynamicImage::ImageRgb8(img);
        // Counterclockwise quarter turn: the right (blue) pixel ends up on top.
        let turned = Transform::Rotate { quarter_turns: 1 }
            .apply(img.clone())
            .to_rgb8();
        assert_eq!(turned.dimensions(), (1, 2));
        assert_eq!(turned.get_pixel(0, 0).0, [0, 0, 255]);
        let mirrored = Transform::FlipHorizontal.apply(img).to_rgb8();
        assert_eq!(mirrored.get_pixel(0, 0).0, [0, 0, 255]);
    }

    #[test]
    fn grid_filter_stitches_and_crops() {
        let grid = TileGrid {
            width: 1000,
            height: 600,
            horizontal_offset: 0,
            vertical_offset: 0,
            subcomponents: vec![
                Tile {
                    stream_index: 0,
                    x: 0,
                    y: 0,
                },
                Tile {
                    stream_index: 1,
                    x: 512,
                    y: 0,
                },
            ],
        };
        assert_eq!(
            grid_filter(&grid, "out"),
            "[0:0][0:1]xstack=inputs=2:layout=0_0|512_0:fill=black,crop=1000:600:0:0[out]"
        );
    }
}
