//! Probing media with `ffprobe`, or in process for the files Peraz reads itself.

use std::collections::HashMap;
use std::path::Path;

use peraz_core::{Alpha, AudioTrack, ColorInfo, MediaInfo, MediaKind, Rational, VideoTrack};
use serde::Deserialize;

use crate::EngineError;
use crate::ffmpeg::{Toolchain, check, command};

/// Probes one file.
pub fn probe(tools: &Toolchain, path: &Path) -> Result<MediaInfo, EngineError> {
    let input_error = |source| EngineError::Input {
        path: path.to_owned(),
        source,
    };
    let size = regular_file_size(path)?;
    if let Some(kind) = sniff_document(path).map_err(input_error)? {
        return Ok(document_info(kind, size));
    }
    if let Some(info) = jpeg_xl_info(path, size)? {
        return Ok(info);
    }
    if let Some(header) = crate::svg::sniff(path).map_err(input_error)? {
        return Ok(svg_info(header, size));
    }
    let absolute = std::path::absolute(path).map_err(input_error)?;
    // The `file:` prefix stops ffprobe from treating names like `concat:a|b` or `http://…`
    // as protocols: only local files are ever opened.
    let mut url = std::ffi::OsString::from("file:");
    url.push(absolute.as_os_str());

    let run = |with_groups: bool| {
        let mut cmd = command(&tools.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ]);
        if with_groups {
            cmd.arg("-show_stream_groups");
        }
        cmd.arg("-i").arg(&url);
        let output = cmd.output().map_err(|source| EngineError::Spawn {
            path: tools.ffprobe.clone(),
            source,
        })?;
        check("ffprobe", output)
    };
    // Stream groups (needed for HEIC/AVIF tile grids) arrived in FFmpeg 7; older ffprobe
    // rejects the option, so retry without it.
    let json = match run(true) {
        Err(EngineError::ToolFailed { message, .. }) if message.contains("show_stream_groups") => {
            run(false)?
        }
        Err(EngineError::ToolFailed { message, .. }) => {
            // ffprobe prefixes errors with the input URL; the caller already knows the path.
            let prefix = format!("{}: ", url.to_string_lossy());
            let message = message.strip_prefix(&prefix).unwrap_or(&message).to_owned();
            return Err(EngineError::NotMedia { message });
        }
        other => other?,
    };
    let mut info = parse_probe_json(&json, size).map_err(|e| EngineError::BadOutput {
        tool: "ffprobe",
        message: e.to_string(),
    })?;
    if info.container == "apng" {
        // ffprobe reports no length for an APNG; its frames carry their timing.
        if info.duration_seconds.is_none()
            && let Some((seconds, frames)) = frame_timing(tools, &url)
        {
            info.duration_seconds = Some(seconds);
            if let Some(video) = info.video.as_mut() {
                video.frame_count.get_or_insert(frames);
            }
        }
        // An animated PNG is an animation, and with a single frame a still picture.
        let frames = crate::apng::png_animation(path)
            .ok()
            .flatten()
            .map(|animation| u64::from(animation.frames))
            .or(info.video.as_ref().and_then(|v| v.frame_count));
        if frames.is_some_and(|frames| frames <= 1) {
            info.kind = MediaKind::Image;
            info.duration_seconds = None;
            info.bit_rate = None;
            if let Some(video) = info.video.as_mut() {
                video.frame_count = Some(1);
                video.frame_rate = None;
                video.bit_rate = None;
            }
        } else if info.video.is_some() {
            info.kind = MediaKind::Animation;
        }
    }
    Ok(info)
}

/// The length in seconds and the frame count of the first video stream, from its packets.
fn frame_timing(tools: &Toolchain, url: &std::ffi::OsStr) -> Option<(f64, u64)> {
    let output = command(&tools.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,duration_time",
            "-of",
            "csv=p=0",
            "-i",
        ])
        .arg(url)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_frame_timing(&String::from_utf8_lossy(&output.stdout))
}

/// Reads `pts_time,duration_time` lines: the end of the last frame, and how many there are.
fn parse_frame_timing(text: &str) -> Option<(f64, u64)> {
    let mut end = 0.0_f64;
    let mut frames = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.trim().split(',').map(str::parse::<f64>);
        let (Some(Ok(pts)), Some(Ok(duration))) = (fields.next(), fields.next()) else {
            return None;
        };
        end = end.max(pts + duration);
        frames += 1;
    }
    (frames > 0 && end > 0.0).then_some((end, frames))
}

