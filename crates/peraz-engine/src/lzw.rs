//! LZW compression for GIF frames, with an option to lose a little.
//!
//! LZW stores a run of pixels it has seen before as one code, so a frame compresses well where
//! it repeats itself. Dithered and noisy pictures rarely do, exactly: but they often repeat
//! themselves *nearly*. With a tolerance, the encoder may continue a run with a color that is
//! close enough to the one the pixel should have, which makes runs longer and the file smaller.
//! Every pixel stays within the tolerance of the color it was meant to show.

/// The largest number of codes a GIF's LZW table holds.
const TABLE: usize = 4096;
const NONE: u16 = u16::MAX;

/// What the encoder may change, and how far.
#[derive(Debug)]
pub struct Loss<'a> {
    /// The color each pixel should show.
    pub wanted: &'a [[u8; 3]],
    /// What is on the screen at each pixel before the frame is drawn, which is what a
    /// transparent pixel leaves there. `None` when transparent pixels stay see-through (the
    /// frame is drawn on a cleared canvas): those are then never exchanged for colors.
    pub under: Option<&'a [[u8; 3]]>,
    /// The frame's palette.
    pub palette: &'a [[u8; 3]],
    /// The palette index that means "leave the pixel as it is", if the frame has one.
    pub transparent: Option<u8>,
    /// How far a pixel may be from the color it should show: the largest sum of the squared
    /// differences of red, green, and blue (green counts double, as the eye does).
    pub tolerance: u32,
}

impl Loss<'_> {
    /// How far pixel `at` would be from its color if it were written as `symbol`; `None` when
    /// that cannot be known, so the symbol is not allowed there.
    fn cost(&self, at: usize, original: u8, symbol: u8) -> Option<u32> {
        let seen = if Some(symbol) == self.transparent {
            self.under?.get(at)?
        } else {
            // A pixel that stays see-through cannot take a color.
            if Some(original) == self.transparent && self.under.is_none() {
                return None;
            }
            self.palette.get(usize::from(symbol))?
        };
        let wanted = self.wanted.get(at)?;
        let apart = |channel: usize| u32::from(seen[channel].abs_diff(wanted[channel])).pow(2);
        Some(apart(0) + 2 * apart(1) + apart(2))
    }
}

/// Writes codes of changing width into bytes, lowest bits first.
struct Bits {
    bytes: Vec<u8>,
    pending: u32,
    count: u32,
}

impl Bits {
    fn put(&mut self, code: u16, width: u32) {
        self.pending |= u32::from(code) << self.count;
        self.count += width;
        while self.count >= 8 {
            self.bytes.push(self.pending.to_le_bytes()[0]);
            self.pending >>= 8;
            self.count -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.bytes.push(self.pending.to_le_bytes()[0]);
        }
        self.bytes
    }
}

/// The table of runs seen so far: each code is an earlier code plus one more pixel. The codes
/// that continue a code are kept as a list (first child, then siblings).
struct Table {
    first_child: Vec<u16>,
    sibling: Vec<u16>,
    symbol: Vec<u8>,
    /// The code that clears the table; the codes below it are the single pixels.
    clear: u16,
    next: u16,
    width: u32,
    minimum_width: u32,
}

impl Table {
    fn new(minimum_width: u32) -> Self {
        let clear = 1u16 << minimum_width;
        Self {
            first_child: vec![NONE; TABLE],
            sibling: vec![NONE; TABLE],
            symbol: vec![0; TABLE],
            clear,
            next: clear + 2,
            width: minimum_width + 1,
            minimum_width,
        }
    }

    fn reset(&mut self) {
        self.first_child.fill(NONE);
        self.next = self.clear + 2;
        self.width = self.minimum_width + 1;
    }

    /// The codes that continue `code`, each with the pixel it adds.
    fn continuations(&self, code: u16) -> impl Iterator<Item = (u16, u8)> + '_ {
        let mut at = self.first_child[usize::from(code)];
        std::iter::from_fn(move || {
            (at != NONE).then(|| {
                let found = (at, self.symbol[usize::from(at)]);
                at = self.sibling[usize::from(at)];
                found
            })
        })
    }

    fn is_full(&self) -> bool {
        usize::from(self.next) >= TABLE
    }

    /// Remembers `code` continued by `symbol` as a new code.
    fn add(&mut self, code: u16, symbol: u8) {
        let new = usize::from(self.next);
        self.symbol[new] = symbol;
        self.sibling[new] = self.first_child[usize::from(code)];
        self.first_child[usize::from(code)] = self.next;
        self.next += 1;
        // A reader learns each code one step later than the writer, so the codes grow a bit
        // wider only once the code after the last one of this width is known.
        if u32::from(self.next) > (1 << self.width) && self.width < 12 {
            self.width += 1;
        }
    }
}

