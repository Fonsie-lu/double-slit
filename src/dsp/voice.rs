//! One polyphonic voice: 2 wavetable oscillators + sub/noise, filter, 2
//! envelopes, 2 voice-scope LFOs and the per-sample modulation evaluation
//! (SPEC 3.1, 4.3).

use std::sync::{Arc, OnceLock};

use super::common::{clamp01, fast_exp2, fast_sin01, fast_tanh, lerp, midi_to_hz_fast, Rng, MATRIX_SLOTS, PI};
use super::envelope::{Envelope, Stage};
use super::filters::MultiFilter;
use super::lfo::Lfo;
use super::mod_matrix::{apply_mod_curve, mod_target_index_for_param, mod_targets, ModCurve, ModSource, Snapshot, NUM_FIXED_SOURCES};
use super::oscillator::{resolve_frame, FrameData, OscInput, Oscillator, StereoSample};
use super::params::{
    ranges, time_div_beats, LfoRetrig, LfoShape, LfoSync, NoiseMode, ParamId, PhaseMode, SubMode, SyncMode,
    NUM_PARAMS,
};
use super::wavetable::Wavetable;

const VIBRATO_RATE_HZ: f32 = 5.0;
const FILTER_ENV_OCTAVES: f32 = 8.0;
const MAX_MOD_TARGETS: usize = 64;

/// Engine-wide modulation sources shared by all voices (filled per block).
#[derive(Clone, Copy, Debug)]
pub struct GlobalMod {
    pub mod_wheel: f32,
    /// -1 … 1
    pub pitch_bend: f32,
    pub aftertouch: f32,
    pub cc: [f32; 128],
    /// Free-running (retrigger = Free) LFO values.
    pub lfo: [f32; 2],
    pub tempo_bpm: f32,
    /// True while an editor is open: voice LFOs and the MOD envelope then keep
    /// running even when nothing listens to them, so their displays stay live.
    pub gui_attached: bool,
}

impl Default for GlobalMod {
    fn default() -> Self {
        Self {
            mod_wheel: 0.0,
            pitch_bend: 0.0,
            aftertouch: 0.0,
            cc: [0.0; 128],
            lfo: [0.0; 2],
            tempo_bpm: 120.0,
            gui_attached: false,
        }
    }
}

/// Per-note expression state (CLAP note expressions, SPEC 7).
#[derive(Clone, Copy, Debug)]
pub struct NoteExpression {
    pub volume: f32,
    /// -1 … 1 offset.
    pub pan: f32,
    /// Semitones.
    pub tuning: f32,
    /// 0 … 1 depth.
    pub vibrato: f32,
    /// Per-note aftertouch.
    pub pressure: f32,
}

impl Default for NoteExpression {
    fn default() -> Self {
        Self { volume: 1.0, pan: 0.0, tuning: 0.0, vibrato: 0.0, pressure: 0.0 }
    }
}

/// Everything a voice reads but does not own, wired once per block.
#[derive(Clone, Copy)]
pub struct VoiceContext<'a> {
    pub params: &'a [f32; NUM_PARAMS],
    pub globals: &'a GlobalMod,
    pub snapshot: &'a Snapshot,
    pub table_a: &'a Wavetable,
    pub table_b: &'a Wavetable,
    pub lfo_shapes: [Option<&'a Arc<Vec<f32>>>; 2],
}

/// Modulation target index per parameter, -1 when not a target.
fn target_index_table() -> &'static [i16; NUM_PARAMS] {
    static TABLE: OnceLock<[i16; NUM_PARAMS]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|i| mod_target_index_for_param(ParamId::from_index(i)).map_or(-1, |t| t as i16))
    })
}

/// Index into the per-sample source cache, or -1 for block-rate sources.
fn audio_rate_source_index(source: usize) -> i8 {
    match source {
        s if s == ModSource::EnvAmp.index() => 0,
        s if s == ModSource::EnvMod.index() => 1,
        s if s == ModSource::Lfo1.index() => 2,
        s if s == ModSource::Lfo2.index() => 3,
        s if s == ModSource::RandomVoiceCycle.index() => 4,
        _ => -1,
    }
}

/// Sources whose value can change within a block.
fn is_audio_rate_source(source: usize) -> bool {
    audio_rate_source_index(source) >= 0
}

#[inline]
fn equal_power_pan(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * 0.25 * PI;
    (angle.cos(), angle.sin())
}

#[inline]
fn choice(v: f32) -> usize {
    v.round().max(0.0) as usize
}

fn phase_mode(v: f32) -> PhaseMode {
    match choice(v) {
        0 => PhaseMode::Free,
        1 => PhaseMode::Random,
        _ => PhaseMode::Reset,
    }
}

fn sync_mode(v: f32) -> SyncMode {
    match choice(v) {
        0 => SyncMode::Off,
        1 => SyncMode::ToOscA,
        _ => SyncMode::SelfSync,
    }
}

fn noise_mode(v: f32) -> NoiseMode {
    match choice(v) {
        0 => NoiseMode::Off,
        1 => NoiseMode::White,
        _ => NoiseMode::Pink,
    }
}

