//! FX-lite (SPEC 3.6): distortion -> chorus -> delay -> reverb, fixed order,
//! bypass per module. Runs at the host sample rate, after the voice mix.

use super::common::{clamp01, db_to_gain, fast_sin01, lerp, soft_clip};
use super::params::{time_div_beats, LfoSync, ParamId, NUM_PARAMS};

/// Simple fractional-delay line.
#[derive(Clone, Debug, Default)]
pub struct DelayLine {
    buffer: Vec<f32>,
    write_pos: usize,
}

impl DelayLine {
    /// Allocates; call from `prepare`, never from the audio thread.
    pub fn prepare(&mut self, max_samples: usize) {
        self.buffer.clear();
        self.buffer.resize(max_samples.max(4), 0.0);
        self.write_pos = 0;
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
    }

    // The reverb alone runs a dozen of these per sample, so the wrap is a
    // compare and a subtract rather than an integer division: the write pointer
    // only ever advances by one, and `pos` below is bounded by 2 * size.
    #[inline(always)]
    pub fn write(&mut self, x: f32) {
        if self.buffer.is_empty() {
            return;
        }
        self.buffer[self.write_pos] = x;
        self.write_pos += 1;
        if self.write_pos >= self.buffer.len() {
            self.write_pos = 0;
        }
    }

    #[inline(always)]
    pub fn read(&self, delay_samples: f32) -> f32 {
        let size = self.buffer.len();
        if size < 4 {
            return 0.0;
        }
        let d = delay_samples.clamp(1.0, size as f32 - 2.0);
        let pos = self.write_pos as f32 - d + size as f32;
        let mut i0 = pos as usize;
        if i0 >= size {
            i0 -= size;
        }
        let mut i1 = i0 + 1;
        if i1 >= size {
            i1 = 0;
        }
        let frac = pos - pos.floor();
        lerp(self.buffer[i0], self.buffer[i1], frac)
    }
}

/// Freeverb-style comb with damping.
#[derive(Clone, Debug)]
pub struct DampedComb {
    line: DelayLine,
    length: f32,
    store: f32,
}

impl Default for DampedComb {
    fn default() -> Self {
        Self { line: DelayLine::default(), length: 100.0, store: 0.0 }
    }
}

impl DampedComb {
    pub fn prepare(&mut self, samples: usize) {
        self.line.prepare(samples + 4);
        self.length = samples as f32;
    }

    pub fn reset(&mut self) {
        self.line.reset();
        self.store = 0.0;
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.line.read(self.length);
        self.store = out * (1.0 - damp) + self.store * damp;
        self.line.write(x + self.store * feedback);
        out
    }
}

#[derive(Clone, Debug)]
pub struct Allpass {
    line: DelayLine,
    length: f32,
}

impl Default for Allpass {
    fn default() -> Self {
        Self { line: DelayLine::default(), length: 100.0 }
    }
}

impl Allpass {
    pub fn prepare(&mut self, samples: usize) {
        self.line.prepare(samples + 4);
        self.length = samples as f32;
    }

    pub fn reset(&mut self) {
        self.line.reset();
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32, gain: f32) -> f32 {
        let delayed = self.line.read(self.length);
        let out = -x + delayed;
        self.line.write(x + delayed * gain);
        out
    }
}

/// Mutually prime comb / allpass lengths (Freeverb heritage), scaled by sample rate.
const COMB_BASE: [usize; 4] = [1557, 1617, 1491, 1422];
const ALLPASS_BASE: [usize; 2] = [556, 441];
const MAX_DELAY_SECONDS: f32 = 2.1;
const MAX_CHORUS_MS: f32 = 30.0;

#[derive(Clone, Debug)]
pub struct FxChain {
    sr: f32,

    // Distortion
    dist_on: bool,
    dist_drive: f32,
    dist_tone: f32,
    dist_mix: f32,
    dist_lp_l: f32,
    dist_lp_r: f32,

    // Chorus
    chorus_on: bool,
    chorus_rate: f32,
    chorus_depth: f32,
    chorus_mix: f32,
    chorus_phase: f32,
    chorus_l: DelayLine,
    chorus_r: DelayLine,

    // Delay
    delay_on: bool,
    ping_pong: bool,
    delay_samples: f32,
    delay_feedback: f32,
    delay_tone: f32,
    delay_mix: f32,
    delay_lp_l: f32,
    delay_lp_r: f32,
    delay_l: DelayLine,
    delay_r: DelayLine,

    // Reverb
    rev_on: bool,
    rev_size: f32,
    rev_damp: f32,
    rev_mix: f32,
    rev_pre_delay_samples: f32,
    combs: [[DampedComb; 4]; 2],
    allpasses: [[Allpass; 2]; 2],
    pre_delay_l: DelayLine,
    pre_delay_r: DelayLine,
}

