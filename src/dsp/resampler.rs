//! Oversampling support (SPEC 3.1). The synth has no audio input, so only
//! decimation is needed: voices render at 2x/4x and are filtered down.

use crate::dsp::common::{PI, TWO_PI};

/// Windowed-sinc half-band FIR decimator (one channel of state).
#[derive(Clone)]
pub struct HalfBandDecimator {
    taps: [f32; Self::TAPS],
    /// Samples are stored twice so the convolution window is always contiguous.
    history: [f32; 2 * Self::TAPS],
    pos: usize,
}

impl Default for HalfBandDecimator {
    fn default() -> Self {
        let mut d = Self {
            taps: [0.0; Self::TAPS],
            history: [0.0; 2 * Self::TAPS],
            pos: 0,
        };
        d.build_taps();
        d
    }
}

impl HalfBandDecimator {
    pub const TAPS: usize = 32;

    pub fn reset(&mut self) {
        self.history = [0.0; 2 * Self::TAPS];
        self.pos = 0;
    }

    /// Reads `2 * out_samples` from `input`, writes `out_samples` to `output`.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], out_samples: usize) {
        for (o, pair) in output[..out_samples].iter_mut().zip(input[..2 * out_samples].as_chunks::<2>().0) {
            self.push(pair[0]);
            self.push(pair[1]);
            *o = self.convolve();
        }
    }

    fn build_taps(&mut self) {
        // The kernel is symmetric, so the oldest-first window needs no reversal.
        // Cutoff at a quarter of the input rate = Nyquist of the output rate.
        let fc = 0.25_f32;
        let mut sum = 0.0;
        for i in 0..Self::TAPS {
            let n = i as f32 - (Self::TAPS - 1) as f32 * 0.5;
            let sinc = if n.abs() < 1.0e-6 {
                2.0 * fc
            } else {
                (TWO_PI * fc * n).sin() / (PI * n)
            };
            let window = 0.54 - 0.46 * (TWO_PI * i as f32 / (Self::TAPS - 1) as f32).cos();
            self.taps[i] = sinc * window;
            sum += self.taps[i];
        }
        for t in &mut self.taps {
            *t /= sum;
        }
    }

    #[inline(always)]
    fn push(&mut self, x: f32) {
        self.history[self.pos] = x;
        self.history[self.pos + Self::TAPS] = x;
        self.pos = (self.pos + 1) & (Self::TAPS - 1);
    }

    #[inline(always)]
    fn convolve(&self) -> f32 {
        // After push(), `pos` is the next write slot, which still holds the
        // oldest of the last TAPS samples - so the oldest-first window starts
        // exactly there. Starting a slot later rotated the oldest sample into the
        // newest position and cost 20-40 dB of stopband rejection.
        let window = &self.history[self.pos..self.pos + Self::TAPS];
        // Eight independent lanes so the compiler can vectorise this (the C++
        // version used AVX2 intrinsics for the same reduction).
        let mut acc = [0.0_f32; 8];
        for (t, w) in self.taps.as_chunks::<8>().0.iter().zip(window.as_chunks::<8>().0) {
            for k in 0..8 {
                acc[k] += t[k] * w[k];
            }
        }
        acc.iter().sum()
    }
}

/// 1x / 2x / 4x decimation for one channel (two cascaded half-band stages).
#[derive(Clone, Default)]
pub struct Decimator {
    stage1: HalfBandDecimator,
    stage2: HalfBandDecimator,
    scratch: Vec<f32>,
}

impl Decimator {
    pub fn reset(&mut self) {
        self.stage1.reset();
        self.stage2.reset();
        self.scratch.fill(0.0);
    }

    /// Allocates the 4x intermediate buffer; call off the audio thread.
    pub fn prepare(&mut self, max_out_samples: usize) {
        self.scratch = vec![0.0; (max_out_samples * 2).max(16)];
        self.reset();
    }

