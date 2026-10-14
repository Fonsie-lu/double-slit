//! Filter section (SPEC 3.4): TPT state-variable multimode, Moog-style ladder,
//! comb and formant, wrapped in a stereo [`MultiFilter`] with drive, mix and
//! resonance compensation. Key tracking and envelope amount are applied by the
//! voice before it hands the cutoff in.

use super::common::{clamp01, db_to_gain, fast_tanh, lerp, soft_clip, soft_limit, PI, TWO_PI};
use super::params::FilterType;

/// Topology-preserving-transform SVF (Zavalishin). All outputs available.
#[derive(Clone, Copy, Debug)]
pub struct Svf {
    g: f32,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    ic1: f32,
    ic2: f32,
}

/// Every SVF output for one input sample.
#[derive(Clone, Copy, Debug)]
pub struct SvfOutputs {
    pub lp: f32,
    pub bp: f32,
    pub hp: f32,
    pub notch: f32,
    pub peak: f32,
}

impl Default for Svf {
    fn default() -> Self {
        Self { g: 0.1, k: 2.0, a1: 1.0, a2: 0.0, a3: 0.0, ic1: 0.0, ic2: 0.0 }
    }
}

impl Svf {
    pub fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    pub fn set_coefficients(&mut self, cutoff_hz: f32, resonance: f32, sample_rate: f32) {
        let fc = cutoff_hz.clamp(10.0, sample_rate * 0.48);
        self.g = (PI * fc / sample_rate).tan();
        // k = 2 is flat, a small k means strong resonance.
        self.k = 2.0 - 1.94 * clamp01(resonance);
        self.a1 = 1.0 / (1.0 + self.g * (self.g + self.k));
        self.a2 = self.g * self.a1;
        self.a3 = self.g * self.a2;
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> SvfOutputs {
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        let lp = v2;
        let bp = v1;
        let hp = x - self.k * v1 - v2;
        SvfOutputs { lp, bp, hp, notch: x - self.k * v1, peak: lp - hp }
    }

    #[inline(always)]
    pub fn process_bp(&mut self, x: f32) -> f32 {
        self.process(x).bp
    }
}

/// Non-linear 4-pole ladder, Huovilainen-style simplification.
#[derive(Clone, Copy, Debug)]
pub struct Ladder {
    stage: [f32; 4],
    /// y[n-2]; `stage[3]` is y[n-1].
    delay_fb: f32,
    g: f32,
    feedback: f32,
}

impl Default for Ladder {
    fn default() -> Self {
        Self { stage: [0.0; 4], delay_fb: 0.0, g: 0.1, feedback: 0.0 }
    }
}

impl Ladder {
    pub fn reset(&mut self) {
        self.stage = [0.0; 4];
        self.delay_fb = 0.0;
    }

    pub fn set_coefficients(&mut self, cutoff_hz: f32, resonance: f32, sample_rate: f32) {
        let fc = cutoff_hz.clamp(10.0, sample_rate * 0.45);
        let wc = TWO_PI * fc / sample_rate;
        self.g = 1.0 - (-wc).exp();
        self.feedback = 4.0 * clamp01(resonance) * 0.95;
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        // Half-sample feedback delay keeps the loop stable at high resonance: it
        // averages the last two outputs, so y[n-1] has to be taken before the
        // stages advance. Reading stage[3] after the update made both terms the
        // same value and the averaging did nothing.
        let y1 = self.stage[3];
        let input = x - self.feedback * (self.delay_fb + y1) * 0.5;
        let mut s = fast_tanh(input);
        for st in self.stage.iter_mut() {
            *st += self.g * (s - *st);
            s = *st;
        }
        self.delay_fb = y1;
        self.stage[3]
    }
}

/// Feedback comb with fractional delay; positive or negative feedback.
#[derive(Clone, Debug)]
pub struct Comb {
    buffer: Vec<f32>,
    max_delay: usize,
    write_pos: usize,
    sr: f32,
    delay_samples: f32,
    fb: f32,
}

impl Default for Comb {
    fn default() -> Self {
        Self { buffer: Vec::new(), max_delay: 1, write_pos: 0, sr: 48_000.0, delay_samples: 100.0, fb: 0.0 }
    }
}

impl Comb {
    /// Allocates for the highest sample rate the engine can run at.
    pub fn prepare(&mut self, max_sample_rate: f32) {
        self.max_delay = (max_sample_rate / 15.0) as usize + 4;
        self.buffer.clear();
        self.buffer.resize(self.max_delay, 0.0);
        self.write_pos = 0;
        self.sr = max_sample_rate;
    }