fn sub_mode(v: f32) -> SubMode {
    match choice(v) {
        0 => SubMode::Sine,
        1 => SubMode::Triangle,
        2 => SubMode::White,
        _ => SubMode::Pink,
    }
}

fn lfo_retrig(v: f32) -> LfoRetrig {
    match choice(v) {
        0 => LfoRetrig::Free,
        1 => LfoRetrig::Voice,
        _ => LfoRetrig::Note,
    }
}

/// Row data pre-scaled for the per-sample loop.
#[derive(Clone, Copy, Debug, Default)]
struct FastRow {
    source: usize,
    target: usize,
    /// amount * target range
    scale: f32,
    curve: ModCurve,
    /// Slot in the per-sample source cache, -1 = block rate.
    fast_index: i8,
}

#[derive(Clone)]
pub struct Voice {
    osc_a: Oscillator,
    osc_b: Oscillator,
    filter: MultiFilter,
    env_amp: Envelope,
    env_mod: Envelope,
    lfo1: Lfo,
    lfo2: Lfo,

    /// Oscillator / filter rate.
    sample_rate: f32,
    /// Envelope / LFO / matrix rate.
    modulation_rate: f32,
    active: bool,
    releasing: bool,
    midi_note: u8,
    midi_channel: u8,
    note_id: i32,
    order: u64,
    velocity: f32,
    glide_pitch: f32,
    glide_target: f32,
    glide_coeff: f32,
    steal_fade: u32,
    steal_fade_length: u32,

    expr: NoteExpression,

    sub_phase: f32,
    vibrato_phase: f32,
    pink_state: [f32; 3],
    rng: Rng,
    random_note: f32,
    random_cycle: f32,
    lfo1_phase_tracker: f32,

    mod_offsets: [f32; MAX_MOD_TARGETS],
    fast_rows: [FastRow; MATRIX_SLOTS],
    num_fast_rows: usize,
    used_targets: [usize; MATRIX_SLOTS],
    group_first_row: [usize; MATRIX_SLOTS],
    group_row_count: [usize; MATRIX_SLOTS],
    num_used_targets: usize,

    filter_cutoff_smoothed: f32,
    last_amp_env: f32,
    last_mod_env: f32,
    last_osc_peak: [f32; 2],
    last_lfo: [f32; 2],
    use_voice_lfo: [bool; 2],
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            osc_a: Oscillator::default(),
            osc_b: Oscillator::default(),
            filter: MultiFilter::default(),
            env_amp: Envelope::default(),
            env_mod: Envelope::default(),
            lfo1: Lfo::default(),
            lfo2: Lfo::default(),
            sample_rate: 48_000.0,
            modulation_rate: 48_000.0,
            active: false,
            releasing: false,
            midi_note: 60,
            midi_channel: 0,
            note_id: -1,
            order: 0,
            velocity: 1.0,
            glide_pitch: 60.0,
            glide_target: 60.0,
            glide_coeff: 1.0,
            steal_fade: 0,
            steal_fade_length: 1,
            expr: NoteExpression::default(),
            sub_phase: 0.0,
            vibrato_phase: 0.0,
            pink_state: [0.0; 3],
            rng: Rng::new(0x2545_f491),
            random_note: 0.0,
            random_cycle: 0.0,
            lfo1_phase_tracker: 0.0,
            mod_offsets: [0.0; MAX_MOD_TARGETS],
            fast_rows: [FastRow::default(); MATRIX_SLOTS],
            num_fast_rows: 0,
            used_targets: [0; MATRIX_SLOTS],
            group_first_row: [0; MATRIX_SLOTS],
            group_row_count: [0; MATRIX_SLOTS],
            num_used_targets: 0,
            filter_cutoff_smoothed: 20_000.0,
            last_amp_env: 0.0,
            last_mod_env: 0.0,
            last_osc_peak: [0.0; 2],
            last_lfo: [0.0; 2],
            use_voice_lfo: [true; 2],
        }
    }
}

impl Voice {
    /// Allocates for `max_sample_rate`; call once, off the audio thread.
    pub fn prepare(&mut self, max_sample_rate: f64) {
        debug_assert!(mod_targets().len() <= MAX_MOD_TARGETS);
        let _ = target_index_table();
        self.filter.prepare(max_sample_rate);
        self.osc_a.prepare(max_sample_rate);
        self.osc_b.prepare(max_sample_rate);
        self.env_amp.prepare(max_sample_rate);
        self.env_mod.prepare(max_sample_rate);
        self.lfo1.prepare(max_sample_rate);
        self.lfo2.prepare(max_sample_rate);
        self.set_sample_rate(max_sample_rate, max_sample_rate);
        self.reset();
    }

