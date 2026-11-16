//! Still pictures that are HDR by their transfer function.
//!
//! Such a picture has no gain map: its pixel values themselves reach past standard white,
//! coded with the PQ curve (SMPTE ST 2084) or the HLG curve (ARIB STD-B67), usually at 10
//! bits or more. Read as a standard picture it comes out dark and flat. It is kept as it is
//! where the output format can say what the values mean (AVIF, HEIC, JPEG XL, PNG), and it
//! is converted to standard range for the others: the values become light, the light is
//! compressed so that the picture's brightest parts are white, and the result is an sRGB
//! picture.

use image::{DynamicImage, Rgba, RgbaImage};

/// The code point of the PQ transfer function (ITU-T H.273).
pub const PQ: u8 = 16;
/// The code point of the HLG transfer function.
pub const HLG: u8 = 18;

/// The brightness of standard white in an HDR picture, in cd/m² (ITU-R BT.2408): what maps
/// to white in the standard range.
const STANDARD_WHITE: f32 = 203.0;
/// The peak of an HLG picture on the display its numbers are defined for, in cd/m².
const HLG_PEAK: f32 = 1000.0;

/// What the pixel values of an HDR picture mean, in the code points of ITU-T H.273.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrColor {
    /// The color primaries: 1 is BT.709 (sRGB's), 9 is BT.2020, 12 is Display P3.
    pub primaries: u8,
    /// The transfer function: [`PQ`] or [`HLG`].
    pub transfer: u8,
    /// The brightest pixel and the brightest frame average, in cd/m², when the file says
    /// (`clli`).
    pub light_level: Option<(u16, u16)>,
}

impl HdrColor {
    /// The color of a picture with these code points: `None` unless its transfer function is
    /// PQ or HLG.
    #[must_use]
    pub fn of_codes(primaries: u8, transfer: u8) -> Option<Self> {
        matches!(transfer, PQ | HLG).then_some(Self {
            primaries,
            transfer,
            light_level: None,
        })
    }

    /// The same from FFmpeg's names for the primaries and the transfer function.
    #[must_use]
    pub fn of_names(primaries: Option<&str>, transfer: Option<&str>) -> Option<Self> {
        let transfer = match transfer? {
            "smpte2084" => PQ,
            "arib-std-b67" => HLG,
            _ => return None,
        };
        let primaries = match primaries {
            Some("bt2020") => 9,
            Some("smpte432") => 12,
            Some("smpte431") => 11,
            _ => 1,
        };
        Self::of_codes(primaries, transfer)
    }

    /// FFmpeg's names for the primaries and the transfer function.
    #[must_use]
    pub fn names(self) -> (&'static str, &'static str) {
        let primaries = match self.primaries {
            9 => "bt2020",
            11 => "smpte431",
            12 => "smpte432",
            _ => "bt709",
        };
        let transfer = if self.transfer == HLG {
            "arib-std-b67"
        } else {
            "smpte2084"
        };
        (primaries, transfer)
    }

    /// The matrix that suits these primaries for storing the picture as brightness and two
    /// color differences: its code point, and the weights of red and blue in the brightness.
    #[must_use]
    pub fn matrix(self) -> (u8, f32, f32) {
        if self.primaries == 9 {
            (9, 0.2627, 0.0593)
        } else {
            (1, 0.2126, 0.0722)
        }
    }

    /// The content of a PNG `cICP` chunk that says this: RGB (no matrix), full range.
    #[must_use]
    pub fn cicp(self) -> [u8; 4] {
        [self.primaries, self.transfer, 0, 1]
    }

    /// The matrix from linear RGB with these primaries to linear RGB with sRGB's (both with
    /// a D65 white), and the weights of the three channels in a color's luminance.
    fn into_srgb_primaries(self) -> ([[f32; 3]; 3], [f32; 3]) {
        match self.primaries {
            9 => (
                [
                    [1.660_491, -0.587_641, -0.072_850],
                    [-0.124_550, 1.132_9, -0.008_349],
                    [-0.018_151, -0.100_579, 1.118_73],
                ],
                [0.2627, 0.6780, 0.0593],
            ),
            11 | 12 => (
                [
                    [1.224_94, -0.224_94, 0.0],
                    [-0.042_057, 1.042_057, 0.0],
                    [-0.019_638, -0.078_636, 1.098_274],
                ],
                [0.2290, 0.6917, 0.0793],
            ),
            _ => (
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                [0.2126, 0.7152, 0.0722],
            ),
        }
    }
}

