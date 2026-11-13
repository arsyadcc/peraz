//! Animated GIF encoding from any video or GIF.
//!
//! FFmpeg streams RGBA frames (already at the target frame rate and size), so memory stays flat
//! however long the source is. Each frame gets its own palette from imagequant, with dithering.
//! Between frames, pixels that barely changed are left transparent so the previous frame shows
//! through, the frame is cropped to the area that changed, and identical frames are merged into
//! the previous frame's delay. Those three steps are what keep GIFs small. Before them, light
//! temporal denoising keeps camera noise from counting as change.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use peraz_core::GifPlan;

use crate::compress::Progress;
use crate::ffmpeg::{Toolchain, command};

/// Why encoding stopped early.
#[derive(Debug)]
pub enum GifError {
    Cancelled,
    Failed(String),
}

impl From<String> for GifError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Encodes `input` into the GIF file `output`, following `plan`.
///
/// `expected_seconds` is the playing time being converted, for progress reporting.
pub fn encode_gif(
    tools: &Toolchain,
    input: &Path,
    output: &Path,
    plan: &GifPlan,
    expected_seconds: f64,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<String, GifError> {
    let width = u16::try_from(plan.width)
        .map_err(|_| "GIF frames are at most 65535 pixels wide".to_owned())?;
    let height = u16::try_from(plan.height)
        .map_err(|_| "GIF frames are at most 65535 pixels high".to_owned())?;

    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
    if let Some(start) = plan.start_ms {
        cmd.args(["-ss", &seconds(start)]);
    }
    cmd.arg("-i").arg(file_url(input)?);
    if let Some(duration) = plan.duration_ms {
        cmd.args(["-t", &seconds(duration)]);
    }
    let filter = format!(
        "fps={},scale={}:{}:flags=lanczos,format=rgba",
        plan.fps, plan.width, plan.height
    );
    cmd.args(["-an", "-sn", "-vf", &filter, "-f", "rawvideo", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", tools.ffmpeg.display()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or("ffmpeg stderr is unavailable".to_owned())?;
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut frames_in = BufReader::with_capacity(
        1 << 20,
        child
            .stdout
            .take()
            .ok_or("ffmpeg stdout is unavailable".to_owned())?,
    );

    let file =
        File::create(output).map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    let mut writer = Writer::new(BufWriter::new(file), width, height, plan)?;
    let frame_len = plan.width as usize * plan.height as usize * 4;
    let mut frame = vec![0u8; frame_len];
    #[allow(clippy::cast_precision_loss)]
    let expected_frames = (expected_seconds * f64::from(plan.fps)).max(1.0);
    let result = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            break Err(GifError::Cancelled);
        }
        match frames_in.read_exact(&mut frame) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(format!("cannot read frames from ffmpeg: {e}").into()),
        }
        if let Err(e) = writer.push(&frame) {
            let _ = child.kill();
            break Err(e.into());
        }
        #[allow(clippy::cast_precision_loss)]
        progress((writer.frames_seen as f64 / expected_frames).min(0.99));
    };
    let status = child
        .wait()
        .map_err(|e| format!("ffmpeg did not finish: {e}"))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    result?;
    if !status.success() {
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error message");
        return Err(format!("ffmpeg failed ({status}): {}", last.trim()).into());
    }
    if writer.frames_seen == 0 {
        return Err("the source has no frames in the selected range"
            .to_owned()
            .into());
    }
    let written = writer.frames_written + usize::from(writer.pending.is_some());
    writer.finish()?;
    progress(1.0);
    Ok(format!(
        "imagequant + gif · {} fps · {} frames",
        plan.fps, written
    ))
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

fn file_url(path: &Path) -> Result<std::ffi::OsString, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut url = std::ffi::OsString::from("file:");
    url.push(absolute.as_os_str());
    Ok(url)
}

/// Turns RGBA frames into GIF frames.
struct Writer<W: std::io::Write> {
    encoder: gif::Encoder<W>,
    plan: GifPlan,
    width: usize,
    height: usize,
    /// What a viewer currently sees, used to skip pixels that did not change.
    screen: Vec<[u8; 4]>,
    /// The source has transparency: frames are drawn whole and cleared, without diffing.
    transparent_source: bool,
    /// The denoised previous frame (RGB per pixel), empty until the first frame.
    smooth: Vec<[f32; 3]>,
    /// The current frame after denoising, reused between frames.
    denoised: Vec<u8>,
    /// Held back so identical following frames can extend its delay instead of being written.
    pending: Option<gif::Frame<'static>>,
    frames_seen: u64,
    frames_written: usize,
    attributes: imagequant::Attributes,
}

