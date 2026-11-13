//! RGBA (sRGB-encoded, display-referred) -> planar YUV 4:2:0, BT.709, limited range ("TV range").
//!
//! Fixed-point, scalar, no SIMD, no intermediate copies: one pass over the source, writing straight into
//! the Y/U/V planes. Chroma is the average of each 2x2 block. Odd sizes are cropped to even ones.

/// Fixed-point precision of the coefficients.
const SHIFT: u32 = 15;
const ONE: f64 = (1u32 << SHIFT) as f64;
/// Limited range: Y' spans 16..=235 (219 steps), Cb/Cr span 16..=240 (224 steps).
const Y_SCALE: f64 = 219.0 / 255.0;
const C_SCALE: f64 = 224.0 / 255.0;

// BT.709 luma weights: Kr = 0.2126, Kb = 0.0722, Kg = 1 - Kr - Kb.
const KR: f64 = 0.2126;
const KB: f64 = 0.0722;
const KG: f64 = 1.0 - KR - KB;

const fn q(v: f64) -> i32 {
    (v * ONE + if v >= 0.0 { 0.5 } else { -0.5 }) as i32
}

const Y_R: i32 = q(KR * Y_SCALE);
const Y_G: i32 = q(KG * Y_SCALE);
const Y_B: i32 = q(KB * Y_SCALE);
// Cb = (B - Y) / (2 (1 - Kb)), Cr = (R - Y) / (2 (1 - Kr)).
const CB_R: i32 = q(-KR / (2.0 * (1.0 - KB)) * C_SCALE);
const CB_G: i32 = q(-KG / (2.0 * (1.0 - KB)) * C_SCALE);
const CB_B: i32 = q(0.5 * C_SCALE);
const CR_R: i32 = q(0.5 * C_SCALE);
const CR_G: i32 = q(-KG / (2.0 * (1.0 - KR)) * C_SCALE);
const CR_B: i32 = q(-KB / (2.0 * (1.0 - KR)) * C_SCALE);

/// Planar I420 picture with tightly packed planes (strides = width, width/2).
#[derive(Default)]
pub struct Yuv420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv420 {
    /// Converts `rgba` (`src_w` x `src_h`, 4 bytes/pixel, alpha ignored) into this picture.
    /// The result is `src_w & !1` x `src_h & !1`. Returns false if the input is too short or too small.
    pub fn convert_from_rgba(&mut self, rgba: &[u8], src_w: usize, src_h: usize) -> bool {
        let (w, h) = (src_w & !1, src_h & !1);
        if w == 0 || h == 0 || rgba.len() < src_w * src_h * 4 {
            return false;
        }
        self.width = w;
        self.height = h;
        self.y.resize(w * h, 0);
        self.u.resize(w * h / 4, 0);
        self.v.resize(w * h / 4, 0);
        let stride = src_w * 4;
        for by in 0..h / 2 {
            let base = by * 2 * stride;
            let (row0, row1) = (&rgba[base..base + stride], &rgba[base + stride..base + 2 * stride]);
            let y0 = by * 2 * w;
            let (ya, yb) = self.y[y0..y0 + 2 * w].split_at_mut(w);
            let crow = by * (w / 2);
            for bx in 0..w / 2 {
                let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
                for (row, yrow) in [(row0, &mut *ya), (row1, &mut *yb)] {
                    for dx in 0..2 {
                        let i = (bx * 2 + dx) * 4;
                        let (r, g, b) = (row[i] as i32, row[i + 1] as i32, row[i + 2] as i32);
                        yrow[bx * 2 + dx] = (((Y_R * r + Y_G * g + Y_B * b + (1 << (SHIFT - 1))) >> SHIFT) + 16) as u8;
                        sr += r;
                        sg += g;
                        sb += b;
                    }
                }
                let rnd = 1 << (SHIFT + 1);
                let cb = ((CB_R * sr + CB_G * sg + CB_B * sb + rnd) >> (SHIFT + 2)) + 128;
                let cr = ((CR_R * sr + CR_G * sg + CR_B * sb + rnd) >> (SHIFT + 2)) + 128;
                self.u[crow + bx] = cb.clamp(16, 240) as u8;
                self.v[crow + bx] = cr.clamp(16, 240) as u8;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(px: [u8; 3]) -> (u8, u8, u8) {
        let rgba: Vec<u8> = (0..4).flat_map(|_| [px[0], px[1], px[2], 0]).collect();
        let mut y = Yuv420::default();
        assert!(y.convert_from_rgba(&rgba, 2, 2));
        assert_eq!((y.width, y.height, y.y.len(), y.u.len(), y.v.len()), (2, 2, 4, 1, 1));
        assert!(y.y.iter().all(|&v| v == y.y[0]));
        (y.y[0], y.u[0], y.v[0])
    }

    fn near(a: (u8, u8, u8), b: (u8, u8, u8)) {
        let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
        assert!(d(a.0, b.0) <= 1 && d(a.1, b.1) <= 1 && d(a.2, b.2) <= 1, "{a:?} vs {b:?}");
    }

    #[test]
    fn known_bt709_limited_values() {
        near(conv([0, 0, 0]), (16, 128, 128));
        near(conv([255, 255, 255]), (235, 128, 128));
        near(conv([128, 128, 128]), (126, 128, 128));
        near(conv([255, 0, 0]), (63, 102, 240));
        near(conv([0, 255, 0]), (173, 42, 26));
        near(conv([0, 0, 255]), (32, 240, 118));
    }

    #[test]
    fn chroma_is_2x2_average_and_odd_sizes_are_cropped() {
        // 3x3 source -> 2x2 output; left column red, right column blue in the top-left 2x2 block.
        let mut rgba = vec![0u8; 3 * 3 * 4];
        for y in 0..3 {
            for x in 0..3 {
                let i = (y * 3 + x) * 4;
                rgba[i..i + 3].copy_from_slice(if x == 0 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let mut yuv = Yuv420::default();
        assert!(yuv.convert_from_rgba(&rgba, 3, 3));
        assert_eq!((yuv.width, yuv.height), (2, 2));
        assert!(yuv.y[0] > yuv.y[1]); // red is brighter than blue
        near((128, yuv.u[0], yuv.v[0]), (128, 171, 179)); // average of red and blue chroma
    }

    #[test]
    fn rejects_short_input() {
        assert!(!Yuv420::default().convert_from_rgba(&[0; 8], 2, 2));
        assert!(!Yuv420::default().convert_from_rgba(&[0; 8], 1, 2));
    }
}
