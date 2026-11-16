//! Fitting a still image in a file size.
//!
//! The quality setting is lowered first: the highest one whose file fits is used (found by
//! halving, in about eight encodes), down to a floor below which pictures look broken. A PNG
//! has more to give then: a palette of fewer colors, with or without dithering. Only when
//! that does not fit either is the picture made smaller, step by step, with the search
//! repeated at each size. Every candidate is a real encode, so the result is known to fit.

use std::sync::atomic::{AtomicBool, Ordering};

use image::imageops::FilterType;
use peraz_core::{ImageEncoding, ImagePlan, PngPalette};

use crate::ffmpeg::Toolchain;
use crate::image::{Decoded, EncodedImage, encode_prepared, prepare};
use crate::score::{quality_of, with_quality};

/// The palette sizes tried for a PNG that does not fit at the lowest palette quality, from
/// the most colors down.
const PALETTE_SIZES: [u16; 6] = [256, 128, 64, 32, 16, 8];

/// The lowest SSIMULACRA2 score ("medium") a picture may have after its palette was cut, when
/// it could be made smaller in pixels instead: below it, fewer pixels look better than fewer
/// colors, so the picture is scaled down, and the cut palette is used only if that does not
/// fit either (flat graphics get larger when scaled). With `--keep-dimensions` there is no
/// such choice, and any palette that fits is used.
const CUT_SCORE_FLOOR: f64 = 50.0;

/// Most times a picture is made smaller before giving up.
const MAX_SHRINKS: u32 = 8;
/// A picture is not made smaller than this on its shorter side.
const MIN_SIDE: u32 = 16;

/// What the search for a size found.
pub(crate) struct Fitted {
    /// The encoded picture, when one fits.
    pub encoded: Option<EncodedImage>,
    /// The smallest file made, in bytes.
    pub smallest: u64,
    /// The picture as the encoder got it, at the size it ended up with: what a quality score
    /// of the result is measured against.
    pub prepared: Decoded,
    /// The format is lossless: there was no quality to lower, so nothing else was tried.
    pub lossless: bool,
}

/// Encodes a still image so that it fits the plan's size target. `plan` is updated with the
/// setting used, the number of encodes, and the size the picture was scaled to, if it was.
pub(crate) fn encode_image_to_size(
    tools: Option<&Toolchain>,
    decoded: Decoded,
    plan: &mut ImagePlan,
    cancel: &AtomicBool,
) -> Result<Fitted, String> {
    let Some(mut target) = plan.target else {
        return Err("internal error: no size target".to_owned());
    };
    let mut prepared = prepare(decoded, plan);
    let Some(floor) = plan.encoding.quality_floor() else {
        // A lossless format: its one result fits, or the size cannot be reached this way.
        let encoded = encode_prepared(tools, &prepared, plan)?;
        target.attempts = 1;
        target.reached = encoded.bytes.len() as u64 <= target.target_bytes;
        plan.target = Some(target);
        return Ok(Fitted {
            smallest: encoded.bytes.len() as u64,
            encoded: target.reached.then_some(encoded),
            prepared,
            lossless: true,
        });
    };
    let start = quality_of(plan.encoding).max(floor);
    let mut shrinks = 0;
    // A PNG at its own size with a palette that fits but looks worse than "medium": used only
    // if making the picture smaller does not get it under the size either, or looks worse.
    let mut held: Option<Held> = None;
    loop {
        let mut round = search(tools, &prepared, plan, (floor, start), shrinks == 0, cancel)?;
        target.attempts += round.attempts;
        if let Some(cut) = round.cut.take() {
            if target.keep_dimensions || cut.score >= CUT_SCORE_FLOOR {
                round.found = Some((round.lowest, Some(cut.palette), cut.encoded));
            } else {
                held = Some(Held {
                    cut,
                    encoding: round.lowest,
                    prepared: prepared.clone(),
                    bounds: (plan.max_width, plan.max_height),
                });
            }
        }
        // A smaller picture fits, and so did the picture at its own size with fewer colors:
        // the one that looks better in the picture's place is used.
        let cut_wins = match (&round.found, &held) {
            (Some((_, _, scaled)), Some(kept)) => {
                crate::score::score_of(&kept.prepared.image, &scaled.bytes)
                    .is_none_or(|score| kept.cut.score >= score)
            }
            _ => false,
        };
        if let (Some((encoding, palette, encoded)), false) = (round.found, cut_wins) {
            plan.encoding = encoding;
            target.reached = true;
            target.palette = palette;
            plan.target = Some(target);
            return Ok(Fitted {
                smallest: encoded.bytes.len() as u64,
                encoded: Some(encoded),
                prepared,
                lossless: false,
            });
        }
        // Even the floor is too large at this size: make the picture smaller.
        let size = (prepared.image.width(), prepared.image.height());
        shrinks += 1;
        let next = smaller(size, round.smallest, target.target_bytes);
        if cut_wins || target.keep_dimensions || shrinks > MAX_SHRINKS || next == size {
            // Nothing better by scaling: the picture at its own size with fewer colors, if
            // that fits.
            let Some(held) = held else {
                plan.target = Some(target);
                return Ok(Fitted {
                    encoded: None,
                    smallest: round.smallest,
                    prepared,
                    lossless: false,
                });
            };
            plan.encoding = held.encoding;
            (plan.max_width, plan.max_height) = held.bounds;
            target.reached = true;
            target.scaled_to = None;
            target.palette = Some(held.cut.palette);
            plan.target = Some(target);
            return Ok(Fitted {
                smallest: held.cut.encoded.bytes.len() as u64,
                encoded: Some(held.cut.encoded),
                prepared: held.prepared,
                lossless: false,
            });
        }
        prepared.image = prepared
            .image
            .resize_exact(next.0, next.1, FilterType::Lanczos3);
        target.scaled_to = Some(next);
        plan.target = Some(target);
        // A measured result is scored against the picture at this size.
        (plan.max_width, plan.max_height) = (Some(next.0), Some(next.1));
    }
}

