//! Engine top level: parameter store, voice allocation, oversampling, FX
//! chain, global modulation and the lock-free feedback the GUI reads
//! (SPEC 3, 7, 9).
//!
//! The audio thread owns a [`Synth`]; everything it shares with the editor and
//! the main thread lives in [`SynthShared`]. Per block the synth copies the
//! matrix snapshot and takes its own `Arc` of each wavetable and LFO shape.
//! Replacing those `Arc`s on the audio thread is not a deallocation:
//! [`SynthShared::set_table`] and [`SynthShared::set_lfo_shape`] keep every
//! replaced value alive until the audio thread has let go of it, and free it
//! on the calling (non-audio) thread.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::common::{fast_exp2, MATRIX_SLOTS, MAX_VOICES};
use super::fx::FxChain;
use super::lfo::Lfo;
use super::mod_matrix::{apply_mod_curve, mod_target, mod_target_index_for_param, ModSource, Snapshot, NUM_FIXED_SOURCES};
use super::params::{defaults, time_div_beats, LfoShape, LfoSync, ParamId, NUM_PARAMS};
use super::resampler::Decimator;
use super::shared::SynthShared;
use super::voice::{GlobalMod, Voice, VoiceContext};
use super::wavetable::Wavetable;

const RELAXED: Ordering = Ordering::Relaxed;

/// Sets flush-to-zero and denormals-are-zero for its lifetime and restores
/// the previous mode on drop (JUCE's `ScopedNoDenormals`).
struct DenormalGuard {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    previous: u32,
}

impl DenormalGuard {
    #[allow(deprecated)]
    fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
            // SAFETY: only the FTZ (bit 15) and DAZ (bit 6) flags change; SSE is
            // always available on x86_64.
            let previous = unsafe { _mm_getcsr() };
            unsafe { _mm_setcsr(previous | 0x8040) };
            Self { previous }
        }
        #[cfg(target_arch = "x86")]
        {
            Self { previous: 0 }
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        {
            Self {}
        }
    }
}

impl Drop for DenormalGuard {
    #[allow(deprecated)]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::_mm_setcsr;
            // SAFETY: restores the mode read in `new`.
            unsafe { _mm_setcsr(self.previous) };
        }
    }
}

/// CLAP note expressions the engine handles (SPEC 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteExpressionKind {
    /// Gain, 0 … 4.
    Volume,
    /// 0 … 1, 0.5 = centre.
    Pan,
    /// Semitones.
    Tuning,
    /// 0 … 1 depth.
    Vibrato,
    /// 0 … 1 per-note pressure.
    Pressure,
}

pub struct Synth {
    shared: Arc<SynthShared>,

    params: [f32; NUM_PARAMS],
    global_mod_by_param: [f32; NUM_PARAMS],

    voices: Vec<Voice>,
    mod_snapshot: Snapshot,
    fx: FxChain,
    globals: GlobalMod,

    free_lfo: [Lfo; 2],
    /// This block's tables and shapes, taken from `shared` once per block.
    active_tables: [Arc<Wavetable>; 2],
    active_shapes: [Option<Arc<Vec<f32>>>; 2],

    /// L, R, dry.
    decimators: [Decimator; 3],
    voice_buf_l: Vec<f32>,
    voice_buf_r: Vec<f32>,
    voice_buf_dry: Vec<f32>,
    dry_out: Vec<f32>,

    sample_rate: f64,
    max_block: usize,
    factor_in_use: usize,
    order_counter: u64,
    note_seed_counter: u32,
    last_note_played: Option<u8>,
    master_gain: super::common::Smoother,
    meter_state: [f32; 2],
}

impl Synth {
    /// Extra slots hold stolen voices while they fade.
    pub const VOICE_POOL: usize = MAX_VOICES + 4;
    /// Inner render chunk (output samples).
    pub const CHUNK: usize = 32;

    pub fn new(shared: Arc<SynthShared>) -> Self {
        let active_tables = [shared.table(0), shared.table(1)];
        let active_shapes = [shared.lfo_shape(0), shared.lfo_shape(1)];
        Self {
            shared,
            params: defaults(),
            global_mod_by_param: [0.0; NUM_PARAMS],
            voices: vec![Voice::default(); Self::VOICE_POOL],
            mod_snapshot: Snapshot::default(),
            fx: FxChain::default(),
            globals: GlobalMod::default(),
            free_lfo: [Lfo::default(), Lfo::default()],
            active_tables,
            active_shapes,
            decimators: [Decimator::default(), Decimator::default(), Decimator::default()],
            voice_buf_l: Vec::new(),
            voice_buf_r: Vec::new(),
            voice_buf_dry: Vec::new(),
            dry_out: Vec::new(),
            sample_rate: 48_000.0,
            max_block: 512,
            factor_in_use: 1,
            order_counter: 0,
            note_seed_counter: 1,
            last_note_played: None,
            master_gain: super::common::Smoother::default(),
            meter_state: [0.0; 2],
        }
    }

    pub fn shared(&self) -> &Arc<SynthShared> {
        &self.shared
    }

    /// Allocates; call off the audio thread.
    pub fn prepare(&mut self, sample_rate: f64, max_block: usize) {
        self.shared.feedback.sample_rate.store(sample_rate as f32, std::sync::atomic::Ordering::Relaxed);
        self.sample_rate = sample_rate;
        self.max_block = max_block.max(1);

        // Voices allocate for the highest rate the engine can switch to (4x).
        for v in &mut self.voices {
            v.prepare(sample_rate * 4.0);
        }

        self.fx.prepare(sample_rate);
        for lfo in &mut self.free_lfo {
            lfo.prepare(sample_rate);
        }

        let inner_max = Self::CHUNK * 4;
        self.voice_buf_l = vec![0.0; inner_max];
        self.voice_buf_r = vec![0.0; inner_max];
        self.voice_buf_dry = vec![0.0; inner_max];
        self.dry_out = vec![0.0; Self::CHUNK];
        for d in &mut self.decimators {
            d.prepare(Self::CHUNK);
        }

        self.shared.feedback.analyzer_wet.clear();
        self.shared.feedback.analyzer_dry.clear();
        self.master_gain.prepare(sample_rate, 0.01);
        self.master_gain.reset(self.params[ParamId::MasterVolume.index()]);

        // Force apply_rate on the first block.
        self.factor_in_use = 0;
        self.apply_rate();
        self.refresh_shared();
        self.reset();
    }

