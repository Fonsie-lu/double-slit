//! Wavetable storage, mip-map band-limiting, factory tables and file I/O
//! (SPEC 3.3). Mip generation and file I/O run on the message thread; only
//! [`Wavetable::frame_pointer`], [`Wavetable::read_frame`],
//! [`Wavetable::sample`] and [`Wavetable::mip_for_increment`] are called from
//! the audio thread, and none of them allocate.

use std::sync::OnceLock;

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::dsp::common::{hermite, lerp, Rng, FRAME_SIZE, MAX_FRAMES, MIP_LEVELS, PI, TWO_PI};
use crate::dsp::fft::Fft;
use crate::dsp::params::{factory_table_choice, NUM_TABLE_CHOICES, TABLE_NAMES};

/// Names of the factory tables plus the trailing "Custom" choice.
pub fn factory_table_choices() -> &'static [&'static str] {
    TABLE_NAMES
}

pub fn factory_table_name(index: usize) -> &'static str {
    TABLE_NAMES[index.min(NUM_TABLE_CHOICES - 1)]
}

/// One shared FFT per mip size. An [`Fft`] is immutable once built, so every
/// caller and thread can share one instead of building a fresh plan per frame
/// per mip level. Level 0 is [`FRAME_SIZE`].
fn mip_fft(level: usize) -> &'static Fft {
    static PLANS: OnceLock<Vec<Fft>> = OnceLock::new();
    let plans = PLANS.get_or_init(|| (0..MIP_LEVELS).map(|l| Fft::new(FRAME_SIZE >> l)).collect());
    &plans[level.min(MIP_LEVELS - 1)]
}

/// Builds one cycle from a harmonic spectrum via inverse FFT. `amps[h - 1]` and
/// `phases[h - 1]` belong to harmonic `h`; empty `phases` means all zero.
fn spectrum_to_frame(amps: &[f32], phases: &[f32]) -> Vec<f32> {
    let mut re = vec![0.0; FRAME_SIZE];
    let mut im = vec![0.0; FRAME_SIZE];
    let max_h = amps.len().min(FRAME_SIZE / 2 - 1);
    for h in 1..=max_h {
        let a = amps[h - 1];
        if a == 0.0 {
            continue;
        }
        let ph = if phases.is_empty() { 0.0 } else { phases[h - 1] };
        // Real signal: X[h] = A/2 * e^{-i(phi + pi/2)} so the partial comes out as a sine.
        let mag = a * 0.5 * FRAME_SIZE as f32;
        let cr = mag * ph.sin();
        let ci = -mag * ph.cos();
        re[h] = cr;
        im[h] = ci;
        re[FRAME_SIZE - h] = cr;
        im[FRAME_SIZE - h] = -ci;
    }
    mip_fft(0).inverse(&mut re, &mut im);
    re
}

/// Loudness target for a factory frame, chosen as roughly the RMS of a
/// peak-normalised saw so the classic shapes keep the level they always had.
const FRAME_RMS_TARGET: f32 = 0.5;
/// Frames may overshoot 1.0 to hit that target, but only this far: a genuinely
/// spiky shape (a 4 % pulse) should still read as quieter than a saw.
const FRAME_PEAK_CEILING: f32 = 1.8;

/// Removes DC, then matches loudness rather than peaks. Peak normalisation left
/// impulse-like frames far quieter than the classic shapes - Digi Harmonics
/// measured 13 dB under Saw and 18 dB under Square - which reads as the table
/// having no sound at all when you switch to it.
fn normalise_frame(f: &mut [f32]) {
    let mean = f.iter().map(|&v| v as f64).sum::<f64>() / f.len() as f64;
    let mut sum_squares = 0.0_f64;
    let mut peak = 0.0_f32;
    for v in f.iter_mut() {
        *v -= mean as f32;
        sum_squares += *v as f64 * *v as f64;
        peak = peak.max(v.abs());
    }
    if peak <= 1.0e-6 {
        return;
    }
    let rms = (sum_squares / f.len() as f64).sqrt() as f32;
    let mut gain = if rms > 1.0e-6 { FRAME_RMS_TARGET / rms } else { 1.0 / peak };
    if peak * gain > FRAME_PEAK_CEILING {
        gain = FRAME_PEAK_CEILING / peak;
    }
    for v in f.iter_mut() {
        *v *= gain;
    }
}

/// Schroeder-style quadratic phase dispersion. Partials at zero phase all line
/// up at p = 0, so a dense spectrum collapses into an impulse; spreading them
/// keeps the same spectrum with a usable crest factor. Only used for the
/// digital tables, whose shape has no canonical form - the analog ones are
/// defined by their waveform and keep the phases that produce it.
fn dispersed_phase(h: i32, num_harmonics: i32) -> f32 {
    let n = num_harmonics.max(1) as f32;
    -PI * h as f32 * h as f32 / n
}

/// Harmonic `h` of a rectangular pulse of duty `d`. The phase term matters:
/// without it the partials sum to an impulse rather than a pulse, which is what
/// left Pulse 25 % and Analog PWM around 8 dB below the other tables.
fn pulse_amp(h: i32, d: f32) -> f32 {
    (PI * h as f32 * d).sin() / h as f32
}

/// Harmonic `h` of a triangle: odd harmonics, 1/h², alternating sign.
fn triangle_amp(h: i32) -> f32 {
    if h % 2 == 1 {
        let sign = if ((h - 1) / 2) % 2 == 0 { 1.0 } else { -1.0 };
        sign / (h * h) as f32
    } else {
        0.0
    }
}

fn pulse_phase(h: i32, d: f32) -> f32 {
    PI * 0.5 - PI * h as f32 * d
}

fn harmonic_frame(num_harmonics: i32, amp_fn: impl Fn(i32) -> f32, phase_fn: Option<&dyn Fn(i32) -> f32>) -> Vec<f32> {
    let amps: Vec<f32> = (1..=num_harmonics).map(&amp_fn).collect();
    let phases: Vec<f32> = match phase_fn {
        Some(p) => (1..=num_harmonics).map(p).collect(),
        None => Vec::new(),
    };
    let mut frame = spectrum_to_frame(&amps, &phases);
    normalise_frame(&mut frame);
    frame
}