/// Compresses the palette indexes of a frame for a GIF with `colors` palette entries: the
/// smallest code size, then the LZW data, as GIF stores them (before they are cut into
/// blocks). With `loss`, pixels may come out as another palette entry within its tolerance;
/// the second value is what each pixel was written as.
#[must_use]
pub fn encode(indexes: &[u8], colors: usize, loss: Option<&Loss>) -> (Vec<u8>, Vec<u8>) {
    // GIF's smallest code size is 2, also for palettes of two colors.
    let minimum_width = (2..8).find(|bits| colors <= 1 << bits).unwrap_or(8);
    let mut table = Table::new(minimum_width);
    let mut bits = Bits {
        bytes: vec![minimum_width.to_le_bytes()[0]],
        pending: 0,
        count: 0,
    };
    let mut written = Vec::with_capacity(indexes.len());
    bits.put(table.clear, table.width);
    let mut run: Option<u16> = None;
    for (at, &pixel) in indexes.iter().enumerate() {
        let Some(code) = run else {
            run = Some(u16::from(pixel));
            written.push(pixel);
            continue;
        };
        // The run goes on with this very pixel, or else with the closest color that the
        // tolerance allows.
        let exact = table
            .continuations(code)
            .find(|(_, symbol)| *symbol == pixel);
        let close = || {
            let loss = loss?;
            table
                .continuations(code)
                .filter_map(|(next, symbol)| Some((loss.cost(at, pixel, symbol)?, next, symbol)))
                .filter(|(cost, _, _)| *cost <= loss.tolerance)
                .min_by_key(|(cost, _, _)| *cost)
                .map(|(_, next, symbol)| (next, symbol))
        };
        if let Some((next, symbol)) = exact.or_else(close) {
            run = Some(next);
            written.push(symbol);
            continue;
        }
        bits.put(code, table.width);
        if table.is_full() {
            bits.put(table.clear, table.width);
            table.reset();
        } else {
            table.add(code, pixel);
        }
        run = Some(u16::from(pixel));
        written.push(pixel);
    }
    if let Some(code) = run {
        bits.put(code, table.width);
    }
    // The code after "clear" ends the data.
    bits.put(table.clear + 1, table.width);
    (bits.finish(), written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a GIF reader makes of the data: the pixels of a frame `width` wide.
    fn decoded(data: &[u8], palette: &[[u8; 3]], width: u16, height: u16) -> Vec<u8> {
        let mut file = Vec::new();
        {
            let flat: Vec<u8> = palette.iter().flatten().copied().collect();
            let mut encoder = gif::Encoder::new(&mut file, width, height, &flat).unwrap();
            let frame = gif::Frame {
                width,
                height,
                buffer: std::borrow::Cow::Borrowed(data),
                ..gif::Frame::default()
            };
            encoder.write_lzw_pre_encoded_frame(&frame).unwrap();
        }
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::Indexed);
        let mut decoder = options.read_info(file.as_slice()).unwrap();
        decoder.read_next_frame().unwrap().unwrap().buffer.to_vec()
    }

    fn palette(colors: usize) -> Vec<[u8; 3]> {
        (0..colors)
            .map(|i| {
                let v = u8::try_from(i * 255 / colors.max(2).saturating_sub(1)).unwrap();
                [v, v.wrapping_mul(3), 255 - v]
            })
            .collect()
    }

    /// Pixels with structure and noise, so that runs repeat, grow, and fill the table.
    fn pixels(count: usize, colors: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9u32;
        (0..count)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 28) as usize % 3;
                u8::try_from(((i / 7) % (colors / 2).max(1) + noise) % colors).unwrap()
            })
            .collect()
    }

    #[test]
    fn without_loss_every_pixel_reads_back() {
        // Every palette size, short and long frames: the long ones fill the table several
        // times and pass every code width.
        for colors in [2, 3, 4, 5, 16, 17, 100, 128, 129, 256] {
            for (width, height) in [(1u16, 1u16), (7, 3), (64, 64), (400, 300)] {
                let frame = pixels(usize::from(width) * usize::from(height), colors);
                let (data, written) = encode(&frame, colors, None);
                assert_eq!(written, frame);
                assert_eq!(
                    decoded(&data, &palette(colors), width, height),
                    frame,
                    "{colors} colors, {width}x{height}"
                );
            }
        }
        // Random pixels of 256 colors: hardly any runs, the table fills fast.
        let mut state = 7u32;
        let noise: Vec<u8> = (0..200_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let (data, _) = encode(&noise, 256, None);
        assert_eq!(decoded(&data, &palette(256), 500, 400), noise);
        // One color: the longest runs there are.
        let flat = vec![3u8; 120_000];
        let (data, _) = encode(&flat, 16, None);
        assert_eq!(decoded(&data, &palette(16), 400, 300), flat);
        assert!(data.len() < 1500, "{} bytes", data.len());
    }

    #[test]
    fn with_loss_every_pixel_stays_within_the_tolerance_and_the_data_is_smaller() {
        // A gray ramp with one step of noise: the neighbors of a color are close to it.
        let colors: Vec<[u8; 3]> = (0..64u8).map(|i| [i * 4, i * 4, i * 4]).collect();
        let (width, height) = (300u16, 200u16);
        let mut state = 1u32;
        let frame: Vec<u8> = (0..usize::from(width) * usize::from(height))
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let ramp = (i % usize::from(width)) * 60 / usize::from(width);
                u8::try_from(ramp + (state >> 30) as usize).unwrap()
            })
            .collect();
        let wanted: Vec<[u8; 3]> = frame.iter().map(|&i| colors[usize::from(i)]).collect();
        let (exact, _) = encode(&frame, colors.len(), None);
        let mut sizes = vec![exact.len()];
        for tolerance in [4 * 16, 4 * 64, 4 * 256] {
            let loss = Loss {
                wanted: &wanted,
                under: None,
                palette: &colors,
                transparent: None,
                tolerance,
            };
            let (data, written) = encode(&frame, colors.len(), Some(&loss));
            // What is read back is what the encoder says it wrote.
            assert_eq!(decoded(&data, &colors, width, height), written);
            for (at, &symbol) in written.iter().enumerate() {
                let cost = loss.cost(at, frame[at], symbol).unwrap();
                assert!(cost <= tolerance, "pixel {at}: {cost} > {tolerance}");
            }
            sizes.push(data.len());
        }
        assert!(
            sizes.windows(2).all(|pair| pair[1] < pair[0]),
            "more tolerance, less data: {sizes:?}"
        );
        assert!(sizes[3] * 2 < sizes[0], "{sizes:?}");
    }

    #[test]
    fn transparent_pixels_are_exchanged_only_for_what_the_screen_shows() {
        let colors = [[0, 0, 0], [200, 0, 0], [205, 0, 0], [0, 0, 250]];
        // Index 0 is "leave the pixel as it is".
        let frame = [1u8, 0, 1, 0, 1, 0, 1, 0, 2, 0, 2, 0, 3, 0, 3, 0];
        // Under the transparent pixels, the screen shows a red close to the frame's reds.
        let screen = vec![[202u8, 0, 0]; frame.len()];
        let wanted: Vec<[u8; 3]> = frame
            .iter()
            .map(|&i| {
                if i == 0 {
                    [202, 0, 0]
                } else {
                    colors[usize::from(i)]
                }
            })
            .collect();
        let loss = |under| Loss {
            wanted: &wanted,
            under,
            palette: &colors,
            transparent: Some(0),
            tolerance: 40,
        };
        // With the screen known, a red pixel and a transparent one may stand in for each
        // other; a blue one stays blue.
        let (data, written) = encode(&frame, 4, Some(&loss(Some(screen.as_slice()))));
        assert_eq!(decoded(&data, &colors, 16, 1), written);
        assert_ne!(written, frame, "something was exchanged");
        for (at, &symbol) in written.iter().enumerate() {
            let shown = if symbol == 0 {
                screen[at]
            } else {
                colors[usize::from(symbol)]
            };
            assert!(shown[0].abs_diff(wanted[at][0]) <= 6 && shown[2] == wanted[at][2]);
        }
        // On a cleared canvas, transparent stays transparent and colors stay colors.
        let (_, written) = encode(&frame, 4, Some(&loss(None)));
        for (at, &symbol) in written.iter().enumerate() {
            assert_eq!(symbol == 0, frame[at] == 0, "pixel {at}");
        }
    }
}