/// Converts `ffprobe -print_format json -show_format -show_streams [-show_stream_groups]`
/// output into [`MediaInfo`]. `size_bytes` is the file size measured by the caller.
pub fn parse_probe_json(json: &str, size_bytes: u64) -> Result<MediaInfo, serde_json::Error> {
    let raw: Raw = serde_json::from_str(json)?;
    let container = raw
        .format
        .as_ref()
        .map(|f| f.format_name.clone())
        .unwrap_or_default();

    let tile_grid = raw
        .stream_groups
        .iter()
        .find(|g| g.kind.as_deref() == Some("Tile Grid"))
        .and_then(|g| g.components.first());

    let videos = || {
        raw.streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("video"))
    };
    let main_video = videos()
        .find(|s| s.disposition.attached_pic == 0 && s.disposition.dependent == 0)
        .or_else(|| videos().find(|s| s.disposition.attached_pic == 0));

    let mut video = main_video.and_then(RawStream::video_track);
    if let (Some(grid), Some(track)) = (tile_grid, video.as_mut()) {
        // A tile grid is one picture: report the full size, not one tile.
        track.width = grid.width.unwrap_or(track.width);
        track.height = grid.height.unwrap_or(track.height);
        track.frame_count = Some(1);
        // Transparency lives in a separate auxiliary image that ffprobe does not expose.
        track.alpha = Alpha::Unknown;
    } else if let Some(track) = video.as_mut() {
        // A single HEIF/AVIF picture may have an alpha plane as a dependent grayscale stream
        // (which may also be a depth or gain map: the decoder tells them apart).
        let auxiliary_gray = videos().any(|s| {
            s.disposition.dependent == 1
                && s.pix_fmt.as_deref().is_some_and(|p| p.starts_with("gray"))
        });
        if auxiliary_gray {
            track.alpha = Alpha::Unknown;
        }
    }

    let audio: Vec<AudioTrack> = raw
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .map(RawStream::audio_track)
        .collect();

    let kind = MediaKind::classify(
        &container,
        video.as_ref(),
        !audio.is_empty(),
        tile_grid.is_some(),
    );
    let still = matches!(kind, MediaKind::Image);
    if still && let Some(track) = video.as_mut() {
        // ffprobe reports a nominal 25 fps and a bit rate for still images; neither is real.
        track.frame_rate = None;
        track.bit_rate = None;
    }

    let format = raw.format.as_ref();
    Ok(MediaInfo {
        kind,
        container,
        size_bytes,
        duration_seconds: if still {
            None
        } else {
            format.and_then(|f| parse_num(f.duration.as_deref()))
        },
        bit_rate: if still {
            None
        } else {
            format.and_then(|f| parse_num(f.bit_rate.as_deref()))
        },
        video,
        audio,
    })
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
    #[serde(default)]
    stream_groups: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: String,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Deserialize)]
struct RawGroup {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    components: Vec<RawComponent>,
}

#[derive(Deserialize)]
struct RawComponent {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    attached_pic: u8,
    #[serde(default)]
    dependent: u8,
}

#[derive(Deserialize)]
struct RawSideData {
    rotation: Option<f64>,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    color_range: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    nb_frames: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    sample_rate: Option<String>,
    #[serde(default)]
    disposition: RawDisposition,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

impl RawStream {
    fn video_track(&self) -> Option<VideoTrack> {
        let pixel_format = self.pix_fmt.clone();
        Some(VideoTrack {
            codec: self.codec_name.clone().unwrap_or_default(),
            width: self.width?,
            height: self.height?,
            rotation: self.rotation(),
            frame_rate: self.frame_rate(),
            frame_count: parse_num(self.nb_frames.as_deref()),
            // FFmpeg always decodes GIF to `bgra`, whether or not the palette has a transparent
            // entry, so the pixel format says nothing about GIF transparency.
            alpha: if self.codec_name.as_deref() == Some("gif") {
                Alpha::Unknown
            } else {
                pixel_format
                    .as_deref()
                    .map_or(Alpha::Unknown, Alpha::from_pixel_format)
            },
            pixel_format,
            color: self.color(),
            bit_rate: parse_num(self.bit_rate.as_deref()),
        })
    }

