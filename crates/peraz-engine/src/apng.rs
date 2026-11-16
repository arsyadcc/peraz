//! Animated PNG: reading animations frame by frame (animated PNG, GIF, and animated WebP are
//! decoded in process, with their exact timing), and writing a small animated PNG.
//!
//! What keeps the output small: each frame holds only the rectangle that changed, pixels inside
//! it that did not change are left transparent so the previous frame shows through, identical
//! frames are merged into the previous frame's delay, and, below the highest quality level, all
//! frames share one palette of at most 256 colors. oxipng then recompresses every frame.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use image::{AnimationDecoder, ImageDecoder, RgbaImage};

/// A format whose animations Peraz decodes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Apng,
    Gif,
    Webp,
}

/// What the header of an animated PNG says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PngAnimation {
    pub width: u32,
    pub height: u32,
    /// Frames in the animation; one for a still picture stored as an animated PNG.
    pub frames: u32,
    /// How many times it plays; 0 means forever.
    pub plays: u32,
    /// The pixels carry transparency (an alpha channel or a transparent palette entry).
    pub alpha: bool,
}

/// Reads the animation header (`acTL`, which comes before the first `IDAT`) of a PNG file;
/// `None` for a plain PNG and for other files.
pub fn png_animation(path: &Path) -> std::io::Result<Option<PngAnimation>> {
    let mut file = BufReader::new(File::open(path)?);
    let mut signature = [0u8; 8];
    if file.read_exact(&mut signature).is_err() || &signature != b"\x89PNG\r\n\x1A\n" {
        return Ok(None);
    }
    let mut size = None;
    let mut alpha = false;
    loop {
        let mut head = [0u8; 8];
        if file.read_exact(&mut head).is_err() {
            return Ok(None);
        }
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let mut data = [0u8; 13];
        let wanted = match &head[4..] {
            b"IHDR" => 13,
            b"acTL" => 8,
            b"tRNS" => {
                alpha = true;
                0
            }
            b"IDAT" | b"IEND" => return Ok(None),
            _ => 0,
        };
        if u32::try_from(wanted).is_ok_and(|wanted| wanted > length)
            || file.read_exact(&mut data[..wanted]).is_err()
        {
            return Ok(None);
        }
        let word =
            |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        match &head[4..] {
            b"IHDR" => {
                size = Some((word(0), word(4)));
                // Color types 4 and 6 have an alpha channel.
                alpha = matches!(data[9], 4 | 6);
            }
            b"acTL" => {
                let Some((width, height)) = size else {
                    return Ok(None);
                };
                return Ok(Some(PngAnimation {
                    width,
                    height,
                    frames: word(0),
                    plays: word(4),
                    alpha,
                }));
            }
            _ => {}
        }
        // Skip the rest of the data and the CRC.
        let rest = i64::from(length) - i64::try_from(wanted).unwrap_or(0) + 4;
        file.seek(SeekFrom::Current(rest))?;
    }
}

/// Recognizes an animation Peraz decodes itself from the first bytes of a file: an animated PNG
/// with more than one frame, a GIF, or an animated WebP.
pub fn sniff(path: &Path) -> std::io::Result<Option<Source>> {
    let mut head = [0u8; 32];
    let filled = {
        let mut file = File::open(path)?;
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        filled
    };
    let head = &head[..filled];
    if head.starts_with(b"GIF8") {
        return Ok(Some(Source::Gif));
    }
    // Extended WebP: flags after the `VP8X` chunk header, bit 1 marks an animation.
    if head.starts_with(b"RIFF")
        && head.get(8..16) == Some(b"WEBPVP8X")
        && head.get(20).is_some_and(|flags| flags & 0x02 != 0)
    {
        return Ok(Some(Source::Webp));
    }
    Ok(png_animation(path)?
        .filter(|animation| animation.frames > 1)
        .map(|_| Source::Apng))
}

/// How long a frame is shown, in seconds, as the fraction an animated PNG stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delay {
    pub numerator: u16,
    pub denominator: u16,
}

impl Delay {
    /// One frame at `fps` frames per second.
    pub fn of_frame_rate(fps: u32) -> Self {
        Self {
            numerator: 1,
            denominator: u16::try_from(fps.max(1)).unwrap_or(u16::MAX),
        }
    }