/// What the search at one size of the picture found.
struct Round {
    /// The best setting that fits, with its file; for a PNG, also a palette cut to fit.
    found: Option<(ImageEncoding, Option<PngPalette>, EncodedImage)>,
    /// For a PNG too large at the lowest palette quality: a smaller palette that fits.
    cut: Option<Cut>,
    /// The plan's encoding at the lowest quality setting.
    lowest: ImageEncoding,
    /// The smallest file made, in bytes.
    smallest: u64,
    /// Encodes it took.
    attempts: u32,
}

/// Searches the quality settings from `floor` to `start` for the highest whose file fits the
/// plan's size target, at the size the picture has now. With `cut_palettes`, a PNG that is too
/// large even at the floor is tried with smaller palettes.
fn search(
    tools: Option<&Toolchain>,
    prepared: &Decoded,
    plan: &ImagePlan,
    (floor, start): (u8, u8),
    cut_palettes: bool,
    cancel: &AtomicBool,
) -> Result<Round, String> {
    let planned = plan.encoding;
    let lowest = with_quality(planned, floor);
    let target = plan.target.ok_or("internal error: no size target")?;
    let limit = target.target_bytes;
    let mut attempts = 0;
    // One encode: with this quality setting, and for a PNG optionally with a palette cut down
    // further.
    let mut encode =
        |encoding: ImageEncoding, palette: Option<PngPalette>| -> Result<EncodedImage, String> {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_owned());
            }
            let mut probe = plan.clone();
            probe.encoding = encoding;
            probe.target = Some(peraz_core::ImageSizeTarget { palette, ..target });
            attempts += 1;
            encode_prepared(tools, prepared, &probe)
        };
    let fits = |encoded: &EncodedImage| encoded.bytes.len() as u64 <= limit;
    // The quality level's own encoding first (what the picture gets without a target), then
    // the floor: between a setting that is too large and one that fits, the highest that fits
    // is found by halving.
    let top = encode(planned, None)?;
    let (mut found, mut cut, mut smallest) = (None, None, u64::MAX);
    if fits(&top) {
        found = Some((planned, None, top));
    } else {
        let bottom = encode(lowest, None)?;
        smallest = bottom.bytes.len() as u64;
        if fits(&bottom) {
            let (mut low, mut high) = (floor, start);
            let mut best = bottom;
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                let encoded = encode(with_quality(planned, middle), None)?;
                if fits(&encoded) {
                    (low, best) = (middle, encoded);
                } else {
                    high = middle;
                }
            }
            found = Some((with_quality(planned, low), None, best));
        } else if matches!(planned, ImageEncoding::Png { .. }) && cut_palettes {
            // A PNG has more to give: fewer colors.
            let least;
            (cut, least) = cut_palette(&prepared.image, limit, |palette| {
                encode(lowest, Some(palette))
            })?;
            smallest = smallest.min(least);
        }
    }
    Ok(Round {
        found,
        cut,
        lowest,
        smallest,
        attempts,
    })
}