    /// The stream's color description; `ffprobe` reports what is not set as `unknown`.
    fn color(&self) -> ColorInfo {
        let known = |value: &Option<String>| {
            value
                .clone()
                .filter(|v| !matches!(v.as_str(), "unknown" | "unspecified" | "reserved"))
        };
        ColorInfo {
            primaries: known(&self.color_primaries),
            transfer: known(&self.color_transfer),
            matrix: known(&self.color_space),
            range: known(&self.color_range),
        }
    }

    fn audio_track(&self) -> AudioTrack {
        AudioTrack {
            codec: self.codec_name.clone().unwrap_or_default(),
            channels: self.channels.unwrap_or(0),
            sample_rate: parse_num(self.sample_rate.as_deref()),
            bit_rate: parse_num(self.bit_rate.as_deref()),
        }
    }

    /// The average frame rate for video. GIF's average is computed from the demuxer's packet
    /// timing and is often wrong (45 frames over 3 s reported as 50/3), so GIF uses the base rate.
    fn frame_rate(&self) -> Option<Rational> {
        let avg = self.avg_frame_rate.as_deref().and_then(Rational::parse);
        let base = self.r_frame_rate.as_deref().and_then(Rational::parse);
        if self.codec_name.as_deref() == Some("gif") {
            base.or(avg)
        } else {
            avg.or(base)
        }
    }