impl Default for FxChain {
    fn default() -> Self {
        Self {
            sr: 48_000.0,
            dist_on: false,
            dist_drive: 0.0,
            dist_tone: 0.5,
            dist_mix: 1.0,
            dist_lp_l: 0.0,
            dist_lp_r: 0.0,
            chorus_on: false,
            chorus_rate: 0.6,
            chorus_depth: 0.4,
            chorus_mix: 0.5,
            chorus_phase: 0.0,
            chorus_l: DelayLine::default(),
            chorus_r: DelayLine::default(),
            delay_on: false,
            ping_pong: false,
            delay_samples: 10_000.0,
            delay_feedback: 0.35,
            delay_tone: 0.5,
            delay_mix: 0.3,
            delay_lp_l: 0.0,
            delay_lp_r: 0.0,
            delay_l: DelayLine::default(),
            delay_r: DelayLine::default(),
            rev_on: false,
            rev_size: 0.5,
            rev_damp: 0.4,
            rev_mix: 0.25,
            rev_pre_delay_samples: 480.0,
            combs: Default::default(),
            allpasses: Default::default(),
            pre_delay_l: DelayLine::default(),
            pre_delay_r: DelayLine::default(),
        }
    }
}

impl FxChain {
    /// Allocates every delay line for `sample_rate`. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sr = sample_rate as f32;
        let sr = self.sr;
        let scale = sr / 44_100.0;

        let chorus = (sr * MAX_CHORUS_MS * 0.001) as usize + 8;
        self.chorus_l.prepare(chorus);
        self.chorus_r.prepare(chorus);
        let delay = (sr * MAX_DELAY_SECONDS) as usize + 8;
        self.delay_l.prepare(delay);
        self.delay_r.prepare(delay);
        let pre = (sr * 0.25) as usize + 8;
        self.pre_delay_l.prepare(pre);
        self.pre_delay_r.prepare(pre);

        for ch in 0..2 {
            for (i, comb) in self.combs[ch].iter_mut().enumerate() {
                comb.prepare(((COMB_BASE[i] + ch * 23) as f32 * scale) as usize);
            }
            for (i, ap) in self.allpasses[ch].iter_mut().enumerate() {
                ap.prepare(((ALLPASS_BASE[i] + ch * 17) as f32 * scale) as usize);
            }
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        self.dist_lp_l = 0.0;
        self.dist_lp_r = 0.0;
        self.delay_lp_l = 0.0;
        self.delay_lp_r = 0.0;
        self.chorus_phase = 0.0;
        self.chorus_l.reset();
        self.chorus_r.reset();
        self.delay_l.reset();
        self.delay_r.reset();
        self.pre_delay_l.reset();
        self.pre_delay_r.reset();
        for ch in 0..2 {
            for c in self.combs[ch].iter_mut() {
                c.reset();
            }
            for a in self.allpasses[ch].iter_mut() {
                a.reset();
            }
        }
    }

    /// Parameter values in natural units. `mod_by_param` carries the matrix
    /// offsets for the FX macros, indexed by [`ParamId`] like `params`.
    pub fn set_params(&mut self, params: &[f32; NUM_PARAMS], mod_by_param: Option<&[f32; NUM_PARAMS]>, tempo_bpm: f32) {
        let value = |id: ParamId| {
            let d = id.desc();
            let v = params[id.index()] + mod_by_param.map_or(0.0, |m| m[id.index()]);
            v.clamp(d.min, d.max)
        };
        let on = |id: ParamId| params[id.index()] > 0.5;

        self.dist_on = on(ParamId::FxDistOn);
        self.dist_drive = value(ParamId::FxDistDrive);
        self.dist_tone = value(ParamId::FxDistTone);
        self.dist_mix = value(ParamId::FxDistMix);

        self.chorus_on = on(ParamId::FxChorusOn);
        self.chorus_rate = value(ParamId::FxChorusRate);
        self.chorus_depth = value(ParamId::FxChorusDepth);
        self.chorus_mix = value(ParamId::FxChorusMix);

        self.delay_on = on(ParamId::FxDelayOn);
        self.ping_pong = on(ParamId::FxDelayPingPong);
        self.delay_feedback = value(ParamId::FxDelayFeedback);
        self.delay_tone = value(ParamId::FxDelayTone);
        self.delay_mix = value(ParamId::FxDelayMix);
        let mut delay_ms = value(ParamId::FxDelayTimeMs);
        if params[ParamId::FxDelaySyncMode.index()].round() as i32 == LfoSync::Tempo as i32 {
            let beats = time_div_beats(params[ParamId::FxDelaySyncDiv.index()].round().max(0.0) as usize);
            delay_ms = 60_000.0 / tempo_bpm.max(10.0) * beats;
            // Matrix modulation of delay time still applies, in ms.
            if let Some(m) = mod_by_param {
                delay_ms += m[ParamId::FxDelayTimeMs.index()];
            }
        }
        self.delay_samples = (delay_ms * 0.001 * self.sr).clamp(2.0, MAX_DELAY_SECONDS * self.sr - 4.0);

        self.rev_on = on(ParamId::FxRevOn);
        self.rev_size = value(ParamId::FxRevSize);
        self.rev_damp = value(ParamId::FxRevDamp);
        self.rev_mix = value(ParamId::FxRevMix);
        self.rev_pre_delay_samples = (value(ParamId::FxRevPreDelay) * 0.001 * self.sr).clamp(1.0, 0.24 * self.sr);
    }