    /// From a number of milliseconds given as a fraction, exactly when 16-bit numbers can hold
    /// it, and rounded to milliseconds (or coarser, for very long delays) otherwise.
    fn from_ms_ratio(numerator: u32, denominator: u32) -> Self {
        let (n, d) = (u64::from(numerator), u64::from(denominator.max(1)) * 1000);
        let divisor = gcd(n, d).max(1);
        if let (Ok(numerator), Ok(denominator)) =
            (u16::try_from(n / divisor), u16::try_from(d / divisor))
        {
            return Self {
                numerator,
                denominator,
            };
        }
        let ms = (n * 1000 + d / 2) / d;
        for (per_second, scale) in [(1000u16, 1u64), (100, 10), (1, 1000)] {
            if let Ok(numerator) = u16::try_from((ms + scale / 2) / scale) {
                return Self {
                    numerator,
                    denominator: per_second,
                };
            }
        }
        Self {
            numerator: u16::MAX,
            denominator: 1,
        }
    }

    /// The two delays one after the other, when 16-bit numbers can hold the sum exactly.
    fn plus(self, other: Self) -> Option<Self> {
        // A denominator of 0 means 100, by the format's definition.
        let den = |d: u16| if d == 0 { 100 } else { u64::from(d) };
        let (a, b) = (den(self.denominator), den(other.denominator));
        let n = u64::from(self.numerator) * b + u64::from(other.numerator) * a;
        let d = a * b;
        let divisor = gcd(n, d).max(1);
        Some(Self {
            numerator: u16::try_from(n / divisor).ok()?,
            denominator: u16::try_from(d / divisor).ok()?,
        })
    }

    /// How long the frame is shown, in milliseconds.
    pub fn milliseconds(self) -> f64 {
        let denominator = if self.denominator == 0 {
            100
        } else {
            self.denominator
        };
        f64::from(self.numerator) * 1000.0 / f64::from(denominator)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The frames of an animation decoded in process: whole pictures at the animation's size, each
/// with its delay.
pub struct Frames {
    inner: image::Frames<'static>,
    pub width: u32,
    pub height: u32,
    /// How many times the animation plays; 0 means forever.
    pub plays: u32,
    pub icc: Option<Vec<u8>>,
}

impl std::fmt::Debug for Frames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frames({}x{})", self.width, self.height)
    }
}

impl Iterator for Frames {
    type Item = Result<(RgbaImage, Delay), String>;

    fn next(&mut self) -> Option<Self::Item> {
        let frame = self.inner.next()?;
        Some(
            frame
                .map(|frame| {
                    let (numerator, denominator) = frame.delay().numer_denom_ms();
                    (
                        frame.into_buffer(),
                        Delay::from_ms_ratio(numerator, denominator),
                    )
                })
                .map_err(|e| format!("cannot decode a frame: {e}")),
        )
    }
}

/// Opens an animation for decoding frame by frame.
pub fn decode(path: &Path, source: Source) -> Result<Frames, String> {
    let name = path.display();
    let open = || {
        File::open(path)
            .map(BufReader::new)
            .map_err(|e| format!("cannot read {name}: {e}"))
    };
    let error = |e: image::ImageError| format!("cannot decode {name}: {e}");
    let plays = |count: image::metadata::LoopCount| match count {
        image::metadata::LoopCount::Infinite => 0,
        image::metadata::LoopCount::Finite(n) => n.get(),
    };
    match source {
        Source::Apng => {
            let mut decoder = image::codecs::png::PngDecoder::new(open()?).map_err(error)?;
            let (width, height) = decoder.dimensions();
            let icc = decoder.icc_profile().map_err(error)?;
            let decoder = decoder.apng().map_err(error)?;
            Ok(Frames {
                width,
                height,
                plays: plays(decoder.loop_count()),
                icc,
                inner: decoder.into_frames(),
            })
        }
        Source::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(open()?).map_err(error)?;
            let (width, height) = decoder.dimensions();
            Ok(Frames {
                width,
                height,
                plays: plays(decoder.loop_count()),
                icc: None,
                inner: decoder.into_frames(),
            })
        }
        Source::Webp => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(open()?).map_err(error)?;
            let (width, height) = decoder.dimensions();
            let icc = decoder.icc_profile().map_err(error)?;
            Ok(Frames {
                width,
                height,
                plays: webp_plays(path).unwrap_or(0),
                icc,
                inner: decoder.into_frames(),
            })
        }
    }
}

