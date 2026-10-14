//! Shared small helpers for the DSP core. Nothing here allocates, and
//! everything that runs per sample avoids the general `exp2`/`sin`/`tanh`/`powf`.

pub const PI: f32 = std::f32::consts::PI;
pub const TWO_PI: f32 = std::f32::consts::TAU;

/// Frames are a fixed power of two so the mip chain and FFT stay simple.
pub const FRAME_SIZE: usize = 2048;
pub const MAX_FRAMES: usize = 256;
/// 2048 -> 2 samples.
pub const MIP_LEVELS: usize = 11;
pub const MAX_UNISON: usize = 16;
pub const MAX_VOICES: usize = 16;
pub const MATRIX_SLOTS: usize = 16;
/// Internal render chunk ceiling.
pub const MAX_BLOCK: usize = 4096;

#[inline(always)]
pub fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

#[inline(always)]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// tanh approximation (Pade-style); error below 5e-3 for |x| <= 3, up to 3e-2 at the clamp edge.
#[inline(always)]
pub fn fast_tanh(x: f32) -> f32 {
    let c = x.clamp(-5.0, 5.0);
    let x2 = c * c;
    c * (27.0 + x2) / (27.0 + 9.0 * x2)
}

/// Soft clip used wherever modulation could push a value past its range.
#[inline(always)]
pub fn soft_clip(x: f32) -> f32 {
    fast_tanh(x)
}

/// Bipolar soft limit into [-lim, lim] keeping the slope near unity at zero.
#[inline(always)]
pub fn soft_limit(x: f32, lim: f32) -> f32 {
    if lim <= 0.0 {
        return 0.0;
    }
    lim * fast_tanh(x / lim)
}

pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db * 0.05)
}

pub fn gain_to_db(g: f32) -> f32 {
    20.0 * g.max(1.0e-9).log10()
}

pub fn midi_to_hz(note: f32) -> f32 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}

/// 2^x with ~1e-4 relative error: exponent by bit manipulation, mantissa by a
/// minimax polynomial. Used per sample for pitch, where `exp2` dominated.
#[inline(always)]
pub fn fast_exp2(x: f32) -> f32 {
    let c = x.clamp(-126.0, 126.0);
    let whole = c.floor();
    let frac = c - whole;
    let poly = 1.0
        + frac
            * (std::f32::consts::LN_2
                + frac * (0.240_226_5 + frac * (0.055_504_1 + frac * (0.009_618_1 + frac * 0.001_333_3))));
    let scale = f32::from_bits((((whole as i32) + 127) << 23) as u32);
    poly * scale
}

/// log2(x) for x > 0: exponent straight from the bits, mantissa from a
/// 5th-order minimax polynomial. Pairs with [`fast_exp2`] to give a per-sample pow.
#[inline(always)]
#[allow(clippy::excessive_precision)]
pub fn fast_log2(x: f32) -> f32 {
    if x <= 0.0 {
        return -126.0;
    }
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127;
    // Mantissa folded into [1, 2).
    let m = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000);
    let poly = -2.800_338_6
        + m * (5.091_631_6 + m * (-3.550_697_2 + m * (1.631_092_9 + m * (-0.416_548_03 + m * 0.044_871_943))));
    exponent as f32 + poly
}

/// x^y for x >= 0. The envelope curves need a pow per sample, where `powf` is
/// far too expensive to sit in the audio path.
#[inline(always)]
pub fn fast_pow(x: f32, y: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    fast_exp2(y * fast_log2(x))
}

/// sin(2*pi*p) for any p via a 7th-order odd polynomial (error < 1e-4).
#[inline(always)]
pub fn fast_sin01(p: f32) -> f32 {
    // Fold into [-0.25, 0.25] where the polynomial is accurate, tracking the sign.
    let mut x = p - p.floor();
    let mut sign = 1.0;
    if x > 0.5 {
        x -= 0.5;
        sign = -1.0;
    }
    if x > 0.25 {
        x = 0.5 - x;
    }
    let t = x * TWO_PI;
    let t2 = t * t;
    sign * t * (1.0 - t2 * (0.166_666_6 - t2 * (0.008_333_3 - t2 * 0.000_198_4)))
}

/// Note number to Hz using [`fast_exp2`]; audio-thread pitch path only.
#[inline(always)]
pub fn midi_to_hz_fast(note: f32) -> f32 {
    440.0 * fast_exp2((note - 69.0) * (1.0 / 12.0))
}

pub fn cents_to_ratio(cents: f32) -> f32 {
    (cents / 1200.0).exp2()
}

/// Catmull-Rom / 4-point Hermite interpolation, used for wavetable reads.
#[inline(always)]
pub fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b_neg = w + a;
    (((a * t) - b_neg) * t + c) * t + x0
}

/// One-pole smoother; time constant in seconds.
#[derive(Clone, Copy, Debug)]
pub struct Smoother {
    coeff: f32,
    current: f32,
}

impl Default for Smoother {
    fn default() -> Self {
        Self { coeff: 0.01, current: 0.0 }
    }
}

impl Smoother {
    pub fn prepare(&mut self, sample_rate: f64, seconds: f32) {
        let t = seconds.max(1.0e-4);
        self.coeff = 1.0 - (-1.0 / (sample_rate as f32 * t)).exp();
    }
    pub fn reset(&mut self, v: f32) {
        self.current = v;
    }
    #[inline(always)]
    pub fn next(&mut self, target: f32) -> f32 {
        self.current += self.coeff * (target - self.current);
        self.current
    }
    #[inline(always)]
    pub fn value(&self) -> f32 {
        self.current
    }
}

/// Cheap deterministic RNG (xorshift32). Deterministic renders are a spec item,
/// so every noise source seeds from the voice/engine, never from the clock.
#[derive(Clone, Copy, Debug)]
pub struct Rng {
    state: u32,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new(0x0123_4567)
    }
}

impl Rng {
    pub const fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { 0x0123_4567 } else { seed } }
    }
    pub fn seed(&mut self, s: u32) {
        self.state = if s == 0 { 0x0123_4567 } else { s };
    }
    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        self.state
    }
    /// Uniform in [0, 1).
    #[inline(always)]
    pub fn next_float(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }
    /// Uniform in [-1, 1).
    #[inline(always)]
    pub fn next_bipolar(&mut self) -> f32 {
        self.next_float() * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_math_matches_std() {
        for i in -200..200 {
            let x = i as f32 * 0.05;
            assert!((fast_exp2(x) / x.exp2() - 1.0).abs() < 2.0e-4, "exp2 {x}");
            assert!((fast_tanh(x * 0.5) - (x * 0.5).tanh()).abs() < 3.5e-2, "tanh {x}");
            assert!((fast_sin01(x) - (x * TWO_PI).sin()).abs() < 1.0e-3, "sin {x}");
        }
        for i in 1..400 {
            let x = i as f32 * 0.037;
            assert!((fast_log2(x) - x.log2()).abs() < 1.0e-3, "log2 {x}");
        }
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
        let v = Rng::new(9).next_float();
        assert!((0.0..1.0).contains(&v));
    }
}