    /// Processes the stereo buffers in place. Allocation-free.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let sr = self.sr;
        let chorus_inc = self.chorus_rate / sr;
        let chorus_base_ms = 12.0;
        let comb_feedback = 0.7 + 0.28 * self.rev_size;
        let damp = clamp01(self.rev_damp) * 0.4;
        let dist_gain = db_to_gain(self.dist_drive * 30.0);
        let dist_makeup = 1.0 / (1.0 + self.dist_drive * 1.5);
        // Tone: 0 = dark, 1 = bright, implemented as a one-pole tilt.
        let dist_lp_coeff = clamp01(0.03 + 0.9 * self.dist_tone);
        let delay_lp_coeff = clamp01(0.05 + 0.9 * self.delay_tone);

        for (lo, ro) in left.iter_mut().zip(right.iter_mut()) {
            let mut l = *lo;
            let mut r = *ro;

            if self.dist_on {
                let dl = soft_clip(l * dist_gain) * dist_makeup;
                let dr = soft_clip(r * dist_gain) * dist_makeup;
                self.dist_lp_l += dist_lp_coeff * (dl - self.dist_lp_l);
                self.dist_lp_r += dist_lp_coeff * (dr - self.dist_lp_r);
                l = lerp(l, self.dist_lp_l, self.dist_mix);
                r = lerp(r, self.dist_lp_r, self.dist_mix);
            }

            if self.chorus_on {
                self.chorus_phase += chorus_inc;
                if self.chorus_phase >= 1.0 {
                    self.chorus_phase -= 1.0;
                }
                let mod1 = fast_sin01(self.chorus_phase);
                let mod2 = fast_sin01(self.chorus_phase + 0.25);
                let depth_ms = self.chorus_depth * 8.0;
                let d1 = (chorus_base_ms + depth_ms * mod1) * 0.001 * sr;
                let d2 = (chorus_base_ms + depth_ms * mod2) * 0.001 * sr;
                let wet_l = self.chorus_l.read(d1);
                let wet_r = self.chorus_r.read(d2);
                self.chorus_l.write(l);
                self.chorus_r.write(r);
                l = lerp(l, 0.5 * (l + wet_l), self.chorus_mix);
                r = lerp(r, 0.5 * (r + wet_r), self.chorus_mix);
            }

            if self.delay_on {
                let wet_l = self.delay_l.read(self.delay_samples);
                let wet_r = self.delay_r.read(self.delay_samples);
                self.delay_lp_l += delay_lp_coeff * (wet_l - self.delay_lp_l);
                self.delay_lp_r += delay_lp_coeff * (wet_r - self.delay_lp_r);
                if self.ping_pong {
                    self.delay_l.write(l + self.delay_lp_r * self.delay_feedback);
                    self.delay_r.write(r + self.delay_lp_l * self.delay_feedback);
                } else {
                    self.delay_l.write(l + self.delay_lp_l * self.delay_feedback);
                    self.delay_r.write(r + self.delay_lp_r * self.delay_feedback);
                }
                l = lerp(l, wet_l, self.delay_mix);
                r = lerp(r, wet_r, self.delay_mix);
            }

            if self.rev_on {
                self.pre_delay_l.write(l);
                self.pre_delay_r.write(r);
                let in_l = self.pre_delay_l.read(self.rev_pre_delay_samples) * 0.4;
                let in_r = self.pre_delay_r.read(self.rev_pre_delay_samples) * 0.4;

                let mut wet_l = 0.0;
                let mut wet_r = 0.0;
                for c in 0..4 {
                    wet_l += self.combs[0][c].process(in_l, comb_feedback, damp);
                    wet_r += self.combs[1][c].process(in_r, comb_feedback, damp);
                }
                for a in 0..2 {
                    wet_l = self.allpasses[0][a].process(wet_l, 0.5);
                    wet_r = self.allpasses[1][a].process(wet_r, 0.5);
                }
                l = lerp(l, wet_l * 0.3, self.rev_mix);
                r = lerp(r, wet_r * 0.3, self.rev_mix);
            }

            *lo = l;
            *ro = r;
        }
    }

    /// Reported to the host as the plugin tail.
    pub fn tail_seconds(&self) -> f32 {
        let mut tail: f32 = 0.0;
        if self.delay_on {
            let repeats = if self.delay_feedback >= 0.99 {
                40.0
            } else {
                (4.0 / (1.0 - self.delay_feedback).max(0.01)).min(40.0)
            };
            tail = tail.max(self.delay_samples / self.sr * repeats);
        }
        if self.rev_on {
            tail = tail.max(1.0 + self.rev_size * 6.0);
        }
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::common::Rng;
    use crate::dsp::params::defaults;

    fn chain(params: &[f32; NUM_PARAMS]) -> FxChain {
        let mut fx = FxChain::default();
        fx.prepare(48_000.0);
        fx.set_params(params, None, 120.0);
        fx
    }

    #[test]
    fn all_modules_bypassed_pass_audio_unchanged() {
        let params = defaults();
        let mut fx = chain(&params);
        let mut rng = Rng::new(3);
        let input: Vec<f32> = (0..512).map(|_| rng.next_bipolar()).collect();
        let (mut l, mut r) = (input.clone(), input.clone());
        fx.process(&mut l, &mut r);
        assert_eq!(l, input);
        assert_eq!(r, input);
        assert_eq!(fx.tail_seconds(), 0.0);
    }

    #[test]
    fn tail_reflects_fx_settings() {
        let mut params = defaults();
        let dry = chain(&params).tail_seconds();
        params[ParamId::FxRevOn.index()] = 1.0;
        params[ParamId::FxRevSize.index()] = 1.0;
        let reverb = chain(&params).tail_seconds();
        assert!(reverb > dry);
        params[ParamId::FxDelayOn.index()] = 1.0;
        params[ParamId::FxDelayFeedback.index()] = 0.95;
        assert!(chain(&params).tail_seconds() >= reverb);
    }

    #[test]
    fn tempo_synced_delay_repeats_after_one_division() {
        let mut params = defaults();
        params[ParamId::FxDelayOn.index()] = 1.0;
        params[ParamId::FxDelaySyncMode.index()] = 1.0;
        // 1/4 at 120 bpm = 500 ms = 24000 samples.
        params[ParamId::FxDelaySyncDiv.index()] = 2.0;
        params[ParamId::FxDelayMix.index()] = 1.0;
        params[ParamId::FxDelayFeedback.index()] = 0.0;
        let mut fx = chain(&params);
        let mut l = vec![0.0; 30_000];
        let mut r = vec![0.0; 30_000];
        l[0] = 1.0;
        r[0] = 1.0;
        fx.process(&mut l, &mut r);
        let peak = l.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert!((peak as i32 - 24_000).abs() <= 1, "echo at {peak}");
    }

    #[test]
    fn every_module_stays_finite_and_bounded() {
        let mut params = defaults();
        for id in [ParamId::FxDistOn, ParamId::FxChorusOn, ParamId::FxDelayOn, ParamId::FxRevOn, ParamId::FxDelayPingPong] {
            params[id.index()] = 1.0;
        }
        params[ParamId::FxDelayFeedback.index()] = 0.95;
        params[ParamId::FxRevSize.index()] = 1.0;
        params[ParamId::FxDistDrive.index()] = 1.0;
        let mut fx = chain(&params);
        let mut rng = Rng::new(11);
        let mut peak = 0.0_f32;
        for _ in 0..40 {
            let mut l: Vec<f32> = (0..1200).map(|_| rng.next_bipolar()).collect();
            let mut r = l.clone();
            fx.process(&mut l, &mut r);
            for v in l.iter().chain(r.iter()) {
                assert!(v.is_finite());
                peak = peak.max(v.abs());
            }
        }
        assert!(peak < 20.0, "peak {peak}");
    }

    #[test]
    fn matrix_offsets_apply_and_clamp() {
        let mut params = defaults();
        params[ParamId::FxDistOn.index()] = 1.0;
        params[ParamId::FxDistMix.index()] = 0.0;
        let mut offsets = [0.0; NUM_PARAMS];
        // +5 would leave the 0..1 range; it clamps to fully wet.
        offsets[ParamId::FxDistMix.index()] = 5.0;
        let mut fx = FxChain::default();
        fx.prepare(48_000.0);
        fx.set_params(&params, Some(&offsets), 120.0);
        let (mut l, mut r) = (vec![0.5; 64], vec![0.5; 64]);
        fx.process(&mut l, &mut r);
        assert!((l[63] - 0.5).abs() > 1.0e-3, "distortion stayed dry");
    }
}