    pub fn reset(&mut self) {
        for v in &mut self.voices {
            v.kill();
        }
        self.fx.reset();
        for d in &mut self.decimators {
            d.reset();
        }
        for lfo in &mut self.free_lfo {
            lfo.reset(0.0);
        }
        self.globals = GlobalMod { gui_attached: self.globals.gui_attached, ..GlobalMod::default() };
        self.order_counter = 0;
        self.note_seed_counter = 1;
        self.last_note_played = None;
        self.meter_state = [0.0; 2];
        let fb = &self.shared.feedback;
        fb.voice_count.store(0, RELAXED);
        for m in fb.meters.iter().chain(fb.osc_meters.iter()) {
            m.store(0.0, RELAXED);
        }
    }

    // ---- Parameters ----------------------------------------------------------

    pub fn set_param(&mut self, id: ParamId, value: f32) {
        self.params[id.index()] = id.desc().clamp(value);
    }

    /// All parameters at once, in natural units; the plugin calls this once
    /// per block with the host's values.
    pub fn set_params(&mut self, values: &[f32; NUM_PARAMS]) {
        for (i, (slot, v)) in self.params.iter_mut().zip(values).enumerate() {
            *slot = ParamId::from_index(i).desc().clamp(*v);
        }
    }

    pub fn param(&self, id: ParamId) -> f32 {
        self.params[id.index()]
    }

    pub fn params(&self) -> &[f32; NUM_PARAMS] {
        &self.params
    }

    fn oversampling_factor(&self) -> usize {
        match self.params[ParamId::MasterOversampling.index()].round() as i32 {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }

    fn apply_rate(&mut self) {
        let factor = self.oversampling_factor();
        if factor == self.factor_in_use {
            return;
        }
        self.factor_in_use = factor;
        for v in &mut self.voices {
            v.set_sample_rate(self.sample_rate * factor as f64, self.sample_rate);
        }
        for d in &mut self.decimators {
            d.reset();
        }
    }

    /// Takes this block's copy of everything the editor publishes.
    fn refresh_shared(&mut self) {
        for i in 0..2 {
            let table = self.shared.tables[i].load();
            if !Arc::ptr_eq(&*table, &self.active_tables[i]) {
                self.active_tables[i] = Arc::clone(&*table);
            }
            let shape = self.shared.lfo_shapes[i].load();
            let same = match ((*shape).as_ref(), self.active_shapes[i].as_ref()) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                self.active_shapes[i] = (*shape).clone();
            }
        }
        self.shared.matrix_snapshot.copy_snapshot(&mut self.mod_snapshot);
    }

    // ---- Events ----------------------------------------------------------------

    fn find_free_voice(&self) -> Option<usize> {
        self.voices.iter().position(|v| !v.is_active())
    }

