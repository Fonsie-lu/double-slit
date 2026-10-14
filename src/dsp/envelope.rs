//! ADSR with per-stage curve shaping (SPEC 3.5). Sample-accurate, no branching
//! surprises on zero-length stages.

use super::common::{clamp01, fast_pow, lerp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Idle = 0,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Debug)]
pub struct Envelope {
    sample_rate: f32,
    attack_time: f32,
    decay_time: f32,
    sustain: f32,
    release_time: f32,
    attack_curve: f32,
    decay_curve: f32,
    release_curve: f32,
    attack_rate: f32,
    decay_rate: f32,
    release_rate: f32,
    rates_valid: bool,
    curves_valid: bool,
    attack_exponent: f32,
    decay_exponent: f32,
    release_exponent: f32,
    level: f32,
    stage_pos: f32,
    release_from: f32,
    stage: Stage,
}

impl Default for Envelope {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            attack_time: 0.005,
            decay_time: 0.3,
            sustain: 0.8,
            release_time: 0.25,
            attack_curve: 0.0,
            decay_curve: 0.0,
            release_curve: 0.0,
            attack_rate: 1.0,
            decay_rate: 1.0,
            release_rate: 1.0,
            rates_valid: false,
            curves_valid: false,
            attack_exponent: 1.0,
            decay_exponent: 1.0,
            release_exponent: 1.0,
            level: 0.0,
            stage_pos: 0.0,
            release_from: 0.0,
            stage: Stage::Idle,
        }
    }
}

impl Envelope {
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.reset();
    }

    /// Oversampling switch mid-note: keep the stage, just change the clock.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.rates_valid = false;
        self.set_times(self.attack_time, self.decay_time, self.sustain, self.release_time);
    }

    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.level = 0.0;
        self.stage_pos = 0.0;
        self.release_from = 0.0;
    }

    pub fn set_times(&mut self, attack_s: f32, decay_s: f32, sustain_level: f32, release_s: f32) {
        // Bit-exact cache check: the caller passes the same values every block.
        if self.rates_valid
            && attack_s == self.attack_time
            && decay_s == self.decay_time
            && sustain_level == self.sustain
            && release_s == self.release_time
        {
            return;
        }
        self.rates_valid = true;
        self.attack_time = attack_s.max(0.0);
        self.decay_time = decay_s.max(0.0);
        self.sustain = clamp01(sustain_level);
        self.release_time = release_s.max(0.0);
        // Rates are precomputed: next() runs per sample and must not divide.
        self.attack_rate = 1.0 / (self.attack_time * self.sample_rate).max(1.0);
        self.decay_rate = 1.0 / (self.decay_time * self.sample_rate).max(1.0);
        self.release_rate = 1.0 / (self.release_time * self.sample_rate).max(1.0);
    }

    pub fn set_curves(&mut self, attack_curve: f32, decay_curve: f32, release_curve: f32) {
        if self.curves_valid
            && attack_curve == self.attack_curve
            && decay_curve == self.decay_curve
            && release_curve == self.release_curve
        {
            return;
        }
        self.curves_valid = true;
        self.attack_curve = attack_curve.clamp(-1.0, 1.0);
        self.decay_curve = decay_curve.clamp(-1.0, 1.0);
        self.release_curve = release_curve.clamp(-1.0, 1.0);
        self.attack_exponent = (2.0 * self.attack_curve).exp2();
        self.decay_exponent = (-2.0 * self.decay_curve).exp2();
        self.release_exponent = (-2.0 * self.release_curve).exp2();
    }

    pub fn note_on(&mut self) {
        self.stage = Stage::Attack;
        self.stage_pos = 0.0;
        // Zero attack must land exactly on 1.0 before the first decay sample.
        if self.attack_time <= 0.0 {
            self.level = 1.0;
            self.stage = Stage::Decay;
            self.stage_pos = 0.0;
            if self.decay_time <= 0.0 {
                self.level = self.sustain;
                self.stage = Stage::Sustain;
            }
        }
    }

    pub fn note_off(&mut self) {
        if self.stage == Stage::Idle {
            return;
        }
        self.release_from = self.level;
        self.stage = Stage::Release;
        self.stage_pos = 0.0;
        if self.release_time <= 0.0 {
            self.level = 0.0;
            self.stage = Stage::Idle;
        }
    }

    /// Immediate cut, used by voice stealing.
    pub fn kill(&mut self) {
        self.reset();
    }

    pub fn is_active(&self) -> bool {
        self.stage != Stage::Idle
    }

    pub fn is_releasing(&self) -> bool {
        self.stage == Stage::Release
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn value(&self) -> f32 {
        self.level
    }

    /// One sample. Not an `Iterator`: the stream never ends and the name
    /// matches the rest of the DSP code.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Idle => self.level = 0.0,
            Stage::Attack => {
                self.stage_pos += self.attack_rate;
                if self.stage_pos >= 1.0 {
                    self.stage_pos = 0.0;
                    self.level = 1.0;
                    if self.decay_time <= 0.0 {
                        self.stage = Stage::Sustain;
                        self.level = self.sustain;
                    } else {
                        self.stage = Stage::Decay;
                    }
                } else {
                    self.level = shape(self.stage_pos, self.attack_exponent);
                }
            }
            Stage::Decay => {
                self.stage_pos += self.decay_rate;
                if self.stage_pos >= 1.0 {
                    self.stage_pos = 0.0;
                    self.level = self.sustain;
                    self.stage = Stage::Sustain;
                } else {
                    self.level = lerp(1.0, self.sustain, shape(self.stage_pos, self.decay_exponent));
                }
            }
            Stage::Sustain => {
                self.level = self.sustain;
                if self.sustain <= 0.0 {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
            Stage::Release => {
                self.stage_pos += self.release_rate;
                if self.stage_pos >= 1.0 {
                    self.stage_pos = 0.0;
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                } else {
                    self.level = self.release_from * (1.0 - shape(self.stage_pos, self.release_exponent));
                }
            }
        }
        self.level
    }
}