    /// Clockwise display rotation in degrees, normalized to 0/90/180/270.
    fn rotation(&self) -> u16 {
        // The display matrix reports counterclockwise degrees; the legacy `rotate` tag is clockwise.
        let clockwise = self
            .side_data_list
            .iter()
            .find_map(|d| d.rotation)
            .map(|ccw| -ccw)
            .or_else(|| self.tags.get("rotate").and_then(|r| r.parse().ok()))
            .unwrap_or(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // value is in 0..4
        let quarter = ((clockwise / 90.0).round() as i64).rem_euclid(4) as u16;
        quarter * 90
    }
}

/// Parses ffprobe's stringly-typed numbers; `N/A` and garbage become `None`.
/// Probes one file without FFmpeg. Works for PDFs, EPUBs, the still images Peraz decodes
/// itself (JPEG, PNG, WebP, BMP, TIFF), SVGs, and animated PNGs; `Ok(None)` means the file needs
/// FFmpeg (videos, other animations, GIFs, HEIC, AVIF, and anything unrecognized).
pub fn probe_without_ffmpeg(path: &Path) -> Result<Option<MediaInfo>, EngineError> {
    let input_error = |source| EngineError::Input {
        path: path.to_owned(),
        source,
    };
    let size = regular_file_size(path)?;
    if let Some(kind) = sniff_document(path).map_err(input_error)? {
        return Ok(Some(document_info(kind, size)));
    }
    if let Some(info) = jpeg_xl_info(path, size)? {
        return Ok(Some(info));
    }
    if let Some(header) = crate::svg::sniff(path).map_err(input_error)? {
        return Ok(Some(svg_info(header, size)));
    }
    if let Some(animation) = crate::apng::png_animation(path).map_err(input_error)?
        && animation.frames > 1
    {
        return Ok(Some(apng_info(animation, size)));
    }
    let Some((codec, container)) = sniff_still_image(path).map_err(input_error)? else {
        return Ok(None);
    };
    let not_media = |e: image::ImageError| EngineError::NotMedia {
        message: e.to_string(),
    };
    let decoder = image::ImageReader::open(path)
        .map_err(input_error)?
        .with_guessed_format()
        .map_err(input_error)?
        .into_decoder()
        .map_err(not_media)?;
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    let alpha = if image::ImageDecoder::color_type(&decoder).has_alpha() {
        Alpha::Present
    } else {
        Alpha::Absent
    };
    Ok(Some(MediaInfo {
        kind: MediaKind::Image,
        container: container.to_owned(),
        size_bytes: size,
        duration_seconds: None,
        bit_rate: None,
        video: Some(VideoTrack {
            codec: codec.to_owned(),
            width,
            height,
            // EXIF orientation is applied when decoding.
            rotation: 0,
            frame_rate: None,
            frame_count: Some(1),
            pixel_format: None,
            color: ColorInfo::default(),
            alpha,
            bit_rate: None,
        }),
        audio: Vec::new(),
    }))
}

/// An SVG, read from its root element: Peraz minifies it as text.
fn svg_info(header: crate::svg::SvgHeader, size_bytes: u64) -> MediaInfo {
    MediaInfo {
        kind: MediaKind::Image,
        container: if header.gzip { "svgz" } else { "svg" }.to_owned(),
        size_bytes,
        duration_seconds: None,
        bit_rate: None,
        video: Some(VideoTrack {
            codec: "svg".to_owned(),
            width: header.width,
            height: header.height,
            rotation: 0,
            frame_rate: None,
            frame_count: Some(1),
            pixel_format: None,
            color: ColorInfo::default(),
            alpha: Alpha::Unknown,
            bit_rate: None,
        }),
        audio: Vec::new(),
    }
}

/// An animated PNG, read from its header: Peraz decodes it itself.
fn apng_info(animation: crate::apng::PngAnimation, size_bytes: u64) -> MediaInfo {
    MediaInfo {
        kind: MediaKind::Animation,
        container: "apng".to_owned(),
        size_bytes,
        duration_seconds: None,
        bit_rate: None,
        video: Some(VideoTrack {
            codec: "apng".to_owned(),
            width: animation.width,
            height: animation.height,
            rotation: 0,
            frame_rate: None,
            frame_count: Some(u64::from(animation.frames)),
            pixel_format: None,
            color: ColorInfo::default(),
            alpha: if animation.alpha {
                Alpha::Present
            } else {
                Alpha::Absent
            },
            bit_rate: None,
        }),
        audio: Vec::new(),
    }
}

/// The size of `path`, which must be a regular file.
fn regular_file_size(path: &Path) -> Result<u64, EngineError> {
    let input_error = |source| EngineError::Input {
        path: path.to_owned(),
        source,
    };
    let metadata = std::fs::metadata(path).map_err(input_error)?;
    if !metadata.is_file() {
        return Err(input_error(std::io::Error::other("not a regular file")));
    }
    Ok(metadata.len())
}

/// JPEG XL is read from its header: `djxl` decodes it, not FFmpeg.
fn jpeg_xl_info(path: &Path, size_bytes: u64) -> Result<Option<MediaInfo>, EngineError> {
    let header = crate::jxl_header::sniff(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidData => EngineError::NotMedia {
            message: e.to_string(),
        },
        _ => EngineError::Input {
            path: path.to_owned(),
            source: e,
        },
    })?;
    Ok(header.map(|header| MediaInfo {
        kind: if header.animated {
            MediaKind::Animation
        } else {
            MediaKind::Image
        },
        container: "jpegxl".to_owned(),
        size_bytes,
        duration_seconds: None,
        bit_rate: None,
        video: Some(VideoTrack {
            codec: JPEG_XL.to_owned(),
            width: header.width,
            height: header.height,
            // The orientation is applied when decoding.
            rotation: 0,
            frame_rate: None,
            frame_count: (!header.animated).then_some(1),
            pixel_format: None,
            color: ColorInfo::default(),
            alpha: header.alpha,
            bit_rate: None,
        }),
        audio: Vec::new(),
    }))
}

/// The codec name for JPEG XL (FFmpeg's).
pub const JPEG_XL: &str = "jpegxl";

fn document_info(kind: MediaKind, size_bytes: u64) -> MediaInfo {
    MediaInfo {
        kind,
        container: if kind == MediaKind::Pdf {
            "pdf"
        } else {
            "epub"
        }
        .to_owned(),
        size_bytes,
        duration_seconds: None,
        bit_rate: None,
        video: None,
        audio: Vec::new(),
    }
}

/// Recognizes still JPEG, PNG, WebP, BMP, and TIFF images from their bytes, as FFmpeg's codec
/// and demuxer names. Animated PNGs with more than one frame are read before this, and animated
/// WebP files are left to FFmpeg.
fn sniff_still_image(path: &Path) -> std::io::Result<Option<(&'static str, &'static str)>> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 32];
    let filled = read_up_to(&mut file, &mut head)?;
    let head = &head[..filled];
    let found = if head.starts_with(b"\xFF\xD8\xFF") {
        Some(("mjpeg", "jpeg_pipe"))
    } else if head.starts_with(b"\x89PNG\r\n\x1A\n") {
        Some(("png", "png_pipe"))
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        // Extended WebP: flags after the `VP8X` chunk header, bit 1 marks an animation.
        let animated =
            head.get(12..16) == Some(b"VP8X") && head.get(20).is_some_and(|f| f & 0x02 != 0);
        (!animated).then_some(("webp", "webp_pipe"))
    } else if head.starts_with(b"BM") {
        Some(("bmp", "bmp_pipe"))
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        Some(("tiff", "tiff_pipe"))
    } else {
        None
    };
    Ok(found)
}