/// The loop count of an animated WebP, from its `ANIM` chunk; 0 means forever.
fn webp_plays(path: &Path) -> Option<u32> {
    let mut file = BufReader::new(File::open(path).ok()?);
    file.seek(SeekFrom::Start(12)).ok()?;
    loop {
        let mut head = [0u8; 8];
        file.read_exact(&mut head).ok()?;
        let length = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if &head[..4] == b"ANIM" {
            // Background color (4 bytes), then the loop count.
            let mut data = [0u8; 6];
            file.read_exact(&mut data).ok()?;
            return Some(u32::from(u16::from_le_bytes([data[4], data[5]])));
        }
        if &head[..4] == b"ANMF" {
            return None;
        }
        // Chunks are padded to an even length.
        file.seek(SeekFrom::Current(i64::from(length) + i64::from(length & 1)))
            .ok()?;
    }
}

/// Why encoding stopped early.
#[derive(Debug)]
pub enum ApngError {
    Cancelled,
    Failed(String),
}

impl From<String> for ApngError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// How a frame's rectangle is put on the picture before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blend {
    /// Replaces the rectangle.
    Source,
    /// Drawn over it: transparent pixels leave what was there.
    Over,
}

/// The part of a frame that is written: the rectangle that changed.
struct Delta {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    blend: Blend,
    delay: Delay,
}

/// Most bytes of frame differences held in memory before giving up: an animated PNG is written
/// only once every frame is known.
const MAX_BUFFERED_BYTES: usize = 3 << 30;

/// An encoded animated PNG.
#[derive(Debug)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    /// What did the work, for the report.
    pub encoder: String,
    /// Frames written, after merging identical ones.
    pub frames: usize,
}