/// exponent > 1 = slow start (convex), < 1 = fast start (concave).
/// `fast_pow`, not `powf`: this runs once per sample per envelope per voice as
/// soon as any curve is non-zero, which is no place for a libm pow.
#[inline(always)]
fn shape(x: f32, exponent: f32) -> f32 {
    let t = clamp01(x);
    if exponent == 1.0 {
        return t;
    }
    clamp01(fast_pow(t, exponent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(a: f32, d: f32, s: f32, r: f32) -> Envelope {
        let mut e = Envelope::default();
        e.prepare(48_000.0);
        e.set_times(a, d, s, r);
        e
    }

    #[test]
    fn zero_attack_jumps_straight_to_decay() {
        let mut e = env(0.0, 0.1, 0.5, 0.1);
        e.note_on();
        // First sample must already be at full level, not ramping up.
        assert!((e.value() - 1.0).abs() < 1.0e-6);
        assert!(e.next() <= 1.0);
        assert!(e.next() > 0.5);
    }

    #[test]
    fn zero_attack_and_decay_lands_on_sustain() {
        let mut e = env(0.0, 0.0, 0.35, 0.1);
        e.note_on();
        assert!((e.next() - 0.35).abs() < 1.0e-6);
        assert_eq!(e.stage(), Stage::Sustain);
    }

    #[test]
    fn zero_release_ends_immediately() {
        let mut e = env(0.01, 0.1, 0.8, 0.0);
        e.note_on();
        for _ in 0..1000 {
            e.next();
        }
        e.note_off();
        assert!(!e.is_active());
        assert!(e.value().abs() < 1.0e-9);
    }

    #[test]
    fn reaches_sustain_and_releases_to_zero() {
        let mut e = env(0.001, 0.002, 0.6, 0.01);
        e.note_on();
        for _ in 0..480 {
            e.next();
        }
        assert!((e.value() - 0.6).abs() < 0.01);
        e.note_off();
        for _ in 0..1000 {
            e.next();
        }
        assert!(!e.is_active());
        assert!(e.value().abs() < 1.0e-6);
    }

    #[test]
    fn curves_stay_in_range_and_monotonic() {
        for curve in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            let mut e = env(0.05, 0.05, 1.0, 0.05);
            e.set_curves(curve, curve, curve);
            e.note_on();
            let mut previous = -1.0;
            for _ in 0..((0.05 * 48_000.0) as i32 - 2) {
                let v = e.next();
                assert!((0.0..=1.0).contains(&v), "curve {curve}: {v}");
                assert!(v >= previous - 1.0e-6, "curve {curve}");
                previous = v;
            }
        }
    }

    #[test]
    fn zero_sustain_ends_voice_during_sustain() {
        let mut e = env(0.0, 0.001, 0.0, 0.1);
        e.note_on();
        for _ in 0..200 {
            e.next();
        }
        assert!(!e.is_active());
    }

    #[test]
    fn fast_pow_matches_libm_over_the_curve_range() {
        // The curve exponents come from exp2(+-2 * curve), so [0.25, 4]. Envelope
        // shaping calls this per sample per voice, where powf does not belong.
        let mut worst = 0.0_f32;
        for exponent in [0.25_f32, 0.5, 1.5, 2.0, 4.0] {
            for i in 0..=1000 {
                let t = i as f32 / 1000.0;
                worst = worst.max((fast_pow(t, exponent) - t.powf(exponent)).abs());
            }
        }
        assert!(worst < 1.0e-3, "fast_pow worst absolute error {worst}");
        assert_eq!(fast_pow(0.0, 2.0), 0.0);
        assert!((fast_pow(1.0, 4.0) - 1.0).abs() < 1.0e-4);
    }
}