/// Reads until `buf` is full or the file ends; returns how many bytes were read.
fn read_up_to(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read as _;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn parse_num<T: std::str::FromStr>(text: Option<&str>) -> Option<T> {
    text?.trim().parse().ok()
}

/// Recognizes PDF and EPUB files from their first bytes (ffprobe reads neither).
fn sniff_document(path: &Path) -> std::io::Result<Option<MediaKind>> {
    let mut head = [0u8; 64];
    let mut file = std::fs::File::open(path)?;
    let filled = read_up_to(&mut file, &mut head)?;
    Ok(classify_head(&head[..filled]))
}

fn classify_head(head: &[u8]) -> Option<MediaKind> {
    // PDF: `%PDF-` within the first bytes (some writers put junk before it).
    if head.windows(5).any(|w| w == b"%PDF-") {
        return Some(MediaKind::Pdf);
    }
    // EPUB: a ZIP whose first entry is an uncompressed `mimetype` holding `application/epub+zip`.
    let epub = head.starts_with(b"PK\x03\x04")
        && head.get(30..38) == Some(b"mimetype")
        && head.get(38..58) == Some(b"application/epub+zip");
    epub.then_some(MediaKind::Epub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timing_is_the_end_of_the_last_frame() {
        let text = "0.000000,0.066000\n0.066000,0.066000\n0.132000,0.500000\n";
        let (seconds, frames) = parse_frame_timing(text).unwrap();
        assert!((seconds - 0.632).abs() < 1e-9);
        assert_eq!(frames, 3);
        assert_eq!(parse_frame_timing(""), None);
        assert_eq!(parse_frame_timing("N/A,N/A\n"), None);
    }

    #[test]
    fn recognizes_pdf_and_epub_headers() {
        assert_eq!(classify_head(b"%PDF-1.7\n%\xe2\xe3"), Some(MediaKind::Pdf));
        let mut epub = b"PK\x03\x04".to_vec();
        epub.extend_from_slice(&[0; 26]);
        epub.extend_from_slice(b"mimetypeapplication/epub+zip");
        assert_eq!(classify_head(&epub), Some(MediaKind::Epub));
        let mut zip = epub.clone();
        zip[30..38].copy_from_slice(b"readme.t");
        assert_eq!(classify_head(&zip), None, "other ZIP files are not EPUB");
        assert_eq!(classify_head(b"\x00\x00\x00\x18ftypmp42"), None);
    }

    #[test]
    fn rotation_is_normalized_clockwise() {
        let stream = |json: &str| serde_json::from_str::<RawStream>(json).unwrap().rotation();
        assert_eq!(stream(r#"{"side_data_list":[{"rotation":-90}]}"#), 90);
        assert_eq!(stream(r#"{"side_data_list":[{"rotation":90}]}"#), 270);
        assert_eq!(stream(r#"{"side_data_list":[{"rotation":180}]}"#), 180);
        assert_eq!(stream(r#"{"tags":{"rotate":"90"}}"#), 90);
        assert_eq!(stream("{}"), 0);
    }

    #[test]
    fn audio_only_and_attached_cover_art() {
        let json = r#"{
            "streams": [
                {"codec_type":"audio","codec_name":"mp3","channels":2,"sample_rate":"44100","bit_rate":"320000"},
                {"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,"disposition":{"attached_pic":1}}
            ],
            "format": {"format_name":"mp3","duration":"180.5","bit_rate":"320000"}
        }"#;
        let info = parse_probe_json(json, 7_200_000).unwrap();
        assert_eq!(info.kind, MediaKind::Audio);
        assert!(info.video.is_none());
        assert_eq!(info.audio[0].sample_rate, Some(44_100));
        assert_eq!(info.duration_seconds, Some(180.5));
    }

    #[test]
    fn not_applicable_values_become_none() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"h264","width":2,"height":2,
            "nb_frames":"N/A","bit_rate":"N/A","avg_frame_rate":"0/0","r_frame_rate":"24/1"}],
            "format":{"format_name":"matroska,webm","duration":"N/A"}}"#;
        let info = parse_probe_json(json, 10).unwrap();
        let video = info.video.unwrap();
        assert_eq!(video.frame_count, None);
        assert_eq!(video.bit_rate, None);
        assert_eq!(video.frame_rate, Some(Rational { num: 24, den: 1 }));
        assert_eq!(info.duration_seconds, None);
    }
}