    /// Rate change (oversampling switch) without reallocating.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
    }

    pub fn set_coefficients(&mut self, cutoff_hz: f32, resonance: f32, negative: bool) {
        self.delay_samples = (self.sr / cutoff_hz.max(20.0)).clamp(2.0, (self.max_delay as f32 - 2.0).max(2.0));
        self.fb = clamp01(resonance) * 0.97 * if negative { -1.0 } else { 1.0 };
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        if self.buffer.is_empty() {
            return x;
        }
        let max_delay = self.max_delay;
        // `delay_samples` is clamped to [2, max_delay - 2], so `read_pos` stays
        // inside [2, 2 * max_delay) and one conditional subtract replaces the division.
        let read_pos = self.write_pos as f32 - self.delay_samples + max_delay as f32;
        let mut i0 = read_pos as usize;
        if i0 >= max_delay {
            i0 -= max_delay;
        }
        let mut i1 = i0 + 1;
        if i1 >= max_delay {
            i1 = 0;
        }
        let frac = read_pos - read_pos.floor();
        let delayed = lerp(self.buffer[i0], self.buffer[i1], frac);
        let out = x + self.fb * delayed;
        self.buffer[self.write_pos] = soft_limit(out, 4.0);
        self.write_pos += 1;
        if self.write_pos >= max_delay {
            self.write_pos = 0;
        }
        out
    }
}

/// A, E, I, O, U formant centres.
const VOWEL_F: [[f32; 3]; 5] = [
    [730.0, 1090.0, 2440.0],
    [530.0, 1840.0, 2480.0],
    [270.0, 2290.0, 3010.0],
    [570.0, 840.0, 2410.0],
    [300.0, 870.0, 2240.0],
];
/// A, E, I, O, U relative formant gains.
const VOWEL_G: [[f32; 3]; 5] = [
    [1.0, 0.5, 0.25],
    [1.0, 0.45, 0.2],
    [1.0, 0.35, 0.15],
    [1.0, 0.6, 0.15],
    [1.0, 0.5, 0.1],
];

/// Position in the vowel table: the two neighbouring vowels and the blend.
fn vowel_pos(vowel: f32) -> (usize, usize, f32) {
    let pos = clamp01(vowel) * 4.0;
    let v0 = (pos as usize).min(4);
    let v1 = (v0 + 1).min(4);
    (v0, v1, pos - v0 as f32)
}

/// Three parallel band-passes morphing through the A-E-I-O-U vowel table.
#[derive(Clone, Copy, Debug)]
pub struct Formant {
    bands: [Svf; 3],
    gains: [f32; 3],
}

impl Default for Formant {
    fn default() -> Self {
        Self { bands: [Svf::default(); 3], gains: [1.0, 0.5, 0.25] }
    }
}

impl Formant {
    pub fn reset(&mut self) {
        for b in self.bands.iter_mut() {
            b.reset();
        }
    }

    pub fn set_coefficients(&mut self, cutoff_hz: f32, resonance: f32, vowel: f32, sample_rate: f32) {
        let shift = (cutoff_hz / 1000.0).clamp(0.15, 4.0);
        let (v0, v1, t) = vowel_pos(vowel);
        for i in 0..3 {
            let f = lerp(VOWEL_F[v0][i], VOWEL_F[v1][i], t) * shift;
            self.gains[i] = lerp(VOWEL_G[v0][i], VOWEL_G[v1][i], t);
            self.bands[i].set_coefficients(f, (0.55 + 0.4 * clamp01(resonance)).clamp(0.0, 0.98), sample_rate);
        }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let mut out = 0.0;
        for (band, gain) in self.bands.iter_mut().zip(self.gains.iter()) {
            out += gain * band.process_bp(x);
        }
        out * 1.6
    }
}

#[derive(Clone, Debug, Default)]
struct Channel {
    svf1: Svf,
    svf2: Svf,
    ladder: Ladder,
    comb: Comb,
    formant: Formant,
}

impl Channel {
    fn reset(&mut self) {
        self.svf1.reset();
        self.svf2.reset();
        self.ladder.reset();
        self.comb.reset();
        self.formant.reset();
    }
}

/// Stereo filter block with everything the FILTER tab exposes.
#[derive(Clone, Debug)]
pub struct MultiFilter {
    channels: [Channel; 2],
    filter_type: FilterType,
    sr: f32,
    drive_gain: f32,
    wet: f32,
    makeup: f32,
    res_comp: f32,
    last_cutoff: f32,
    last_res: f32,
    last_drive: f32,
    last_mix: f32,
    last_vowel: f32,
    coefficients_valid: bool,
}