    fn oldest_voice(&self, include_stealing: bool) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, v) in self.voices.iter().enumerate() {
            if !v.is_active() || (!include_stealing && v.is_stealing()) {
                continue;
            }
            if best.is_none_or(|b| v.start_order() < self.voices[b].start_order()) {
                best = Some(i);
            }
        }
        best
    }

    fn playing_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active() && !v.is_stealing()).count()
    }

    pub fn note_on(&mut self, channel: u8, note: u8, velocity: f32, note_id: i32) {
        let polyphony = (self.params[ParamId::MasterPolyphony.index()].round() as usize).clamp(1, MAX_VOICES);
        if self.playing_voices() >= polyphony {
            // Oldest-note stealing with a short fade; the fading voice keeps its
            // slot from the reserve pool so the new note can start immediately.
            if let Some(victim) = self.oldest_voice(false) {
                self.voices[victim].steal();
            }
        }

        let index = match self.find_free_voice() {
            Some(i) => i,
            None => match self.oldest_voice(true) {
                Some(i) => {
                    self.voices[i].kill();
                    i
                }
                None => return,
            },
        };

        let glide = self.params[ParamId::MasterGlide.index()];
        let glide_from = if glide > 0.0 { self.last_note_played.map(|n| n as f32) } else { None };
        let seed = self.note_seed_counter;
        self.note_seed_counter = self.note_seed_counter.wrapping_add(1);
        self.order_counter += 1;
        let order = self.order_counter;

        // Built from the fields directly so the voice can be borrowed mutably.
        let ctx = VoiceContext {
            params: &self.params,
            globals: &self.globals,
            snapshot: &self.mod_snapshot,
            table_a: &self.active_tables[0],
            table_b: &self.active_tables[1],
            lfo_shapes: [self.active_shapes[0].as_ref(), self.active_shapes[1].as_ref()],
        };
        let voice = &mut self.voices[index];
        voice.note_on(&ctx, note, velocity, channel, note_id, seed, glide_from);
        voice.set_start_order(order);
        self.last_note_played = Some(note);
    }

    /// Hosts that send CLAP notes give us note IDs; plain MIDI has none, so
    /// fall back to channel + key. A negative `note_id` means "no ID".
    fn voice_matches(voice: &Voice, channel: Option<u8>, key: Option<u8>, note_id: i32) -> bool {
        if note_id >= 0 && voice.note_id() >= 0 {
            return voice.note_id() == note_id;
        }
        key.is_none_or(|k| voice.note() == k) && channel.is_none_or(|c| voice.channel() == c)
    }

    pub fn note_off(&mut self, channel: u8, note: u8, note_id: i32) {
        for v in &mut self.voices {
            if !v.is_active() || v.is_releasing() || v.is_stealing() {
                continue;
            }
            if Self::voice_matches(v, Some(channel), Some(note), note_id) {
                v.note_off();
            }
        }
    }

    pub fn all_notes_off(&mut self) {
        for v in self.voices.iter_mut().filter(|v| v.is_active()) {
            v.note_off();
        }
    }

    pub fn all_sound_off(&mut self) {
        for v in &mut self.voices {
            v.kill();
        }
        self.shared.feedback.voice_count.store(0, RELAXED);
    }

    pub fn set_pitch_bend(&mut self, bipolar: f32) {
        self.globals.pitch_bend = bipolar.clamp(-1.0, 1.0);
    }

    pub fn set_mod_wheel(&mut self, value: f32) {
        self.globals.mod_wheel = value.clamp(0.0, 1.0);
    }

    pub fn set_channel_pressure(&mut self, value: f32) {
        self.globals.aftertouch = value.clamp(0.0, 1.0);
    }

    pub fn set_poly_pressure(&mut self, channel: u8, note: u8, value: f32) {
        for v in &mut self.voices {
            if v.is_active() && v.note() == note && v.channel() == channel {
                v.expression().pressure = value.clamp(0.0, 1.0);
            }
        }
    }

    pub fn set_control_change(&mut self, cc: u8, value: f32) {
        if cc > 127 {
            return;
        }
        let v = value.clamp(0.0, 1.0);
        self.globals.cc[cc as usize] = v;
        if cc == 1 {
            self.globals.mod_wheel = v;
        }
    }

    pub fn set_note_expression(&mut self, kind: NoteExpressionKind, channel: u8, note: u8, note_id: i32, value: f32) {
        for v in &mut self.voices {
            if !v.is_active() || !Self::voice_matches(v, Some(channel), Some(note), note_id) {
                continue;
            }
            let e = v.expression();
            match kind {
                NoteExpressionKind::Volume => e.volume = value.clamp(0.0, 4.0),
                NoteExpressionKind::Pan => e.pan = (value * 2.0 - 1.0).clamp(-1.0, 1.0),
                NoteExpressionKind::Tuning => e.tuning = value.clamp(-120.0, 120.0),
                NoteExpressionKind::Vibrato => e.vibrato = value.clamp(0.0, 1.0),
                NoteExpressionKind::Pressure => e.pressure = value.clamp(0.0, 1.0),
            }
        }
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.globals.tempo_bpm = bpm.clamp(10.0, 400.0);
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    // ---- Global modulation -------------------------------------------------------

    fn update_global_lfos(&mut self, out_samples: usize) {
        use ParamId::*;
        let ids = [
            (Lfo1Rate, Lfo1Shape, Lfo1SyncMode, Lfo1SyncDiv, Lfo1Smooth),
            (Lfo2Rate, Lfo2Shape, Lfo2SyncMode, Lfo2SyncDiv, Lfo2Smooth),
        ];
        for (i, (rate, shape, sync, div, smooth)) in ids.into_iter().enumerate() {
            let p = &self.params;
            let mut hz = p[rate.index()];
            if p[sync.index()].round() as usize == LfoSync::Tempo as usize {
                let beats = time_div_beats(p[div.index()].round().max(0.0) as usize);
                hz = self.globals.tempo_bpm / 60.0 / beats.max(0.01);
            }
            if mod_target_index_for_param(rate).is_some() {
                let octaves = self.global_mod_by_param[rate.index()];
                if octaves != 0.0 {
                    hz *= fast_exp2(octaves);
                }
            }

            let lfo = &mut self.free_lfo[i];
            lfo.set_rate(hz.clamp(0.0, 400.0));
            lfo.set_shape(LfoShape::from_value(p[shape.index()]));
            lfo.set_smoothing(p[smooth.index()]);
            lfo.set_custom_shape(self.active_shapes[i].as_ref());

            let mut value = self.globals.lfo[i];
            for _ in 0..out_samples {
                value = lfo.next();
            }
            self.globals.lfo[i] = value;
        }
    }

    fn evaluate_global_mod(&mut self) {
        self.global_mod_by_param = [0.0; NUM_PARAMS];
        let fb = &self.shared.feedback;
        let snap = self.mod_snapshot;
        if snap.count == 0 {
            for a in &fb.row_activity {
                a.store(0.0, RELAXED);
            }
            return;
        }

        // Voice-scope sources (env, LFO, velocity, note) are represented by the
        // most recently started active voice, so global targets still follow
        // the sound.
        let reference = self
            .voices
            .iter()
            .filter(|v| v.is_active())
            .max_by_key(|v| v.start_order());

        for (r, row) in snap.rows().iter().enumerate() {
            let g = &self.globals;
            let raw = match row.source {
                s if s == ModSource::ModWheel.index() => g.mod_wheel,
                s if s == ModSource::PitchBend.index() => g.pitch_bend,
                s if s == ModSource::Aftertouch.index() => g.aftertouch,
                s if s == ModSource::Lfo1.index() => g.lfo[0],
                s if s == ModSource::Lfo2.index() => g.lfo[1],
                s if s >= NUM_FIXED_SOURCES => g.cc[(s - NUM_FIXED_SOURCES + 1).min(127)],
                s => reference.map_or(0.0, |v| v.source_value(s, g)),
            };
            let shaped = apply_mod_curve(raw, row.curve);
            let target = mod_target(row.target);
            self.global_mod_by_param[target.param.index()] += shaped * row.amount * target.range;
            fb.row_activity[r].store((shaped * row.amount).abs().min(1.0), RELAXED);
        }
        for a in &fb.row_activity[snap.count.min(MATRIX_SLOTS)..] {
            a.store(0.0, RELAXED);
        }
    }

    // ---- Audio -------------------------------------------------------------------

    /// Renders into the buffers (overwriting them). Any length, including 0
    /// and 1. Allocation-free.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        // Filter and reverb tails decay into denormals, which cost ~2x on x86.
        let _denormals = DenormalGuard::new();
        let num_samples = left.len().min(right.len());
        self.apply_rate();
        // Snapshot the shared tables/shapes and matrix once per block (SPEC 7).
        self.refresh_shared();

        let factor = self.factor_in_use;
        let capture = self.shared.feedback.gui_attached.load(RELAXED);
        self.globals.gui_attached = capture;

        let mut peak_l: f32 = 0.0;
        let mut peak_r: f32 = 0.0;

        let mut offset = 0;
        while offset < num_samples {
            let chunk = Self::CHUNK.min(num_samples - offset);
            let inner = chunk * factor;

            self.voice_buf_l[..inner].fill(0.0);
            self.voice_buf_r[..inner].fill(0.0);
            if capture {
                self.voice_buf_dry[..inner].fill(0.0);
            }

            self.update_global_lfos(chunk);
            self.evaluate_global_mod();

            {
                // Split borrows: voices mutably, everything else shared.
                let ctx = VoiceContext {
                    params: &self.params,
                    globals: &self.globals,
                    snapshot: &self.mod_snapshot,
                    table_a: &self.active_tables[0],
                    table_b: &self.active_tables[1],
                    lfo_shapes: [self.active_shapes[0].as_ref(), self.active_shapes[1].as_ref()],
                };
                let buf_l = &mut self.voice_buf_l[..inner];
                let buf_r = &mut self.voice_buf_r[..inner];
                let buf_dry = &mut self.voice_buf_dry[..inner];
                for v in self.voices.iter_mut().filter(|v| v.is_active()) {
                    let dry = if capture { Some(&mut *buf_dry) } else { None };
                    v.render(&ctx, buf_l, buf_r, dry, factor);
                }
            }

            let out_l = &mut left[offset..offset + chunk];
            let out_r = &mut right[offset..offset + chunk];
            self.decimators[0].process(&self.voice_buf_l[..inner], out_l, chunk, factor);
            self.decimators[1].process(&self.voice_buf_r[..inner], out_r, chunk, factor);
            if capture {
                // The dry tap only feeds the analyser display, so averaging is enough.
                Decimator::average(&self.voice_buf_dry[..inner], &mut self.dry_out, chunk, factor);
            }

            self.fx.set_params(&self.params, Some(&self.global_mod_by_param), self.globals.tempo_bpm);
            self.fx.process(out_l, out_r);

            let volume_target = (self.params[ParamId::MasterVolume.index()]
                + self.global_mod_by_param[ParamId::MasterVolume.index()])
            .clamp(0.0, 1.5);
            for (l, r) in out_l.iter_mut().zip(out_r.iter_mut()) {
                let g = self.master_gain.next(volume_target);
                *l *= g;
                *r *= g;
                peak_l = peak_l.max(l.abs());
                peak_r = peak_r.max(r.abs());
            }

            if capture {
                self.shared.feedback.analyzer_wet.push(out_l);
                self.shared.feedback.analyzer_dry.push(&self.dry_out[..chunk]);
            }

            offset += chunk;
        }

        self.publish_feedback(num_samples, peak_l, peak_r);
    }

    /// GUI feedback: the newest voice drives the displays, frames take the max.
    fn publish_feedback(&mut self, num_samples: usize, peak_l: f32, peak_r: f32) {
        let fb = &self.shared.feedback;
        let mut active = 0u32;
        let mut frame = [0.0_f32; 2];
        let mut osc_peak = [0.0_f32; 2];
        let mut newest: Option<&Voice> = None;
        for v in self.voices.iter().filter(|v| v.is_active()) {
            active += 1;
            for (osc, f) in frame.iter_mut().enumerate() {
                let frames = self.active_tables[osc].num_frames();
                if frames > 1 {
                    *f = f.max(v.osc_frame(osc) as f32 / (frames - 1) as f32);
                }
                osc_peak[osc] = osc_peak[osc].max(v.osc_peak(osc));
            }
            if newest.is_none_or(|n| v.start_order() > n.start_order()) {
                newest = Some(v);
            }
        }

        fb.voice_count.store(active, RELAXED);
        // Meters fall at a fixed rate regardless of the block size.
        let decay = (-(num_samples as f64) / (0.15 * self.sample_rate)).exp() as f32;
        for (ch, peak) in [peak_l, peak_r].into_iter().enumerate() {
            self.meter_state[ch] = (self.meter_state[ch] * decay).max(peak);
            fb.meters[ch].store(self.meter_state[ch], RELAXED);
        }
        for osc in 0..2 {
            fb.osc_meters[osc].store(osc_peak[osc], RELAXED);
            fb.display_frame[osc].store(frame[osc], RELAXED);
        }

        let cutoff = match newest {
            Some(v) => {
                fb.env_level[0].store(v.amp_env_value(), RELAXED);
                fb.env_level[1].store(v.mod_env_value(), RELAXED);
                fb.env_stage[0].store(v.amp_env_stage() as u8, RELAXED);
                fb.env_stage[1].store(v.mod_env_stage() as u8, RELAXED);
                fb.lfo_value[0].store(v.lfo_value(0), RELAXED);
                fb.lfo_value[1].store(v.lfo_value(1), RELAXED);
                v.cutoff()
            }
            None => {
                fb.env_level[0].store(0.0, RELAXED);
                fb.env_level[1].store(0.0, RELAXED);
                fb.env_stage[0].store(0, RELAXED);
                fb.env_stage[1].store(0, RELAXED);
                fb.lfo_value[0].store(self.globals.lfo[0], RELAXED);
                fb.lfo_value[1].store(self.globals.lfo[1], RELAXED);
                let octaves = self.global_mod_by_param[ParamId::FilterCutoff.index()];
                (self.params[ParamId::FilterCutoff.index()] * fast_exp2(octaves))
                    .clamp(20.0, self.sample_rate as f32 * 0.45)
            }
        };
        fb.cutoff_hz.store(cutoff, RELAXED);
    }

    pub fn tail_seconds(&self) -> f32 {
        let release = self.params[ParamId::EnvAmpRelease.index()].max(self.params[ParamId::EnvModRelease.index()]);
        release.max(self.fx.tail_seconds())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::common::MAX_UNISON;
    use crate::dsp::mod_matrix::{cc_source_index, mod_target_from_id, ModCurve};
    use crate::dsp::params::{FilterType, NoiseMode, PhaseMode, SubMode};

    struct Render {
        left: Vec<f32>,
        right: Vec<f32>,
        peak: f32,
        finite: bool,
    }

    fn synth(sample_rate: f64, block: usize) -> Synth {
        let mut s = Synth::new(Arc::new(SynthShared::default()));
        s.prepare(sample_rate, block);
        s
    }

    fn add_row(s: &Synth, source: usize, target: &str, amount: f32) {
        let t = mod_target_from_id(target);
        assert!(t.is_some(), "unknown target {target}");
        s.shared().edit_matrix(true, |m| m.add_row(source, t, amount, ModCurve::Linear));
    }

    fn render_blocks(s: &mut Synth, blocks: usize, block: usize) -> Render {
        let mut out = Render { left: Vec::new(), right: Vec::new(), peak: 0.0, finite: true };
        let mut l = vec![0.0; block];
        let mut r = vec![0.0; block];
        for _ in 0..blocks {
            l.fill(0.0);
            r.fill(0.0);
            s.process(&mut l, &mut r);
            for (a, b) in l.iter().zip(&r) {
                out.finite &= a.is_finite() && b.is_finite();
                out.peak = out.peak.max(a.abs()).max(b.abs());
            }
            out.left.extend_from_slice(&l);
            out.right.extend_from_slice(&r);
        }
        out
    }

    #[test]
    fn silent_when_idle() {
        let mut s = synth(48_000.0, 128);
        let out = render_blocks(&mut s, 8, 128);
        assert!(out.finite);
        assert!(out.peak < 1.0e-9);
        assert_eq!(s.active_voice_count(), 0);
    }

    #[test]
    fn note_on_produces_sound() {
        let mut s = synth(48_000.0, 128);
        s.note_on(0, 60, 1.0, -1);
        let out = render_blocks(&mut s, 20, 128);
        assert!(out.finite);
        assert!(out.peak > 0.05, "{}", out.peak);
        assert_eq!(s.active_voice_count(), 1);
    }

    #[test]
    fn note_off_releases_voice() {
        let mut s = synth(48_000.0, 128);
        s.set_param(ParamId::EnvAmpRelease, 0.02);
        s.note_on(0, 64, 1.0, -1);
        render_blocks(&mut s, 10, 128);
        s.note_off(0, 64, -1);
        render_blocks(&mut s, 40, 128);
        assert_eq!(s.active_voice_count(), 0);
    }

    #[test]
    fn renders_are_deterministic() {
        let run = || {
            let mut s = synth(48_000.0, 64);
            s.set_param(ParamId::OscAUniVoices, 5.0);
            s.set_param(ParamId::OscAPhaseMode, PhaseMode::Random as usize as f32);
            s.set_param(ParamId::OscBOn, 1.0);
            s.set_param(ParamId::OscBNoiseMode, NoiseMode::Pink as usize as f32);
            s.set_param(ParamId::OscCLevel, 0.4);
            s.set_param(ParamId::OscCMode, SubMode::White as usize as f32);
            s.set_param(ParamId::FxRevOn, 1.0);
            s.set_param(ParamId::FxDelayOn, 1.0);
            add_row(&s, ModSource::Lfo1.index(), "filter.cutoff", 0.5);
            s.note_on(0, 55, 0.8, 1);
            s.note_on(0, 62, 0.6, 2);
            render_blocks(&mut s, 30, 64)
        };
        let a = run();
        let b = run();
        assert_eq!(a.left.len(), b.left.len());
        for i in 0..a.left.len() {
            assert_eq!(a.left[i].to_bits(), b.left[i].to_bits(), "{i}");
            assert_eq!(a.right[i].to_bits(), b.right[i].to_bits(), "{i}");
        }
    }

    #[test]
    fn block_size_split_does_not_change_output() {
        let run = |block: usize, total: usize| {
            let mut s = synth(48_000.0, 512);
            s.set_param(ParamId::EnvAmpAttack, 0.05);
            s.note_on(0, 60, 1.0, -1);
            let mut l = vec![0.0; block];
            let mut r = vec![0.0; block];
            let mut all = Vec::new();
            let mut done = 0;
            while done < total {
                let n = block.min(total - done);
                l.fill(0.0);
                r.fill(0.0);
                s.process(&mut l[..n], &mut r[..n]);
                all.extend_from_slice(&l[..n]);
                done += n;
            }
            all
        };
        let big = run(512, 4096);
        // 1-sample blocks are a spec requirement.
        let small = run(1, 4096);
        assert_eq!(big.len(), small.len());
        for i in 0..big.len() {
            assert!((big[i] - small[i]).abs() < 1.0e-6, "{i}: {} vs {}", big[i], small[i]);
        }
    }

    #[test]
    fn unison_blend_never_silences_the_oscillator() {
        // An even unison stack has no single centre voice. Treating "not the
        // centre" as "gain = blend" collapsed every voice to zero at blend = 0,
        // so 2/4/8/16 voices went completely silent - the counts people use.
        for voices in 1..=MAX_UNISON {
            for blend in [0.0, 0.5, 1.0] {
                let mut s = synth(48_000.0, 128);
                s.set_param(ParamId::OscBOn, 0.0);
                s.set_param(ParamId::OscAUniVoices, voices as f32);
                s.set_param(ParamId::OscAUniBlend, blend);
                s.note_on(0, 60, 1.0, 1);
                let mut l = vec![0.0; 128];
                let mut r = vec![0.0; 128];
                let mut peak: f32 = 0.0;
                for _ in 0..24 {
                    s.process(&mut l, &mut r);
                    peak = l.iter().fold(peak, |m, v| m.max(v.abs()));
                }
                assert!(peak > 0.05, "unison {voices} blend {blend}");
            }
        }
    }

    #[test]
    fn extreme_pitch_does_not_wedge_the_oscillator() {
        // An increment of a whole cycle or more used to leave the phase
        // accumulator permanently above 1. Above Nyquist the output is meant to
        // be silent, but the oscillator has to recover once the pitch drops.
        let mut s = synth(44_100.0, 128);
        s.set_param(ParamId::OscBOn, 0.0);
        s.set_param(ParamId::OscAOctave, 3.0);
        s.set_param(ParamId::OscASemi, 12.0);
        s.note_on(0, 120, 1.0, 1);
        let mut l = vec![0.0; 128];
        let mut r = vec![0.0; 128];
        for _ in 0..16 {
            s.process(&mut l, &mut r);
        }
        s.set_param(ParamId::OscAOctave, 0.0);
        s.set_param(ParamId::OscASemi, 0.0);
        let mut peak: f32 = 0.0;
        for _ in 0..24 {
            s.process(&mut l, &mut r);
            for v in &l {
                assert!(v.is_finite());
                peak = peak.max(v.abs());
            }
        }
        assert!(peak > 0.05, "{peak}");
    }

    /// Rising zero crossings per second of the left channel.
    fn crossing_rate(out: &Render, sample_rate: f32) -> f32 {
        let n = out.left.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        n as f32 * sample_rate / out.left.len() as f32
    }

    #[test]
    fn octave_and_semi_modulation_move_the_pitch() {
        // Stepped in the host, continuous under modulation; soft-limited at
        // the range edge like every target (base 0, headroom 12 st / 3 oct).
        let render = |target: Option<(&str, f32)>| {
            let mut s = synth(48_000.0, 128);
            s.shared().set_table(0, Arc::new(Wavetable::make_factory(0)));
            s.set_param(ParamId::OscATable, 0.0);
            s.set_param(ParamId::OscBOn, 0.0);
            if let Some((id, amount)) = target {
                add_row(&s, ModSource::ModWheel.index(), id, amount);
            }
            s.set_mod_wheel(1.0);
            s.note_on(0, 57, 1.0, 1);
            render_blocks(&mut s, 8, 128);
            crossing_rate(&render_blocks(&mut s, 150, 128), 48_000.0)
        };
        let base = render(None);
        assert!((base / 220.0 - 1.0).abs() < 0.02, "{base}");
        let semi = 12.0 * (24.0 * 0.25 / 12.0_f32).tanh();
        let up = render(Some(("oscA.semi", 0.25)));
        assert!((up / (base * (semi / 12.0).exp2()) - 1.0).abs() < 0.02, "{up}");
        let oct = 3.0 * (6.0 * -0.25 / 3.0_f32).tanh();
        let down = render(Some(("oscA.octave", -0.25)));
        assert!((down / (base * oct.exp2()) - 1.0).abs() < 0.02, "{down}");
    }

    #[test]
    fn polyphony_limit_steals_oldest_note() {
        let mut s = synth(48_000.0, 128);
        s.set_param(ParamId::MasterPolyphony, 8.0);
        for i in 0..12 {
            s.note_on(0, 48 + i, 1.0, i as i32);
            render_blocks(&mut s, 1, 128);
        }
        render_blocks(&mut s, 2, 128);
        // 8 sounding voices, plus fading stolen ones which must not exceed the reserve.
        assert!(s.active_voice_count() <= Synth::VOICE_POOL);
        assert!(s.active_voice_count() >= 8);
        render_blocks(&mut s, 20, 128);
        assert_eq!(s.active_voice_count(), 8);
    }

    #[test]
    fn stealing_does_not_click() {
        let mut s = synth(48_000.0, 64);
        // Sine: any jump is a real click.
        s.shared().set_table(0, Arc::new(Wavetable::make_factory(0)));
        s.set_param(ParamId::MasterPolyphony, 8.0);
        s.set_param(ParamId::OscALevel, 0.3);
        for i in 0..8 {
            s.note_on(0, 40 + i * 2, 1.0, i as i32);
        }
        render_blocks(&mut s, 20, 64);
        // Forces a steal.
        s.note_on(0, 72, 1.0, 99);
        let out = render_blocks(&mut s, 10, 64);
        assert!(out.finite);
        let max_jump = out.left.windows(2).fold(0.0_f32, |m, w| m.max((w[1] - w[0]).abs()));
        assert!(max_jump < 0.15, "{max_jump}");
    }

    #[test]
    fn all_oversampling_factors_render_cleanly() {
        for os in [0.0, 1.0, 2.0] {
            let mut s = synth(48_000.0, 128);
            s.set_param(ParamId::MasterOversampling, os);
            s.set_param(ParamId::FilterType, FilterType::LadderLp as usize as f32);
            s.set_param(ParamId::FilterRes, 0.9);
            s.set_param(ParamId::FilterDrive, 0.8);
            s.note_on(0, 60, 1.0, -1);
            let out = render_blocks(&mut s, 20, 128);
            assert!(out.finite, "{os}");
            assert!(out.peak > 0.01, "{os}");
            assert!(out.peak < 8.0, "{os}");
        }
    }

    #[test]
    fn oversampling_switch_at_runtime_stays_stable() {
        let mut s = synth(48_000.0, 128);
        s.note_on(0, 57, 1.0, -1);
        for os in [0.0, 2.0, 1.0, 0.0] {
            s.set_param(ParamId::MasterOversampling, os);
            let out = render_blocks(&mut s, 5, 128);
            assert!(out.finite, "{os}");
        }
    }

    #[test]
    fn modulation_is_clamped_at_the_target() {
        let mut s = synth(48_000.0, 128);
        // Four rows all pushing level far past its maximum must stay bounded.
        for _ in 0..4 {
            add_row(&s, ModSource::Velocity.index(), "oscA.level", 1.0);
        }
        s.set_param(ParamId::OscALevel, 1.0);
        s.note_on(0, 60, 1.0, -1);
        let out = render_blocks(&mut s, 20, 128);
        assert!(out.finite);
        assert!(out.peak < 4.0, "{}", out.peak);
    }

    #[test]
    fn pitch_bend_and_expression_affect_output() {
        let mut flat_synth = synth(48_000.0, 128);
        flat_synth.note_on(0, 60, 1.0, 7);
        let flat = render_blocks(&mut flat_synth, 6, 128);

        let mut bent = synth(48_000.0, 128);
        bent.set_pitch_bend(1.0);
        bent.note_on(0, 60, 1.0, 7);
        let sharp = render_blocks(&mut bent, 6, 128);
        assert!(flat.left.iter().zip(&sharp.left).any(|(a, b)| (a - b).abs() > 1.0e-4));

        let mut expr = synth(48_000.0, 128);
        expr.note_on(0, 60, 1.0, 7);
        // Volume 0 silences the note.
        expr.set_note_expression(NoteExpressionKind::Volume, 0, 60, 7, 0.0);
        let silent = render_blocks(&mut expr, 6, 128);
        assert!(silent.peak < 1.0e-6, "{}", silent.peak);
    }

    #[test]
    fn midi_cc_source_drives_modulation() {
        let mut quiet_synth = synth(48_000.0, 128);
        add_row(&quiet_synth, cc_source_index(74), "oscA.level", -1.0);
        quiet_synth.note_on(0, 60, 1.0, -1);
        let quiet = render_blocks(&mut quiet_synth, 10, 128);

        let mut open = synth(48_000.0, 128);
        add_row(&open, cc_source_index(74), "oscA.level", -1.0);
        open.set_control_change(74, 1.0);
        open.note_on(0, 60, 1.0, -1);
        let quieter = render_blocks(&mut open, 10, 128);
        assert!(quieter.peak < quiet.peak, "{} vs {}", quieter.peak, quiet.peak);
    }

    #[test]
    fn tail_reflects_fx_settings() {
        let mut s = synth(48_000.0, 128);
        let dry = s.tail_seconds();
        s.set_param(ParamId::FxRevOn, 1.0);
        s.set_param(ParamId::FxRevSize, 1.0);
        // No-op block; params are applied on the next render.
        s.process(&mut [], &mut []);
        let mut l = vec![0.0; 128];
        let mut r = vec![0.0; 128];
        s.process(&mut l, &mut r);
        assert!(s.tail_seconds() > dry);
    }

    #[test]
    fn high_sample_rates_work() {
        for rate in [44_100.0, 96_000.0, 192_000.0] {
            let mut s = synth(rate, 256);
            s.note_on(0, 69, 1.0, -1);
            let out = render_blocks(&mut s, 10, 256);
            assert!(out.finite, "{rate}");
            assert!(out.peak > 0.01, "{rate}");
        }
    }

    #[test]
    fn all_notes_off_stops_everything() {
        let mut s = synth(48_000.0, 128);
        s.set_param(ParamId::EnvAmpRelease, 0.01);
        for i in 0..6 {
            s.note_on(0, 50 + i, 1.0, i as i32);
        }
        render_blocks(&mut s, 4, 128);
        s.all_notes_off();
        render_blocks(&mut s, 30, 128);
        assert_eq!(s.active_voice_count(), 0);

        for i in 0..6 {
            s.note_on(0, 50 + i, 1.0, i as i32);
        }
        render_blocks(&mut s, 2, 128);
        s.all_sound_off();
        assert_eq!(s.active_voice_count(), 0);
    }

    #[test]
    fn matrix_row_order_does_not_change_output() {
        // Modulation sums per target, so grouping rows by target internally
        // must not depend on the order the rows were added in.
        let render = |interleaved: bool| {
            let mut s = synth(48_000.0, 64);
            s.set_param(ParamId::OscALevel, 0.8);
            let rows: [(ModSource, &str, f32); 5] = if interleaved {
                [
                    (ModSource::Velocity, "oscA.level", -0.3),
                    (ModSource::Lfo1, "filter.cutoff", 0.5),
                    (ModSource::EnvMod, "oscA.level", -0.2),
                    (ModSource::Lfo2, "oscA.wtPos", 0.4),
                    (ModSource::ModWheel, "filter.cutoff", 0.25),
                ]
            } else {
                [
                    (ModSource::Velocity, "oscA.level", -0.3),
                    (ModSource::EnvMod, "oscA.level", -0.2),
                    (ModSource::Lfo1, "filter.cutoff", 0.5),
                    (ModSource::ModWheel, "filter.cutoff", 0.25),
                    (ModSource::Lfo2, "oscA.wtPos", 0.4),
                ]
            };
            for (src, target, amount) in rows {
                add_row(&s, src.index(), target, amount);
            }
            s.set_mod_wheel(0.7);
            s.note_on(0, 60, 0.9, 1);
            render_blocks(&mut s, 20, 64)
        };
        let grouped = render(false);
        let mixed = render(true);
        assert_eq!(grouped.left.len(), mixed.left.len());
        for i in 0..grouped.left.len() {
            assert_eq!(grouped.left[i].to_bits(), mixed.left[i].to_bits(), "{i}");
        }
    }

    #[test]
    fn modulation_lands_on_the_intended_target() {
        // A row that closes osc A's level must still close it when other rows,
        // pointing at other targets, sit in front of it in the list.
        let render = |include_level_row: bool| {
            let mut s = synth(48_000.0, 64);
            s.set_param(ParamId::OscALevel, 1.0);
            s.set_param(ParamId::EnvAmpAttack, 0.0);
            add_row(&s, ModSource::Lfo1.index(), "filter.cutoff", 0.5);
            add_row(&s, ModSource::Lfo2.index(), "filter.res", 0.3);
            if include_level_row {
                add_row(&s, ModSource::Velocity.index(), "oscA.level", -1.0);
            }
            s.note_on(0, 60, 1.0, 1);
            render_blocks(&mut s, 20, 64)
        };
        let loud = render(false);
        let quiet = render(true);
        assert!(loud.peak > 0.2, "{}", loud.peak);
        assert!(quiet.peak < loud.peak * 0.35, "{} vs {}", quiet.peak, loud.peak);
    }

    #[test]
    fn zero_depth_modulation_leaves_the_target_alone() {
        // SPEC 4.3's soft limit is applied to the modulation excursion, not to
        // the absolute value: a row at (almost) zero depth must sound like no
        // row at all. Limiting around the range centre instead made an
        // infinitesimal offset snap the parameter onto the tanh curve - wtPos
        // 0 % jumped to 11 %, attack 5 ms to over a second - so every bipolar
        // source clicked as it crossed zero.
        let render = |amount: f32| {
            let mut s = synth(48_000.0, 64);
            // A multi-frame table, so wtPos matters.
            s.set_param(ParamId::OscATable, 6.0);
            s.set_param(ParamId::OscAWtPos, 0.0);
            s.shared().set_table(0, Arc::new(Wavetable::make_factory(6)));
            if amount != 0.0 {
                add_row(&s, ModSource::Velocity.index(), "oscA.wtPos", amount);
            }
            s.note_on(0, 60, 1.0, 1);
            render_blocks(&mut s, 20, 64)
        };
        let unmodulated = render(0.0);
        let tiny = render(1.0e-4);
        assert_eq!(unmodulated.left.len(), tiny.left.len());
        let diff = unmodulated.left.iter().zip(&tiny.left).fold(0.0_f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(unmodulated.peak > 0.05);
        assert!(diff < 1.0e-3, "{diff}");
    }

    #[test]
    fn envelope_time_modulation_stays_continuous_around_zero() {
        // Same fix seen from the amp envelope: a mod row that is muted by depth
        // must not stretch a 5 ms attack into a slow fade-in.
        let peak = |amount: f32| {
            let mut s = synth(48_000.0, 64);
            s.set_param(ParamId::EnvAmpAttack, 0.005);
            if amount != 0.0 {
                add_row(&s, ModSource::Velocity.index(), "envAmp.attack", amount);
            }
            s.note_on(0, 60, 1.0, 1);
            render_blocks(&mut s, 8, 64).peak
        };
        let plain = peak(0.0);
        let nudged = peak(1.0e-4);
        assert!(plain > 0.05);
        assert!((nudged - plain).abs() < plain * 0.05, "{nudged} vs {plain}");
    }

    // ---- Beyond the C++ suite: the shared-state contract ----------------------

    #[test]
    fn feedback_follows_a_playing_note() {
        let mut s = synth(48_000.0, 128);
        s.shared().feedback.gui_attached.store(true, RELAXED);
        add_row(&s, ModSource::Lfo1.index(), "filter.cutoff", 0.5);
        s.note_on(0, 60, 1.0, 1);
        render_blocks(&mut s, 10, 128);
        let fb = &s.shared().feedback;
        assert_eq!(fb.voice_count.load(RELAXED), 1);
        assert!(fb.meters[0].load(RELAXED) > 0.01);
        assert!(fb.osc_meters[0].load(RELAXED) > 0.01);
        assert_ne!(fb.env_stage[0].load(RELAXED), 0);
        assert!(fb.env_level[0].load(RELAXED) > 0.1);
        assert!(fb.row_activity[0].load(RELAXED) >= 0.0);
        let mut wet = [0.0; 256];
        fb.analyzer_wet.copy_latest(&mut wet);
        assert!(wet.iter().any(|v| v.abs() > 1.0e-3));
        let cutoff = fb.cutoff_hz.load(RELAXED);
        // The voice clamps at 0.45 of its (oversampled) rate.
        assert!((20.0..=48_000.0 * 4.0 * 0.45).contains(&cutoff), "{cutoff}");
    }

    #[test]
    fn replaced_tables_are_freed_off_the_audio_thread() {
        let mut s = synth(48_000.0, 64);
        s.note_on(0, 60, 1.0, 1);
        render_blocks(&mut s, 2, 64);
        let first = s.shared().table(0);
        let weak = Arc::downgrade(&first);
        drop(first);
        s.shared().set_table(0, Arc::new(Wavetable::make_factory(1)));
        // The audio thread still holds the old table, so it must still exist.
        assert!(weak.upgrade().is_some());
        render_blocks(&mut s, 1, 64);
        // The audio thread let go of it; it is parked, not freed there.
        assert!(weak.upgrade().is_some());
        // The next publish on a non-audio thread frees it.
        s.shared().set_table(0, Arc::new(Wavetable::make_factory(2)));
        assert!(weak.upgrade().is_none());
    }
}