/// The PQ curve: a value from 0 to 1 as light in cd/m² (0 to 10,000).
fn pq_to_light(value: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let root = value.clamp(0.0, 1.0).powf(1.0 / M2);
    10_000.0 * ((root - C1).max(0.0) / (C2 - C3 * root)).powf(1.0 / M1)
}

/// The PQ curve the other way: light in cd/m² as a value from 0 to 1.
#[cfg(test)]
fn light_to_pq(light: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let power = (light.clamp(0.0, 10_000.0) / 10_000.0).powf(M1);
    ((C1 + C2 * power) / (1.0 + C3 * power)).powf(M2)
}

/// The HLG curve: a value from 0 to 1 as the light of the scene, from 0 to 1.
fn hlg_to_scene(value: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let value = value.clamp(0.0, 1.0);
    if value <= 0.5 {
        value * value / 3.0
    } else {
        (((value - C) / A).exp() + B) / 12.0
    }
}

/// The sRGB curve: light from 0 (black) to 1 (white) as a value from 0 to 1.
fn srgb_from_light(light: f32) -> f32 {
    let light = light.clamp(0.0, 1.0);
    if light <= 0.003_130_8 {
        light * 12.92
    } else {
        1.055 * light.powf(1.0 / 2.4) - 0.055
    }
}

/// How light is brought into the standard range: Reinhard's curve with a white point. Light
/// far below standard white stays nearly as it is, brighter light is compressed more and
/// more, and the picture's `peak` lands on white. A picture that is no brighter than standard
/// white is not changed at all.
#[derive(Debug, Clone, Copy)]
struct RollOff {
    /// The picture's peak, in multiples of standard white (at least 1).
    peak: f32,
}

impl RollOff {
    fn new(peak: f32) -> Self {
        Self {
            peak: (peak / STANDARD_WHITE).max(1.0),
        }
    }

    /// Light in cd/m² brought into the standard range.
    fn apply(self, light: f32) -> f32 {
        let x = light.max(0.0) / STANDARD_WHITE;
        let mapped = x * (1.0 + x / (self.peak * self.peak)) / (1.0 + x);
        // With a peak of 1 the curve is the identity; below it, nothing is made brighter.
        let mapped = if self.peak <= 1.0 { x } else { mapped };
        mapped.min(1.0) * STANDARD_WHITE
    }
}

/// The part of a picture's pixels that may be brighter than what counts as its peak: a few
/// glints do not set how the whole picture is shown.
const GLINTS: f32 = 0.001;