    /// Oversampling switch: oscillators and filter run at `oversampled_rate`,
    /// while envelopes, LFOs and the matrix run at `base_rate` (SPEC 4.3 block
    /// rate). Called when the factor changes, possibly mid-note, so the voice
    /// keeps its envelope stage, phases and active flag.
    pub fn set_sample_rate(&mut self, oversampled_rate: f64, base_rate: f64) {
        self.sample_rate = oversampled_rate as f32;
        self.modulation_rate = base_rate as f32;
        self.osc_a.set_sample_rate(oversampled_rate);
        self.osc_b.set_sample_rate(oversampled_rate);
        self.filter.set_sample_rate(oversampled_rate);
        // Envelopes, LFOs and the matrix are block-rate sources: no need to run
        // them at the oversampled rate, and it keeps the modulation cost flat with 4x.
        self.env_amp.set_sample_rate(base_rate);
        self.env_mod.set_sample_rate(base_rate);
        self.lfo1.set_sample_rate(base_rate);
        self.lfo2.set_sample_rate(base_rate);
        // 10 ms steal fade.
        self.steal_fade_length = ((self.sample_rate * 0.01) as u32).max(1);
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.releasing = false;
        self.steal_fade = 0;
        self.env_amp.reset();
        self.env_mod.reset();
        self.osc_a.reset();
        self.osc_b.reset();
        self.filter.reset();
        self.lfo1.reset(0.0);
        self.lfo2.reset(0.0);
        self.sub_phase = 0.0;
        self.vibrato_phase = 0.0;
        self.pink_state = [0.0; 3];
        self.mod_offsets = [0.0; MAX_MOD_TARGETS];
        self.last_amp_env = 0.0;
        self.last_mod_env = 0.0;
        self.last_lfo = [0.0; 2];
        self.filter_cutoff_smoothed = 20_000.0;
    }

    // ---- Allocation --------------------------------------------------------

    /// `glide_from_note` is `None` when there is no previous note to glide from.
    #[allow(clippy::too_many_arguments)]
    pub fn note_on(
        &mut self,
        ctx: &VoiceContext,
        midi_note: u8,
        velocity: f32,
        channel: u8,
        note_id: i32,
        seed: u32,
        glide_from_note: Option<f32>,
    ) {
        self.midi_note = midi_note.min(127);
        self.midi_channel = channel;
        self.note_id = note_id;
        self.velocity = clamp01(velocity);
        self.active = true;
        self.releasing = false;
        self.steal_fade = 0;
        self.expr = NoteExpression::default();

        self.rng.seed(seed);
        self.random_note = self.rng.next_bipolar();
        self.random_cycle = self.rng.next_bipolar();
        self.osc_a.seed(seed.wrapping_mul(2_654_435_761).wrapping_add(1));
        self.osc_b.seed(seed.wrapping_mul(40_503).wrapping_add(7));
        self.lfo1.seed(seed ^ 0x5bf0_3635);
        self.lfo2.seed(seed ^ 0x9e37_79b9);

        self.glide_target = self.midi_note as f32;
        self.glide_pitch = glide_from_note.unwrap_or(self.glide_target);

        // Matrix offsets are from the last render; a fresh voice starts clean.
        self.mod_offsets = [0.0; MAX_MOD_TARGETS];
        self.update_voice_rates(ctx);

        let p = ctx.params;
        self.osc_a.start(phase_mode(p[ParamId::OscAPhaseMode.index()]), p[ParamId::OscAPhase.index()]);
        self.osc_b.start(phase_mode(p[ParamId::OscBPhaseMode.index()]), p[ParamId::OscBPhase.index()]);
        self.sub_phase = 0.0;
        self.vibrato_phase = 0.0;

        if lfo_retrig(p[ParamId::Lfo1Retrig.index()]) != LfoRetrig::Free {
            self.lfo1.reset(p[ParamId::Lfo1Phase.index()] / 360.0);
        }
        if lfo_retrig(p[ParamId::Lfo2Retrig.index()]) != LfoRetrig::Free {
            self.lfo2.reset(p[ParamId::Lfo2Phase.index()] / 360.0);
        }

        self.env_amp.note_on();
        self.env_mod.note_on();
        self.filter.reset();
        self.filter_cutoff_smoothed = p[ParamId::FilterCutoff.index()].clamp(20.0, self.sample_rate * 0.45);
    }

    pub fn note_off(&mut self) {
        if !self.active {
            return;
        }
        self.releasing = true;
        self.env_amp.note_off();
        self.env_mod.note_off();
    }

    /// Starts the 10 ms steal fade (SPEC 12: avoids clicks with big unison stacks).
    pub fn steal(&mut self) {
        if self.active && self.steal_fade == 0 {
            self.steal_fade = self.steal_fade_length;
        }
    }

