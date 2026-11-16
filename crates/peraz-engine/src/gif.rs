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
    let file =
        File::create(output).map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    // Made from a video or another format: it loops, as such GIFs are expected to.
    let mut writer = Writer::new(
        BufWriter::new(file),
        width,
        height,
        plan,
        gif::Repeat::Infinite,
        None,
    )?;
    #[allow(clippy::cast_precision_loss)]
    let expected_frames = (expected_seconds * f64::from(plan.fps)).max(1.0);
    let stream = FrameStream {
        start_ms: plan.start_ms,
        duration_ms: plan.duration_ms,
        fps: plan.fps,
        width: plan.width,
        height: plan.height,
    };
    stream_frames(tools, input, &stream, cancel, &mut |frame| {
        writer.push_at_rate(frame)?;
        #[allow(clippy::cast_precision_loss)]
        progress((writer.frames_seen as f64 / expected_frames).min(0.99));
        Ok(())
    })?;
    let written = writer.frames_written + usize::from(writer.pending.is_some());
    writer.finish()?;
    progress(1.0);
    Ok(format!(
        "imagequant + gif · {} fps · {} frames",
        plan.fps, written
    ))
}

/// The loop count stored in a GIF file: `Finite(0)` when it stores none, which means "play
/// once"; `Infinite` when it cannot be read.
pub fn repeat_of(path: &Path) -> gif::Repeat {
    let Ok(file) = File::open(path) else {
        return gif::Repeat::Infinite;
    };
    let Ok(mut decoder) = gif::DecodeOptions::new().read_info(BufReader::new(file)) else {
        return gif::Repeat::Infinite;
    };
    // The loop count comes before the first frame.
    let _ = decoder.next_frame_info();
    decoder.repeat()
}

/// How long a frame with this stored delay (in hundredths of a second) is shown: browsers and
/// most players show a delay of 0 or 1 as 10.
#[must_use]
pub fn shown_centiseconds(delay: u16) -> u16 {
    if delay <= 1 { 10 } else { delay }
}

/// What a first pass over a GIF's frames finds.
struct Survey {
    /// How many frames there are.
    total: usize,
    /// A frame has transparency: every frame is then drawn whole, because what is cleared
    /// cannot be diffed.
    transparent: bool,
    /// The frames are scaled to the plan's size.
    resize: bool,
    /// Every color of every frame, when the frames keep their size (and so their colors) and
    /// one palette holds them all.
    colors: Option<Vec<[u8; 3]>>,
}

fn survey(input: &Path, plan: &GifPlan, cancel: &AtomicBool) -> Result<Survey, GifError> {
    let frames = crate::apng::decode(input, crate::apng::Source::Gif)?;
    let resize = (frames.width, frames.height) != (plan.width, plan.height);
    let (mut total, mut transparent) = (0, false);
    let mut colors = (!resize).then(std::collections::BTreeSet::new);
    for frame in frames {
        if cancel.load(Ordering::SeqCst) {
            return Err(GifError::Cancelled);
        }
        let (pixels, _) = frame?;
        total += 1;
        transparent |= pixels.pixels().any(|p| p[3] < 128);
        if let Some(found) = &mut colors {
            found.extend(
                pixels
                    .pixels()
                    .filter(|p| p[3] >= 128)
                    .map(|p| [p[0], p[1], p[2]]),
            );
            // One entry of the palette stays free, for "leave the pixel as it is".
            if found.len() > 255 {
                colors = None;
            }
        }
    }
    Ok(Survey {
        total,
        transparent,
        resize,
        colors: colors.map(|found| found.into_iter().collect()),
    })
}

/// A frame waiting to be written, until it is known how long it is shown.
struct Held {
    pixels: image::RgbaImage,
    /// Its delay as stored in the source.
    stored: u16,
    /// The time it is shown with the dropped frames after it, in hundredths of a second.
    shown: u32,
    /// Frames after it were dropped: it takes their time.
    extended: bool,
}

impl Held {
    /// The delay to write: the stored one, exactly, unless the frame stands in for others.
    fn delay(&self) -> u16 {
        if self.extended {
            u16::try_from(self.shown).unwrap_or(u16::MAX)
        } else {
            self.stored
        }
    }
}