impl<W: std::io::Write> Writer<W> {
    fn new(out: W, width: u16, height: u16, plan: &GifPlan) -> Result<Self, String> {
        let gif_error = |e: gif::EncodingError| format!("gif: {e}");
        let mut encoder = gif::Encoder::new(out, width, height, &[]).map_err(gif_error)?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(gif_error)?;
        let mut attributes = imagequant::new();
        attributes
            .set_quality(plan.min_quality, plan.max_quality)
            .map_err(|e| format!("imagequant: {e}"))?;
        attributes
            .set_speed(5)
            .map_err(|e| format!("imagequant: {e}"))?;
        Ok(Self {
            encoder,
            plan: plan.clone(),
            width: usize::from(width),
            height: usize::from(height),
            screen: vec![[0; 4]; usize::from(width) * usize::from(height)],
            transparent_source: false,
            smooth: Vec::new(),
            denoised: Vec::new(),
            pending: None,
            frames_seen: 0,
            frames_written: 0,
            attributes,
        })
    }

    /// Delay in centiseconds for frame `index`, spreading rounding so the total time is exact.
    fn delay(&self, index: u64) -> u16 {
        let at = |i: u64| (i * 100 + u64::from(self.plan.fps) / 2) / u64::from(self.plan.fps);
        u16::try_from(at(index + 1) - at(index)).unwrap_or(u16::MAX)
    }

    fn push(&mut self, rgba: &[u8]) -> Result<(), String> {
        let index = self.frames_seen;
        self.frames_seen += 1;
        let delay = self.delay(index);
        let first = index == 0;
        if first {
            self.transparent_source = rgba.as_chunks::<4>().0.iter().any(|p| p[3] < 128);
        }
        let diffing = !first && !self.transparent_source;
        let threshold = self.plan.change_threshold;
        let mut denoised = std::mem::take(&mut self.denoised);
        let rgba = if self.plan.denoise > 0 && !self.transparent_source {
            self.denoise(rgba, &mut denoised);
            &denoised[..]
        } else {
            rgba
        };
        let result = self.encode(rgba, delay, diffing, threshold);
        self.denoised = denoised;
        result
    }

