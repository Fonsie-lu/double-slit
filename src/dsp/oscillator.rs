//! Wavetable oscillator with unison, hard sync, FM/AM and noise modes
//! (SPEC 3.2). Everything in [`Oscillator::process`] is audio-thread safe: the
//! table is a snapshot the voice takes once per block, and the frame data is
//! resolved once per block (or per modulation tick) into a plain slice.

use super::common::{cents_to_ratio, clamp01, Rng, MAX_UNISON, MIP_LEVELS, PI};
use super::params::{NoiseMode, PhaseMode, SyncMode};
use super::wavetable::Wavetable;

const SQRT2: f32 = std::f32::consts::SQRT_2;

/// Per-sample oscillator input, already modulated by the voice.
#[derive(Clone, Copy, Debug)]
pub struct OscInput {
    /// Base pitch in cycles/sample.
    pub increment: f32,
    /// FM/PM offset in cycles.
    pub phase_mod: f32,
    pub level: f32,
    /// Pre-computed equal-power pan gains (saves per-sample trig).
    pub pan_l: f32,
    pub pan_r: f32,
    /// Band-limit level for the current pitch.
    pub mip_level: usize,
    /// Master wrapped this sample (hard sync from osc A).
    pub sync_reset: bool,
    pub sync_ratio: f32,
}