/// Encodes the GIF `input` into the GIF file `output` in process: every frame with its own
/// delay, the same loop count, at the plan's size. With `fps`, frames are dropped down to that
/// rate: a frame is kept when it is on screen at one of the rate's moments, and takes the time
/// of the dropped frames after it.
pub fn encode_gif_from_gif(
    input: &Path,
    output: &Path,
    plan: &GifPlan,
    fps: Option<u32>,
    progress: Progress,
    cancel: &AtomicBool,
) -> Result<String, GifError> {
    use crate::apng::{Source, decode};
    let width = u16::try_from(plan.width)
        .map_err(|_| "GIF frames are at most 65535 pixels wide".to_owned())?;
    let height = u16::try_from(plan.height)
        .map_err(|_| "GIF frames are at most 65535 pixels high".to_owned())?;
    let Survey {
        total,
        transparent,
        resize,
        colors,
    } = survey(input, plan, cancel)?;
    let file =
        File::create(output).map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    let mut writer = Writer::new(
        BufWriter::new(file),
        width,
        height,
        plan,
        repeat_of(input),
        colors,
    )?;
    writer.transparent_source = transparent;
    let frames = decode(input, Source::Gif)?;
    #[allow(clippy::cast_precision_loss)]
    let (start, end) = (
        plan.start_ms.map_or(0.0, |ms| ms as f64),
        plan.duration_ms
            .map_or(f64::INFINITY, |ms| (plan.start_ms.unwrap_or(0) + ms) as f64),
    );
    // With a frame rate: the time between the moments at which a frame is taken.
    let interval = fps.map(|fps| 1000.0 / f64::from(fps.max(1)));
    let mut next_moment: Option<f64> = None;
    let mut held: Option<Held> = None;
    let mut at = 0.0;
    for (index, frame) in frames.enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err(GifError::Cancelled);
        }
        let (pixels, delay) = frame?;
        // GIF delays are whole hundredths of a second.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let stored = (delay.milliseconds() / 10.0).round() as u16;
        let shown = shown_centiseconds(stored);
        let (begins, ends) = (at, at + f64::from(shown) * 10.0);
        at = ends;
        // The selected part holds the frames that start inside it.
        if begins < start {
            continue;
        }
        if begins >= end {
            break;
        }
        let taken = match (interval, next_moment) {
            (Some(_), Some(moment)) => moment < ends - 1e-6,
            _ => true,
        };
        if taken {
            if let Some(frame) = held.take() {
                writer.push(frame.pixels.as_raw(), frame.delay())?;
            }
            if let Some(interval) = interval {
                // The first moment at or after this frame's end.
                let moment = next_moment.unwrap_or(begins);
                let steps = ((ends - moment) / interval - 1e-9).ceil().max(1.0);
                next_moment = Some(moment + steps * interval);
            }
            let pixels = if resize {
                image::imageops::resize(
                    &pixels,
                    plan.width,
                    plan.height,
                    image::imageops::FilterType::Lanczos3,
                )
            } else {
                pixels
            };
            held = Some(Held {
                pixels,
                stored,
                shown: u32::from(shown),
                extended: false,
            });
        } else if let Some(frame) = held.as_mut() {
            frame.shown += u32::from(shown);
            frame.extended = true;
        }
        #[allow(clippy::cast_precision_loss)]
        progress(((index + 1) as f64 / total.max(1) as f64).min(0.99));
    }
    if let Some(frame) = held {
        writer.push(frame.pixels.as_raw(), frame.delay())?;
    }
    if writer.frames_seen == 0 {
        return Err("the source has no frames in the selected range"
            .to_owned()
            .into());
    }
    let written = writer.frames_written + usize::from(writer.pending.is_some());
    writer.finish()?;
    progress(1.0);
    Ok(format!("imagequant + gif · {written} frames"))
}

/// The frames FFmpeg is asked for: the selected part of the source, at a fixed rate and size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameStream {
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