impl Default for MultiFilter {
    fn default() -> Self {
        Self {
            channels: [Channel::default(), Channel::default()],
            filter_type: FilterType::Lp24,
            sr: 48_000.0,
            drive_gain: 1.0,
            wet: 1.0,
            makeup: 1.0,
            res_comp: 1.0,
            last_cutoff: -1.0,
            last_res: -1.0,
            last_drive: -1.0,
            last_mix: -1.0,
            last_vowel: -1.0,
            coefficients_valid: false,
        }
    }
}

impl MultiFilter {
    /// Allocates for `max_sample_rate` (base rate x max oversampling).
    pub fn prepare(&mut self, max_sample_rate: f64) {
        self.sr = max_sample_rate as f32;
        for ch in self.channels.iter_mut() {
            ch.comb.prepare(self.sr);
        }
        self.reset();
    }

    /// Switches the working rate without allocating.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.coefficients_valid = false;
        self.sr = sample_rate as f32;
        for ch in self.channels.iter_mut() {
            ch.comb.set_sample_rate(self.sr);
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        for ch in self.channels.iter_mut() {
            ch.reset();
        }
    }

    pub fn set_params(&mut self, filter_type: FilterType, cutoff_hz: f32, resonance: f32, drive: f32, mix: f32, vowel: f32) {
        // Coefficient maths (tan/exp) is skipped when nothing changed, which
        // matters for hosts running very small buffers. Bit-exact comparison is
        // deliberate: this is a cache check.
        if self.coefficients_valid
            && filter_type == self.filter_type
            && cutoff_hz == self.last_cutoff
            && resonance == self.last_res
            && drive == self.last_drive
            && mix == self.last_mix
            && vowel == self.last_vowel
        {
            return;
        }
        self.coefficients_valid = true;
        self.last_cutoff = cutoff_hz;
        self.last_res = resonance;
        self.last_drive = drive;
        self.last_mix = mix;
        self.last_vowel = vowel;
        self.filter_type = filter_type;

        let sr = self.sr;
        let fc = cutoff_hz.clamp(20.0, sr * 0.48);
        let res = clamp01(resonance);
        self.drive_gain = db_to_gain(clamp01(drive) * 24.0);
        self.makeup = 1.0 / self.drive_gain.sqrt();
        self.wet = clamp01(mix);
        // Resonance raises the peak gain a lot; trim the input so a fully
        // resonant filter stays in a usable range instead of clipping the VCA.
        self.res_comp = 1.0 / (1.0 + 3.0 * res * res);

        for ch in self.channels.iter_mut() {
            match filter_type {
                FilterType::LadderLp => ch.ladder.set_coefficients(fc, res, sr),
                FilterType::CombPos | FilterType::CombNeg => {
                    ch.comb.set_coefficients(fc, res, filter_type == FilterType::CombNeg)
                }
                FilterType::Formant => ch.formant.set_coefficients(fc, res, vowel, sr),
                _ => {
                    // 24 dB modes cascade two stages but keep a single resonance peak.
                    ch.svf1.set_coefficients(fc, res, sr);
                    let second = if matches!(filter_type, FilterType::Lp24 | FilterType::Hp24) { 0.0 } else { res };
                    ch.svf2.set_coefficients(fc, second, sr);
                }
            }
        }
    }

    /// Processes one stereo frame in place.
    #[inline(always)]
    pub fn process(&mut self, left: &mut f32, right: &mut f32) {
        *left = self.process_channel(0, *left);
        *right = self.process_channel(1, *right);
    }

    #[inline(always)]
    fn process_channel(&mut self, channel: usize, x: f32) -> f32 {
        let shaped = if self.drive_gain > 1.0001 { soft_clip(x * self.drive_gain) * self.makeup } else { x };
        let driven = shaped * self.res_comp;
        let ch = &mut self.channels[channel];
        let mut out = match self.filter_type {
            FilterType::Lp12 => ch.svf1.process(driven).lp,
            FilterType::Lp24 => {
                let a = ch.svf1.process(driven).lp;
                ch.svf2.process(a).lp
            }
            FilterType::Hp12 => ch.svf1.process(driven).hp,
            FilterType::Hp24 => {
                let a = ch.svf1.process(driven).hp;
                ch.svf2.process(a).hp
            }
            FilterType::Bp12 => ch.svf1.process(driven).bp,
            FilterType::Notch => ch.svf1.process(driven).notch,
            FilterType::Peak => ch.svf1.process(driven).peak,
            FilterType::LadderLp => ch.ladder.process(driven),
            FilterType::CombPos | FilterType::CombNeg => ch.comb.process(driven) * 0.5,
            FilterType::Formant => ch.formant.process(driven),
        };

        if !out.is_finite() {
            self.reset();
            out = 0.0;
        }
        lerp(x, out, self.wet)
    }