/// Collects frames (whole pictures, RGBA) and writes them as an animated PNG.
pub struct Encoder {
    width: u32,
    height: u32,
    /// What a viewer currently sees.
    canvas: Vec<u8>,
    deltas: Vec<Delta>,
    buffered: usize,
    /// Frames pushed.
    pub frames_seen: u64,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Encoder({}x{}, {} frames)",
            self.width, self.height, self.frames_seen
        )
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            canvas: Vec::new(),
            deltas: Vec::new(),
            buffered: 0,
            frames_seen: 0,
        }
    }

    /// Adds the next frame: `rgba` holds the whole picture, shown for `delay`.
    pub fn push(&mut self, rgba: &[u8], delay: Delay) -> Result<(), String> {
        let (width, height) = (self.width as usize, self.height as usize);
        if rgba.len() != width * height * 4 {
            return Err("internal error: a frame has the wrong size".to_owned());
        }
        self.frames_seen += 1;
        let delta = if self.canvas.is_empty() {
            Delta {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
                rgba: rgba.to_vec(),
                blend: Blend::Source,
                delay,
            }
        } else if let Some(delta) = self.difference(rgba, delay) {
            delta
        } else {
            // Nothing changed: the previous frame stays longer. When its delay cannot hold
            // the sum, a one-pixel frame that changes nothing carries the time.
            let previous = self.deltas.last_mut().ok_or("internal error: no frame")?;
            if let Some(sum) = previous.delay.plus(delay) {
                previous.delay = sum;
                return Ok(());
            }
            Delta {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                blend: Blend::Over,
                delay,
            }
        };
        self.buffered += delta.rgba.len();
        if self.buffered > MAX_BUFFERED_BYTES {
            return Err(
                "too many frames for an animated PNG; use a shorter --duration, a smaller \
                 --max-width, or a lower --fps"
                    .to_owned(),
            );
        }
        self.deltas.push(delta);
        self.canvas.clear();
        self.canvas.extend_from_slice(rgba);
        Ok(())
    }

    /// The rectangle of `rgba` that differs from the picture before it; `None` when nothing does.
    fn difference(&self, rgba: &[u8], delay: Delay) -> Option<Delta> {
        let (width, height) = (self.width as usize, self.height as usize);
        let changed = |x: usize, y: usize| {
            let at = (y * width + x) * 4;
            rgba[at..at + 4] != self.canvas[at..at + 4]
        };
        let row_changed = |y: usize| {
            rgba[y * width * 4..(y + 1) * width * 4]
                != self.canvas[y * width * 4..(y + 1) * width * 4]
        };
        let top = (0..height).find(|y| row_changed(*y))?;
        let bottom = (top..height).rev().find(|y| row_changed(*y))?;
        let left = (0..width).find(|x| (top..=bottom).any(|y| changed(*x, y)))?;
        let right = (left..width)
            .rev()
            .find(|x| (top..=bottom).any(|y| changed(*x, y)))?;

        // Unchanged pixels can be left transparent when the frame is drawn over the previous
        // one, which is only the same picture when every changed pixel is opaque.
        let mut over = true;
        let mut pixels = Vec::with_capacity((right - left + 1) * (bottom - top + 1) * 4);
        for y in top..=bottom {
            for x in left..=right {
                let at = (y * width + x) * 4;
                if changed(x, y) {
                    over &= rgba[at + 3] == 255;
                    pixels.extend_from_slice(&rgba[at..at + 4]);
                } else {
                    pixels.extend_from_slice(&[0; 4]);
                }
            }
        }
        if !over {
            pixels.clear();
            for y in top..=bottom {
                let row = (y * width + left) * 4..(y * width + right + 1) * 4;
                pixels.extend_from_slice(&rgba[row]);
            }
        }
        Some(Delta {
            x: u32::try_from(left).ok()?,
            y: u32::try_from(top).ok()?,
            width: u32::try_from(right - left + 1).ok()?,
            height: u32::try_from(bottom - top + 1).ok()?,
            rgba: pixels,
            blend: if over { Blend::Over } else { Blend::Source },
            delay,
        })
    }

    /// The average frame rate of what was pushed, rounded, at least 1.
    pub fn average_fps(&self) -> u32 {
        let ms: f64 = self.deltas.iter().map(|d| d.delay.milliseconds()).sum();
        if ms <= 0.0 {
            return 1;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let fps = (self.frames_seen as f64 * 1000.0 / ms)
            .round()
            .clamp(1.0, 1000.0) as u32;
        fps
    }

    /// Writes the animation. `plays` is how many times it plays (0: forever). With a `palette`
    /// quality range (0–100, pngquant-style), all frames share one palette when it reaches the
    /// minimum; otherwise, and without a range, the frames stay true-color (lossless).
    pub fn finish(
        self,
        plays: u32,
        palette: Option<(u8, u8)>,
        icc: Option<&[u8]>,
        cancel: &AtomicBool,
    ) -> Result<Encoded, ApngError> {
        if self.deltas.is_empty() {
            return Err("the source has no frames in the selected range"
                .to_owned()
                .into());
        }
        let indexed = match palette {
            Some((min, max)) => quantize(&self.deltas, min, max, cancel)?,
            None => None,
        };
        if cancel.load(Ordering::SeqCst) {
            return Err(ApngError::Cancelled);
        }
        let frames = self.deltas.len();
        let options = oxipng::Options {
            strip: oxipng::StripChunks::Safe,
            ..oxipng::Options::from_preset(2)
        };
        let optimized = |indexed: Option<&Indexed>| -> Result<Vec<u8>, String> {
            let png = write(self.width, self.height, &self.deltas, indexed, plays, icc)?;
            oxipng::optimize_from_memory(&png, &options).map_err(|e| format!("oxipng: {e}"))
        };
        // Dithered palette frames can compress worse than smooth true-color ones, and then the
        // lossless frames are both smaller and better: both are made, and the smaller is kept.
        let (bytes, indexed) = match indexed {
            Some(indexed) => {
                let (palette, lossless) = std::thread::scope(|scope| {
                    let lossless = scope.spawn(|| optimized(None));
                    (optimized(Some(&indexed)), lossless.join())
                });
                let lossless = lossless.map_err(|_| "the PNG optimizer crashed".to_owned())??;
                let palette = palette?;
                if lossless.len() <= palette.len() {
                    (lossless, None)
                } else {
                    (palette, Some(indexed))
                }
            }
            None => (optimized(None)?, None),
        };
        let encoder = match &indexed {
            Some(indexed) => format!(
                "imagequant q{} + oxipng · {frames} frames",
                indexed.achieved
            ),
            None => format!("oxipng (lossless) · {frames} frames"),
        };
        Ok(Encoded {
            bytes,
            encoder,
            frames,
        })
    }
}

/// Frames reduced to one shared palette.
struct Indexed {
    /// RGB per entry.
    palette: Vec<u8>,
    /// Alpha per entry, without the opaque entries at the end.
    alpha: Vec<u8>,
    /// Palette indexes per frame, in frame order.
    frames: Vec<Vec<u8>>,
    achieved: u8,
}