fn time_frame(f: impl Fn(f32) -> f32) -> Vec<f32> {
    let mut frame: Vec<f32> = (0..FRAME_SIZE).map(|i| f(i as f32 / FRAME_SIZE as f32)).collect();
    normalise_frame(&mut frame);
    frame
}

/// Vowel formant centres in Hz (applied to harmonics of a nominal 110 Hz
/// fundamental) and their gains.
struct Formants {
    f: [f32; 3],
    g: [f32; 3],
}

const VOWELS: [Formants; 5] = [
    Formants { f: [730.0, 1090.0, 2440.0], g: [1.0, 0.5, 0.2] },   // A
    Formants { f: [530.0, 1840.0, 2480.0], g: [1.0, 0.45, 0.2] },  // E
    Formants { f: [270.0, 2290.0, 3010.0], g: [1.0, 0.35, 0.15] }, // I
    Formants { f: [570.0, 840.0, 2410.0], g: [1.0, 0.6, 0.15] },   // O
    Formants { f: [300.0, 870.0, 2240.0], g: [1.0, 0.5, 0.1] },    // U
];

fn formant_amp(v: &Formants, harmonic_hz: f32) -> f32 {
    (0..3)
        .map(|i| {
            let bw = 90.0 + 40.0 * i as f32;
            let d = (harmonic_hz - v.f[i]) / bw;
            v.g[i] * (-0.5 * d * d).exp()
        })
        .sum()
}

/// Resamples an arbitrary-length single cycle to `out_size` samples using
/// Hermite interpolation (wrapping around the cycle).
pub fn resample_cycle(input: &[f32], out_size: usize) -> Vec<f32> {
    let mut out = vec![0.0; out_size];
    if input.is_empty() {
        return out;
    }
    let n = input.len() as i64;
    let at = |k: i64| input[k.rem_euclid(n) as usize];
    for (i, o) in out.iter_mut().enumerate() {
        let pos = i as f32 * n as f32 / out_size as f32;
        let i0 = pos as i64;
        let frac = pos - i0 as f32;
        *o = hermite(at(i0 - 1), at(i0), at(i0 + 1), at(i0 + 2), frac);
    }
    out
}

fn to_frame_size(samples: &[f32]) -> Vec<f32> {
    if samples.len() == FRAME_SIZE {
        samples.to_vec()
    } else {
        resample_cycle(samples, FRAME_SIZE)
    }
}

/// A table of up to [`MAX_FRAMES`] single-cycle frames of [`FRAME_SIZE`]
/// samples, each with an 11-level FFT-built mip chain so oscillators never need
/// oversampling for anti-aliasing.
#[derive(Clone, Debug)]
pub struct Wavetable {
    factory: Option<usize>,
    table_name: String,
    frames: usize,
    /// `frames * FRAME_SIZE` base (unfiltered) samples.
    base: Vec<f32>,
    /// All mip levels back to back.
    mip_storage: Vec<f32>,
    /// Start of each level in `mip_storage`.
    mip_offset: [usize; MIP_LEVELS],
    hash: u64,
}

impl Default for Wavetable {
    /// A single sine frame named "Sine". Not marked as a factory table.
    fn default() -> Self {
        let base = (0..FRAME_SIZE)
            .map(|i| (TWO_PI * i as f32 / FRAME_SIZE as f32).sin())
            .collect();
        let mut t = Self {
            factory: None,
            table_name: "Sine".to_owned(),
            frames: 1,
            base,
            mip_storage: Vec::new(),
            mip_offset: [0; MIP_LEVELS],
            hash: 0,
        };
        t.generate_mips();
        t.update_hash();
        t
    }
}

