//! Mixing of microphone (mono) and application sound (stereo) into one stereo signal with a soft limiter.

/// -3 dB headroom applied to the sum.
pub const HEADROOM: f32 = 0.707_945_8;
/// Level above which the limiter starts to bend the signal (full scale = 1.0).
pub const KNEE: f32 = 0.85;

/// Transparent below [`KNEE`], then a smooth (tanh) curve that approaches but never reaches +-1.0.
pub fn soft_limit(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

/// Mixes one block. `mic` is mono (`n` samples), `app` is interleaved stereo (`2 * n` samples); `None` = silence.
/// The microphone is put on both channels. Output: planar `left` / `right` of `n` samples each.
pub fn mix_block(mic: Option<&[f32]>, app: Option<&[f32]>, left: &mut [f32], right: &mut [f32]) {
    let n = left.len();
    debug_assert_eq!(n, right.len());
    for i in 0..n {
        let m = mic.map_or(0.0, |m| m[i]);
        let (l, r) = app.map_or((0.0, 0.0), |a| (a[2 * i], a[2 * i + 1]));
        left[i] = soft_limit((m + l) * HEADROOM);
        right[i] = soft_limit((m + r) * HEADROOM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_is_transparent_below_knee_and_never_clips() {
        for x in [-0.8, -0.3, 0.0, 0.5, 0.85] {
            assert_eq!(soft_limit(x), x);
        }
        let mut prev = soft_limit(0.0);
        let mut x = 0.0f32;
        while x < 20.0 {
            x += 0.01;
            let y = soft_limit(x);
            assert!(y >= prev && y <= 1.0, "x={x} y={y}");
            assert_eq!(soft_limit(-x), -y);
            prev = y;
        }
        assert!(soft_limit(f32::NAN) == 0.0 && soft_limit(f32::INFINITY) == 0.0);
    }

    #[test]
    fn mic_goes_to_both_channels_and_sum_has_headroom() {
        let mic = [0.5f32, -0.5];
        let app = [0.2, -0.2, 0.0, 0.1];
        let (mut l, mut r) = ([0.0; 2], [0.0; 2]);
        mix_block(Some(&mic), Some(&app), &mut l, &mut r);
        assert!((l[0] - 0.7 * HEADROOM).abs() < 1e-6 && (r[0] - 0.3 * HEADROOM).abs() < 1e-6);
        assert!((l[1] + 0.5 * HEADROOM).abs() < 1e-6 && (r[1] + 0.4 * HEADROOM).abs() < 1e-6);
        // Only the microphone: identical channels, -3 dB.
        mix_block(Some(&mic), None, &mut l, &mut r);
        assert_eq!(l, r);
        assert!((l[0] - 0.5 * HEADROOM).abs() < 1e-6);
        // Nothing: silence.
        mix_block(None, None, &mut l, &mut r);
        assert_eq!((l, r), ([0.0; 2], [0.0; 2]));
    }

    #[test]
    fn two_full_scale_signals_stay_below_full_scale() {
        let mic = [1.0f32; 4];
        let app = [1.0f32; 8];
        let (mut l, mut r) = ([0.0; 4], [0.0; 4]);
        mix_block(Some(&mic), Some(&app), &mut l, &mut r);
        assert!(l.iter().chain(&r).all(|v| *v < 1.0 && *v > 0.95), "{l:?}");
    }
}
