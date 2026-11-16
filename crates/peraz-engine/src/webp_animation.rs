//! Animated WebP, encoded in process with libwebp: an animated WebP stays an animated WebP,
//! with every frame, its timing, the loop count, and transparency.

use image::RgbaImage;

use crate::apng::Delay;

/// The frames of an animation to encode, each with the time it is shown.
#[derive(Debug, Default)]
pub struct Animation {
    frames: Vec<(RgbaImage, Delay)>,
    bytes: usize,
}

/// Most bytes of frames held in memory: libwebp needs every frame before it writes.
const MAX_BUFFERED_BYTES: usize = 3 << 30;

impl Animation {
    pub fn push(&mut self, pixels: RgbaImage, delay: Delay) -> Result<(), String> {
        self.bytes += pixels.as_raw().len();
        if self.bytes > MAX_BUFFERED_BYTES {
            return Err(
                "too many frames for an animated WebP; use a shorter --duration or a smaller \
                 --max-width"
                    .to_owned(),
            );
        }
        self.frames.push((pixels, delay));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The average frame rate, rounded, at least 1.
    pub fn average_fps(&self) -> u32 {
        let ms: f64 = self.frames.iter().map(|(_, d)| d.milliseconds()).sum();
        if ms <= 0.0 {
            return 1;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let fps = (self.frames.len() as f64 * 1000.0 / ms)
            .round()
            .clamp(1.0, 1000.0) as u32;
        fps
    }

    /// Encodes the frames at a libwebp quality (0–100). `plays` is how many times the
    /// animation plays; 0 means forever.
    pub fn encode(&self, quality: u8, plays: u32) -> Result<Vec<u8>, String> {
        let Some((first, _)) = self.frames.first() else {
            return Err("the source has no frames in the selected range".to_owned());
        };
        let (width, height) = first.dimensions();
        let mut config = webp::WebPConfig::new()
            .map_err(|()| "libwebp: cannot create a configuration".to_owned())?;
        config.quality = f32::from(quality);
        config.alpha_quality = 100;
        // Method 0–6 trades speed for size; 4 keeps many frames quick.
        config.method = 4;
        let mut encoder = webp::AnimEncoder::new(width, height, &config);
        encoder.set_loop_count(i32::try_from(plays).unwrap_or(0));
        // A frame starts when the ones before it have been shown: times in whole
        // milliseconds, counted from exact sums so that rounding never adds up.
        let mut elapsed = 0.0;
        for (pixels, delay) in &self.frames {
            #[allow(clippy::cast_possible_truncation)]
            let starts_at = f64::round(elapsed) as i32;
            encoder.add_frame(webp::AnimFrame::from_rgba(
                pixels.as_raw(),
                width,
                height,
                starts_at,
            ));
            elapsed += delay.milliseconds();
        }
        let file = encoder
            .try_encode()
            .map_err(|e| format!("libwebp: {e:?}"))?;
        // The encoder is not told when the last frame ends and guesses: set it right.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let total = elapsed.round().max(0.0) as u64;
        Ok(with_total_duration(&file, total))
    }
}

/// An animated WebP whose last frame is shown until `total_ms` after the start. Each frame
/// (`ANMF` chunk) stores its own duration in 24 bits; the frames before the last keep theirs.
fn with_total_duration(webp: &[u8], total_ms: u64) -> Vec<u8> {
    let mut out = webp.to_vec();
    let mut before_last = 0u64;
    let mut last: Option<(usize, u64)> = None;
    let mut at = 12;
    while let Some(head) = webp.get(at..at + 8) {
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        // The duration follows the frame's position and size: 12 bytes into the chunk's data.
        if &head[..4] == b"ANMF"
            && let Some(field) = webp.get(at + 20..at + 23)
        {
            let duration = u64::from(u32::from_le_bytes([field[0], field[1], field[2], 0]));
            if let Some((_, previous)) = last.replace((at + 20, duration)) {
                before_last += previous;
            }
        }
        at += 8 + size + (size & 1);
    }
    if let Some((field, _)) = last
        && let Some(duration) = total_ms.checked_sub(before_last)
        && let Ok(duration) = u32::try_from(duration.min(0x00FF_FFFF))
    {
        out[field..field + 3].copy_from_slice(&duration.to_le_bytes()[..3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_frame_ends_when_the_animation_does() {
        let chunk = |kind: &[u8; 4], data: &[u8]| {
            let mut out = kind.to_vec();
            out.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(0);
            }
            out
        };
        // A frame: position and size (12 bytes), duration (3 bytes), flags, then its picture.
        let frame = |duration: u32| {
            let mut data = vec![0; 12];
            data.extend_from_slice(&duration.to_le_bytes()[..3]);
            data.extend([0, 1, 2, 3]);
            chunk(b"ANMF", &data)
        };
        let file = |durations: &[u32]| {
            let mut out = b"RIFF\0\0\0\0WEBP".to_vec();
            out.extend(chunk(b"VP8X", &[0x02; 10]));
            out.extend(chunk(b"ANIM", &[0; 6]));
            for duration in durations {
                out.extend(frame(*duration));
            }
            out
        };
        assert_eq!(
            with_total_duration(&file(&[80, 120, 100]), 500),
            file(&[80, 120, 300])
        );
        assert_eq!(with_total_duration(&file(&[40]), 1000), file(&[1000]));
        // A total that the earlier frames already exceed leaves the file as it is.
        assert_eq!(
            with_total_duration(&file(&[80, 120, 100]), 150),
            file(&[80, 120, 100])
        );
        assert_eq!(with_total_duration(b"RIFF", 10), b"RIFF");
    }
}