impl Default for OscInput {
    fn default() -> Self {
        Self {
            increment: 0.0,
            phase_mod: 0.0,
            level: 1.0,
            pan_l: 1.0,
            pan_r: 1.0,
            mip_level: 0,
            sync_reset: false,
            sync_ratio: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StereoSample {
    pub left: f32,
    pub right: f32,
    pub mono: f32,
}

/// Frame index resolved for the current WT position. The voice turns it into
/// a data slice with [`Wavetable::frame_pointer`] once per block.
#[derive(Clone, Copy, Debug, Default)]
pub struct OscFrame {
    pub frame: usize,
    pub level: usize,
}

/// Picks the frame and mip level for a WT position (0..1) and pitch.
#[inline]
pub fn resolve_frame(table: &Wavetable, wt_pos: f32, mip_level: usize) -> OscFrame {
    let frames = table.num_frames();
    let frame = if frames <= 1 {
        0
    } else {
        ((clamp01(wt_pos) * (frames - 1) as f32 + 0.5) as usize).min(frames - 1)
    };
    OscFrame { frame, level: mip_level.min(MIP_LEVELS - 1) }
}

/// Hermite read from a frame slice whose length is a power of two. Same maths
/// as [`Wavetable::read_frame`], without the bounds checks: the masks keep
/// every index inside `data`.
#[inline(always)]
fn read_frame_fast(data: &[f32], mask: usize, size_f: f32, phase: f32) -> f32 {
    debug_assert_eq!(data.len(), mask + 1);
    let pos = phase * size_f;
    let i0 = (pos as i32 as usize) & mask;
    let frac = pos - pos.floor();
    // SAFETY: every index is masked with `data.len() - 1` and the length is a
    // power of two, so all four reads are in bounds.
    let (xm1, x0, x1, x2) = unsafe {
        (
            *data.get_unchecked(i0.wrapping_sub(1) & mask),
            *data.get_unchecked(i0),
            *data.get_unchecked((i0 + 1) & mask),
            *data.get_unchecked((i0 + 2) & mask),
        )
    };
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b_neg = w + a;
    (((a * frac) - b_neg) * frac + c) * frac + x0
}

#[derive(Clone, Debug)]
pub struct Oscillator {
    sample_rate: f32,

    active_voices: usize,
    detune_cents: f32,
    spread_amount: f32,
    blend_amount: f32,
    detune_ratio: [f32; MAX_UNISON],
    voice_gain: [f32; MAX_UNISON],
    unison_pan_l: [f32; MAX_UNISON],
    unison_pan_r: [f32; MAX_UNISON],
    base_phase: [f32; MAX_UNISON],
    read_phase: [f32; MAX_UNISON],
    normalise: f32,
    normalise_mono: f32,
    widest_detune: f32,

    sync_mode: SyncMode,
    noise_mode: NoiseMode,
    wrapped: bool,
    gains_valid: bool,
    display_frame: usize,
    pink: [f32; 3],
    rng: Rng,
    /// AVX2 + FMA available at runtime (checked once, off the audio thread).
    simd: bool,
}

impl Default for Oscillator {
    fn default() -> Self {
        let mut o = Self {
            sample_rate: 48_000.0,
            active_voices: 1,
            detune_cents: 0.0,
            spread_amount: 0.5,
            blend_amount: 0.75,
            detune_ratio: [1.0; MAX_UNISON],
            voice_gain: [0.0; MAX_UNISON],
            unison_pan_l: [0.0; MAX_UNISON],
            unison_pan_r: [0.0; MAX_UNISON],
            base_phase: [0.0; MAX_UNISON],
            read_phase: [0.0; MAX_UNISON],
            normalise: 1.0,
            normalise_mono: 1.0,
            widest_detune: 1.0,
            sync_mode: SyncMode::Off,
            noise_mode: NoiseMode::Off,
            wrapped: false,
            gains_valid: false,
            display_frame: 0,
            pink: [0.0; 3],
            rng: Rng::new(0x1f12_3bb5),
            simd: detect_simd(),
        };
        o.update_gains();
        o
    }
}

impl Oscillator {
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
        self.gains_valid = false;
        self.set_unison(self.active_voices, self.detune_cents, self.spread_amount, self.blend_amount);
        self.reset();
    }

    /// Oversampling switch: no phase reset, increments come from the caller.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate as f32;
    }

    pub fn reset(&mut self) {
        self.base_phase = [0.0; MAX_UNISON];
        self.read_phase = [0.0; MAX_UNISON];
        self.pink = [0.0; 3];
        self.wrapped = false;
    }

    pub fn set_sync_mode(&mut self, mode: SyncMode) {
        self.sync_mode = mode;
    }

    pub fn set_noise_mode(&mut self, mode: NoiseMode) {
        self.noise_mode = mode;
    }

    pub fn seed(&mut self, s: u32) {
        self.rng.seed(s);
    }

    /// True when unison voice 0 wrapped during the last `process()` call.
    pub fn did_wrap(&self) -> bool {
        self.wrapped
    }

    /// Frame the oscillator last read, for the animated display.
    pub fn last_frame(&self) -> usize {
        self.display_frame
    }

    /// Highest detune ratio in the unison stack; used to pick the mip level.
    pub fn max_detune_ratio(&self) -> f32 {
        self.widest_detune
    }

    pub fn set_unison(&mut self, voices: usize, detune: f32, spread: f32, blend: f32) {
        let voices = voices.clamp(1, MAX_UNISON);
        let detune = detune.clamp(0.0, 200.0);
        let spread = clamp01(spread);
        let blend = clamp01(blend);
        // Skip the trig in update_gains() when nothing actually changed.
        if self.gains_valid
            && voices == self.active_voices
            && detune.to_bits() == self.detune_cents.to_bits()
            && spread.to_bits() == self.spread_amount.to_bits()
            && blend.to_bits() == self.blend_amount.to_bits()
        {
            return;
        }
        self.active_voices = voices;
        self.detune_cents = detune;
        self.spread_amount = spread;
        self.blend_amount = blend;
        self.update_gains();
    }

    fn update_gains(&mut self) {
        let n = self.active_voices;
        let mut gain_sum = 0.0;
        let mut widest: f32 = 1.0;
        for i in 0..n {
            // -1 … +1 across the unison stack, 0 for a single voice.
            let pos = if n == 1 { 0.0 } else { 2.0 * i as f32 / (n - 1) as f32 - 1.0 };
            // Slightly super-linear detune spread, as on hardware supersaws.
            let curved = pos * (0.6 + 0.4 * pos.abs());
            self.detune_ratio[i] = cents_to_ratio(curved * self.detune_cents);

            // Blend fades the outer voices against the centre. An even stack has
            // no single centre voice, so the innermost pair takes that role -
            // otherwise every gain collapses to blend and blend = 0 silences the
            // oscillator outright for the counts people actually pick (2, 4, 8, 16).
            let is_centre = if n % 2 == 1 { i == n / 2 } else { i == n / 2 - 1 || i == n / 2 };
            self.voice_gain[i] = if is_centre || n == 1 { 1.0 } else { self.blend_amount };

            let pan = pos * self.spread_amount;
            self.unison_pan_l[i] = ((pan + 1.0) * 0.25 * PI).cos();
            self.unison_pan_r[i] = ((pan + 1.0) * 0.25 * PI).sin();
            gain_sum += self.voice_gain[i] * self.voice_gain[i];
            widest = widest.max(self.detune_ratio[i]);
        }
        for i in n..MAX_UNISON {
            self.detune_ratio[i] = 1.0;
            self.voice_gain[i] = 0.0;
            self.unison_pan_l[i] = std::f32::consts::FRAC_1_SQRT_2;
            self.unison_pan_r[i] = std::f32::consts::FRAC_1_SQRT_2;
        }
        self.normalise = 1.0 / gain_sum.max(0.25).sqrt();
        self.normalise_mono = self.normalise;
        self.widest_detune = widest;
        self.gains_valid = true;
    }

    /// Resets phases according to the phase mode; call on note-on.
    pub fn start(&mut self, mode: PhaseMode, phase_degrees: f32) {
        // The phase parameter is folded straight into the accumulator so the
        // audio loop never has to add an offset per sample.
        let base = phase_degrees / 360.0 - (phase_degrees / 360.0).floor();
        for i in 0..MAX_UNISON {
            match mode {
                // Keep the running phase.
                PhaseMode::Free => {}
                PhaseMode::Random => {
                    let random = self.rng.next_float();
                    self.base_phase[i] = random;
                    self.read_phase[i] = random;
                }
                PhaseMode::Reset => {
                    self.base_phase[i] = base;
                    self.read_phase[i] = base;
                }
            }
        }
    }

    /// One output sample. `data` is the resolved frame at the current mip
    /// level (power-of-two length), `frame` its index for the display.
    #[inline]
    pub fn process(&mut self, input: &OscInput, data: &[f32], frame: usize) -> StereoSample {
        self.wrapped = false;
        if data.is_empty() {
            return StereoSample::default();
        }

        if self.noise_mode != NoiseMode::Off {
            let white = self.rng.next_bipolar();
            let mut v = white;
            if self.noise_mode == NoiseMode::Pink {
                // -3 dB/oct approximation (Paul Kellett's three-pole filter).
                self.pink[0] = 0.99765 * self.pink[0] + white * 0.099_046;
                self.pink[1] = 0.96300 * self.pink[1] + white * 0.296_516_4;
                self.pink[2] = 0.57000 * self.pink[2] + white * 1.052_691_3;
                v = (self.pink[0] + self.pink[1] + self.pink[2] + white * 0.1848) * 0.35;
            }
            let g = input.level;
            return StereoSample { left: v * g * input.pan_l, right: v * g * input.pan_r, mono: v * g };
        }

        self.display_frame = frame;

        let ratio = input.sync_ratio.max(0.01);
        // Cap the phase rate just under one cycle per sample. Past Nyquist the
        // mip chain already returns silence, so this changes nothing audible -
        // it keeps the accumulators inside [0, 1) so the single subtract below
        // is always enough, and so the oscillator recovers when the pitch drops
        // back down instead of being stuck above 1 for good.
        let max_step = 0.9 / self.widest_detune.max(1.0);
        let mask = data.len() - 1;
        let size_f = data.len() as f32;
        let n = self.active_voices;
        let (mut sum_l, mut sum_r, mut sum_mono) = (0.0, 0.0, 0.0);

        if self.sync_mode == SyncMode::Off && self.simd && n > 4 {
            let step = (input.increment * ratio).min(max_step);
            #[cfg(target_arch = "x86_64")]
            // SAFETY: `simd` is only true when AVX2 and FMA were detected.
            unsafe {
                self.read_unison_avx2(step, input.phase_mod, data, &mut sum_l, &mut sum_r, &mut sum_mono);
            }
        } else if self.sync_mode == SyncMode::Off {
            // No sync: the read phase *is* the base phase, so only track one.
            let step = (input.increment * ratio).min(max_step);
            let pm = input.phase_mod;
            let pm_active = pm != 0.0;
            // The unison stack, written over fixed-size slices so the loop has
            // no bounds checks and the arithmetic can be vectorised.
            let phases = &mut self.read_phase[..n];
            let ratios = &self.detune_ratio[..n];
            let gains = &self.voice_gain[..n];
            let pan_l = &self.unison_pan_l[..n];
            let pan_r = &self.unison_pan_r[..n];
            for (i, phase_slot) in phases.iter_mut().enumerate() {
                let mut phase = *phase_slot + step * ratios[i];
                if phase >= 1.0 {
                    // max_step keeps the sum below 2.
                    phase -= 1.0;
                    if i == 0 {
                        self.wrapped = true;
                    }
                }
                *phase_slot = phase;
                if pm_active {
                    phase += pm;
                    phase -= phase.floor();
                }
                let s = read_frame_fast(data, mask, size_f, phase) * gains[i];
                sum_l += s * pan_l[i];
                sum_r += s * pan_r[i];
                sum_mono += s;
            }
        } else {
            for i in 0..n {
                let inc = input.increment.min(max_step) * self.detune_ratio[i];
                // read_phase wraps with floor(), so the ratio can reach 8.
                let read_inc = inc * ratio;

                self.base_phase[i] += inc;
                let mut voice_wrapped = false;
                if self.base_phase[i] >= 1.0 {
                    self.base_phase[i] -= 1.0;
                    voice_wrapped = true;
                    if i == 0 {
                        self.wrapped = true;
                    }
                }

                self.read_phase[i] += read_inc;
                if self.read_phase[i] >= 1.0 {
                    self.read_phase[i] -= self.read_phase[i].floor();
                }

                if (self.sync_mode == SyncMode::SelfSync && voice_wrapped)
                    || (self.sync_mode == SyncMode::ToOscA && input.sync_reset)
                {
                    self.read_phase[i] = 0.0;
                }

                let mut phase = self.read_phase[i] + input.phase_mod;
                phase -= phase.floor();

                let s = read_frame_fast(data, mask, size_f, phase) * self.voice_gain[i];
                sum_l += s * self.unison_pan_l[i];
                sum_r += s * self.unison_pan_r[i];
                sum_mono += s;
            }
        }

        let lvl = input.level * self.normalise;
        let g_l = input.pan_l * SQRT2;
        let g_r = input.pan_r * SQRT2;
        StereoSample { left: sum_l * lvl * g_l, right: sum_r * lvl * g_r, mono: sum_mono * lvl * self.normalise_mono }
    }
}

fn detect_simd() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
impl Oscillator {
    /// Eight unison voices per pass: phase update, wrap, 4-point Hermite
    /// gather. The unison stack is the hottest code in the synth (SPEC 9);
    /// gathers do not auto-vectorise, hence the intrinsics. Lanes past the
    /// active count have zero gain, so a partial last block is harmless.
    #[target_feature(enable = "avx2,fma")]
    unsafe fn read_unison_avx2(
        &mut self,
        step: f32,
        phase_mod: f32,
        data: &[f32],
        sum_l: &mut f32,
        sum_r: &mut f32,
        sum_mono: &mut f32,
    ) {
        use std::arch::x86_64::*;
        let mask = data.len() as i32 - 1;
        let one = _mm256_set1_ps(1.0);
        let half = _mm256_set1_ps(0.5);
        let step_v = _mm256_set1_ps(step);
        let size_v = _mm256_set1_ps(data.len() as f32);
        let pm_v = _mm256_set1_ps(phase_mod);
        let mask_v = _mm256_set1_epi32(mask);
        let pm_active = phase_mod != 0.0;

        let mut acc_l = _mm256_setzero_ps();
        let mut acc_r = _mm256_setzero_ps();
        let mut acc_mono = _mm256_setzero_ps();

        let blocks = self.active_voices.div_ceil(8);
        for block in 0..blocks {
            let o = block * 8;
            // SAFETY (all loads/stores): o + 8 <= MAX_UNISON = 16, and every
            // gather index is masked with `data.len() - 1` (a power of two).
            let mut phase = _mm256_add_ps(
                _mm256_loadu_ps(self.read_phase.as_ptr().add(o)),
                _mm256_mul_ps(step_v, _mm256_loadu_ps(self.detune_ratio.as_ptr().add(o))),
            );
            let wrap = _mm256_cmp_ps::<_CMP_GE_OQ>(phase, one);
            phase = _mm256_sub_ps(phase, _mm256_and_ps(wrap, one));
            _mm256_storeu_ps(self.read_phase.as_mut_ptr().add(o), phase);
            if block == 0 && (_mm256_movemask_ps(wrap) & 1) != 0 {
                self.wrapped = true;
            }
            if pm_active {
                phase = _mm256_add_ps(phase, pm_v);
                phase = _mm256_sub_ps(phase, _mm256_floor_ps(phase));
            }

            let pos = _mm256_mul_ps(phase, size_v);
            let index = _mm256_cvttps_epi32(pos);
            let frac = _mm256_sub_ps(pos, _mm256_cvtepi32_ps(index));
            let i0 = _mm256_and_si256(index, mask_v);
            let im1 = _mm256_and_si256(_mm256_sub_epi32(index, _mm256_set1_epi32(1)), mask_v);
            let ip1 = _mm256_and_si256(_mm256_add_epi32(index, _mm256_set1_epi32(1)), mask_v);
            let ip2 = _mm256_and_si256(_mm256_add_epi32(index, _mm256_set1_epi32(2)), mask_v);
            let base = data.as_ptr();
            let xm1 = _mm256_i32gather_ps::<4>(base, im1);
            let x0 = _mm256_i32gather_ps::<4>(base, i0);
            let x1 = _mm256_i32gather_ps::<4>(base, ip1);
            let x2 = _mm256_i32gather_ps::<4>(base, ip2);

            // Same Hermite form as the scalar path, lane by lane.
            let c = _mm256_mul_ps(_mm256_sub_ps(x1, xm1), half);
            let v = _mm256_sub_ps(x0, x1);
            let w = _mm256_add_ps(c, v);
            let a = _mm256_add_ps(_mm256_add_ps(w, v), _mm256_mul_ps(_mm256_sub_ps(x2, x0), half));
            let b_neg = _mm256_add_ps(w, a);
            let mut out = _mm256_fmadd_ps(a, frac, _mm256_sub_ps(_mm256_setzero_ps(), b_neg));
            out = _mm256_fmadd_ps(out, frac, c);
            out = _mm256_fmadd_ps(out, frac, x0);

            out = _mm256_mul_ps(out, _mm256_loadu_ps(self.voice_gain.as_ptr().add(o)));
            acc_mono = _mm256_add_ps(acc_mono, out);
            acc_l = _mm256_fmadd_ps(out, _mm256_loadu_ps(self.unison_pan_l.as_ptr().add(o)), acc_l);
            acc_r = _mm256_fmadd_ps(out, _mm256_loadu_ps(self.unison_pan_r.as_ptr().add(o)), acc_r);
        }

        let hsum = |v: __m256| {
            let s = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
            let s = _mm_hadd_ps(s, s);
            _mm_cvtss_f32(_mm_hadd_ps(s, s))
        };
        *sum_l += hsum(acc_l);
        *sum_r += hsum(acc_r);
        *sum_mono += hsum(acc_mono);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saw() -> Wavetable {
        Wavetable::make_factory(2)
    }

    #[test]
    fn single_voice_matches_the_table_read() {
        let table = saw();
        let mut osc = Oscillator::default();
        osc.prepare(48_000.0);
        osc.start(PhaseMode::Reset, 0.0);
        let f = resolve_frame(&table, 0.0, 0);
        let data = table.frame_pointer(f.frame, f.level);
        let input = OscInput { increment: 0.01, ..Default::default() };
        for k in 1..50 {
            let out = osc.process(&input, data, f.frame);
            let expected = Wavetable::read_frame(data, (k as f32 * 0.01).fract());
            assert!((out.mono - expected).abs() < 1.0e-4, "sample {k}");
        }
    }

    #[test]
    fn simd_and_scalar_unison_agree() {
        let table = saw();
        let f = resolve_frame(&table, 0.0, 0);
        let data = table.frame_pointer(f.frame, f.level);
        for n in [5, 8, 11, 16] {
            let run = |simd: bool| {
                let mut osc = Oscillator::default();
                osc.simd = simd && osc.simd;
                osc.set_unison(n, 25.0, 0.7, 0.4);
                osc.start(PhaseMode::Reset, 30.0);
                let input = OscInput { increment: 0.013, phase_mod: 0.1, ..Default::default() };
                (0..400).map(|_| osc.process(&input, data, 0)).collect::<Vec<_>>()
            };
            let (a, b) = (run(true), run(false));
            for (x, y) in a.iter().zip(&b) {
                assert!((x.left - y.left).abs() < 1.0e-4 && (x.right - y.right).abs() < 1.0e-4, "unison {n}");
            }
        }
    }

    #[test]
    fn unison_gains_never_all_zero() {
        for n in 1..=MAX_UNISON {
            let mut osc = Oscillator::default();
            osc.set_unison(n, 20.0, 0.5, 0.0);
            assert!(osc.voice_gain[..n].iter().any(|g| *g > 0.5), "unison {n}");
            assert!(osc.max_detune_ratio() >= 1.0);
        }
    }

    #[test]
    fn noise_mode_is_deterministic() {
        let table = saw();
        let data = table.frame_pointer(0, 0);
        let run = || {
            let mut osc = Oscillator::default();
            osc.seed(42);
            osc.set_noise_mode(NoiseMode::Pink);
            (0..64).map(|_| osc.process(&OscInput::default(), data, 0).mono).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
