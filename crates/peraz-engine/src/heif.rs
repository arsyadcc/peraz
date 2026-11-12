//! HEIC/HEIF and AVIF input.
//!
//! FFmpeg decodes the pixels: a photo is usually stored as a grid of tiles, which are stitched
//! with `xstack` and cropped to the image size. FFmpeg does not apply the HEIF transformations
//! or export the color profile with the pixels, so this module reads them from the file's
//! ISO-BMFF boxes (`pitm`, `ipco`, `ipma`, `colr`, `irot`, `imir`) and applies them itself.

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
    let filter = tile_filter(tools, &url)?;
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
        "rgb24",
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

    let bytes =
        std::fs::read(input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;
    let properties = read_properties(&bytes);
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
    codec_type: Option<String>,
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
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    components: Vec<TileGrid>,
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

#[derive(Deserialize)]
struct Tile {
    stream_index: u32,
    #[serde(rename = "tile_horizontal_offset")]
    x: u32,
    #[serde(rename = "tile_vertical_offset")]
    y: u32,
}

/// Builds the filter graph that produces the full picture as `[out]`.
fn tile_filter(tools: &Toolchain, url: &std::ffi::OsStr) -> Result<String, String> {
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
    let grid = probe
        .stream_groups
        .iter()
        .filter(|g| g.kind.as_deref() == Some("Tile Grid"))
        .find_map(|g| g.components.first());
    if let Some(grid) = grid.filter(|g| !g.subcomponents.is_empty()) {
        return Ok(grid_filter(grid));
    }
    let stream = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video") && s.disposition.dependent == 0)
        .or_else(|| {
            probe
                .streams
                .iter()
                .find(|s| s.codec_type.as_deref() == Some("video"))
        })
        .ok_or("the file contains no picture")?;
    Ok(format!("[0:{}]null[out]", stream.index))
}

fn grid_filter(grid: &TileGrid) -> String {
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
        return format!("{inputs}{crop}[out]");
    }
    format!(
        "{inputs}xstack=inputs={}:layout={}:fill=black,{crop}[out]",
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
    pub icc: Option<Vec<u8>>,
    pub transforms: Vec<Transform>,
}

/// Reads the primary item's ICC profile and transformations. Malformed or missing boxes yield
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
    for child in boxes(meta_children) {
        match &child.kind {
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
    let own: Vec<&Bx> = associations
        .iter()
        .filter(|(item, _)| *item == primary)
        .flat_map(|(_, indexes)| indexes.iter())
        .filter_map(|&i| properties.get(usize::from(i).checked_sub(1)?))
        .collect();

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
    Some(Properties { icc, transforms })
}

fn icc_payload(colr: &[u8]) -> Option<Vec<u8>> {
    let kind = colr.get(0..4)?;
    (kind == b"prof" || kind == b"rICC").then(|| colr[4..].to_vec())
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
    payload: &'a [u8],
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
        Some(Bx { kind, payload })
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
            grid_filter(&grid),
            "[0:0][0:1]xstack=inputs=2:layout=0_0|512_0:fill=black,crop=1000:600:0:0[out]"
        );
    }
}