    /// Magnitude response for the GUI curve (analog prototype, display only).
    pub fn magnitude_response(filter_type: FilterType, cutoff_hz: f32, resonance: f32, freq_hz: f32, vowel: f32) -> f32 {
        let fc = cutoff_hz.max(10.0);
        let w = freq_hz.max(1.0) / fc;
        let q = (0.5 + 9.5 * clamp01(resonance)).max(0.5);
        let w2 = w * w;
        let denom = ((1.0 - w2) * (1.0 - w2) + (w / q) * (w / q)).sqrt();

        match filter_type {
            FilterType::Lp12 => 1.0 / denom,
            FilterType::Lp24 => 1.0 / (denom * denom),
            FilterType::Hp12 => w2 / denom,
            FilterType::Hp24 => (w2 * w2) / (denom * denom),
            FilterType::Bp12 => (w / q) / denom,
            FilterType::Notch => (1.0 - w2).abs() / denom,
            FilterType::Peak => (1.0 - w2 - w2).abs() / denom,
            FilterType::LadderLp => {
                let lp1 = 1.0 / (1.0 + w2).sqrt();
                let four_pole = lp1 * lp1 * lp1 * lp1;
                let k = 4.0 * clamp01(resonance) * 0.95;
                // Closed-loop magnitude of the 4-pole loop with feedback k.
                four_pole / (1.0 + k * four_pole * if w < 1.0 { -1.0 } else { 1.0 }).abs().max(0.05)
            }
            FilterType::CombPos | FilterType::CombNeg => {
                let phase = PI * freq_hz / fc;
                let fb = clamp01(resonance) * 0.97 * if filter_type == FilterType::CombNeg { -1.0 } else { 1.0 };
                let re = 1.0 + fb * (2.0 * phase).cos();
                let im = fb * (2.0 * phase).sin();
                0.5 * (re * re + im * im).sqrt()
            }
            FilterType::Formant => {
                let shift = (fc / 1000.0).clamp(0.15, 4.0);
                let (v0, v1, blend) = vowel_pos(vowel);
                let mut mag = 0.0;
                for i in 0..3 {
                    let centre = lerp(VOWEL_F[v0][i], VOWEL_F[v1][i], blend) * shift;
                    let bw = centre / (2.0 + 6.0 * clamp01(resonance));
                    let d = (freq_hz - centre) / bw.max(10.0);
                    mag += lerp(VOWEL_G[v0][i], VOWEL_G[v1][i], blend) / (1.0 + d * d).sqrt();
                }
                mag
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::common::Rng;

    fn run_noise(filter: &mut MultiFilter, samples: usize, amplitude: f32) -> f32 {
        let mut rng = Rng::new(12345);
        let mut peak = 0.0_f32;
        for _ in 0..samples {
            let mut l = rng.next_bipolar() * amplitude;
            let mut r = rng.next_bipolar() * amplitude;
            filter.process(&mut l, &mut r);
            if !l.is_finite() || !r.is_finite() {
                return f32::INFINITY;
            }
            peak = peak.max(l.abs().max(r.abs()));
        }
        peak
    }

    fn sine_peak(filter: &mut MultiFilter, freq: f32, samples: usize, settle: usize) -> f32 {
        let mut peak = 0.0_f32;
        for i in 0..samples {
            let x = (TWO_PI * freq * i as f32 / 48_000.0).sin();
            let (mut l, mut r) = (x, x);
            filter.process(&mut l, &mut r);
            if i > settle {
                peak = peak.max(l.abs());
            }
        }
        peak
    }

    #[test]
    fn all_types_stay_stable_at_max_resonance() {
        for t in FilterType::ALL {
            let mut filter = MultiFilter::default();
            filter.prepare(48_000.0);
            // Nyquist/2 cutoff with full resonance and drive is the worst case.
            filter.set_params(t, 12_000.0, 1.0, 1.0, 1.0, 0.5);
            let peak = run_noise(&mut filter, 48_000, 1.0);
            assert!(peak.is_finite(), "{t:?}");
            assert!(peak < 60.0, "{t:?} peak {peak}");
        }
    }

    #[test]
    fn svf_stable_at_extreme_cutoffs() {
        for cutoff in [20.0, 200.0, 12_000.0, 23_000.0] {
            let mut filter = MultiFilter::default();
            filter.prepare(48_000.0);
            filter.set_params(FilterType::Lp24, cutoff, 0.99, 0.0, 1.0, 0.0);
            assert!(run_noise(&mut filter, 24_000, 1.0).is_finite(), "{cutoff}");
        }
    }

    #[test]
    fn low_pass_attenuates_high_frequencies() {
        let mut filter = MultiFilter::default();
        filter.prepare(48_000.0);
        filter.set_params(FilterType::Lp24, 500.0, 0.0, 0.0, 1.0, 0.0);
        assert!(sine_peak(&mut filter, 8000.0, 4800, 2400) < 0.05);
    }

    #[test]
    fn low_pass_passes_low_frequencies() {
        let mut filter = MultiFilter::default();
        filter.prepare(48_000.0);
        filter.set_params(FilterType::Lp24, 8000.0, 0.0, 0.0, 1.0, 0.0);
        assert!(sine_peak(&mut filter, 200.0, 4800, 480) > 0.85);
    }

    #[test]
    fn high_pass_removes_dc() {
        let mut filter = MultiFilter::default();
        filter.prepare(48_000.0);
        filter.set_params(FilterType::Hp24, 1000.0, 0.0, 0.0, 1.0, 0.0);
        let mut last = 0.0;
        for _ in 0..48_000 {
            let (mut l, mut r) = (1.0, 1.0);
            filter.process(&mut l, &mut r);
            last = l;
        }
        assert!(last.abs() < 0.01, "{last}");
    }

    #[test]
    fn mix_blends_dry_and_wet() {
        let mut filter = MultiFilter::default();
        filter.prepare(48_000.0);
        filter.set_params(FilterType::Lp12, 100.0, 0.0, 0.0, 0.0, 0.0);
        let (mut l, mut r) = (0.5, -0.25);
        filter.process(&mut l, &mut r);
        // mix = 0 is fully dry.
        assert!((l - 0.5).abs() < 1.0e-6);
        assert!((r + 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn magnitude_response_shapes_match_types() {
        let m = MultiFilter::magnitude_response;
        assert!(m(FilterType::Lp24, 1000.0, 0.1, 100.0, 0.0) > m(FilterType::Lp24, 1000.0, 0.1, 8000.0, 0.0));
        assert!(m(FilterType::Hp24, 1000.0, 0.1, 100.0, 0.0) < m(FilterType::Hp24, 1000.0, 0.1, 8000.0, 0.0));
        assert!(m(FilterType::Bp12, 1000.0, 0.1, 1000.0, 0.0) > m(FilterType::Bp12, 1000.0, 0.1, 100.0, 0.0));
        assert!(m(FilterType::Notch, 1000.0, 0.1, 1000.0, 0.0) < 0.2);
        for t in FilterType::ALL {
            for f in [20.0, 500.0, 5000.0, 20_000.0] {
                assert!(m(t, 1000.0, 0.9, f, 0.0).is_finite(), "{t:?} {f}");
            }
        }
    }

    #[test]
    fn magnitude_lowpass_attenuates_and_resonance_peaks_near_cutoff() {
        let m = MultiFilter::magnitude_response;
        for t in [FilterType::Lp12, FilterType::Lp24, FilterType::LadderLp] {
            assert!(m(t, 1000.0, 0.0, 10_000.0, 0.0) < 0.1 * m(t, 1000.0, 0.0, 50.0, 0.0), "{t:?}");
        }
        for t in [FilterType::Lp12, FilterType::Lp24] {
            let flat = m(t, 1000.0, 0.0, 1000.0, 0.0);
            let resonant = m(t, 1000.0, 0.9, 1000.0, 0.0);
            assert!(resonant > 2.0 * flat, "{t:?}: {flat} -> {resonant}");
            // The peak sits at the cutoff, not an octave away.
            assert!(resonant > m(t, 1000.0, 0.9, 500.0, 0.0));
            assert!(resonant > m(t, 1000.0, 0.9, 2000.0, 0.0));
        }
    }

    #[test]
    fn sample_rate_switch_does_not_break_stability() {
        let mut filter = MultiFilter::default();
        // Allocate for the highest rate.
        filter.prepare(192_000.0);
        for rate in [48_000.0, 96_000.0, 192_000.0] {
            filter.set_sample_rate(rate);
            filter.set_params(FilterType::CombPos, 80.0, 0.95, 0.5, 1.0, 0.0);
            assert!(run_noise(&mut filter, 8000, 1.0).is_finite(), "{rate}");
        }
    }
}