    /// Temporal denoising: each channel moves from its denoised value in the previous frame
    /// towards the new value by a weight that grows with the size of the change. Changes well
    /// below the strength (noise) are mostly averaged away; larger ones (motion, cuts) pass
    /// through almost unchanged, so moving edges do not leave trails.
    fn denoise(&mut self, rgba: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(rgba);
        let pixels = rgba.as_chunks::<4>().0;
        if self.smooth.len() != pixels.len() {
            self.smooth = pixels
                .iter()
                .map(|p| [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])])
                .collect();
            return;
        }
        let strength = f32::from(self.plan.denoise);
        let scale = 1.0 / (2.0 * strength * strength);
        for ((p, s), o) in pixels
            .iter()
            .zip(self.smooth.iter_mut())
            .zip(out.as_chunks_mut::<4>().0.iter_mut())
        {
            for c in 0..3 {
                let d = f32::from(p[c]) - s[c];
                let weight = 1.0 - (-d * d * scale).exp();
                s[c] += weight * d;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // s stays within 0..=255: it moves between two values in that range
                {
                    o[c] = s[c].round() as u8;
                }
            }
        }
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        delay: u16,
        diffing: bool,
        threshold: u8,
    ) -> Result<(), String> {
        // Pixels to encode: unchanged ones become fully transparent ("keep what is there").
        let mut pixels: Vec<imagequant::RGBA> = Vec::with_capacity(self.width * self.height);
        let (mut left, mut top, mut right, mut bottom) = (self.width, self.height, 0, 0);
        for (i, p) in rgba.as_chunks::<4>().0.iter().enumerate() {
            let changed = !diffing || {
                let s = self.screen[i];
                (0..3).any(|c| p[c].abs_diff(s[c]) > threshold)
            };
            if changed && p[3] >= 128 {
                let (x, y) = (i % self.width, i / self.width);
                (left, top, right, bottom) =
                    (left.min(x), top.min(y), right.max(x + 1), bottom.max(y + 1));
                pixels.push(imagequant::RGBA::new(p[0], p[1], p[2], 255));
            } else {
                pixels.push(imagequant::RGBA::new(0, 0, 0, 0));
            }
        }
        if diffing && right == 0 {
            // Nothing changed: show the previous frame longer.
            if let Some(pending) = self.pending.as_mut() {
                pending.delay = pending.delay.saturating_add(delay);
            }
            return Ok(());
        }
        if !diffing {
            (left, top, right, bottom) = (0, 0, self.width, self.height);
        }
        let (crop_w, crop_h) = (right - left, bottom - top);
        let cropped: Vec<imagequant::RGBA> = (top..bottom)
            .flat_map(|y| {
                pixels[y * self.width + left..y * self.width + right]
                    .iter()
                    .copied()
            })
            .collect();

        let (palette, mut indexes) = self.quantize(cropped, crop_w, crop_h)?;
        // GIF has one transparent index: merge every (nearly) transparent palette entry into it.
        let transparent = palette.iter().position(|c| c.a < 128);
        if let Some(t) = transparent {
            let t = u8::try_from(t).unwrap_or(0);
            for index in &mut indexes {
                if palette[usize::from(*index)].a < 128 {
                    *index = t;
                }
            }
        }
        for (row, y) in (top..bottom).enumerate() {
            for (col, x) in (left..right).enumerate() {
                let c = palette[usize::from(indexes[row * crop_w + col])];
                if c.a >= 128 {
                    self.screen[y * self.width + x] = [c.r, c.g, c.b, 255];
                }
            }
        }
        let frame = gif::Frame {
            delay,
            dispose: if self.transparent_source {
                gif::DisposalMethod::Background
            } else {
                gif::DisposalMethod::Keep
            },
            transparent: transparent.and_then(|t| u8::try_from(t).ok()),
            left: u16::try_from(left).unwrap_or(0),
            top: u16::try_from(top).unwrap_or(0),
            width: u16::try_from(crop_w).unwrap_or(0),
            height: u16::try_from(crop_h).unwrap_or(0),
            palette: Some(palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect()),
            buffer: Cow::Owned(indexes),
            ..gif::Frame::default()
        };
        self.flush_pending()?;
        self.pending = Some(frame);
        Ok(())
    }

    fn quantize(
        &mut self,
        pixels: Vec<imagequant::RGBA>,
        width: usize,
        height: usize,
    ) -> Result<(Vec<imagequant::RGBA>, Vec<u8>), String> {
        let liq = |e: imagequant::Error| format!("imagequant: {e}");
        let mut image = self
            .attributes
            .new_image(pixels, width, height, 0.0)
            .map_err(liq)?;
        let mut result = match self.attributes.quantize(&mut image) {
            Ok(result) => result,
            // A GIF frame must have a palette even when the quality target is out of reach.
            Err(imagequant::Error::QualityTooLow) => {
                let mut relaxed = self.attributes.clone();
                relaxed.set_quality(0, self.plan.max_quality).map_err(liq)?;
                relaxed.quantize(&mut image).map_err(liq)?
            }
            Err(e) => return Err(liq(e)),
        };
        result
            .set_dithering_level(f32::from(self.plan.dithering_percent) / 100.0)
            .map_err(liq)?;
        result.remapped(&mut image).map_err(liq)
    }

    fn flush_pending(&mut self) -> Result<(), String> {
        if let Some(frame) = self.pending.take() {
            self.encoder
                .write_frame(&frame)
                .map_err(|e| format!("gif: {e}"))?;
            self.frames_written += 1;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        self.flush_pending()?;
        let mut out = self.encoder.into_inner().map_err(|e| format!("gif: {e}"))?;
        out.flush()
            .map_err(|e| format!("cannot write the GIF: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(width: u32, height: u32) -> GifPlan {
        GifPlan {
            fps: 12,
            width,
            height,
            min_quality: 0,
            max_quality: 100,
            dithering_percent: 0,
            change_threshold: 4,
            denoise: 0,
            start_ms: None,
            duration_ms: None,
            converts_format: true,
        }
    }

    fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat(width * height)
    }

    fn decode(bytes: &[u8]) -> Vec<gif::Frame<'static>> {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::Indexed);
        let mut decoder = options.read_info(bytes).unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            frames.push(frame.clone());
        }
        frames
    }

    #[test]
    fn delays_add_up_to_the_exact_duration() {
        let writer = Writer::new(Vec::new(), 4, 4, &plan(4, 4)).unwrap();
        let total: u32 = (0..12).map(|i| u32::from(writer.delay(i))).sum();
        assert_eq!(total, 100, "12 frames at 12 fps last one second");
    }

    #[test]
    fn identical_frames_merge_and_changes_are_cropped() {
        let (w, h) = (16, 8);
        let mut writer = Writer::new(Vec::new(), 16, 8, &plan(16, 8)).unwrap();
        let red = solid(w, h, [200, 0, 0, 255]);
        writer.push(&red).unwrap();
        writer.push(&red).unwrap(); // identical: merged
        let mut changed = red.clone();
        for y in 2..4 {
            for x in 5..9 {
                changed[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[0, 0, 200, 255]);
            }
        }
        writer.push(&changed).unwrap();
        writer.flush_pending().unwrap();
        let bytes = writer.encoder.into_inner().unwrap();
        let frames = decode(&bytes);
        assert_eq!(frames.len(), 2);
        let timing = Writer::new(Vec::new(), 1, 1, &plan(1, 1)).unwrap();
        assert_eq!(
            frames[0].delay,
            timing.delay(0) + timing.delay(1),
            "two frames' worth of delay"
        );
        assert_eq!(
            (
                frames[1].left,
                frames[1].top,
                frames[1].width,
                frames[1].height
            ),
            (5, 2, 4, 2)
        );
    }

    #[test]
    fn denoising_absorbs_small_flicker_and_keeps_real_changes() {
        let (w, h) = (8, 8);
        let mut denoising = plan(8, 8);
        denoising.denoise = 8;
        denoising.change_threshold = 2;
        let mut writer = Writer::new(Vec::new(), 8, 8, &denoising).unwrap();
        // A grey scene whose pixels flicker by up to 6 levels from frame to frame (sensor noise).
        for i in 0..6u8 {
            let frame: Vec<u8> = (0..w * h)
                .flat_map(|p| {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = 120 + ((p as u8).wrapping_mul(7).wrapping_add(i * 5) % 7);
                    [v, v, v, 255]
                })
                .collect();
            writer.push(&frame).unwrap();
        }
        // A real change: a white square.
        let mut moved = solid(w, h, [123, 123, 123, 255]);
        for y in 0..3 {
            for x in 0..3 {
                moved[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        writer.push(&moved).unwrap();
        let screen_corner = writer.screen[0];
        writer.flush_pending().unwrap();
        let frames = decode(&writer.encoder.into_inner().unwrap());
        assert!(
            frames.len() < 5,
            "most noisy frames merge: {} frames",
            frames.len()
        );
        assert!(
            screen_corner[0] > 240,
            "the white square shows at once: {screen_corner:?}"
        );

        let mut plain = plan(8, 8);
        plain.change_threshold = 2;
        let mut writer = Writer::new(Vec::new(), 8, 8, &plain).unwrap();
        for i in 0..6u8 {
            let frame: Vec<u8> = (0..w * h)
                .flat_map(|p| {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = 120 + ((p as u8).wrapping_mul(7).wrapping_add(i * 5) % 7);
                    [v, v, v, 255]
                })
                .collect();
            writer.push(&frame).unwrap();
        }
        writer.flush_pending().unwrap();
        assert_eq!(
            decode(&writer.encoder.into_inner().unwrap()).len(),
            6,
            "without denoising every noisy frame is written"
        );
    }

    #[test]
    fn transparent_sources_are_drawn_whole() {
        let mut writer = Writer::new(Vec::new(), 4, 4, &plan(4, 4)).unwrap();
        let mut frame = solid(4, 4, [10, 200, 10, 255]);
        frame[3] = 0; // one transparent pixel
        writer.push(&frame).unwrap();
        writer.push(&frame).unwrap();
        writer.flush_pending().unwrap();
        let frames = decode(&writer.encoder.into_inner().unwrap());
        assert_eq!(frames.len(), 2, "no diffing for transparent sources");
        assert!(
            frames
                .iter()
                .all(|f| f.dispose == gif::DisposalMethod::Background && f.width == 4)
        );
        assert!(frames[0].transparent.is_some());
    }
}