/// Has FFmpeg decode `input` and hands each frame to `on_frame` as RGBA bytes, one frame in
/// memory at a time. Fails when the selected range holds no frame.
pub(crate) fn stream_frames(
    tools: &Toolchain,
    input: &Path,
    stream: &FrameStream,
    cancel: &AtomicBool,
    on_frame: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), GifError> {
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
    if let Some(start) = stream.start_ms {
        cmd.args(["-ss", &seconds(start)]);
    }
    cmd.arg("-i").arg(file_url(input)?);
    if let Some(duration) = stream.duration_ms {
        cmd.args(["-t", &seconds(duration)]);
    }
    let filter = format!(
        "fps={},scale={}:{}:flags=lanczos,format=rgba",
        stream.fps, stream.width, stream.height
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

    let frame_len = stream.width as usize * stream.height as usize * 4;
    let mut frame = vec![0u8; frame_len];
    let mut frames = 0u64;
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
        if let Err(e) = on_frame(&frame) {
            let _ = child.kill();
            break Err(e.into());
        }
        frames += 1;
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
    if frames == 0 {
        return Err("the source has no frames in the selected range"
            .to_owned()
            .into());
    }
    Ok(())
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

/// The colors of a frame as its palette, when a GIF palette holds them all: every pixel then
/// keeps its exact color, with nothing to choose and nothing to dither. This is what frames of
/// a GIF that is not scaled are, and flat graphics such as screen recordings. `None` for more
/// than 256 colors (transparency counts as one).
fn exact_palette(pixels: &[imagequant::RGBA]) -> Option<(Vec<imagequant::RGBA>, Vec<u8>)> {
    let mut palette: Vec<imagequant::RGBA> = Vec::new();
    let mut places: std::collections::HashMap<[u8; 4], u8> = std::collections::HashMap::new();
    let mut indexes = Vec::with_capacity(pixels.len());
    for pixel in pixels {
        // Every transparent pixel is the same "leave it as it is".
        let key = if pixel.a < 128 {
            [0; 4]
        } else {
            [pixel.r, pixel.g, pixel.b, 255]
        };
        let index = if let Some(index) = places.get(&key) {
            *index
        } else {
            let index = u8::try_from(palette.len()).ok()?;
            palette.push(imagequant::RGBA::new(key[0], key[1], key[2], key[3]));
            places.insert(key, index);
            index
        };
        indexes.push(index);
    }
    Some((palette, indexes))
}

/// The rectangle of the canvas that a frame covers.
struct Area {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
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
    /// One palette for every frame, written once for the file, with the place of each color
    /// in it; `None` when each frame brings its own.
    shared: Option<SharedPalette>,
}

/// The colors of a palette that every frame uses, and the place of each color in it.
type SharedPalette = (Vec<[u8; 3]>, std::collections::HashMap<[u8; 3], u8>);

impl<W: std::io::Write> Writer<W> {
    /// `repeat` is the loop count to store; `Finite(0)` stores none ("play once"). `shared`
    /// holds every color of every frame, when they are few enough for one palette: it is then
    /// written once for the whole file, not once per frame.
    fn new(
        out: W,
        width: u16,
        height: u16,
        plan: &GifPlan,
        repeat: gif::Repeat,
        shared: Option<Vec<[u8; 3]>>,
    ) -> Result<Self, String> {
        let gif_error = |e: gif::EncodingError| format!("gif: {e}");
        // After the colors, one more entry stands for "leave the pixel as it is".
        let shared = shared.filter(|colors| colors.len() < 256);
        let global: Vec<u8> = shared
            .iter()
            .flat_map(|colors| colors.iter().flatten().copied().chain([0; 3]))
            .collect();
        let mut encoder = gif::Encoder::new(out, width, height, &global).map_err(gif_error)?;
        if repeat != gif::Repeat::Finite(0) {
            encoder.set_repeat(repeat).map_err(gif_error)?;
        }
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
            shared: shared.map(|colors| {
                let places = colors
                    .iter()
                    .enumerate()
                    .filter_map(|(at, color)| Some((*color, u8::try_from(at).ok()?)))
                    .collect();
                (colors, places)
            }),
        })
    }

    /// The frame's pixels as indexes into the shared palette, with that palette (its last
    /// entry is the transparent one). `None` without a shared palette, or for a color that is
    /// not in it: the frame then gets a palette of its own.
    fn shared_indexes(
        &self,
        pixels: &[imagequant::RGBA],
    ) -> Option<(Vec<imagequant::RGBA>, Vec<u8>)> {
        let (colors, places) = self.shared.as_ref()?;
        let transparent = u8::try_from(colors.len()).ok()?;
        let indexes = pixels
            .iter()
            .map(|pixel| {
                if pixel.a < 128 {
                    Some(transparent)
                } else {
                    places.get(&[pixel.r, pixel.g, pixel.b]).copied()
                }
            })
            .collect::<Option<Vec<u8>>>()?;
        let palette = colors
            .iter()
            .map(|[red, green, blue]| imagequant::RGBA::new(*red, *green, *blue, 255))
            .chain([imagequant::RGBA::new(0, 0, 0, 0)])
            .collect();
        Some((palette, indexes))
    }

    /// Delay in centiseconds for frame `index`, spreading rounding so the total time is exact.
    fn delay(&self, index: u64) -> u16 {
        let at = |i: u64| (i * 100 + u64::from(self.plan.fps) / 2) / u64::from(self.plan.fps);
        u16::try_from(at(index + 1) - at(index)).unwrap_or(u16::MAX)
    }

    /// Adds the next frame of a source read at the plan's frame rate.
    fn push_at_rate(&mut self, rgba: &[u8]) -> Result<(), String> {
        let delay = self.delay(self.frames_seen);
        self.push(rgba, delay)
    }

    /// Adds a frame (the whole picture), shown for `delay` hundredths of a second.
    fn push(&mut self, rgba: &[u8], delay: u16) -> Result<(), String> {
        let index = self.frames_seen;
        self.frames_seen += 1;
        let first = index == 0;
        if first {
            self.transparent_source |= rgba.as_chunks::<4>().0.iter().any(|p| p[3] < 128);
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
            // Unchanged: close to what the screen shows, by the change threshold or within
            // what the plan lets a pixel be off by anyway.
            let changed = !diffing || {
                let s = self.screen[i];
                let apart = |c: usize| u32::from(p[c].abs_diff(s[c]));
                (0..3).any(|c| apart(c) > u32::from(threshold))
                    && apart(0).pow(2) + 2 * apart(1).pow(2) + apart(2).pow(2) > self.plan.loss
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

        let from_shared = self.shared_indexes(&cropped);
        let own_palette = from_shared.is_none();
        let (palette, mut indexes) = match from_shared {
            Some(found) => found,
            None => self.quantize(cropped, crop_w, crop_h)?,
        };
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
        let transparent = transparent.and_then(|t| u8::try_from(t).ok());
        let colors: Vec<[u8; 3]> = palette.iter().map(|c| [c.r, c.g, c.b]).collect();
        let area = Area {
            left,
            top,
            width: crop_w,
            height: crop_h,
        };
        // On a canvas that is not cleared between frames, a transparent pixel shows what the
        // screen has there.
        let data = self.compress(rgba, &area, &colors, &indexes, transparent, diffing);
        let frame = gif::Frame {
            delay,
            dispose: if self.transparent_source {
                gif::DisposalMethod::Background
            } else {
                gif::DisposalMethod::Keep
            },
            transparent,
            left: u16::try_from(left).unwrap_or(0),
            top: u16::try_from(top).unwrap_or(0),
            width: u16::try_from(crop_w).unwrap_or(0),
            height: u16::try_from(crop_h).unwrap_or(0),
            palette: own_palette.then(|| colors.into_iter().flatten().collect()),
            // Compressed already: written as it is.
            buffer: Cow::Owned(data),
            ..gif::Frame::default()
        };
        self.flush_pending()?;
        self.pending = Some(frame);
        Ok(())
    }

    /// Compresses a frame's palette indexes, and notes on the screen what each pixel was
    /// written as. With the plan's `loss`, a pixel may be written as another color close to
    /// the one it should show (`rgba` holds the whole frame's), or left as the screen has it.
    fn compress(
        &mut self,
        rgba: &[u8],
        area: &Area,
        colors: &[[u8; 3]],
        indexes: &[u8],
        transparent: Option<u8>,
        on_screen: bool,
    ) -> Vec<u8> {
        let canvas_width = self.width;
        // Where each of the frame's pixels is on the canvas.
        let places: Vec<usize> = (area.top..area.top + area.height)
            .flat_map(|y| (area.left..area.left + area.width).map(move |x| y * canvas_width + x))
            .collect();
        let (data, written) = if self.plan.loss == 0 {
            crate::lzw::encode(indexes, colors.len(), None)
        } else {
            let frame = rgba.as_chunks::<4>().0;
            let wanted: Vec<[u8; 3]> = places
                .iter()
                .map(|&at| [frame[at][0], frame[at][1], frame[at][2]])
                .collect();
            let under: Option<Vec<[u8; 3]>> = on_screen.then(|| {
                places
                    .iter()
                    .map(|&at| {
                        let [red, green, blue, _] = self.screen[at];
                        [red, green, blue]
                    })
                    .collect()
            });
            let loss = crate::lzw::Loss {
                wanted: &wanted,
                under: under.as_deref(),
                palette: colors,
                transparent,
                tolerance: self.plan.loss,
            };
            crate::lzw::encode(indexes, colors.len(), Some(&loss))
        };
        for (at, symbol) in places.into_iter().zip(written) {
            if Some(symbol) != transparent
                && let Some([red, green, blue]) = colors.get(usize::from(symbol))
            {
                self.screen[at] = [*red, *green, *blue, 255];
            }
        }
        data
    }

    fn quantize(
        &mut self,
        pixels: Vec<imagequant::RGBA>,
        width: usize,
        height: usize,
    ) -> Result<(Vec<imagequant::RGBA>, Vec<u8>), String> {
        if let Some(exact) = exact_palette(&pixels) {
            return Ok(exact);
        }
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
                .write_lzw_pre_encoded_frame(&frame)
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
            loss: 0,
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

    /// What a viewer sees of a GIF held in memory: every frame as the whole picture.
    fn shown(bytes: &[u8]) -> Vec<image::RgbaImage> {
        use image::AnimationDecoder as _;
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .unwrap()
            .into_frames()
            .map(|frame| frame.unwrap().into_buffer())
            .collect()
    }

    /// Frames of a moving gradient of `shades` grays with noise of one shade: few colors, and
    /// nothing that repeats exactly.
    fn noisy_frames(width: usize, height: usize, shades: usize, count: usize) -> Vec<Vec<u8>> {
        let mut state = 0x2545_F491u32;
        (0..count)
            .map(|frame| {
                (0..width * height)
                    .flat_map(|at| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        let ramp = (at % width + frame * 3) * (shades - 2) / (width + count * 3);
                        let shade = ramp + (state >> 31) as usize;
                        let value = u8::try_from(shade * 255 / (shades - 1)).unwrap();
                        [value, value, value, 255]
                    })
                    .collect()
            })
            .collect()
    }

    fn written(plan: &GifPlan, frames: &[Vec<u8>], shared: Option<Vec<[u8; 3]>>) -> Vec<u8> {
        let (width, height) = (
            u16::try_from(plan.width).unwrap(),
            u16::try_from(plan.height).unwrap(),
        );
        let mut writer = Writer::new(
            Vec::new(),
            width,
            height,
            plan,
            gif::Repeat::Infinite,
            shared,
        )
        .unwrap();
        for frame in frames {
            writer.push_at_rate(frame).unwrap();
        }
        writer.flush_pending().unwrap();
        writer.encoder.into_inner().unwrap()
    }

    #[test]
    fn frames_with_few_colors_keep_every_color() {
        // A palette that would be reduced and dithered if it were chosen: with 64 colors in
        // the frames, there is nothing to choose.
        let exact = GifPlan {
            max_quality: 40,
            dithering_percent: 100,
            change_threshold: 0,
            ..plan(96, 64)
        };
        let frames = noisy_frames(96, 64, 64, 3);
        let shown = shown(&written(&exact, &frames, None));
        assert_eq!(shown.len(), 3);
        for (frame, picture) in frames.iter().zip(&shown) {
            assert_eq!(picture.as_raw(), frame, "every pixel as it was");
        }
    }

    #[test]
    fn with_loss_the_file_is_smaller_and_every_pixel_stays_within_it() {
        let frames = noisy_frames(96, 64, 64, 4);
        let exact = GifPlan {
            change_threshold: 1,
            ..plan(96, 64)
        };
        let lossless = written(&exact, &frames, None);
        let mut sizes = vec![lossless.len()];
        for loss in [100, 300] {
            let lossy = written(
                &GifPlan {
                    loss,
                    ..exact.clone()
                },
                &frames,
                None,
            );
            let shown = shown(&lossy);
            assert_eq!(shown.len(), frames.len(), "every frame is still there");
            for (frame, picture) in frames.iter().zip(&shown) {
                for (wanted, got) in frame.as_chunks::<4>().0.iter().zip(picture.pixels()) {
                    let apart = |c: usize| u32::from(wanted[c].abs_diff(got[c])).pow(2);
                    let cost = apart(0) + 2 * apart(1) + apart(2);
                    assert!(
                        cost <= loss,
                        "a pixel is {cost} from its color, over {loss}"
                    );
                }
            }
            sizes.push(lossy.len());
        }
        assert!(
            sizes[1] * 10 < sizes[0] * 9 && sizes[2] < sizes[1],
            "more loss, smaller file: {sizes:?}"
        );
    }

    #[test]
    fn one_palette_serves_every_frame_when_their_colors_fit() {
        let frames = noisy_frames(96, 64, 64, 4);
        let colors: std::collections::BTreeSet<[u8; 3]> = frames
            .iter()
            .flat_map(|frame| frame.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]))
            .collect();
        let exact = GifPlan {
            change_threshold: 0,
            ..plan(96, 64)
        };
        let own = written(&exact, &frames, None);
        let shared = written(&exact, &frames, Some(colors.into_iter().collect()));
        // The file has a palette of its own, and no frame brings one.
        assert_eq!(shared[10] & 0x80, 0x80, "a global palette");
        assert!(decode(&shared).iter().all(|frame| frame.palette.is_none()));
        assert!(decode(&own).iter().all(|frame| frame.palette.is_some()));
        assert!(
            shared.len() < own.len(),
            "{} against {}",
            shared.len(),
            own.len()
        );
        // The pictures are the same either way.
        assert_eq!(shown(&shared), shown(&own));
        for (frame, picture) in frames.iter().zip(shown(&shared)) {
            assert_eq!(picture.as_raw(), frame);
        }
        // A color that is not in the shared palette gets the frame a palette of its own.
        let mut odd = frames.clone();
        odd[1][..4].copy_from_slice(&[250, 10, 10, 255]);
        let colors: Vec<[u8; 3]> = (0..64u8)
            .map(|i| [i * 4 + 3, i * 4 + 3, i * 4 + 3])
            .collect();
        let mixed = written(&exact, &odd, Some(colors));
        assert_eq!(shown(&mixed)[1].get_pixel(0, 0).0, [250, 10, 10, 255]);
    }

    #[test]
    fn delays_add_up_to_the_exact_duration() {
        let writer =
            Writer::new(Vec::new(), 4, 4, &plan(4, 4), gif::Repeat::Infinite, None).unwrap();
        let total: u32 = (0..12).map(|i| u32::from(writer.delay(i))).sum();
        assert_eq!(total, 100, "12 frames at 12 fps last one second");
    }

    #[test]
    fn identical_frames_merge_and_changes_are_cropped() {
        let (w, h) = (16, 8);
        let mut writer =
            Writer::new(Vec::new(), 16, 8, &plan(16, 8), gif::Repeat::Infinite, None).unwrap();
        let red = solid(w, h, [200, 0, 0, 255]);
        writer.push_at_rate(&red).unwrap();
        writer.push_at_rate(&red).unwrap(); // identical: merged
        let mut changed = red.clone();
        for y in 2..4 {
            for x in 5..9 {
                changed[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[0, 0, 200, 255]);
            }
        }
        writer.push_at_rate(&changed).unwrap();
        writer.flush_pending().unwrap();
        let bytes = writer.encoder.into_inner().unwrap();
        let frames = decode(&bytes);
        assert_eq!(frames.len(), 2);
        let timing =
            Writer::new(Vec::new(), 1, 1, &plan(1, 1), gif::Repeat::Infinite, None).unwrap();
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
        let mut writer =
            Writer::new(Vec::new(), 8, 8, &denoising, gif::Repeat::Infinite, None).unwrap();
        // A grey scene whose pixels flicker by up to 6 levels from frame to frame (sensor noise).
        for i in 0..6u8 {
            let frame: Vec<u8> = (0..w * h)
                .flat_map(|p| {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = 120 + ((p as u8).wrapping_mul(7).wrapping_add(i * 5) % 7);
                    [v, v, v, 255]
                })
                .collect();
            writer.push_at_rate(&frame).unwrap();
        }
        // A real change: a white square.
        let mut moved = solid(w, h, [123, 123, 123, 255]);
        for y in 0..3 {
            for x in 0..3 {
                moved[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        writer.push_at_rate(&moved).unwrap();
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
        let mut writer =
            Writer::new(Vec::new(), 8, 8, &plain, gif::Repeat::Infinite, None).unwrap();
        for i in 0..6u8 {
            let frame: Vec<u8> = (0..w * h)
                .flat_map(|p| {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = 120 + ((p as u8).wrapping_mul(7).wrapping_add(i * 5) % 7);
                    [v, v, v, 255]
                })
                .collect();
            writer.push_at_rate(&frame).unwrap();
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
        let mut writer =
            Writer::new(Vec::new(), 4, 4, &plan(4, 4), gif::Repeat::Infinite, None).unwrap();
        let mut frame = solid(4, 4, [10, 200, 10, 255]);
        frame[3] = 0; // one transparent pixel
        writer.push_at_rate(&frame).unwrap();
        writer.push_at_rate(&frame).unwrap();
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
