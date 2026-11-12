//! What Peraz knows about an input file before compressing it.

use serde::{Deserialize, Serialize};

/// The kind of media, which decides which compression pipeline applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Moving pictures, with or without audio.
    Video,
    /// A single still picture (including HEIC/AVIF stored in an ISO-BMFF container).
    Image,
    /// An animated picture format such as GIF.
    Animation,
    /// Audio only.
    Audio,
    /// Nothing Peraz can work with.
    Unknown,
}

/// Whether a picture carries transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alpha {
    Present,
    Absent,
    /// The pixel format cannot tell (e.g. palette images, whose palette may hold transparency).
    Unknown,
}

impl Alpha {
    /// Infers transparency from an FFmpeg pixel format name.
    pub fn from_pixel_format(pix_fmt: &str) -> Self {
        const WITH_ALPHA: &[&str] = &[
            "rgba", "bgra", "argb", "abgr", "ya8", "ya16", "rgba64", "bgra64",
        ];
        if pix_fmt.starts_with("pal") {
            Self::Unknown
        } else if pix_fmt.starts_with("yuva")
            || pix_fmt.starts_with("gbrap")
            || WITH_ALPHA.iter().any(|f| pix_fmt.starts_with(f))
        {
            Self::Present
        } else {
            Self::Absent
        }
    }
}

/// An exact ratio such as a frame rate of 30000/1001.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    /// Parses FFmpeg's `num/den` notation. Returns `None` for zero or malformed values.
    pub fn parse(text: &str) -> Option<Self> {
        let (num, den) = text.split_once('/')?;
        let (num, den) = (num.trim().parse().ok()?, den.trim().parse().ok()?);
        (num != 0 && den != 0).then_some(Self { num, den })
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }
}

/// The main picture track of a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTrack {
    /// FFmpeg codec name, e.g. `h264`, `hevc`, `png`, `gif`.
    pub codec: String,
    /// Stored width in pixels (before rotation).
    pub width: u32,
    /// Stored height in pixels (before rotation).
    pub height: u32,
    /// Clockwise display rotation in degrees: 0, 90, 180, or 270.
    pub rotation: u16,
    pub frame_rate: Option<Rational>,
    pub frame_count: Option<u64>,
    pub pixel_format: Option<String>,
    pub alpha: Alpha,
    pub bit_rate: Option<u64>,
}

impl VideoTrack {
    /// Width and height as shown to the viewer, after applying rotation.
    pub fn display_size(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

/// An audio track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: Option<u32>,
    pub bit_rate: Option<u64>,
}

/// Everything Peraz learned from probing a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub kind: MediaKind,
    /// FFmpeg demuxer name(s), e.g. `mov,mp4,m4a,3gp,3g2,mj2`.
    pub container: String,
    pub size_bytes: u64,
    /// Playback duration; `None` for still images.
    pub duration_seconds: Option<f64>,
    /// Overall bit rate; `None` for still images.
    pub bit_rate: Option<u64>,
    pub video: Option<VideoTrack>,
    pub audio: Vec<AudioTrack>,
}

impl MediaKind {
    /// Classifies a file from its container and tracks.
    ///
    /// `is_tiled_image` is true when the container stores one picture as a grid of tiles
    /// (how HEIC and AVIF photos are usually stored).
    pub fn classify(
        container: &str,
        video: Option<&VideoTrack>,
        has_audio: bool,
        is_tiled_image: bool,
    ) -> Self {
        if is_tiled_image {
            return Self::Image;
        }
        let Some(video) = video else {
            return if has_audio {
                Self::Audio
            } else {
                Self::Unknown
            };
        };
        let single_frame = video.frame_count == Some(1);
        if video.codec == "gif" || container == "gif" {
            return if single_frame {
                Self::Image
            } else {
                Self::Animation
            };
        }
        let image_demuxer = container == "image2" || container.ends_with("_pipe");
        if image_demuxer || (single_frame && !has_audio) {
            return Self::Image;
        }
        Self::Video
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(codec: &str, frames: Option<u64>) -> VideoTrack {
        VideoTrack {
            codec: codec.to_owned(),
            width: 1920,
            height: 1080,
            rotation: 0,
            frame_rate: None,
            frame_count: frames,
            pixel_format: None,
            alpha: Alpha::Absent,
            bit_rate: None,
        }
    }

    #[test]
    fn alpha_from_pixel_format() {
        assert_eq!(Alpha::from_pixel_format("rgba"), Alpha::Present);
        assert_eq!(Alpha::from_pixel_format("yuva420p"), Alpha::Present);
        assert_eq!(Alpha::from_pixel_format("gbrap10le"), Alpha::Present);
        assert_eq!(Alpha::from_pixel_format("yuv420p"), Alpha::Absent);
        assert_eq!(Alpha::from_pixel_format("rgb24"), Alpha::Absent);
        assert_eq!(Alpha::from_pixel_format("pal8"), Alpha::Unknown);
    }

    #[test]
    fn rational_parsing() {
        assert_eq!(
            Rational::parse("30000/1001"),
            Some(Rational {
                num: 30000,
                den: 1001
            })
        );
        assert_eq!(Rational::parse("0/0"), None);
        assert_eq!(Rational::parse("abc"), None);
        assert!((Rational::parse("30000/1001").unwrap().as_f64() - 29.97).abs() < 0.01);
    }

    #[test]
    fn rotation_swaps_display_size() {
        let mut t = track("h264", None);
        t.rotation = 90;
        assert_eq!(t.display_size(), (1080, 1920));
        t.rotation = 180;
        assert_eq!(t.display_size(), (1920, 1080));
    }

    #[test]
    fn classification() {
        let mp4 = "mov,mp4,m4a,3gp,3g2,mj2";
        assert_eq!(
            MediaKind::classify(mp4, Some(&track("h264", Some(300))), true, false),
            MediaKind::Video
        );
        assert_eq!(
            MediaKind::classify(mp4, Some(&track("hevc", Some(1))), false, true),
            MediaKind::Image
        );
        assert_eq!(
            MediaKind::classify(mp4, Some(&track("hevc", Some(1))), false, false),
            MediaKind::Image
        );
        assert_eq!(
            MediaKind::classify("png_pipe", Some(&track("png", None)), false, false),
            MediaKind::Image
        );
        assert_eq!(
            MediaKind::classify("image2", Some(&track("mjpeg", None)), false, false),
            MediaKind::Image
        );
        assert_eq!(
            MediaKind::classify("gif", Some(&track("gif", Some(45))), false, false),
            MediaKind::Animation
        );
        assert_eq!(
            MediaKind::classify("gif", Some(&track("gif", Some(1))), false, false),
            MediaKind::Image
        );
        assert_eq!(
            MediaKind::classify("mp3", None, true, false),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::classify("pdf", None, false, false),
            MediaKind::Unknown
        );
    }

    #[test]
    fn serializes_kind_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&MediaKind::Animation).unwrap(),
            "\"animation\""
        );
    }
}