/// One palette for every frame, when the quality range can be met and transparency survives it
/// exactly (frames drawn over the previous one depend on that); `None` otherwise.
fn quantize(
    deltas: &[Delta],
    min_quality: u8,
    max_quality: u8,
    cancel: &AtomicBool,
) -> Result<Option<Indexed>, ApngError> {
    let liq_error = |e: imagequant::Error| format!("imagequant: {e}");
    let mut attributes = imagequant::new();
    attributes
        .set_quality(min_quality, max_quality)
        .map_err(liq_error)?;
    attributes.set_speed(4).map_err(liq_error)?;
    let image = |delta: &Delta| {
        let pixels: Vec<imagequant::RGBA> = delta
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3]))
            .collect();
        attributes
            .new_image(pixels, delta.width as usize, delta.height as usize, 0.0)
            .map_err(liq_error)
    };
    let mut histogram = imagequant::Histogram::new(&attributes);
    for delta in deltas {
        if cancel.load(Ordering::SeqCst) {
            return Err(ApngError::Cancelled);
        }
        histogram
            .add_image(&attributes, &mut image(delta)?)
            .map_err(liq_error)?;
    }
    let mut result = match histogram.quantize(&attributes) {
        Ok(result) => result,
        Err(imagequant::Error::QualityTooLow) => return Ok(None),
        Err(e) => return Err(liq_error(e).into()),
    };
    result.set_dithering_level(1.0).map_err(liq_error)?;
    let achieved = result.quantization_quality().unwrap_or(max_quality);
    let mut shared: Option<Vec<imagequant::RGBA>> = None;
    let mut frames = Vec::with_capacity(deltas.len());
    for delta in deltas {
        if cancel.load(Ordering::SeqCst) {
            return Err(ApngError::Cancelled);
        }
        let (palette, indexes) = result.remapped(&mut image(delta)?).map_err(liq_error)?;
        // Every frame must mean the same colors by the same indexes.
        let first = shared.get_or_insert_with(|| palette.clone());
        let exact = *first == palette
            && delta
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .zip(&indexes)
                .all(|(p, index)| {
                    let alpha = palette[usize::from(*index)].a;
                    match p[3] {
                        0 => alpha == 0,
                        255 => alpha == 255,
                        _ => true,
                    }
                });
        if !exact {
            return Ok(None);
        }
        frames.push(indexes);
    }
    let palette = shared.unwrap_or_default();
    let mut alpha: Vec<u8> = palette.iter().map(|c| c.a).collect();
    while alpha.last() == Some(&255) {
        alpha.pop();
    }
    Ok(Some(Indexed {
        palette: palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect(),
        alpha,
        frames,
        achieved,
    }))
}

