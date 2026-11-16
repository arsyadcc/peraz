//! Fitting a still image in a file size.
//!
//! The quality setting is lowered first: the highest one whose file fits is used (found by
//! halving, in about eight encodes), down to a floor below which pictures look broken. Only when the floor does not fit is the picture made
//! smaller, step by step, with the quality searched again at each size. Every candidate is a
//! real encode, so the result is known to fit.

use std::sync::atomic::{AtomicBool, Ordering};

use image::imageops::FilterType;
use peraz_core::ImagePlan;

use crate::ffmpeg::Toolchain;
use crate::image::{Decoded, EncodedImage, encode_prepared, prepare};
use crate::score::{quality_of, with_quality};

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
    loop {
        let planned = plan.encoding;
        let mut encode = |encoding: peraz_core::ImageEncoding| -> Result<EncodedImage, String> {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_owned());
            }
            let mut probe = plan.clone();
            probe.encoding = encoding;
            target.attempts += 1;
            encode_prepared(tools, &prepared, &probe)
        };
        let fits = |encoded: &EncodedImage| encoded.bytes.len() as u64 <= target.target_bytes;
        // The quality level's own encoding first (what the picture gets without a target),
        // then the floor: between a setting that is too large and one that fits, the highest
        // that fits is found by halving.
        let top = encode(planned)?;
        let mut found = fits(&top).then_some((planned, top));
        let mut smallest = u64::MAX;
        if found.is_none() {
            let bottom = encode(with_quality(planned, floor))?;
            smallest = bottom.bytes.len() as u64;
            if fits(&bottom) {
                let (mut low, mut high) = (floor, start);
                let mut best = bottom;
                while high - low > 1 {
                    let middle = low + (high - low) / 2;
                    let encoded = encode(with_quality(planned, middle))?;
                    if fits(&encoded) {
                        (low, best) = (middle, encoded);
                    } else {
                        high = middle;
                    }
                }
                found = Some((with_quality(planned, low), best));
            }
        }
        if let Some((encoding, encoded)) = found {
            plan.encoding = encoding;
            target.reached = true;
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
        let next = smaller(size, smallest, target.target_bytes);
        if target.keep_dimensions || shrinks > MAX_SHRINKS || next == size {
            plan.target = Some(target);
            return Ok(Fitted {
                encoded: None,
                smallest,
                prepared,
                lossless: false,
            });
        }
        prepared.image = prepared
            .image
            .resize_exact(next.0, next.1, FilterType::Lanczos3);
        target.scaled_to = Some(next);
        // A measured result is scored against the picture at this size.
        (plan.max_width, plan.max_height) = (Some(next.0), Some(next.1));
    }
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