    pub fn kill(&mut self) {
        self.active = false;
        self.releasing = false;
        self.steal_fade = 0;
        self.env_amp.kill();
        self.env_mod.kill();
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn is_releasing(&self) -> bool {
        self.releasing
    }
    pub fn is_stealing(&self) -> bool {
        self.steal_fade > 0
    }
    pub fn note(&self) -> u8 {
        self.midi_note
    }
    pub fn channel(&self) -> u8 {
        self.midi_channel
    }
    pub fn note_id(&self) -> i32 {
        self.note_id
    }
    pub fn start_order(&self) -> u64 {
        self.order
    }
    pub fn set_start_order(&mut self, order: u64) {
        self.order = order;
    }
    pub fn expression(&mut self) -> &mut NoteExpression {
        &mut self.expr
    }

    // ---- GUI feedback ------------------------------------------------------

    pub fn amp_env_value(&self) -> f32 {
        self.last_amp_env
    }
    pub fn mod_env_value(&self) -> f32 {
        self.last_mod_env
    }
    pub fn amp_env_stage(&self) -> Stage {
        self.env_amp.stage()
    }
    pub fn mod_env_stage(&self) -> Stage {
        self.env_mod.stage()
    }
    /// Peak level each oscillator contributed during the last render call.
    pub fn osc_peak(&self, osc: usize) -> f32 {
        self.last_osc_peak[osc.min(1)]
    }
    pub fn lfo_value(&self, index: usize) -> f32 {
        self.last_lfo[index.min(1)]
    }
    pub fn osc_frame(&self, osc: usize) -> usize {
        if osc == 0 {
            self.osc_a.last_frame()
        } else {
            self.osc_b.last_frame()
        }
    }
    /// Smoothed cutoff the filter is running at.
    pub fn cutoff(&self) -> f32 {
        self.filter_cutoff_smoothed
    }

    // ---- Modulation --------------------------------------------------------

    #[inline(always)]
    fn mod_of(&self, id: ParamId) -> f32 {
        let t = target_index_table()[id.index()];
        if t < 0 {
            0.0
        } else {
            self.mod_offsets[t as usize]
        }
    }

    /// Parameter value with modulation applied and range-limited (soft clipped).
    #[inline(always)]
    fn modulated(&self, p: &[f32; NUM_PARAMS], id: ParamId) -> f32 {
        let base = p[id.index()];
        let offset = self.mod_of(id);
        if offset == 0.0 {
            return base;
        }
        // SPEC 4.3: modulation is soft-clipped so it never hard-clips at the
        // range edge. The limit has to be applied to the *excursion*, not to the
        // absolute value: soft-clipping around the range centre made an
        // infinitesimal modulation snap the parameter onto the tanh curve
        // (oscA.wtPos 0 -> 11 %, envAmp.attack 5 ms -> 1.1 s), which clicks every
        // time a bipolar source crosses zero. Scaling by the headroom on the side
        // we are moving towards keeps offset = 0 exactly at the base value and
        // unity slope around it, while still approaching the parameter limit
        // asymptotically.
        let range = &ranges()[id.index()];
        let headroom = if offset > 0.0 {
            (range.centre + range.half_span) - base
        } else {
            base - (range.centre - range.half_span)
        };
        if headroom <= 1.0e-9 {
            return base;
        }
        base + headroom * fast_tanh(offset / headroom)
    }

    /// Current value of a modulation source for this voice.
    pub fn source_value(&self, source: usize, globals: &GlobalMod) -> f32 {
        match source {
            s if s == ModSource::None.index() => 0.0,
            s if s == ModSource::Velocity.index() => self.velocity,
            s if s == ModSource::Aftertouch.index() => globals.aftertouch,
            s if s == ModSource::PolyAftertouch.index() => self.expr.pressure,
            s if s == ModSource::ModWheel.index() => globals.mod_wheel,
            s if s == ModSource::PitchBend.index() => globals.pitch_bend,
            s if s == ModSource::NotePitch.index() => ((self.midi_note as f32 - 60.0) / 48.0).clamp(-1.0, 1.0),
            s if s == ModSource::NoteGate.index() => {
                if self.releasing {
                    0.0
                } else {
                    1.0
                }
            }
            s if s == ModSource::EnvAmp.index() => self.last_amp_env,
            s if s == ModSource::EnvMod.index() => self.last_mod_env,
            s if s == ModSource::Lfo1.index() => self.last_lfo[0],
            s if s == ModSource::Lfo2.index() => self.last_lfo[1],
            s if s == ModSource::RandomNote.index() => self.random_note,
            s if s == ModSource::RandomVoiceCycle.index() => self.random_cycle,
            s => {
                let cc = s as isize - NUM_FIXED_SOURCES as isize + 1;
                if (1..=119).contains(&cc) {
                    globals.cc[cc as usize]
                } else {
                    0.0
                }
            }
        }
    }

    /// Rebuilds the compact row list from the snapshot; once per block.
    fn compile_rows(&mut self, snapshot: &Snapshot) {
        self.num_fast_rows = 0;
        self.num_used_targets = 0;
        let targets = mod_targets();

        // Pass 1: distinct targets, in first-seen order.
        for row in snapshot.rows() {
            if !self.used_targets[..self.num_used_targets].contains(&row.target) {
                self.used_targets[self.num_used_targets] = row.target;
                self.num_used_targets += 1;
            }
        }

        // Pass 2: rows grouped by target, so the per-sample loop accumulates
        // each target in a register and stores it exactly once.
        for g in 0..self.num_used_targets {
            self.group_first_row[g] = self.num_fast_rows;
            self.group_row_count[g] = 0;
            for row in snapshot.rows() {
                if row.target != self.used_targets[g] {
                    continue;
                }
                self.fast_rows[self.num_fast_rows] = FastRow {
                    source: row.source,
                    target: row.target,
                    scale: row.amount * targets[row.target].range,
                    curve: row.curve,
                    fast_index: audio_rate_source_index(row.source),
                };
                self.num_fast_rows += 1;
                self.group_row_count[g] += 1;
            }
        }
    }

    fn evaluate_matrix(&mut self, globals: &GlobalMod) {
        // Audio-rate source values are refreshed once per sample and shared by rows.
        let fast = [self.last_amp_env, self.last_mod_env, self.last_lfo[0], self.last_lfo[1], self.random_cycle];
        for g in 0..self.num_used_targets {
            let first = self.group_first_row[g];
            let last = first + self.group_row_count[g];
            let mut sum = 0.0;
            for r in first..last {
                let row = self.fast_rows[r];
                let value = if row.fast_index >= 0 {
                    fast[row.fast_index as usize]
                } else {
                    self.source_value(row.source, globals)
                };
                sum += if row.curve == ModCurve::Linear { value } else { apply_mod_curve(value, row.curve) } * row.scale;
            }
            self.mod_offsets[self.used_targets[g]] = sum;
        }
    }

    fn update_voice_rates(&mut self, ctx: &VoiceContext) {
        use ParamId::*;
        let p = ctx.params;
        self.env_amp.set_times(
            self.modulated(p, EnvAmpAttack),
            self.modulated(p, EnvAmpDecay),
            self.modulated(p, EnvAmpSustain),
            self.modulated(p, EnvAmpRelease),
        );
        self.env_amp.set_curves(
            p[EnvAmpAttackCurve.index()],
            p[EnvAmpDecayCurve.index()],
            p[EnvAmpReleaseCurve.index()],
        );
        self.env_mod.set_times(
            self.modulated(p, EnvModAttack),
            self.modulated(p, EnvModDecay),
            self.modulated(p, EnvModSustain),
            self.modulated(p, EnvModRelease),
        );
        self.env_mod.set_curves(
            p[EnvModAttackCurve.index()],
            p[EnvModDecayCurve.index()],
            p[EnvModReleaseCurve.index()],
        );

        let bpm = ctx.globals.tempo_bpm.max(10.0);
        let ids = [
            (Lfo1Rate, Lfo1Shape, Lfo1SyncMode, Lfo1SyncDiv, Lfo1Retrig, Lfo1Smooth),
            (Lfo2Rate, Lfo2Shape, Lfo2SyncMode, Lfo2SyncDiv, Lfo2Retrig, Lfo2Smooth),
        ];
        for (i, (rate, shape, sync, div, retrig, smooth)) in ids.into_iter().enumerate() {
            let mut hz = p[rate.index()];
            if choice(p[sync.index()]) == LfoSync::Tempo as usize {
                let beats = time_div_beats(choice(p[div.index()]));
                hz = bpm / 60.0 / beats.max(0.01);
            }
            let offset = self.mod_of(rate);
            if offset != 0.0 {
                hz *= fast_exp2(offset);
            }
            let lfo = if i == 0 { &mut self.lfo1 } else { &mut self.lfo2 };
            lfo.set_rate(hz.clamp(0.0, 400.0));
            lfo.set_shape(LfoShape::from_value(p[shape.index()]));
            lfo.set_smoothing(p[smooth.index()]);
            lfo.set_custom_shape(ctx.lfo_shapes[i]);
            self.use_voice_lfo[i] = lfo_retrig(p[retrig.index()]) != LfoRetrig::Free;
        }

        let detune_a = self.modulated(p, OscAUniDetune);
        let detune_b = self.modulated(p, OscBUniDetune);
        self.osc_a.set_unison(
            (p[OscAUniVoices.index()] + 0.5) as usize,
            detune_a,
            p[OscAUniSpread.index()],
            p[OscAUniBlend.index()],
        );
        self.osc_b.set_unison(
            (p[OscBUniVoices.index()] + 0.5) as usize,
            detune_b,
            p[OscBUniSpread.index()],
            p[OscBUniBlend.index()],
        );
        self.osc_b.set_sync_mode(sync_mode(p[OscBSyncMode.index()]));
        self.osc_b.set_noise_mode(noise_mode(p[OscBNoiseMode.index()]));

        let glide = p[MasterGlide.index()];
        self.glide_coeff = if glide <= 0.0 { 1.0 } else { 1.0 - (-1.0 / (glide * self.modulation_rate)).exp() };
    }

    fn update_filter(&mut self, p: &[f32; NUM_PARAMS], num_samples: usize) {
        use ParamId::*;
        let mut cutoff = p[FilterCutoff.index()];
        let offset = self.mod_of(FilterCutoff);
        if offset != 0.0 {
            cutoff *= fast_exp2(offset);
        }

        let key_track = p[FilterKeyTrack.index()];
        if key_track > 0.0 {
            cutoff *= fast_exp2((self.midi_note as f32 - 60.0) / 12.0 * key_track);
        }

        let env_amount = p[FilterEnvAmount.index()];
        if env_amount != 0.0 {
            cutoff *= fast_exp2(env_amount * FILTER_ENV_OCTAVES * self.last_mod_env);
        }

        let cutoff = cutoff.clamp(20.0, self.sample_rate * 0.45);

        // 5 ms minimum ramp per SPEC 7 so cutoff jumps never click.
        let coeff = 1.0 - (-(num_samples as f32) / (0.005 * self.sample_rate)).exp();
        self.filter_cutoff_smoothed += clamp01(coeff) * (cutoff - self.filter_cutoff_smoothed);

        self.filter.set_params(
            super::params::FilterType::from_value(p[ParamId::FilterType.index()]),
            self.filter_cutoff_smoothed,
            clamp01(self.modulated(p, FilterRes)),
            clamp01(self.modulated(p, FilterDrive)),
            clamp01(self.modulated(p, FilterMix)),
            clamp01(self.modulated(p, FilterVowel)),
        );
    }

    /// Adds the voice output into the buffers. Called with chunk sizes small
    /// enough that the filter coefficients follow modulation closely. `dry`,
    /// when given, receives the pre-filter mix for the analyser.
    pub fn render(
        &mut self,
        ctx: &VoiceContext,
        left: &mut [f32],
        right: &mut [f32],
        mut dry: Option<&mut [f32]>,
        oversampling_factor: usize,
    ) {
        use ParamId::*;
        if !self.active {
            return;
        }
        let num_samples = left.len().min(right.len());
        let p = ctx.params;
        let globals = ctx.globals;

        self.compile_rows(ctx.snapshot);
        self.mod_offsets = [0.0; MAX_MOD_TARGETS];
        self.evaluate_matrix(globals);
        self.update_voice_rates(ctx);
        self.update_filter(p, num_samples);

        // SPEC 4.3: modulation is evaluated per sample only for audio-rate
        // sources (envelopes, LFOs, per-cycle random); everything else per block.
        let mut audio_rate = false;
        let mut modulated_target = [false; MAX_MOD_TARGETS];
        let gui_attached = globals.gui_attached;
        let mut lfo_needed = [gui_attached, gui_attached];
        let mut mod_env_used = gui_attached || p[FilterEnvAmount.index()] != 0.0;
        for row in &self.fast_rows[..self.num_fast_rows] {
            modulated_target[row.target] = true;
            audio_rate |= is_audio_rate_source(row.source);
            lfo_needed[0] |= row.source == ModSource::Lfo1.index() || row.source == ModSource::RandomVoiceCycle.index();
            lfo_needed[1] |= row.source == ModSource::Lfo2.index();
            mod_env_used |= row.source == ModSource::EnvMod.index();
        }
        let tidx = target_index_table();
        let is_dynamic = |id: ParamId| {
            let t = tidx[id.index()];
            audio_rate && t >= 0 && modulated_target[t as usize]
        };

        let osc_a_on = p[OscAOn.index()] > 0.5;
        let osc_b_on = p[OscBOn.index()] > 0.5;
        let filter_on = p[FilterOn.index()] > 0.5;
        let sub = sub_mode(p[OscCMode.index()]);
        let sync_ratio = p[OscBSyncRatio.index()];

        let bend_range = p[MasterBendRange.index()];
        let vel_gain = lerp(1.0, self.velocity, p[EnvAmpVelAmount.index()]);
        let vibrato_inc = VIBRATO_RATE_HZ / self.modulation_rate;
        let factor = oversampling_factor.max(1);
        let bend = globals.pitch_bend * bend_range;

        // Per-chunk values; the dynamic ones are refreshed inside the loop.
        let mut level_a = clamp01(self.modulated(p, OscALevel));
        let mut level_b = clamp01(self.modulated(p, OscBLevel));
        let mut wt_pos_a = self.modulated(p, OscAWtPos) * 0.01;
        let mut wt_pos_b = self.modulated(p, OscBWtPos) * 0.01;
        let mut phase_mod_a = self.mod_of(OscAPhase) / 360.0;
        let mut phase_mod_b = self.mod_of(OscBPhase) / 360.0;
        let mut fm_index = clamp01(self.modulated(p, OscBFmIndex));
        let mut am_index = clamp01(self.modulated(p, OscBAmIndex));
        let mut sub_level = clamp01(self.modulated(p, OscCLevel));
        // The 1/sqrt(2) of equal-power panning is folded into the gains up front.
        let master_pan = |v: &Self| {
            let (l, r) = equal_power_pan((v.modulated(p, MasterPan) + v.expr.pan).clamp(-1.0, 1.0));
            (l * std::f32::consts::SQRT_2, r * std::f32::consts::SQRT_2)
        };
        let (mut pan_gain_l, mut pan_gain_r) = master_pan(self);
        let (mut pan_a_l, mut pan_a_r) = equal_power_pan(self.modulated(p, OscAPan).clamp(-1.0, 1.0));
        let (mut pan_b_l, mut pan_b_r) = equal_power_pan(self.modulated(p, OscBPan).clamp(-1.0, 1.0));

        let dyn_level_a = is_dynamic(OscALevel);
        let dyn_level_b = is_dynamic(OscBLevel);
        let dyn_wt_a = is_dynamic(OscAWtPos);
        let dyn_wt_b = is_dynamic(OscBWtPos);
        let dyn_phase_a = is_dynamic(OscAPhase);
        let dyn_phase_b = is_dynamic(OscBPhase);
        let dyn_fm = is_dynamic(OscBFmIndex);
        let dyn_am = is_dynamic(OscBAmIndex);
        let dyn_sub_level = is_dynamic(OscCLevel);
        let dyn_master_pan = is_dynamic(MasterPan);
        let dyn_pan_a = is_dynamic(OscAPan);
        let dyn_pan_b = is_dynamic(OscBPan);

        let glide_moving = (self.glide_target - self.glide_pitch).abs() > 1.0e-4;
        let pitch_dynamic = glide_moving
            || self.expr.vibrato > 0.0
            || is_dynamic(OscAOctave)
            || is_dynamic(OscASemi)
            || is_dynamic(OscAFine)
            || is_dynamic(OscBOctave)
            || is_dynamic(OscBSemi)
            || is_dynamic(OscBFine)
            || is_dynamic(OscCPitch)
            || is_dynamic(MasterTune);

        let mut peak_a: f32 = 0.0;
        let mut peak_b: f32 = 0.0;

        let mut in_a = OscInput { sync_ratio: 1.0, ..Default::default() };
        let mut in_b = OscInput { sync_ratio, ..Default::default() };
        let mut sub_inc = 0.0;

        // Increments follow pitch per sample; the mip level is picked once per
        // block (a 32-sample step is far below any audible band-limit change).
        // Octave and semi are stepped in the host but modulate continuously.
        let inv_sample_rate = 1.0 / self.sample_rate;

        let update_pitch = |v: &Self, vibrato: f32, a: &mut OscInput, b: &mut OscInput, sub_inc: &mut f32| {
            let common = v.glide_pitch + bend + v.modulated(p, MasterTune) * 0.01 + v.expr.tuning + vibrato;
            let octave_a = v.modulated(p, OscAOctave) * 12.0 + v.modulated(p, OscASemi);
            let octave_b = v.modulated(p, OscBOctave) * 12.0 + v.modulated(p, OscBSemi);
            a.increment = midi_to_hz_fast(common + octave_a + v.modulated(p, OscAFine) * 0.01) * inv_sample_rate;
            b.increment = midi_to_hz_fast(common + octave_b + v.modulated(p, OscBFine) * 0.01) * inv_sample_rate;
            *sub_inc = midi_to_hz_fast(common + v.modulated(p, OscCPitch)) * inv_sample_rate;
        };
        update_pitch(self, 0.0, &mut in_a, &mut in_b, &mut sub_inc);

        let table_a = ctx.table_a;
        let table_b = ctx.table_b;
        in_a.mip_level = table_a.mip_for_increment(in_a.increment * self.osc_a.max_detune_ratio());
        in_b.mip_level =
            table_b.mip_for_increment(in_b.increment * self.osc_b.max_detune_ratio() * sync_ratio.max(1.0));

        let mut frame_a = resolve_frame(table_a, wt_pos_a, in_a.mip_level);
        let mut frame_b = resolve_frame(table_b, wt_pos_b, in_b.mip_level);
        let mut data_a = FrameData::resolve(table_a, &frame_a);
        let mut data_b = FrameData::resolve(table_b, &frame_b);

        let mut mod_countdown: usize = 0;
        for i in 0..num_samples {
            // Modulation ticks once per output sample; audio runs at the
            // oversampled rate.
            let mod_tick = mod_countdown == 0;
            mod_countdown = if mod_countdown == 0 { factor - 1 } else { mod_countdown - 1 };

            if mod_tick {
                self.last_amp_env = self.env_amp.next();
                if mod_env_used {
                    self.last_mod_env = self.env_mod.next();
                }

                // Voice-scope LFOs only run when something listens to them.
                if lfo_needed[0] {
                    let value = self.lfo1.next();
                    self.last_lfo[0] = if self.use_voice_lfo[0] { value } else { globals.lfo[0] };
                    // "Random (per voice cycle)" refreshes on each LFO 1 upward
                    // zero crossing.
                    if self.lfo1_phase_tracker <= 0.0 && self.last_lfo[0] > 0.0 {
                        self.random_cycle = self.rng.next_bipolar();
                    }
                    self.lfo1_phase_tracker = self.last_lfo[0];
                }
                if lfo_needed[1] {
                    let value = self.lfo2.next();
                    self.last_lfo[1] = if self.use_voice_lfo[1] { value } else { globals.lfo[1] };
                }

                if audio_rate {
                    self.evaluate_matrix(globals);
                }

                if pitch_dynamic {
                    self.glide_pitch += self.glide_coeff * (self.glide_target - self.glide_pitch);
                    self.vibrato_phase += vibrato_inc;
                    if self.vibrato_phase >= 1.0 {
                        self.vibrato_phase -= 1.0;
                    }
                    let vibrato = self.expr.vibrato * fast_sin01(self.vibrato_phase) * 0.5;
                    update_pitch(self, vibrato, &mut in_a, &mut in_b, &mut sub_inc);
                }

                if dyn_level_a {
                    level_a = clamp01(self.modulated(p, OscALevel));
                }
                if dyn_level_b {
                    level_b = clamp01(self.modulated(p, OscBLevel));
                }
                if dyn_wt_a {
                    wt_pos_a = self.modulated(p, OscAWtPos) * 0.01;
                    frame_a = resolve_frame(table_a, wt_pos_a, in_a.mip_level);
                    data_a = FrameData::resolve(table_a, &frame_a);
                }
                if dyn_wt_b {
                    wt_pos_b = self.modulated(p, OscBWtPos) * 0.01;
                    frame_b = resolve_frame(table_b, wt_pos_b, in_b.mip_level);
                    data_b = FrameData::resolve(table_b, &frame_b);
                }
                if dyn_phase_a {
                    phase_mod_a = self.mod_of(OscAPhase) / 360.0;
                }
                if dyn_phase_b {
                    phase_mod_b = self.mod_of(OscBPhase) / 360.0;
                }
                if dyn_fm {
                    fm_index = clamp01(self.modulated(p, OscBFmIndex));
                }
                if dyn_am {
                    am_index = clamp01(self.modulated(p, OscBAmIndex));
                }
                if dyn_sub_level {
                    sub_level = clamp01(self.modulated(p, OscCLevel));
                }
                if dyn_pan_a {
                    (pan_a_l, pan_a_r) = equal_power_pan(self.modulated(p, OscAPan).clamp(-1.0, 1.0));
                }
                if dyn_pan_b {
                    (pan_b_l, pan_b_r) = equal_power_pan(self.modulated(p, OscBPan).clamp(-1.0, 1.0));
                }
                if dyn_master_pan {
                    (pan_gain_l, pan_gain_r) = master_pan(self);
                }
            }

            in_a.level = level_a;
            in_a.pan_l = pan_a_l;
            in_a.pan_r = pan_a_r;
            in_a.phase_mod = phase_mod_a;
            let a = if osc_a_on { self.osc_a.process(&in_a, data_a, frame_a.nearest()) } else { StereoSample::default() };

            in_b.level = level_b;
            in_b.pan_l = pan_b_l;
            in_b.pan_r = pan_b_r;
            in_b.phase_mod = phase_mod_b + fm_index * a.mono;
            in_b.sync_reset = self.osc_a.did_wrap();
            let mut b = if osc_b_on { self.osc_b.process(&in_b, data_b, frame_b.nearest()) } else { StereoSample::default() };
            if am_index > 0.0 {
                let am_gain = 1.0 - am_index + am_index * (0.5 + 0.5 * a.mono);
                b.left *= am_gain;
                b.right *= am_gain;
            }

            // Osc C: sine/triangle sub or noise, level only (plus modulatable pitch).
            let mut sub_out = 0.0;
            if sub_level > 0.0 {
                match sub {
                    SubMode::White | SubMode::Pink => {
                        let white = self.rng.next_bipolar();
                        sub_out = if sub == SubMode::White {
                            white
                        } else {
                            let ps = &mut self.pink_state;
                            ps[0] = 0.99765 * ps[0] + white * 0.099_046;
                            ps[1] = 0.96300 * ps[1] + white * 0.296_516_4;
                            ps[2] = 0.57000 * ps[2] + white * 1.052_691_3;
                            (ps[0] + ps[1] + ps[2] + white * 0.1848) * 0.35
                        };
                    }
                    SubMode::Sine | SubMode::Triangle => {
                        self.sub_phase += sub_inc;
                        if self.sub_phase >= 1.0 {
                            self.sub_phase -= self.sub_phase.floor();
                        }
                        sub_out = if sub == SubMode::Sine {
                            fast_sin01(self.sub_phase)
                        } else {
                            4.0 * (self.sub_phase - 0.5).abs() - 1.0
                        };
                    }
                }
                sub_out *= sub_level;
            }

            if mod_tick {
                peak_a = peak_a.max(a.mono.abs());
                peak_b = peak_b.max(b.mono.abs());
            }

            let mut l = a.left + b.left + sub_out;
            let mut r = a.right + b.right + sub_out;
            let pre_filter = 0.5 * (l + r);

            if filter_on {
                self.filter.process(&mut l, &mut r);
            }

            let mut gain = self.last_amp_env * vel_gain * self.expr.volume;
            let mut fade_finished = false;
            if self.steal_fade > 0 {
                gain *= self.steal_fade as f32 / self.steal_fade_length as f32;
                self.steal_fade -= 1;
                fade_finished = self.steal_fade == 0;
            }

            left[i] += l * gain * pan_gain_l;
            right[i] += r * gain * pan_gain_r;
            if let Some(d) = dry.as_deref_mut() {
                d[i] += pre_filter * gain;
            }

            if fade_finished || !self.env_amp.is_active() {
                self.active = false;
                self.releasing = false;
                self.steal_fade = 0;
                break;
            }
        }

        self.last_osc_peak = [peak_a * self.last_amp_env, peak_b * self.last_amp_env];
    }
}