fn write(
    width: u32,
    height: u32,
    deltas: &[Delta],
    indexed: Option<&Indexed>,
    plays: u32,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let png_error = |e: png::EncodingError| format!("png: {e}");
    let mut info = png::Info::with_size(width, height);
    info.bit_depth = png::BitDepth::Eight;
    info.icc_profile = icc.map(|p| p.to_vec().into());
    match indexed {
        Some(indexed) => {
            info.color_type = png::ColorType::Indexed;
            info.palette = Some(indexed.palette.clone().into());
            if !indexed.alpha.is_empty() {
                info.trns = Some(indexed.alpha.clone().into());
            }
        }
        None => info.color_type = png::ColorType::Rgba,
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::with_info(&mut out, info).map_err(png_error)?;
    // oxipng compresses every frame again: the first pass only has to be quick.
    encoder.set_compression(png::Compression::Fast);
    let animated = deltas.len() > 1;
    if animated {
        let frames = u32::try_from(deltas.len()).map_err(|_| "too many frames".to_owned())?;
        encoder.set_animated(frames, plays).map_err(png_error)?;
    }
    let mut writer = encoder.write_header().map_err(png_error)?;
    for (i, delta) in deltas.iter().enumerate() {
        if animated {
            // The rectangle must fit at every step: shrink it, move it, then size it.
            writer.set_frame_dimension(1, 1).map_err(png_error)?;
            writer
                .set_frame_position(delta.x, delta.y)
                .map_err(png_error)?;
            writer
                .set_frame_dimension(delta.width, delta.height)
                .map_err(png_error)?;
            writer
                .set_frame_delay(delta.delay.numerator, delta.delay.denominator)
                .map_err(png_error)?;
            writer
                .set_blend_op(match delta.blend {
                    Blend::Source => png::BlendOp::Source,
                    Blend::Over => png::BlendOp::Over,
                })
                .map_err(png_error)?;
            writer
                .set_dispose_op(png::DisposeOp::None)
                .map_err(png_error)?;
        }
        let data = indexed.map_or(delta.rgba.as_slice(), |indexed| {
            indexed.frames[i].as_slice()
        });
        writer.write_image_data(data).map_err(png_error)?;
    }
    writer.finish().map_err(png_error)?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small synthetic images
mod tests {
    use super::*;

    static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("peraz-apng-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A gradient with an opaque square that moves one step per frame.
    fn moving_square(step: u32) -> RgbaImage {
        RgbaImage::from_fn(64, 48, |x, y| {
            if (step * 6..step * 6 + 10).contains(&x) && (10..20).contains(&y) {
                image::Rgba([250, 40, 40, 255])
            } else {
                image::Rgba([(x * 4) as u8, (y * 5) as u8, 90, 255])
            }
        })
    }

    fn delay(numerator: u16, denominator: u16) -> Delay {
        Delay {
            numerator,
            denominator,
        }
    }

    /// Decodes an animated PNG held in memory into whole frames with their delays.
    fn read_back(bytes: &[u8], name: &str) -> (Vec<(RgbaImage, Delay)>, u32) {
        let path = temp(name);
        std::fs::write(&path, bytes).unwrap();
        let frames = decode(&path, Source::Apng).unwrap();
        let plays = frames.plays;
        let frames = frames.map(Result::unwrap).collect();
        std::fs::remove_file(&path).unwrap();
        (frames, plays)
    }

    #[test]
    fn lossless_frames_timing_and_loops_survive() {
        let mut encoder = Encoder::new(64, 48);
        let pictures: Vec<RgbaImage> = (0..5).map(moving_square).collect();
        let delays = [
            delay(1, 15),
            delay(2, 15),
            delay(7, 100),
            delay(1, 15),
            delay(1, 2),
        ];
        for (picture, delay) in pictures.iter().zip(delays) {
            encoder.push(picture.as_raw(), delay).unwrap();
        }
        let made = encoder.finish(3, None, None, &NOT_CANCELLED).unwrap();
        assert_eq!(made.frames, 5);
        assert_eq!(made.encoder, "oxipng (lossless) · 5 frames");
        let header = {
            let path = temp("header.png");
            std::fs::write(&path, &made.bytes).unwrap();
            let header = png_animation(&path).unwrap().unwrap();
            assert_eq!(sniff(&path).unwrap(), Some(Source::Apng));
            header
        };
        assert_eq!(
            (header.width, header.height, header.frames, header.plays),
            (64, 48, 5, 3)
        );
        let (frames, plays) = read_back(&made.bytes, "lossless.png");
        assert_eq!(plays, 3);
        assert_eq!(frames.len(), 5);
        for ((frame, shown), (picture, wanted)) in frames.iter().zip(pictures.iter().zip(delays)) {
            assert_eq!(frame, picture, "every pixel of every frame is kept");
            assert!(
                (shown.milliseconds() - wanted.milliseconds()).abs() < 1e-9,
                "{shown:?} for {wanted:?}"
            );
        }
        // Only what changed is stored: far less than five whole frames.
        let whole: usize = pictures.iter().map(|p| p.as_raw().len()).sum();
        assert!(made.bytes.len() * 8 < whole, "{}", made.bytes.len());
    }

    #[test]
    fn identical_frames_extend_the_delay_before_them() {
        let mut encoder = Encoder::new(64, 48);
        for step in [0, 0, 0, 1, 1] {
            encoder
                .push(moving_square(step).as_raw(), delay(1, 10))
                .unwrap();
        }
        assert_eq!(encoder.average_fps(), 10);
        let made = encoder.finish(0, None, None, &NOT_CANCELLED).unwrap();
        assert_eq!(made.frames, 2);
        let (frames, plays) = read_back(&made.bytes, "merged.png");
        assert_eq!(plays, 0);
        let shown: Vec<f64> = frames.iter().map(|(_, d)| d.milliseconds()).collect();
        assert_eq!(shown, [300.0, 200.0]);

        // A delay too long for one frame is carried by a frame that changes nothing.
        let mut encoder = Encoder::new(64, 48);
        let still = moving_square(0);
        encoder.push(still.as_raw(), delay(60_000, 1)).unwrap();
        encoder.push(still.as_raw(), delay(60_000, 1)).unwrap();
        let made = encoder.finish(0, None, None, &NOT_CANCELLED).unwrap();
        assert_eq!(made.frames, 2);
        let (frames, _) = read_back(&made.bytes, "long.png");
        assert_eq!(frames[1].0, still);
        assert!((frames[1].1.milliseconds() - 60_000_000.0).abs() < 1e-3);

        // A single frame is a plain PNG.
        let mut encoder = Encoder::new(64, 48);
        encoder.push(still.as_raw(), delay(1, 10)).unwrap();
        let made = encoder.finish(0, None, None, &NOT_CANCELLED).unwrap();
        let path = temp("single.png");
        std::fs::write(&path, &made.bytes).unwrap();
        assert_eq!(png_animation(&path).unwrap(), None);
        assert_eq!(image::open(&path).unwrap().to_rgba8(), still);
    }

    #[test]
    fn changes_in_transparency_are_exact() {
        // A translucent shape that moves over a transparent background: pixels become more
        // and less opaque, which drawing over the previous frame could not express.
        let picture = |step: u32| {
            RgbaImage::from_fn(40, 30, |x, y| {
                if (step * 5..step * 5 + 12).contains(&x) && (8..20).contains(&y) {
                    image::Rgba([20, 200, 90, 128])
                } else {
                    image::Rgba([0, 0, 0, 0])
                }
            })
        };
        let mut encoder = Encoder::new(40, 30);
        for step in 0..4 {
            encoder.push(picture(step).as_raw(), delay(1, 12)).unwrap();
        }
        let made = encoder.finish(0, None, None, &NOT_CANCELLED).unwrap();
        let (frames, _) = read_back(&made.bytes, "translucent.png");
        for (step, (frame, _)) in frames.iter().enumerate() {
            assert_eq!(*frame, picture(step as u32), "frame {step}");
        }
    }

    #[test]
    fn a_shared_palette_keeps_the_animation_close_and_its_shape_exact() {
        // A sticker: a shaded disc on a transparent background that changes shade.
        let picture = |step: u32| {
            RgbaImage::from_fn(80, 80, |x, y| {
                let (dx, dy) = (x.abs_diff(40), y.abs_diff(40));
                if dx * dx + dy * dy < 30 * 30 {
                    // Grain, as in a photo: costly to keep exactly, cheap to approximate.
                    let grain = x
                        .wrapping_mul(2_654_435_761)
                        .wrapping_add(y * 40_503 + step);
                    let (r, g) = ((grain >> 9) % 24, (grain >> 17) % 24);
                    image::Rgba([(x * 2 + r) as u8, (y * 2 + g) as u8, (step * 40) as u8, 255])
                } else {
                    image::Rgba([0, 0, 0, 0])
                }
            })
        };
        let lossless = {
            let mut encoder = Encoder::new(80, 80);
            for step in 0..5 {
                encoder.push(picture(step).as_raw(), delay(1, 10)).unwrap();
            }
            encoder.finish(0, None, None, &NOT_CANCELLED).unwrap()
        };
        let mut encoder = Encoder::new(80, 80);
        for step in 0..5 {
            encoder.push(picture(step).as_raw(), delay(1, 10)).unwrap();
        }
        let icc = b"a color profile".to_vec();
        let made = encoder
            .finish(0, Some((30, 90)), Some(&icc), &NOT_CANCELLED)
            .unwrap();
        assert!(made.encoder.starts_with("imagequant q"), "{}", made.encoder);
        assert!(made.bytes.len() < lossless.bytes.len());
        let (frames, _) = read_back(&made.bytes, "palette.png");
        assert_eq!(frames.len(), 5);
        for (step, (frame, _)) in frames.iter().enumerate() {
            let wanted = picture(step as u32);
            for (got, want) in frame.pixels().zip(wanted.pixels()) {
                assert_eq!(got[3], want[3], "transparency is exact");
                if want[3] == 255 {
                    for channel in 0..3 {
                        assert!(
                            got[channel].abs_diff(want[channel]) < 64,
                            "{got:?} {want:?}"
                        );
                    }
                }
            }
        }
        // The profile is carried over.
        let path = temp("profile.png");
        std::fs::write(&path, &made.bytes).unwrap();
        let mut decoder =
            image::codecs::png::PngDecoder::new(BufReader::new(File::open(&path).unwrap()))
                .unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), Some(icc));

        // A range that cannot be met leaves the frames lossless.
        let noisy = |step: u32| {
            RgbaImage::from_fn(96, 96, |x, y| {
                let v = x
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(y * 40_503 + step * 977);
                image::Rgba([(v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8, 255])
            })
        };
        let mut encoder = Encoder::new(96, 96);
        for step in 0..3 {
            encoder.push(noisy(step).as_raw(), delay(1, 10)).unwrap();
        }
        let made = encoder
            .finish(0, Some((99, 100)), None, &NOT_CANCELLED)
            .unwrap();
        assert!(
            made.encoder.starts_with("oxipng (lossless)"),
            "{}",
            made.encoder
        );
        let (frames, _) = read_back(&made.bytes, "noisy.png");
        assert_eq!(frames[2].0, noisy(2));
    }

    #[test]
    fn gifs_and_animated_webps_are_read_with_their_timing() {
        // A GIF: three frames with different delays, played twice more.
        let gif_path = temp("source.gif");
        {
            let file = File::create(&gif_path).unwrap();
            let mut encoder = gif::Encoder::new(file, 64, 48, &[]).unwrap();
            encoder.set_repeat(gif::Repeat::Finite(2)).unwrap();
            for (step, centiseconds) in [(0, 10u16), (1, 25), (2, 7)] {
                let mut pixels = moving_square(step).into_raw();
                let mut frame = gif::Frame::from_rgba_speed(64, 48, &mut pixels, 10);
                frame.delay = centiseconds;
                encoder.write_frame(&frame).unwrap();
            }
        }
        assert_eq!(sniff(&gif_path).unwrap(), Some(Source::Gif));
        let frames = decode(&gif_path, Source::Gif).unwrap();
        assert_eq!((frames.width, frames.height, frames.plays), (64, 48, 2));
        let shown: Vec<f64> = frames.map(|f| f.unwrap().1.milliseconds()).collect();
        assert_eq!(shown, [100.0, 250.0, 70.0]);

        // An animated WebP: lossless frames at 0, 80, and 200 ms, and a last one at 300 ms.
        let webp_path = temp("source.webp");
        let pictures: Vec<RgbaImage> = (0..3).map(moving_square).collect();
        {
            let mut config = webp::WebPConfig::new().unwrap();
            config.lossless = 1;
            let mut encoder = webp::AnimEncoder::new(64, 48, &config);
            encoder.set_loop_count(4);
            for (picture, at) in pictures.iter().zip([0, 80, 200]) {
                encoder.add_frame(webp::AnimFrame::from_rgba(picture.as_raw(), 64, 48, at));
            }
            // A frame's time is when it starts: one more frame gives the third its length.
            let end = RgbaImage::new(64, 48);
            encoder.add_frame(webp::AnimFrame::from_rgba(end.as_raw(), 64, 48, 300));
            std::fs::write(&webp_path, &*encoder.encode()).unwrap();
        }
        assert_eq!(sniff(&webp_path).unwrap(), Some(Source::Webp));
        let frames = decode(&webp_path, Source::Webp).unwrap();
        assert_eq!((frames.width, frames.height, frames.plays), (64, 48, 4));
        let frames: Vec<(RgbaImage, Delay)> = frames.map(Result::unwrap).collect();
        assert_eq!(frames.len(), 4);
        for (frame, picture) in frames.iter().zip(&pictures) {
            let worst = frame
                .0
                .as_raw()
                .iter()
                .zip(picture.as_raw())
                .map(|(a, b)| a.abs_diff(*b))
                .max();
            // The WebP decoder blends frames with rounding: within one step of the source.
            assert!(worst <= Some(1), "{worst:?}");
        }
        let shown: Vec<f64> = frames.iter().take(3).map(|f| f.1.milliseconds()).collect();
        assert_eq!(shown, [80.0, 120.0, 100.0]);
        for path in [gif_path, webp_path] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn delays_convert_exactly_when_they_fit() {
        // 1000/15 ms is 1/15 s.
        assert_eq!(Delay::from_ms_ratio(200, 3), delay(1, 15));
        assert_eq!(Delay::from_ms_ratio(100, 1), delay(1, 10));
        assert_eq!(Delay::from_ms_ratio(0, 1), delay(0, 1));
        // Too fine for 16-bit numbers: rounded to milliseconds.
        assert_eq!(Delay::from_ms_ratio(100_003, 7), delay(14_286, 1000));
        // Too long for milliseconds: hundredths of a second.
        assert_eq!(Delay::from_ms_ratio(120_000, 1), delay(120, 1));
        assert_eq!(delay(1, 15).plus(delay(2, 15)), Some(delay(1, 5)));
        assert_eq!(delay(1, 0).plus(delay(1, 100)), Some(delay(1, 50)));
        assert_eq!(delay(60_000, 1).plus(delay(60_000, 1)), None);
        assert_eq!(Delay::of_frame_rate(12), delay(1, 12));
    }

    #[test]
    fn plain_pngs_and_other_files_are_not_animations() {
        let png = temp("plain.png");
        moving_square(0).save(&png).unwrap();
        assert_eq!(png_animation(&png).unwrap(), None);
        assert_eq!(sniff(&png).unwrap(), None);
        let text = temp("notes.txt");
        std::fs::write(&text, "plain text").unwrap();
        assert_eq!(sniff(&text).unwrap(), None);
        for path in [png, text] {
            std::fs::remove_file(path).unwrap();
        }
    }
}
