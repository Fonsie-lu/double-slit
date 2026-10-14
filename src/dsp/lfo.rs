//! LFO with free/tempo rate, the SPEC 3.5 shape set and an optional drawable
//! custom shape (persisted with the patch).

use std::sync::Arc;

use super::common::{clamp01, fast_sin01, lerp, Rng, Smoother};
use super::params::LfoShape;

#[derive(Clone, Debug)]
pub struct Lfo {
    sample_rate: f32,
    phase: f32,
    increment: f32,
    current: f32,
    rate_hz: f32,
    held: f32,
    last_hold_phase: f32,
    smooth_amount: f32,
    smoother_valid: bool,
    shape: LfoShape,
    /// Drawable shape, shared with the engine. Swapping it only bumps a
    /// reference count; the engine keeps its own reference alive, so the last
    /// drop never happens here on the audio thread.
    custom: Option<Arc<Vec<f32>>>,
    smoother: Smoother,
    rng: Rng,
}

impl Default for Lfo {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            phase: 0.0,
            increment: 0.0,
            current: 0.0,
            rate_hz: 0.0,
            held: 0.0,
            last_hold_phase: -1.0,
            smooth_amount: 0.0,
            smoother_valid: false,
            shape: LfoShape::Sine,
            custom: None,
            smoother: Smoother::default(),
            rng: Rng::new(0x9e37_79b9),
        }
    }
}

impl Lfo {
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.smoother.prepare(self.sample_rate as f64, 0.01);
        self.reset(0.0);
    }

    pub fn reset(&mut self, start_phase: f32) {
        self.phase = start_phase - start_phase.floor();
        self.held = self.rng.next_bipolar();
        self.last_hold_phase = -1.0;
        self.smoother.reset(0.0);
        self.current = 0.0;
    }

    pub fn seed(&mut self, s: u32) {
        self.rng.seed(s);
    }

    pub fn set_shape(&mut self, shape: LfoShape) {
        self.shape = shape;
    }

    pub fn set_smoothing(&mut self, amount: f32) {
        let clamped = clamp01(amount);
        if self.smoother_valid && clamped == self.smooth_amount {
            return;
        }
        self.smoother_valid = true;
        self.smooth_amount = clamped;
        self.smoother.prepare(self.sample_rate as f64, lerp(0.0005, 0.2, self.smooth_amount));
    }

    /// Frequency in Hz; tempo sync is resolved by the caller.
    pub fn set_rate(&mut self, hz: f32) {
        self.rate_hz = hz.clamp(0.0, 400.0);
        self.increment = self.rate_hz / self.sample_rate;
    }

    /// Oversampling switch mid-note: keep phase and shape, re-derive the step.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.increment = self.rate_hz / self.sample_rate;
        self.smoother_valid = false;
        self.set_smoothing(self.smooth_amount);
    }

    /// Custom drawable shape: at least 2 points, wrapped. Called once per
    /// block; only clones the `Arc` when the shape actually changed.
    pub fn set_custom_shape(&mut self, points: Option<&Arc<Vec<f32>>>) {
        match (points, &self.custom) {
            (Some(new), Some(old)) if Arc::ptr_eq(new, old) => {}
            (None, None) => {}
            (new, _) => self.custom = new.cloned(),
        }
    }

    pub fn value(&self) -> f32 {
        self.current
    }

    /// Current phase in cycles, [0, 1).
    pub fn phase(&self) -> f32 {
        self.phase
    }

    /// One sample. Not an `Iterator`: the stream never ends and the name
    /// matches the rest of the DSP code.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        let raw = self.evaluate(self.phase);
        self.phase += self.increment;
        if self.phase >= 1.0 {
            self.phase -= self.phase.floor();
        }
        self.current = if self.smooth_amount > 0.0 { self.smoother.next(raw) } else { raw };
        self.current
    }

    #[inline]
    fn evaluate(&mut self, p: f32) -> f32 {
        match self.shape {
            LfoShape::Sine => fast_sin01(p),
            LfoShape::Triangle => 4.0 * (p - 0.5).abs() - 1.0,
            LfoShape::SawUp => 2.0 * p - 1.0,
            LfoShape::SawDown => 1.0 - 2.0 * p,
            LfoShape::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoShape::SampleHold => {
                // New random value once per cycle wrap.
                if self.last_hold_phase < 0.0 || p < self.last_hold_phase {
                    self.held = self.rng.next_bipolar();
                }
                self.last_hold_phase = p;
                self.held
            }
            LfoShape::Custom => match &self.custom {
                Some(pts) if pts.len() >= 2 => {
                    let n = pts.len();
                    let pos = p * n as f32;
                    let i0 = (pos as usize) % n;
                    let i1 = (i0 + 1) % n;
                    lerp(pts[i0], pts[i1], pos - pos.floor())
                }
                _ => fast_sin01(p),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_stay_bipolar_and_bounded() {
        for shape in &LfoShape::ALL[..=LfoShape::SampleHold as usize] {
            let mut lfo = Lfo::default();
            lfo.prepare(48_000.0);
            lfo.set_shape(*shape);
            lfo.set_rate(37.0);
            for _ in 0..5000 {
                let v = lfo.next();
                assert!(v.is_finite(), "{shape:?}");
                assert!((-1.0001..=1.0001).contains(&v), "{shape:?}: {v}");
            }
        }
    }

    #[test]
    fn sine_completes_one_cycle_per_period() {
        let mut lfo = Lfo::default();
        lfo.prepare(1000.0);
        lfo.set_shape(LfoShape::Sine);
        lfo.set_rate(1.0);
        lfo.reset(0.0);
        let sum: f32 = (0..1000).map(|_| lfo.next()).sum();
        // A full cycle averages out.
        assert!((sum / 1000.0).abs() < 0.01);
    }

    #[test]
    fn custom_shape_is_used_when_provided() {
        let points = Arc::new(vec![1.0, 1.0, -1.0, -1.0]);
        let mut lfo = Lfo::default();
        lfo.prepare(48_000.0);
        lfo.set_shape(LfoShape::Custom);
        lfo.set_custom_shape(Some(&points));
        lfo.set_rate(1.0);
        lfo.reset(0.0);
        assert!((lfo.next() - 1.0).abs() < 1.0e-4);
        lfo.reset(0.55);
        assert!(lfo.next() < 0.0);
    }

    #[test]
    fn smoothing_removes_jumps() {
        let mut lfo = Lfo::default();
        lfo.prepare(48_000.0);
        lfo.set_shape(LfoShape::Square);
        lfo.set_rate(2.0);
        lfo.set_smoothing(1.0);
        lfo.reset(0.0);
        let mut previous = lfo.next();
        for _ in 0..48_000 {
            let v = lfo.next();
            assert!((v - previous).abs() < 0.05);
            previous = v;
        }
    }
}