/// The brightest light in a picture, in cd/m², but for its few brightest values.
fn peak_of(light: &[[f32; 4]]) -> f32 {
    let mut brightest: Vec<f32> = light
        .iter()
        .map(|pixel| pixel[0].max(pixel[1]).max(pixel[2]))
        .collect();
    if brightest.is_empty() {
        return STANDARD_WHITE;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let skipped = (brightest.len() as f32 * GLINTS) as usize;
    let at = brightest.len() - 1 - skipped.min(brightest.len() - 1);
    let (_, peak, _) = brightest.select_nth_unstable_by(at, f32::total_cmp);
    peak.max(STANDARD_WHITE)
}

/// The picture's pixels as light: for each, red, green, and blue in cd/m² with sRGB's
/// primaries (negative where a color is outside them), and the opacity.
fn light_of(image: &DynamicImage, color: HdrColor) -> Vec<[f32; 4]> {
    let (matrix, weights) = color.into_srgb_primaries();
    // One value of the curve for each of the 65,536 stored values.
    let curve: Vec<f32> = (0..=u16::MAX)
        .map(|stored| {
            let value = f32::from(stored) / f32::from(u16::MAX);
            if color.transfer == HLG {
                hlg_to_scene(value)
            } else {
                pq_to_light(value)
            }
        })
        .collect();
    image
        .to_rgba16()
        .pixels()
        .map(|pixel| {
            let mut rgb = [0, 1, 2].map(|channel| curve[usize::from(pixel[channel])]);
            if color.transfer == HLG {
                // From the light of the scene to the light of a display with a peak of
                // 1,000 cd/m²: brighter scenes are shown with more contrast.
                let scene: f32 = rgb.iter().zip(weights).map(|(c, w)| c * w).sum();
                let gain = HLG_PEAK * scene.max(1e-6).powf(0.2);
                rgb = rgb.map(|c| c * gain);
            }
            let srgb = matrix.map(|row| row.iter().zip(rgb).map(|(m, c)| m * c).sum::<f32>());
            [
                srgb[0],
                srgb[1],
                srgb[2],
                f32::from(pixel[3]) / f32::from(u16::MAX),
            ]
        })
        .collect()
}

/// An HDR picture as a standard one: 8 bits per channel, sRGB. Its light is compressed so
/// that the picture's own peak is white (see [`RollOff`]), and colors outside sRGB are
/// brought to its edge.
#[must_use]
pub fn to_standard(image: &DynamicImage, color: HdrColor) -> DynamicImage {
    let light = light_of(image, color);
    let roll_off = RollOff::new(peak_of(&light));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let stored = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut pixels = light.into_iter();
    let standard = RgbaImage::from_fn(image.width(), image.height(), |_, _| {
        let [r, g, b, a] = pixels.next().unwrap_or_default();
        let rgb = [r.max(0.0), g.max(0.0), b.max(0.0)];
        // The brightest channel is rolled off and the others follow, so the hue stays.
        let brightest = rgb[0].max(rgb[1]).max(rgb[2]);
        let scale = if brightest > 0.0 {
            roll_off.apply(brightest) / brightest
        } else {
            0.0
        };
        let [r, g, b] = rgb.map(|c| stored(srgb_from_light(c * scale / STANDARD_WHITE)));
        Rgba([r, g, b, stored(a)])
    });
    if image.color().has_alpha() {
        DynamicImage::ImageRgba8(standard)
    } else {
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(standard).to_rgb8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn close(a: f32, b: f32, within: f32) -> bool {
        (a - b).abs() <= within
    }

    #[test]
    fn the_curves_give_known_values() {
        // PQ: 0.58 is close to standard white, 0.75 to 1,000 cd/m², 1 is 10,000.
        assert!(close(pq_to_light(0.5807), 203.0, 1.0));
        assert!(close(pq_to_light(0.7518), 1000.0, 5.0));
        assert!(close(pq_to_light(1.0), 10_000.0, 0.5));
        assert!(pq_to_light(0.0).abs() < f32::EPSILON);
        for light in [0.1, 5.0, 203.0, 1000.0, 4000.0] {
            assert!(close(pq_to_light(light_to_pq(light)), light, light * 0.001));
        }
        // HLG: one half is a twelfth of the scene's peak, and 1 is the peak.
        assert!(close(hlg_to_scene(0.5), 1.0 / 12.0, 1e-5));
        assert!(close(hlg_to_scene(1.0), 1.0, 1e-4));
        // sRGB: 18% gray, and the ends.
        assert!(close(srgb_from_light(0.18), 0.4614, 0.001));
        assert!(close(srgb_from_light(1.5), 1.0, 1e-6));
    }

    #[test]
    fn light_is_compressed_up_to_the_peak() {
        let roll_off = RollOff::new(1000.0);
        // Dim light stays nearly as it is; nothing gets brighter.
        assert!(close(roll_off.apply(2.0), 2.0, 0.05));
        assert!(close(roll_off.apply(20.0), 18.3, 0.2));
        // The peak lands on standard white, and brighter light is never darker.
        assert!(close(roll_off.apply(1000.0), STANDARD_WHITE, 0.5));
        assert!(close(roll_off.apply(5000.0), STANDARD_WHITE, 0.01));
        let mut last = 0.0;
        for step in 0..=200u8 {
            let light = f32::from(step) * 5.0;
            let mapped = roll_off.apply(light);
            assert!(mapped >= last && mapped <= light + 0.01, "{step}");
            last = mapped;
        }
        // A picture no brighter than standard white is left as it is.
        let dim = RollOff::new(150.0);
        for light in [1.0, 60.0, 120.0, 203.0] {
            assert!(close(dim.apply(light), light, 0.01), "{light}");
        }
    }

    #[test]
    fn a_few_glints_do_not_set_the_peak() {
        let mut light = vec![[100.0, 150.0, 120.0, 1.0]; 10_000];
        assert!(close(peak_of(&light), STANDARD_WHITE, 0.01));
        light[0] = [4000.0; 4];
        assert!(close(peak_of(&light), STANDARD_WHITE, 0.01));
        // A hundredth of the picture is no glint.
        for pixel in &mut light[..100] {
            *pixel = [900.0, 100.0, 100.0, 1.0];
        }
        assert!(close(peak_of(&light), 900.0, 0.01));
        assert!(close(peak_of(&[]), STANDARD_WHITE, 0.01));
    }

    #[test]
    fn names_and_codes_say_the_same() {
        let pq = HdrColor::of_names(Some("bt2020"), Some("smpte2084")).unwrap();
        assert_eq!((pq.primaries, pq.transfer), (9, PQ));
        assert_eq!(pq.names(), ("bt2020", "smpte2084"));
        assert_eq!(pq.cicp(), [9, 16, 0, 1]);
        let hlg = HdrColor::of_names(None, Some("arib-std-b67")).unwrap();
        assert_eq!((hlg.primaries, hlg.transfer), (1, HLG));
        assert_eq!(HdrColor::of_names(Some("bt2020"), Some("bt709")), None);
        assert_eq!(HdrColor::of_codes(1, 13), None);
    }

    /// A picture of one color, stored at 16 bits.
    fn flat(value: [f32; 3]) -> DynamicImage {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let stored = value.map(|v| (v * 65535.0).round() as u16);
        DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(4, 4, Rgb(stored)))
    }

    fn first(image: &DynamicImage) -> [u8; 3] {
        image.to_rgb8().get_pixel(0, 0).0
    }

    #[test]
    fn a_pq_picture_becomes_a_standard_one() {
        let pq = HdrColor::of_codes(1, PQ).unwrap();
        // A picture no brighter than standard white is shown as it is in light: white stays
        // white, and a mid gray stays the mid gray it is.
        let white = to_standard(&flat([light_to_pq(203.0); 3]), pq);
        assert!(
            first(&white).iter().all(|c| *c >= 254),
            "{:?}",
            first(&white)
        );
        let gray = to_standard(&flat([light_to_pq(203.0 * 0.18); 3]), pq);
        assert!(
            first(&gray).iter().all(|c| c.abs_diff(118) <= 1),
            "{:?}",
            first(&gray)
        );
        // Read as a standard picture, that gray would be far too bright.
        assert!(light_to_pq(203.0 * 0.18) > 0.35);

        // A picture with highlights: they are white, standard white is a light gray below
        // them, and a mid gray is a little darker than it was.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let stored = |light: f32| Rgb([(light_to_pq(light) * 65535.0).round() as u16; 3]);
        let mut picture = image::ImageBuffer::from_pixel(8, 8, stored(203.0 * 0.18));
        for x in 0..8 {
            picture.put_pixel(x, 0, stored(1000.0));
            picture.put_pixel(x, 1, stored(203.0));
        }
        let standard = to_standard(&DynamicImage::ImageRgb16(picture), pq).to_rgb8();
        assert!(standard.get_pixel(0, 0).0.iter().all(|c| *c >= 254));
        let was_white = standard.get_pixel(0, 1)[0];
        assert!((180..=200).contains(&was_white), "{was_white}");
        let gray = standard.get_pixel(0, 5)[0];
        assert!((100..118).contains(&gray), "{gray}");
    }

    #[test]
    fn wide_primaries_and_hlg_are_converted_too() {
        // BT.2020's pure red is outside sRGB: it becomes sRGB's red, not a dull one.
        let pq = HdrColor::of_codes(9, PQ).unwrap();
        let red = first(&to_standard(&flat([light_to_pq(100.0), 0.0, 0.0]), pq));
        assert!(red[0] > 180 && red[1] == 0 && red[2] == 0, "{red:?}");
        // A gray stays gray whatever the primaries.
        let gray = first(&to_standard(&flat([light_to_pq(40.0); 3]), pq));
        assert!(gray[0].abs_diff(gray[1]) <= 1 && gray[1].abs_diff(gray[2]) <= 1);

        // HLG: 0.75 is the reference white of an HLG picture (203 cd/m² on a 1,000 cd/m²
        // display), and 1 is the peak.
        let hlg = HdrColor::of_codes(9, HLG).unwrap();
        let mut picture = flat([0.75; 3]).to_rgb16();
        for x in 0..4 {
            picture.put_pixel(x, 0, Rgb([65535; 3]));
        }
        let standard = to_standard(&DynamicImage::ImageRgb16(picture), hlg).to_rgb8();
        let white = standard.get_pixel(0, 1).0;
        assert!(white.iter().all(|c| (180..=200).contains(c)), "{white:?}");
        assert!(standard.get_pixel(0, 0).0.iter().all(|c| *c >= 254));
        // Transparency is kept.
        let mut clear = flat([0.5; 3]).to_rgba16();
        clear.put_pixel(0, 0, image::Rgba([30000, 30000, 30000, 0]));
        let standard = to_standard(&DynamicImage::ImageRgba16(clear), hlg).to_rgba8();
        assert_eq!(standard.get_pixel(0, 0)[3], 0);
        assert_eq!(standard.get_pixel(1, 1)[3], 255);
    }
}