impl Wavetable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the table content; frames are resampled to [`FRAME_SIZE`] as
    /// needed. Regenerates the mip chain and the content hash.
    pub fn set_frames(&mut self, new_frames: &[Vec<f32>], name: &str) {
        self.table_name = name.to_owned();
        self.factory = None;
        let count = new_frames.len().clamp(1, MAX_FRAMES);
        self.frames = count;
        self.base = vec![0.0; count * FRAME_SIZE];
        for (f, src) in new_frames.iter().take(count).enumerate() {
            let resampled = to_frame_size(src);
            self.base[f * FRAME_SIZE..(f + 1) * FRAME_SIZE].copy_from_slice(&resampled);
        }
        self.generate_mips();
        self.update_hash();
    }

    /// Single-frame convenience used by the editor. Out-of-range indices are ignored.
    pub fn set_frame(&mut self, frame_index: usize, samples: &[f32]) {
        if frame_index >= self.frames {
            return;
        }
        let resampled = to_frame_size(samples);
        self.base[frame_index * FRAME_SIZE..(frame_index + 1) * FRAME_SIZE].copy_from_slice(&resampled);
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    pub fn add_frame(&mut self, samples: &[f32]) {
        if self.frames >= MAX_FRAMES {
            return;
        }
        self.base.extend_from_slice(&to_frame_size(samples));
        self.frames += 1;
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    pub fn duplicate_frame(&mut self, frame_index: usize) {
        if self.frames >= MAX_FRAMES || frame_index >= self.frames {
            return;
        }
        let copy = self.frame_copy(frame_index);
        let at = (frame_index + 1) * FRAME_SIZE;
        self.base.splice(at..at, copy);
        self.frames += 1;
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    /// Removes a frame; the last remaining frame is protected.
    pub fn remove_frame(&mut self, frame_index: usize) {
        if self.frames <= 1 || frame_index >= self.frames {
            return;
        }
        self.base.drain(frame_index * FRAME_SIZE..(frame_index + 1) * FRAME_SIZE);
        self.frames -= 1;
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    pub fn move_frame(&mut self, from: usize, to: usize) {
        if from >= self.frames || to >= self.frames || from == to {
            return;
        }
        let moved: Vec<f32> = self.base.drain(from * FRAME_SIZE..(from + 1) * FRAME_SIZE).collect();
        self.base.splice(to * FRAME_SIZE..to * FRAME_SIZE, moved);
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    /// Recomputes mips + hash after in-place edits via [`Self::mutable_frame`].
    pub fn refresh(&mut self) {
        self.factory = None;
        self.generate_mips();
        self.update_hash();
    }

    pub fn num_frames(&self) -> usize {
        self.frames
    }

    pub fn name(&self) -> &str {
        &self.table_name
    }

    pub fn set_name(&mut self, name: &str) {
        self.table_name = name.to_owned();
        self.factory = None;
        self.update_hash();
    }

    /// FNV-1a over frame count, samples and name. The patch pool is keyed on it.
    pub fn content_hash(&self) -> u64 {
        self.hash
    }

    /// Index into the factory list while this is still an untouched factory
    /// table, `None` once it has been edited, imported or loaded from a patch.
    /// Lets a patch store the index instead of embedding ~180 kB of samples.
    pub fn factory_index(&self) -> Option<usize> {
        self.factory
    }

    /// Base (unfiltered) frame data, [`FRAME_SIZE`] samples. The index is clamped.
    pub fn frame_data(&self, frame_index: usize) -> &[f32] {
        let f = frame_index.min(self.frames - 1);
        &self.base[f * FRAME_SIZE..(f + 1) * FRAME_SIZE]
    }

    pub fn frame_copy(&self, frame_index: usize) -> Vec<f32> {
        self.frame_data(frame_index).to_vec()
    }

    /// Mutable base frame; call [`Self::refresh`] after editing.
    pub fn mutable_frame(&mut self, frame_index: usize) -> &mut [f32] {
        let f = frame_index.min(self.frames - 1);
        &mut self.base[f * FRAME_SIZE..(f + 1) * FRAME_SIZE]
    }

    /// Mip level for a phase increment in cycles/sample; a higher level has
    /// fewer harmonics.
    pub fn mip_for_increment(&self, increment: f32) -> usize {
        let inc = increment.abs().max(1.0e-7);
        // Highest harmonic that stays below Nyquist.
        let allowed = 0.5 / inc;
        for level in 0..MIP_LEVELS {
            let max_harmonic = ((FRAME_SIZE >> level) / 2) - 1;
            if max_harmonic as f32 <= allowed {
                return level;
            }
        }
        MIP_LEVELS - 1
    }

    /// One frame at one mip level, `FRAME_SIZE >> level` samples. Hot paths
    /// fetch this once per block and then read through [`Self::read_frame`].
    /// Frame and level are clamped.
    #[inline]
    pub fn frame_pointer(&self, frame_index: usize, level: usize) -> &[f32] {
        let lvl = level.min(MIP_LEVELS - 1);
        let size = FRAME_SIZE >> lvl;
        let start = self.mip_offset[lvl] + frame_index.min(self.frames - 1) * size;
        &self.mip_storage[start..start + size]
    }

    /// Hermite read from a frame slice (power-of-two length) at `phase` in
    /// cycles, wrapping around the cycle.
    #[inline(always)]
    pub fn read_frame(data: &[f32], phase: f32) -> f32 {
        let size = data.len();
        let mask = size - 1;
        let pos = phase * size as f32;
        // Truncation then masking, as two's complement, so slightly negative
        // phases still wrap correctly.
        let i0 = (pos as i32 as usize) & mask;
        let frac = pos - pos.floor();
        hermite(
            data[i0.wrapping_sub(1) & mask],
            data[i0],
            data[(i0 + 1) & mask],
            data[(i0 + 2) & mask],
            frac,
        )
    }

    /// Convenience read (tests, GUI).
    #[inline]
    pub fn sample(&self, frame_index: usize, level: usize, phase: f32) -> f32 {
        Self::read_frame(self.frame_pointer(frame_index, level), phase)
    }

    /// Peak-normalised copy of a frame for display, `points` (at least 2) long.
    pub fn display_frame(&self, frame_index: usize, points: usize) -> Vec<f32> {
        let n = points.max(2);
        let mut out = vec![0.0; n];
        let src = self.frame_data(frame_index);
        let peak = src.iter().fold(1.0e-6_f32, |m, v| m.max(v.abs()));

        // One bucket per output point, keeping the sample that reaches furthest
        // from zero. Sampling every (FRAME_SIZE / n)th sample instead simply
        // dropped the rest, so anything with real harmonic content aliased into
        // a shape that had nothing to do with what you were hearing.
        for (i, o) in out.iter_mut().enumerate() {
            let from = i * FRAME_SIZE / n;
            let to = (from + 1).max((i + 1) * FRAME_SIZE / n);
            let mut extreme = 0.0_f32;
            for &v in &src[from.min(FRAME_SIZE)..to.min(FRAME_SIZE)] {
                if v.abs() >= extreme.abs() {
                    extreme = v;
                }
            }
            *o = extreme / peak;
        }
        out
    }

    // ---- Factory / file I/O ------------------------------------------------

    /// Builds the factory table of table choice `factory_index` (Custom and
    /// out-of-range values clamp as in [`factory_table_choice`]).
    pub fn make_factory(factory_index: usize) -> Wavetable {
        let index = factory_table_choice(factory_index);
        let morph_frames = 16;
        let morph_t = |f: usize| f as f32 / (morph_frames - 1) as f32;
        let mut frames: Vec<Vec<f32>> = Vec::new();

        match index {
            // Sine
            0 => frames.push(harmonic_frame(1, |_| 1.0, None)),
            // Triangle
            1 => frames.push(harmonic_frame(256, triangle_amp, None)),
            // Saw
            2 => frames.push(harmonic_frame(512, |h| 1.0 / h as f32, None)),
            // Square
            3 => frames.push(harmonic_frame(512, |h| if h % 2 == 1 { 1.0 / h as f32 } else { 0.0 }, None)),
            // Pulse 25%
            4 => frames.push(harmonic_frame(512, |h| pulse_amp(h, 0.25), Some(&|h| pulse_phase(h, 0.25)))),
            // Analog Saws: saw with a progressively softer harmonic roll-off
            5 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    frames.push(harmonic_frame(
                        512,
                        |h| {
                            let rolloff = (-(h as f32) * t * 0.05).exp();
                            rolloff / h as f32
                        },
                        None,
                    ));
                }
            }
            // Analog PWM: pulse width sweep
            6 => {
                for f in 0..morph_frames {
                    let w = lerp(0.5, 0.04, morph_t(f));
                    frames.push(harmonic_frame(512, |h| pulse_amp(h, w), Some(&|h| pulse_phase(h, w))));
                }
            }
            // Analog Organ: drawbar mix morph
            7 => {
                const BARS: [i32; 6] = [1, 2, 3, 4, 6, 8];
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    frames.push(harmonic_frame(
                        32,
                        |h| match BARS.iter().position(|&b| b == h) {
                            Some(b) => {
                                let centre = t * 5.0;
                                let d = (b as f32 - centre) / 2.0;
                                (-d * d).exp()
                            }
                            None => 0.0,
                        },
                        None,
                    ));
                }
            }
            // Digi Harmonics: sweeping comb notch over a 1/h source
            8 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    frames.push(harmonic_frame(
                        256,
                        |h| {
                            let comb = (PI * h as f32 * (0.05 + t * 0.45)).sin().abs();
                            // A floor under the notch and a 1/h source keep the
                            // fundamental alive: the bare comb could zero h = 1,
                            // and the table then faded to nothing up the keyboard
                            // as the mip chain removed everything above it.
                            (0.25 + 0.75 * comb) / h as f32
                        },
                        Some(&|h| dispersed_phase(h, 256)),
                    ));
                }
            }
            // Digi Bells: sparse quasi-inharmonic partial set
            9 => {
                const PARTIALS: [i32; 7] = [1, 2, 3, 5, 7, 11, 13];
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    frames.push(harmonic_frame(
                        64,
                        |h| match PARTIALS.iter().position(|&p| p == h) {
                            Some(p) => (0.6 + 0.4 * t).powf(p as f32) / (1.0 + 0.3 * p as f32),
                            None => 0.0,
                        },
                        Some(&|h| dispersed_phase(h, 64)),
                    ));
                }
            }
            // Digi Growl: moving formant peak
            10 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    frames.push(harmonic_frame(
                        256,
                        |h| {
                            let centre = 2.0 + t * 40.0;
                            let d = (h as f32 - centre) / (4.0 + t * 12.0);
                            (1.0 / h as f32) + 0.8 * (-0.5 * d * d).exp()
                        },
                        Some(&|h| dispersed_phase(h, 256)),
                    ));
                }
            }
            // Digi Wavefold: progressive sine folding
            11 => {
                for f in 0..morph_frames {
                    let drive = 1.0 + f as f32 * 0.9;
                    frames.push(time_frame(|p| {
                        (TWO_PI * p * 1.0).sin() * drive - 2.0 * ((TWO_PI * p).sin() * drive * 0.5 + 0.5).floor()
                    }));
                }
            }
            // Digi Sync: hard-synced saw copies
            12 => {
                for f in 0..morph_frames {
                    let ratio = 1.0 + f as f32 * 0.25;
                    frames.push(time_frame(|p| {
                        let ph = p * ratio;
                        2.0 * (ph - ph.floor()) - 1.0
                    }));
                }
            }
            // Digi Vowel: morph A-E-I-O-U
            13 => {
                for f in 0..17 {
                    let t = f as f32 / 16.0 * 4.0;
                    let v0 = (t as usize).min(4);
                    let v1 = (v0 + 1).min(4);
                    let mix_amt = t - v0 as f32;
                    frames.push(harmonic_frame(
                        128,
                        |h| {
                            let hz = h as f32 * 110.0;
                            let a = formant_amp(&VOWELS[v0], hz);
                            let b = formant_amp(&VOWELS[v1], hz);
                            // Glottal source shaped by the formant envelope, with
                            // a floor under the envelope. The envelope on its own
                            // has no energy at the fundamental, so the table lost
                            // 50 dB by the top of the keyboard once the mip chain
                            // took the formant harmonics away.
                            (0.15 + lerp(a, b, mix_amt)) / h as f32
                        },
                        Some(&|h| dispersed_phase(h, 128)),
                    ));
                }
            }
            // 14 is Custom, which factory_table_choice never yields.
            // Basic Shapes: sine -> triangle -> saw -> square, crossfaded
            15 => {
                let shapes = [
                    harmonic_frame(1, |_| 1.0, None),
                    harmonic_frame(256, triangle_amp, None),
                    harmonic_frame(512, |h| 1.0 / h as f32, None),
                    harmonic_frame(512, |h| if h % 2 == 1 { 1.0 / h as f32 } else { 0.0 }, None),
                ];
                for f in 0..morph_frames {
                    let pos = morph_t(f) * 3.0;
                    let seg = (pos as usize).min(2);
                    let mix_amt = pos - seg as f32;
                    let mut frame: Vec<f32> =
                        shapes[seg].iter().zip(&shapes[seg + 1]).map(|(&a, &b)| lerp(a, b, mix_amt)).collect();
                    normalise_frame(&mut frame);
                    frames.push(frame);
                }
            }
            // Analog Reso: saw through a resonant two-pole low-pass sweeping up
            16 => {
                const Q: f32 = 4.0;
                for f in 0..morph_frames {
                    let cutoff = 2.0 * 64.0_f32.powf(morph_t(f));
                    let response = |h: i32| {
                        let r = h as f32 / cutoff;
                        (1.0 - r * r, r / Q)
                    };
                    frames.push(harmonic_frame(
                        512,
                        |h| {
                            let (re, im) = response(h);
                            1.0 / (h as f32 * (re * re + im * im).sqrt())
                        },
                        Some(&|h| {
                            let (re, im) = response(h);
                            -im.atan2(re)
                        }),
                    ));
                }
            }
            // Analog Drive: sine into a slightly biased tanh stage, more drive per frame
            17 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    let drive = 16.0_f32.powf(t);
                    let bias = 0.2 * t;
                    frames.push(time_frame(|p| (drive * ((TWO_PI * p).sin() + bias)).tanh()));
                }
            }
            // Digi Additive: a saw built up harmonic by harmonic
            18 => {
                const COUNTS: [i32; 16] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 64, 128, 512];
                for &count in &COUNTS {
                    frames.push(harmonic_frame(count, |h| 1.0 / h as f32, None));
                }
            }
            // Digi PD: Casio-style phase distortion, cosine towards a saw
            19 => {
                for f in 0..morph_frames {
                    let knee = lerp(0.5, 0.02, morph_t(f));
                    frames.push(time_frame(|p| {
                        let warped = if p < knee { 0.5 * p / knee } else { 0.5 + 0.5 * (p - knee) / (1.0 - knee) };
                        (TWO_PI * warped).cos()
                    }));
                }
            }
            // Digi FM: 1:1 two-operator FM, index rising to 8
            20 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    let index = 8.0 * t * t;
                    frames.push(time_frame(|p| (TWO_PI * p + index * (TWO_PI * p).sin()).sin()));
                }
            }
            // Digi Crush: sine with fewer levels and a coarser sample hold per frame
            21 => {
                for f in 0..morph_frames {
                    let t = morph_t(f);
                    let levels = 16.0 * 0.0625_f32.powf(t);
                    let steps = (FRAME_SIZE >> (t * 6.0).round() as usize) as f32;
                    frames.push(time_frame(|p| {
                        let held = (p * steps).floor() / steps;
                        ((TWO_PI * held).sin() * levels).round() / levels
                    }));
                }
            }
            // Digi Spectra: random spectra over a fixed fundamental, morphing
            // between five seeded keyframes (the same table on every build)
            _ => {
                const KEYS: usize = 5;
                const HARMONICS: usize = 128;
                let mut rng = Rng::new(0x5EC7_2A11);
                let phases: Vec<f32> = (0..HARMONICS).map(|_| rng.next_float() * TWO_PI).collect();
                let keys: Vec<Vec<f32>> = (0..KEYS)
                    .map(|_| {
                        (1..=HARMONICS)
                            .map(|h| if h == 1 { 1.0 } else { rng.next_float() * (h as f32).powf(-0.7) })
                            .collect()
                    })
                    .collect();
                for f in 0..morph_frames {
                    let pos = morph_t(f) * (KEYS - 1) as f32;
                    let k = (pos as usize).min(KEYS - 2);
                    let mix_amt = pos - k as f32;
                    frames.push(harmonic_frame(
                        HARMONICS as i32,
                        |h| lerp(keys[k][h as usize - 1], keys[k + 1][h as usize - 1], mix_amt),
                        Some(&|h| phases[h as usize - 1]),
                    ));
                }
            }
        }

        let mut table = Wavetable::default();
        table.set_frames(&frames, TABLE_NAMES[index]);
        // set_frames cleared it; this really is factory `index`.
        table.factory = Some(index);
        table.update_hash();
        table
    }

    /// Parses a RIFF WAVE file (PCM 8/16/24/32 or float32/64, any channel
    /// count; the first channel is used). Frames are split on [`FRAME_SIZE`]
    /// boundaries when the length allows, else treated as one cycle and
    /// resampled. `None` for anything that is not a usable WAV.
    pub fn parse_wav(bytes: &[u8]) -> Option<Vec<Vec<f32>>> {
        if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return None;
        }

        let mut channels = 1_usize;
        let mut bits = 16_usize;
        let mut format = 1_u16;
        let mut data_pos = 0_usize;
        let mut data_size = 0_usize;

        let mut pos = 12_usize;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = read_u32(bytes, pos + 4) as usize;
            let body = pos + 8;
            if id == b"fmt " {
                format = read_u16(bytes, body);
                channels = (read_u16(bytes, body + 2) as usize).max(1);
                bits = read_u16(bytes, body + 14) as usize;
                // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID starts at +24.
                if format == 0xFFFE && size >= 40 {
                    format = read_u16(bytes, body + 24);
                }
            } else if id == b"data" {
                data_pos = body;
                data_size = size.min(bytes.len() - body);
            }
            pos = body.saturating_add(size).saturating_add(size & 1);
        }

        if data_pos == 0 || data_size == 0 || bits < 8 {
            return None;
        }

        let bytes_per_sample = bits / 8;
        let total_samples = data_size / bytes_per_sample;
        let mut mono = Vec::with_capacity(total_samples / channels);
        let mut i = 0;
        while i + channels <= total_samples {
            let p = data_pos + i * bytes_per_sample;
            let v = match (format, bits) {
                (3, 32) => f32::from_bits(read_u32(bytes, p)),
                (3, 64) => {
                    let raw = read_u32(bytes, p) as u64 | ((read_u32(bytes, p + 4) as u64) << 32);
                    f64::from_bits(raw) as f32
                }
                (_, 8) => (bytes[p] as f32 - 128.0) / 128.0,
                (_, 16) => read_u16(bytes, p) as i16 as f32 / 32768.0,
                (_, 24) => {
                    let raw = ((bytes[p] as u32) << 8) | ((bytes[p + 1] as u32) << 16) | ((bytes[p + 2] as u32) << 24);
                    (raw as i32 >> 8) as f32 / 8_388_608.0
                }
                (_, 32) => read_u32(bytes, p) as i32 as f32 / 2_147_483_648.0,
                _ => return None,
            };
            mono.push(v);
            i += channels;
        }

        if mono.is_empty() {
            return None;
        }

        // Serum-style tables are a whole number of 2048-sample cycles; anything
        // else is treated as a single cycle and resampled.
        if mono.len() % FRAME_SIZE == 0 {
            let count = (mono.len() / FRAME_SIZE).min(MAX_FRAMES);
            Some(mono.as_chunks::<FRAME_SIZE>().0[..count].iter().map(|c| c.to_vec()).collect())
        } else {
            Some(vec![resample_cycle(&mono, FRAME_SIZE)])
        }
    }

    /// Mono float32 WAV of every frame back to back.
    pub fn write_wav(&self, sample_rate: f64) -> Vec<u8> {
        let samples = (self.frames * FRAME_SIZE) as u32;
        let data_bytes = samples * 4;
        let mut out = Vec::with_capacity(44 + data_bytes as usize);
        out.extend_from_slice(b"RIFF");
        push_u32(&mut out, 36 + data_bytes);
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        push_u32(&mut out, 16);
        push_u16(&mut out, 3); // IEEE float
        push_u16(&mut out, 1); // mono
        push_u32(&mut out, sample_rate as u32);
        push_u32(&mut out, sample_rate as u32 * 4);
        push_u16(&mut out, 4);
        push_u16(&mut out, 32);
        out.extend_from_slice(b"data");
        push_u32(&mut out, data_bytes);
        for v in &self.base {
            push_u32(&mut out, v.to_bits());
        }
        out
    }

    /// Custom `.wt` container: "OXSW" magic, version 1, frame count, frame
    /// size, name length + UTF-8 name, then float32 samples, all little-endian.
    pub fn write_wt(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + self.table_name.len() + self.base.len() * 4);
        out.extend_from_slice(b"OXSW");
        push_u32(&mut out, 1); // version
        push_u32(&mut out, self.frames as u32);
        push_u32(&mut out, FRAME_SIZE as u32);
        push_u32(&mut out, self.table_name.len() as u32);
        out.extend_from_slice(self.table_name.as_bytes());
        for v in &self.base {
            push_u32(&mut out, v.to_bits());
        }
        out
    }

    /// Parses a `.wt` container into frames (resampled to [`FRAME_SIZE`]) and
    /// the table name.
    pub fn parse_wt(bytes: &[u8]) -> Option<(Vec<Vec<f32>>, String)> {
        if bytes.len() < 20 || &bytes[0..4] != b"OXSW" {
            return None;
        }
        if read_u32(bytes, 4) != 1 {
            return None;
        }
        let count = read_u32(bytes, 8) as usize;
        let frame_size = read_u32(bytes, 12) as usize;
        let name_len = read_u32(bytes, 16) as usize;
        let mut pos = 20;
        if count == 0 || count > MAX_FRAMES || frame_size == 0 || pos + name_len > bytes.len() {
            return None;
        }
        let name = String::from_utf8_lossy(&bytes[pos..pos + name_len]).into_owned();
        pos += name_len;
        let needed = count.checked_mul(frame_size)?.checked_mul(4)?;
        if pos + needed > bytes.len() {
            return None;
        }
        let frames = (0..count)
            .map(|f| {
                let frame: Vec<f32> = (0..frame_size)
                    .map(|i| f32::from_bits(read_u32(bytes, pos + (f * frame_size + i) * 4)))
                    .collect();
                to_frame_size(&frame)
            })
            .collect();
        Some((frames, name))
    }

    // ---- Patch state ------------------------------------------------------

    /// `{"name", "frames", "frameSize", "hash" (decimal string), "data"
    /// (base64 of the little-endian float32 samples)}`, the same shape the C++
    /// version wrote, so existing patches load.
    pub fn to_json(&self) -> Value {
        let mut raw = Vec::with_capacity(self.base.len() * 4);
        for v in &self.base {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        json!({
            "name": self.table_name,
            "frames": self.frames,
            "frameSize": FRAME_SIZE,
            "hash": self.hash.to_string(),
            "data": STANDARD.encode(&raw),
        })
    }

    /// Inverse of [`Self::to_json`]. Malformed input yields the default sine
    /// table; the result is never marked as a factory table.
    pub fn from_json(value: &Value) -> Wavetable {
        let mut table = Wavetable::default();
        if !value.is_object() {
            return table;
        }
        let as_int = |key: &str, fallback: i64| value[key].as_f64().map_or(fallback, |v| v as i64);
        let count = as_int("frames", 1).clamp(1, MAX_FRAMES as i64) as usize;
        let frame_size = as_int("frameSize", FRAME_SIZE as i64).max(2) as usize;
        let bytes = base64_decode_lenient(value["data"].as_str().unwrap_or(""));
        let floats = bytes.len() / 4;
        if floats < count * frame_size {
            return table;
        }
        let frames: Vec<Vec<f32>> = (0..count)
            .map(|f| {
                let frame: Vec<f32> = bytes[f * frame_size * 4..(f + 1) * frame_size * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                to_frame_size(&frame)
            })
            .collect();
        let name = value["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("Custom");
        table.set_frames(&frames, name);
        table
    }

    fn generate_mips(&mut self) {
        let mut total = 0;
        for level in 0..MIP_LEVELS {
            self.mip_offset[level] = total;
            total += self.frames * (FRAME_SIZE >> level);
        }
        self.mip_storage = vec![0.0; total];

        // Scratch is allocated once for the whole table rather than per frame
        // per level; each Fft only touches the first size() entries.
        let mut re = vec![0.0; FRAME_SIZE];
        let mut im = vec![0.0; FRAME_SIZE];
        let mut lre = vec![0.0; FRAME_SIZE];
        let mut lim = vec![0.0; FRAME_SIZE];

        for f in 0..self.frames {
            re.copy_from_slice(&self.base[f * FRAME_SIZE..(f + 1) * FRAME_SIZE]);
            im.fill(0.0);
            mip_fft(0).forward(&mut re, &mut im);

            for level in 0..MIP_LEVELS {
                let size = FRAME_SIZE >> level;
                lre[..size].fill(0.0);
                lim[..size].fill(0.0);

                let scale = size as f32 / FRAME_SIZE as f32;
                let max_h = size / 2 - 1;
                for h in 1..=max_h {
                    let cr = re[h] * scale;
                    let ci = im[h] * scale;
                    lre[h] = cr;
                    lim[h] = ci;
                    lre[size - h] = cr;
                    lim[size - h] = -ci;
                }
                mip_fft(level).inverse(&mut lre, &mut lim);

                let start = self.mip_offset[level] + f * size;
                self.mip_storage[start..start + size].copy_from_slice(&lre[..size]);
            }
        }
    }

    fn update_hash(&mut self) {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        mix(&(self.frames as u32).to_le_bytes());
        for v in &self.base {
            mix(&v.to_le_bytes());
        }
        // The name is part of the identity: the patch pool is keyed on this
        // hash, so leaving it out let two same-sample tables collide and one
        // lose its name.
        mix(self.table_name.as_bytes());
        self.hash = h;
    }
}

/// Base64 decode that skips anything outside the alphabet (padding,
/// whitespace) and tolerates non-canonical trailing bits, as the C++ decoder did.
fn base64_decode_lenient(text: &str) -> Vec<u8> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let mut cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    // A dangling single character carries no complete byte.
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    LENIENT.decode(cleaned).unwrap_or_default()
}

fn read_u32(b: &[u8], pos: usize) -> u32 {
    match b.get(pos..pos + 4) {
        Some(s) => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

fn read_u16(b: &[u8], pos: usize) -> u16 {
    match b.get(pos..pos + 2) {
        Some(s) => u16::from_le_bytes([s[0], s[1]]),
        None => 0,
    }
}

fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn push_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::common::midi_to_hz;
    use crate::dsp::params::factory_tables;

    /// Energy above the mip level's harmonic limit, relative to total energy.
    fn alias_energy_ratio(table: &Wavetable, level: usize) -> f32 {
        let size = FRAME_SIZE >> level;
        let frame: Vec<f32> = (0..size).map(|i| table.sample(0, level, i as f32 / size as f32)).collect();
        let fft = Fft::new(size);
        let mut re = frame;
        let mut im = vec![0.0; size];
        fft.forward(&mut re, &mut im);

        let limit = size / 2 - 1;
        let mut total = 0.0;
        let mut above = 0.0;
        for h in 1..size / 2 {
            let mag = re[h] * re[h] + im[h] * im[h];
            total += mag;
            if h > limit {
                above += mag;
            }
        }
        if total > 0.0 {
            above / total
        } else {
            0.0
        }
    }

    #[test]
    fn default_table_is_sine() {
        let table = Wavetable::default();
        assert_eq!(table.num_frames(), 1);
        // A sine reads back with the expected quarter-cycle peaks.
        assert!((table.sample(0, 0, 0.25) - 1.0).abs() < 0.01);
        assert!((table.sample(0, 0, 0.75) + 1.0).abs() < 0.01);
        assert!(table.sample(0, 0, 0.0).abs() < 0.01);
    }

    #[test]
    fn mip_levels_are_band_limited() {
        // Saw: harmonics up to 512.
        let saw = Wavetable::make_factory(2);
        for level in 0..8 {
            let ratio = alias_energy_ratio(&saw, level);
            assert!(ratio < 1.0e-6, "level {level}: {ratio}");
        }
    }

    #[test]
    fn mip_selection_follows_pitch() {
        let table = Wavetable::default();
        // 20 Hz at 48 kHz can carry all harmonics, 5 kHz almost none.
        assert_eq!(table.mip_for_increment(20.0 / 48000.0), 0);
        assert!(table.mip_for_increment(5000.0 / 48000.0) > 4);
        // Never out of range, even at absurd increments.
        assert!(table.mip_for_increment(0.9) < MIP_LEVELS);
        assert_eq!(table.mip_for_increment(1.0e-9), 0);
    }

    #[test]
    fn factory_tables_are_finite_and_loudness_matched() {
        // Frames are normalised to a common RMS, not a common peak. Peak
        // normalisation left dense, impulse-like spectra up to 18 dB quieter
        // than the classic shapes, so switching table sounded like losing the
        // sound. A genuinely spiky frame (a 4 % pulse) still lands under the
        // target because it hits the peak ceiling first - that is honest, and it
        // stays within a few dB rather than the 18 dB the old normalisation produced.
        let mut quietest = 1.0e9_f32;
        let mut loudest = 0.0_f32;
        for i in factory_tables() {
            let table = Wavetable::make_factory(i);
            assert!(table.num_frames() >= 1 && table.num_frames() <= MAX_FRAMES, "{i}");
            for f in 0..table.num_frames() {
                let data = table.frame_data(f);
                let mut peak = 0.0_f32;
                let mut sum_squares = 0.0_f64;
                for &s in data {
                    assert!(s.is_finite(), "table {i} frame {f}");
                    peak = peak.max(s.abs());
                    sum_squares += s as f64 * s as f64;
                }
                let rms = (sum_squares / FRAME_SIZE as f64).sqrt() as f32;
                assert!(rms > 0.0 && rms <= 0.51, "table {i} frame {f} rms {rms}");
                assert!(peak > 0.3 && peak <= 1.81, "table {i} frame {f} peak {peak}");
                quietest = quietest.min(rms);
                loudest = loudest.max(rms);
            }
        }
        let spread_db = 20.0 * (loudest / quietest).log10();
        assert!(spread_db < 4.5, "loudness spread across factory frames: {spread_db} dB");
    }

    #[test]
    fn factory_tables_stay_audible_across_the_keyboard() {
        // Tables whose energy sits only in high harmonics used to fade to nothing
        // as the mip chain removed them - Digi Vowel lost 50 dB by the top of the range.
        for i in factory_tables() {
            let table = Wavetable::make_factory(i);
            let top_mip = table.mip_for_increment(midi_to_hz(108.0) / 48000.0);
            let size = FRAME_SIZE >> top_mip;
            let sum_squares: f64 = (0..size)
                .map(|s| {
                    let v = table.sample(0, top_mip, s as f32 / size as f32) as f64;
                    v * v
                })
                .sum();
            let rms = (sum_squares / size as f64).sqrt() as f32;
            assert!(rms > 0.15, "{} at MIDI 108 (mip {top_mip}): {rms}", factory_table_name(i));
        }
    }

    #[test]
    fn renaming_changes_the_content_hash() {
        // The patch table pool is keyed on this hash, so two same-sample tables
        // with different names must not collide and lose one of the names.
        let a = Wavetable::make_factory(2);
        let mut b = Wavetable::make_factory(2);
        assert_eq!(a.content_hash(), b.content_hash());
        b.set_name("Renamed");
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn factory_marker_survives_only_until_the_table_is_edited() {
        let mut table = Wavetable::make_factory(6);
        assert_eq!(table.factory_index(), Some(6));
        let copy = table.frame_copy(0);
        table.set_frame(0, &copy);
        assert_eq!(table.factory_index(), None);

        let plain = Wavetable::default();
        assert_eq!(plain.factory_index(), None);
    }

    #[test]
    fn frame_editing_keeps_size_and_hash_changes() {
        let mut table = Wavetable::default();
        let before = table.content_hash();
        let ramp: Vec<f32> = (0..FRAME_SIZE).map(|i| i as f32 / FRAME_SIZE as f32 * 2.0 - 1.0).collect();

        table.add_frame(&ramp);
        assert_eq!(table.num_frames(), 2);
        assert_ne!(table.content_hash(), before);

        table.duplicate_frame(1);
        assert_eq!(table.num_frames(), 3);
        table.move_frame(2, 0);
        assert_eq!(table.num_frames(), 3);
        table.remove_frame(0);
        assert_eq!(table.num_frames(), 2);
        table.remove_frame(0);
        // The last frame is protected.
        table.remove_frame(0);
        assert_eq!(table.num_frames(), 1);
    }

    #[test]
    fn move_frame_reorders() {
        let mut table = Wavetable::default();
        let ramp: Vec<f32> = (0..FRAME_SIZE).map(|i| i as f32 / FRAME_SIZE as f32).collect();
        table.add_frame(&ramp);
        table.move_frame(1, 0);
        assert_eq!(table.frame_data(0)[100], ramp[100]);
        assert!(table.frame_data(1)[512] > 0.99);
    }

    #[test]
    fn wav_round_trip() {
        // Analog PWM, 16 frames.
        let source = Wavetable::make_factory(6);
        let bytes = source.write_wav(44100.0);
        let frames = Wavetable::parse_wav(&bytes).expect("parse");
        assert_eq!(frames.len(), source.num_frames());
        for (f, frame) in frames.iter().enumerate() {
            for (a, b) in frame.iter().zip(source.frame_data(f)) {
                assert!((a - b).abs() < 1.0e-6);
            }
        }
    }

    #[test]
    fn pcm16_stereo_wav_takes_the_first_channel() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        push_u32(&mut bytes, 36 + 8 * 600);
        bytes.extend_from_slice(b"WAVEfmt ");
        push_u32(&mut bytes, 16);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 2);
        push_u32(&mut bytes, 44100);
        push_u32(&mut bytes, 44100 * 4);
        push_u16(&mut bytes, 4);
        push_u16(&mut bytes, 16);
        bytes.extend_from_slice(b"data");
        push_u32(&mut bytes, 4 * 600);
        for i in 0..600 {
            let v = ((TWO_PI * i as f32 / 600.0).sin() * 16000.0) as i16;
            push_u16(&mut bytes, v as u16);
            push_u16(&mut bytes, 0);
        }
        let frames = Wavetable::parse_wav(&bytes).expect("parse");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), FRAME_SIZE);
        assert!((frames[0][FRAME_SIZE / 4] - 16000.0 / 32768.0).abs() < 0.01);
    }

    #[test]
    fn wt_round_trip() {
        let source = Wavetable::make_factory(9);
        let bytes = source.write_wt();
        let (frames, name) = Wavetable::parse_wt(&bytes).expect("parse");
        assert_eq!(name, source.name());
        assert_eq!(frames.len(), source.num_frames());
        assert!((frames[0][100] - source.frame_data(0)[100]).abs() < 1.0e-6);
    }

    #[test]
    fn rejects_garbage_files() {
        let garbage = vec![0x7f_u8; 64];
        assert!(Wavetable::parse_wav(&garbage).is_none());
        assert!(Wavetable::parse_wt(&garbage).is_none());
    }

    #[test]
    fn json_round_trip_preserves_samples_and_hash() {
        let source = Wavetable::make_factory(13);
        let json = source.to_json();
        let restored = Wavetable::from_json(&json);
        assert_eq!(restored.num_frames(), source.num_frames());
        assert_eq!(restored.content_hash(), source.content_hash());
        assert_eq!(restored.name(), source.name());
        // JSON text survives too, including a decoder that ignores whitespace.
        let text = serde_json::to_string(&json).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(Wavetable::from_json(&reparsed).content_hash(), source.content_hash());
    }

    #[test]
    fn base64_decoder_is_lenient() {
        let raw: Vec<u8> = (0..=255).collect();
        let encoded = STANDARD.encode(&raw);
        let spaced: String = encoded.chars().flat_map(|c| [c, '\n']).collect();
        assert_eq!(base64_decode_lenient(&encoded), raw);
        assert_eq!(base64_decode_lenient(&spaced), raw);
    }

    #[test]
    fn resample_cycle_handles_odd_lengths() {
        let input: Vec<f32> = (0..777).map(|i| (TWO_PI * i as f32 / 777.0).sin()).collect();
        let out = resample_cycle(&input, FRAME_SIZE);
        assert_eq!(out.len(), FRAME_SIZE);
        assert!((out[FRAME_SIZE / 4] - 1.0).abs() < 0.02);
    }

    #[test]
    fn display_frame_is_peak_normalised() {
        let table = Wavetable::make_factory(2);
        let shape = table.display_frame(0, 128);
        assert_eq!(shape.len(), 128);
        let peak = shape.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 1.0e-4);
    }
}