/// For a PNG that is too large at the lowest palette quality: the palette with the most
/// colors that fits in `limit` bytes, with dithering and without it, and of those two the one
/// that scores better against the picture. Without dithering, flat graphics lose nothing and
/// compress far better; photos usually look better with fewer colors that are dithered.
/// (Fewer levels per channel, posterization, was measured too: it saves nothing on top of
/// fewer colors.) Returns that palette with its file and score, if any fits, and the size of
/// the smallest file made.
fn cut_palette(
    picture: &image::DynamicImage,
    limit: u64,
    mut encode: impl FnMut(PngPalette) -> Result<EncodedImage, String>,
) -> Result<(Option<Cut>, u64), String> {
    let mut smallest = u64::MAX;
    // The largest palette that fits, with its score.
    let mut first_fit = |dithering: bool| -> Result<Option<Cut>, String> {
        for colors in PALETTE_SIZES {
            // 256 dithered colors are what the lowest quality setting already gave.
            if dithering && colors == 256 {
                continue;
            }
            let palette = PngPalette { colors, dithering };
            let encoded = encode(palette)?;
            let size = encoded.bytes.len() as u64;
            smallest = smallest.min(size);
            if size <= limit {
                // A picture too small to score counts as good enough.
                let score = crate::score::score_of(picture, &encoded.bytes).unwrap_or(100.0);
                return Ok(Some(Cut {
                    palette,
                    encoded,
                    score,
                }));
            }
        }
        Ok(None)
    };
    let (dithered, plain) = (first_fit(true)?, first_fit(false)?);
    // With equal scores, the dithered one.
    let best = match (dithered, plain) {
        (Some(dithered), Some(plain)) if plain.score > dithered.score => Some(plain),
        (dithered, plain) => dithered.or(plain),
    };
    Ok((best, smallest))
}

/// A palette that fits, with its file and how the file scores against the picture.
struct Cut {
    palette: PngPalette,
    encoded: EncodedImage,
    score: f64,
}

/// A cut palette kept in reserve, with what is needed to go back to it.
struct Held {
    cut: Cut,
    encoding: ImageEncoding,
    /// The picture at its own size.
    prepared: Decoded,
    /// The plan's bounding box before the picture was made smaller.
    bounds: (Option<u32>, Option<u32>),
}

/// The next smaller size to try for a picture whose smallest file was `bytes`: file sizes go
/// roughly with the number of pixels, so the sides shrink by the square root of what is
/// missing, a little more for safety, and by at least a tenth and at most a half per step.
fn smaller((width, height): (u32, u32), bytes: u64, target_bytes: u64) -> (u32, u32) {
    #[allow(clippy::cast_precision_loss)]
    let ratio = target_bytes as f64 / bytes.max(1) as f64;
    let factor = (ratio.sqrt() * 0.95).clamp(0.5, 0.9);
    let floor = f64::from(MIN_SIDE) / f64::from(width.min(height).max(1));
    let factor = factor.max(floor).min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scale = |side: u32| ((f64::from(side) * factor).round() as u32).clamp(1, side);
    (scale(width), scale(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_shrink_by_what_is_missing_within_bounds() {
        // Twice too large: the sides lose about a third.
        assert_eq!(smaller((1000, 800), 200_000, 100_000), (672, 537));
        // Far too large: at most a half per step.
        assert_eq!(smaller((1000, 800), 10_000_000, 100_000), (500, 400));
        // Barely too large: still a tenth, so the search moves on.
        assert_eq!(smaller((1000, 800), 101_000, 100_000), (900, 720));
        // Never below the smallest side worth keeping.
        assert_eq!(smaller((20, 16), 10_000, 100), (20, 16));
        assert_eq!(smaller((64, 32), 10_000, 100), (32, 16));
    }
}