    /// Display-only decimation: box average, far cheaper than the FIR.
    pub fn average(input: &[f32], output: &mut [f32], out_samples: usize, factor: usize) {
        if factor <= 1 {
            output[..out_samples].copy_from_slice(&input[..out_samples]);
            return;
        }
        let scale = 1.0 / factor as f32;
        for (o, chunk) in output[..out_samples].iter_mut().zip(input.chunks_exact(factor)) {
            *o = chunk.iter().sum::<f32>() * scale;
        }
    }

    /// `factor` must be 1, 2 or 4; `input` holds `out_samples * factor` samples.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], out_samples: usize, factor: usize) {
        match factor {
            0 | 1 => output[..out_samples].copy_from_slice(&input[..out_samples]),
            2 => self.stage1.process(input, output, out_samples),
            _ => {
                self.stage1.process(input, &mut self.scratch, out_samples * 2);
                self.stage2.process(&self.scratch, output, out_samples);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state output level for a sine at `f_in` cycles per INPUT sample.
    fn decimator_level(factor: usize, f_in: f64) -> f32 {
        const OUT: usize = 256;
        let mut decimator = Decimator::default();
        decimator.prepare(OUT);
        let mut input = vec![0.0_f32; OUT * factor];
        let mut output = vec![0.0_f32; OUT];
        let mut phase = 0.0_f64;
        let mut peak = 0.0_f32;
        for block in 0..40 {
            for v in &mut input {
                *v = (2.0 * std::f64::consts::PI * phase).sin() as f32;
                phase += f_in;
            }
            decimator.process(&input, &mut output, OUT, factor);
            if block >= 20 {
                for v in &output {
                    peak = peak.max(v.abs());
                }
            }
        }
        peak
    }

    fn to_db(linear: f32) -> f32 {
        20.0 * linear.max(1.0e-9).log10()
    }

    #[test]
    fn passband_is_flat() {
        for factor in [2, 4] {
            for f in [0.005, 0.02, 0.05] {
                let level = decimator_level(factor, f);
                assert!((level - 1.0).abs() < 0.06, "factor {factor} f {f}: {level}");
            }
        }
    }

    #[test]
    fn deep_stopband_is_rejected() {
        // The convolution window used to start one slot late, which rotated the
        // oldest sample into the newest position. That left a broadband ~-55 dB
        // floor where the kernel should reach -80 dB and below, so these points
        // are where a regression would show up first.
        for f in [0.42, 0.45, 0.48, 0.49] {
            let db = to_db(decimator_level(2, f));
            assert!(db < -78.0, "2x at f = {f}: {db} dB");
        }
        for f in [0.26, 0.28, 0.30, 0.45] {
            let db = to_db(decimator_level(4, f));
            assert!(db < -85.0, "4x at f = {f}: {db} dB");
        }
    }

    #[test]
    fn stopband_floor_holds_everywhere() {
        // 32 taps buy a wide transition band, so full rejection only starts well
        // above the output Nyquist: -63 dB at the worst point for 2x, -65 dB for
        // 4x. This guards the floor; deep_stopband_is_rejected guards the nulls.
        let mut worst2 = -1000.0_f32;
        let mut worst4 = -1000.0_f32;
        for i in 0..=60 {
            let t = i as f64 / 60.0;
            worst2 = worst2.max(to_db(decimator_level(2, 0.35 + 0.14 * t)));
            worst4 = worst4.max(to_db(decimator_level(4, 0.23 + 0.26 * t)));
        }
        assert!(worst2 < -60.0, "2x worst-case stopband {worst2}");
        assert!(worst4 < -60.0, "4x worst-case stopband {worst4}");
    }

    #[test]
    fn unity_factor_is_bit_exact() {
        let mut decimator = Decimator::default();
        decimator.prepare(64);
        let input: Vec<f32> = (0..64).map(|i| i as f32 * 0.01 - 0.3).collect();
        let mut output = vec![0.0; 64];
        decimator.process(&input, &mut output, 64, 1);
        assert_eq!(input, output);
    }
}
