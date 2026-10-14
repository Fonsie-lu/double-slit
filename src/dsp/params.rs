//! Single source of truth for the parameter set (SPEC 8). The engine's flat
//! parameter array, the nih-plug parameter layer (`crate::params`), patch
//! serialisation and the modulation-target list are all generated from this
//! table, so parameter IDs stay stable and the layers cannot drift apart.
//!
//! Values are in natural units everywhere except at the host boundary. The
//! `id` strings (`"oscA.wtPos"`) are the stable automation and patch keys:
//! never rename one.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Every parameter, in table order. The order of `add` calls in [`build`]
/// must match; a debug assertion catches drift.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParamId {
    // ---- Osc A -----------------------------------------------------------
    OscAOn,
    OscATable,
    OscAWtPos,
    OscALevel,
    OscAPan,
    OscAOctave,
    OscASemi,
    OscAFine,
    OscAUniVoices,
    OscAUniDetune,
    OscAUniSpread,
    OscAUniBlend,
    OscAPhase,
    OscAPhaseMode,

    // ---- Osc B -----------------------------------------------------------
    OscBOn,
    OscBTable,
    OscBWtPos,
    OscBLevel,
    OscBPan,
    OscBOctave,
    OscBSemi,
    OscBFine,
    OscBUniVoices,
    OscBUniDetune,
    OscBUniSpread,
    OscBUniBlend,
    OscBPhase,
    OscBPhaseMode,
    OscBNoiseMode,
    OscBSyncMode,
    OscBSyncRatio,
    OscBFmIndex,
    OscBAmIndex,

    // ---- Osc C (sub / noise) -----------------------------------------------
    OscCMode,
    OscCLevel,
    OscCPitch,

    // ---- Filter ------------------------------------------------------------
    FilterOn,
    FilterType,
    FilterCutoff,
    FilterRes,
    FilterDrive,
    FilterMix,
    FilterKeyTrack,
    FilterEnvAmount,
    FilterVowel,

    // ---- Amp envelope ------------------------------------------------------
    EnvAmpAttack,
    EnvAmpDecay,
    EnvAmpSustain,
    EnvAmpRelease,
    EnvAmpAttackCurve,
    EnvAmpDecayCurve,
    EnvAmpReleaseCurve,
    EnvAmpVelAmount,

    // ---- Mod envelope ------------------------------------------------------
    EnvModAttack,
    EnvModDecay,
    EnvModSustain,
    EnvModRelease,
    EnvModAttackCurve,
    EnvModDecayCurve,
    EnvModReleaseCurve,
    EnvModVelAmount,

    // ---- LFO 1 ---------------------------------------------------------------
    Lfo1Rate,
    Lfo1Shape,
    Lfo1SyncMode,
    Lfo1SyncDiv,
    Lfo1Retrig,
    Lfo1Phase,
    Lfo1Smooth,

    // ---- LFO 2 ---------------------------------------------------------------
    Lfo2Rate,
    Lfo2Shape,
    Lfo2SyncMode,
    Lfo2SyncDiv,
    Lfo2Retrig,
    Lfo2Phase,
    Lfo2Smooth,

    // ---- FX: distortion ------------------------------------------------------
    FxDistOn,
    FxDistDrive,
    FxDistTone,
    FxDistMix,

    // ---- FX: chorus ------------------------------------------------------------
    FxChorusOn,
    FxChorusRate,
    FxChorusDepth,
    FxChorusMix,

    // ---- FX: delay -------------------------------------------------------------
    FxDelayOn,
    FxDelaySyncMode,
    FxDelaySyncDiv,
    FxDelayTimeMs,
    FxDelayFeedback,
    FxDelayTone,
    FxDelayMix,
    FxDelayPingPong,

    // ---- FX: reverb ------------------------------------------------------------
    FxRevOn,
    FxRevSize,
    FxRevDamp,
    FxRevPreDelay,
    FxRevMix,

    // ---- Master ------------------------------------------------------------------
    MasterVolume,
    MasterPan,
    MasterTune,
    MasterBendRange,
    MasterPolyphony,
    MasterOversampling,
    MasterGlide,
}

pub const NUM_PARAMS: usize = ParamId::MasterGlide as usize + 1;

impl ParamId {
    #[inline(always)]
    pub fn index(self) -> usize {
        self as usize
    }

    /// The parameter at a table index. Panics when out of range.
    pub fn from_index(index: usize) -> ParamId {
        assert!(index < NUM_PARAMS, "parameter index {index} out of range");
        // SAFETY: `ParamId` is `repr(usize)` with contiguous discriminants
        // starting at 0, and `index` was checked against the count.
        unsafe { std::mem::transmute::<usize, ParamId>(index) }
    }

    pub fn desc(self) -> &'static ParamDesc {
        &table().descs[self as usize]
    }

    /// `None` when the ID string is unknown (older or newer patch).
    pub fn from_id(id: &str) -> Option<ParamId> {
        table().by_id.get(id).copied()
    }

    pub fn all() -> impl Iterator<Item = ParamId> {
        (0..NUM_PARAMS).map(ParamId::from_index)
    }
}

#[derive(Clone, Debug)]
pub struct ParamDesc {
    pub id: ParamId,
    /// Stable automation ID, e.g. `"oscA.wtPos"`.
    pub id_string: String,
    /// Short display name.
    pub name: &'static str,
    /// Group label for the host.
    pub module: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: &'static str,
    /// 0 = continuous, else the number of discrete values.
    pub steps: usize,
    /// Non-empty for enum-style parameters.
    pub choices: &'static [&'static str],
    /// 0 = linear range, else the value that sits at the middle of the
    /// normalised range (JUCE `NormalisableRange` skew centre).
    pub skew_centre: f32,
    pub bipolar: bool,
}

impl ParamDesc {
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }

    /// Exponent mapping a linear proportion onto the normalised range, as in
    /// `normalised = proportion ^ skew`. 1.0 for linear parameters.
    pub fn skew_factor(&self) -> f32 {
        if self.skew_centre <= 0.0 || self.steps > 0 {
            return 1.0;
        }
        let p = (self.skew_centre - self.min) / (self.max - self.min);
        if p <= 0.0 || p >= 1.0 {
            return 1.0;
        }
        0.5_f32.ln() / p.ln()
    }

    pub fn to_normalized(&self, v: f32) -> f32 {
        let p = ((self.clamp(v) - self.min) / (self.max - self.min)).clamp(0.0, 1.0);
        p.powf(self.skew_factor())
    }

    pub fn from_normalized(&self, n: f32) -> f32 {
        let p = n.clamp(0.0, 1.0).powf(1.0 / self.skew_factor());
        let v = self.min + p * (self.max - self.min);
        if self.steps > 0 {
            v.round()
        } else {
            v
        }
    }

    /// Display text in natural units, e.g. "350 ms", "LP 24", "+12 st".
    pub fn format(&self, v: f32) -> String {
        if !self.choices.is_empty() {
            let i = (v.round() as isize - self.min as isize).clamp(0, self.choices.len() as isize - 1);
            return self.choices[i as usize].to_owned();
        }
        let sign = if self.bipolar && v > 0.0 { "+" } else { "" };
        let body = match self.unit {
            "s" if v < 1.0 => return format!("{:.0} ms", v * 1000.0),
            "s" => format!("{v:.2}"),
            "Hz" if v >= 1000.0 => return format!("{:.2} kHz", v / 1000.0),
            "Hz" if v < 10.0 => format!("{v:.2}"),
            "Hz" => format!("{v:.0}"),
            "%" | "deg" | "ms" | "ct" => format!("{v:.0}"),
            _ if self.steps > 0 => format!("{v:.0}"),
            // Unitless 0..1 controls read as percentages.
            "" if self.max - self.min <= 2.0 => return format!("{sign}{:.0}%", v * 100.0),
            _ => format!("{v:.2}"),
        };
        if self.unit.is_empty() {
            format!("{sign}{body}")
        } else {
            format!("{sign}{body} {}", self.unit)
        }
    }
}

/// Precomputed range geometry for the per-sample soft limiter (SPEC 4.3).
#[derive(Clone, Copy, Debug)]
pub struct ParamRange {
    pub centre: f32,
    pub half_span: f32,
    pub inv_half_span: f32,
}

pub const ON_OFF: &[&str] = &["Off", "On"];
pub const PHASE_MODES: &[&str] = &["Free", "Random", "Reset"];
pub const NOISE_MODES: &[&str] = &["Off", "White", "Pink"];
pub const SYNC_MODES: &[&str] = &["Off", "Osc A", "Self"];
pub const SUB_MODES: &[&str] = &["Sine", "Triangle", "White", "Pink"];
pub const FILTER_TYPES: &[&str] = &[
    "LP 12", "LP 24", "HP 12", "HP 24", "BP 12", "Notch", "Peak", "Ladder", "Comb +", "Comb -", "Formant",
];
pub const LFO_SHAPES: &[&str] = &["Sine", "Triangle", "Saw Up", "Saw Down", "Square", "S&H", "Custom"];
pub const LFO_SYNC: &[&str] = &["Free", "Tempo"];
pub const LFO_RETRIG: &[&str] = &["Free", "Voice", "Note"];
pub const TIME_DIVS: &[&str] = &[
    "1/1", "1/2", "1/4", "1/8", "1/16", "1/2.", "1/4.", "1/8.", "1/16.", "1/2T", "1/4T", "1/8T", "1/16T",
];
pub const OVERSAMPLING: &[&str] = &["1x", "2x", "4x"];

/// Built-in wavetables, plus one trailing "Custom" choice for edited tables.
pub const NUM_FACTORY_TABLES: usize = 14;
pub const NUM_TABLE_CHOICES: usize = NUM_FACTORY_TABLES + 1;
pub const CUSTOM_TABLE_CHOICE: usize = NUM_FACTORY_TABLES;
pub const TABLE_NAMES: &[&str] = &[
    "Sine",
    "Triangle",
    "Saw",
    "Square",
    "Pulse 25%",
    "Analog Saws",
    "Analog PWM",
    "Analog Organ",
    "Digi Harmonics",
    "Digi Bells",
    "Digi Growl",
    "Digi Wavefold",
    "Digi Sync",
    "Digi Vowel",
    "Custom",
];

// ---- Enum value orderings used by the DSP --------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseMode {
    Free = 0,
    Random,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseMode {
    Off = 0,
    White,
    Pink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Off = 0,
    ToOscA,
    SelfSync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubMode {
    Sine = 0,
    Triangle,
    White,
    Pink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterType {
    Lp12 = 0,
    Lp24,
    Hp12,
    Hp24,
    Bp12,
    Notch,
    Peak,
    LadderLp,
    CombPos,
    CombNeg,
    Formant,
}

impl FilterType {
    pub const ALL: [FilterType; 11] = [
        FilterType::Lp12,
        FilterType::Lp24,
        FilterType::Hp12,
        FilterType::Hp24,
        FilterType::Bp12,
        FilterType::Notch,
        FilterType::Peak,
        FilterType::LadderLp,
        FilterType::CombPos,
        FilterType::CombNeg,
        FilterType::Formant,
    ];
    pub fn from_value(v: f32) -> FilterType {
        Self::ALL[(v.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LfoShape {
    Sine = 0,
    Triangle,
    SawUp,
    SawDown,
    Square,
    SampleHold,
    Custom,
}

impl LfoShape {
    pub const ALL: [LfoShape; 7] = [
        LfoShape::Sine,
        LfoShape::Triangle,
        LfoShape::SawUp,
        LfoShape::SawDown,
        LfoShape::Square,
        LfoShape::SampleHold,
        LfoShape::Custom,
    ];
    pub fn from_value(v: f32) -> LfoShape {
        Self::ALL[(v.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LfoSync {
    Free = 0,
    Tempo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LfoRetrig {
    Free = 0,
    Voice,
    Note,
}

/// Tempo divisions in the order of [`TIME_DIVS`].
pub const NUM_TIME_DIVS: usize = 13;

/// Beats per cycle for a tempo division index (order of [`TIME_DIVS`]).
pub fn time_div_beats(div: usize) -> f32 {
    match div {
        0 => 4.0,
        1 => 2.0,
        2 => 1.0,
        3 => 0.5,
        4 => 0.25,
        5 => 3.0,
        6 => 1.5,
        7 => 0.75,
        8 => 0.375,
        9 => 4.0 / 3.0,
        10 => 2.0 / 3.0,
        11 => 1.0 / 3.0,
        12 => 1.0 / 6.0,
        _ => 1.0,
    }
}

// ---- Table ---------------------------------------------------------------------

struct Table {
    descs: Vec<ParamDesc>,
    by_id: HashMap<String, ParamId>,
    ranges: Vec<ParamRange>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let descs = build();
        debug_assert_eq!(descs.len(), NUM_PARAMS);
        let by_id = descs.iter().map(|d| (d.id_string.clone(), d.id)).collect();
        let ranges = descs
            .iter()
            .map(|d| {
                let half_span = ((d.max - d.min) * 0.5).max(1.0e-6);
                ParamRange {
                    centre: (d.max + d.min) * 0.5,
                    half_span,
                    inv_half_span: 1.0 / half_span,
                }
            })
            .collect();
        Table { descs, by_id, ranges }
    })
}

/// All descriptors in table order.
pub fn descs() -> &'static [ParamDesc] {
    &table().descs
}

/// Range geometry for every parameter, in table order; hot paths cache the slice.
pub fn ranges() -> &'static [ParamRange] {
    &table().ranges
}

pub fn default_value(id: ParamId) -> f32 {
    id.desc().default
}

pub fn clamp_param(id: ParamId, v: f32) -> f32 {
    id.desc().clamp(v)
}

/// Default values for every parameter, in table order.
pub fn defaults() -> [f32; NUM_PARAMS] {
    std::array::from_fn(|i| descs()[i].default)
}

struct Builder {
    descs: Vec<ParamDesc>,
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        id: ParamId,
        id_string: &str,
        name: &'static str,
        module: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: &'static str,
        steps: usize,
        choices: &'static [&'static str],
        skew_centre: f32,
        bipolar: bool,
    ) {
        debug_assert_eq!(id as usize, self.descs.len(), "params.rs add() order must match ParamId");
        self.descs.push(ParamDesc {
            id,
            id_string: id_string.to_owned(),
            name,
            module,
            min,
            max,
            default,
            unit,
            steps,
            choices,
            skew_centre,
            bipolar,
        });
    }

    /// Continuous, linear, unipolar.
    #[allow(clippy::too_many_arguments)]
    fn cont(&mut self, id: ParamId, s: &str, name: &'static str, module: &'static str, lo: f32, hi: f32, def: f32, unit: &'static str) {
        self.add(id, s, name, module, lo, hi, def, unit, 0, &[], 0.0, false);
    }

    /// Continuous with a skew centre.
    #[allow(clippy::too_many_arguments)]
    fn skewed(&mut self, id: ParamId, s: &str, name: &'static str, module: &'static str, lo: f32, hi: f32, def: f32, unit: &'static str, centre: f32) {
        self.add(id, s, name, module, lo, hi, def, unit, 0, &[], centre, false);
    }

    /// Continuous bipolar.
    #[allow(clippy::too_many_arguments)]
    fn bipolar(&mut self, id: ParamId, s: &str, name: &'static str, module: &'static str, lo: f32, hi: f32, def: f32, unit: &'static str) {
        self.add(id, s, name, module, lo, hi, def, unit, 0, &[], 0.0, true);
    }

    /// Discrete choice list starting at 0.
    fn choice(&mut self, id: ParamId, s: &str, name: &'static str, module: &'static str, def: f32, choices: &'static [&'static str]) {
        let n = choices.len();
        self.add(id, s, name, module, 0.0, (n - 1) as f32, def, "", n, choices, 0.0, false);
    }

    fn set_default(&mut self, id: ParamId, v: f32) {
        self.descs[id as usize].default = v;
    }

    /// Osc A and B share their first 14 slots in identical order; the helper
    /// keeps the two blocks provably in sync.
    fn osc(&mut self, prefix: &str, module: &'static str, first: ParamId) {
        let p = |o: usize| ParamId::from_index(first as usize + o);
        let id = |leaf: &str| format!("{prefix}.{leaf}");
        self.choice(p(0), &id("on"), "On", module, 1.0, ON_OFF);
        self.choice(p(1), &id("table"), "Table", module, 2.0, TABLE_NAMES);
        self.cont(p(2), &id("wtPos"), "WT Pos", module, 0.0, 100.0, 0.0, "%");
        self.cont(p(3), &id("level"), "Level", module, 0.0, 1.0, 0.75, "");
        self.bipolar(p(4), &id("pan"), "Pan", module, -1.0, 1.0, 0.0, "");
        self.add(p(5), &id("octave"), "Octave", module, -3.0, 3.0, 0.0, "oct", 7, &[], 0.0, true);
        self.add(p(6), &id("semi"), "Semi", module, -12.0, 12.0, 0.0, "st", 25, &[], 0.0, true);
        self.bipolar(p(7), &id("fine"), "Fine", module, -100.0, 100.0, 0.0, "ct");
        self.add(p(8), &id("uniVoices"), "Unison", module, 1.0, 16.0, 1.0, "", 16, &[], 0.0, false);
        self.cont(p(9), &id("uniDetune"), "Detune", module, 0.0, 100.0, 15.0, "ct");
        self.cont(p(10), &id("uniSpread"), "Spread", module, 0.0, 1.0, 0.5, "");
        self.cont(p(11), &id("uniBlend"), "Blend", module, 0.0, 1.0, 0.75, "");
        self.cont(p(12), &id("phase"), "Phase", module, 0.0, 360.0, 0.0, "deg");
        self.choice(p(13), &id("phaseMode"), "Phase Mode", module, 0.0, PHASE_MODES);
    }

    #[allow(clippy::too_many_arguments)]
    fn env(&mut self, prefix: &str, module: &'static str, first: ParamId, a: f32, d: f32, s: f32, r: f32) {
        let p = |o: usize| ParamId::from_index(first as usize + o);
        let id = |leaf: &str| format!("{prefix}.{leaf}");
        self.skewed(p(0), &id("attack"), "Attack", module, 0.0, 10.0, a, "s", 0.3);
        self.skewed(p(1), &id("decay"), "Decay", module, 0.0, 10.0, d, "s", 0.3);
        self.cont(p(2), &id("sustain"), "Sustain", module, 0.0, 1.0, s, "");
        self.skewed(p(3), &id("release"), "Release", module, 0.0, 10.0, r, "s", 0.3);
        self.bipolar(p(4), &id("attackCurve"), "A Curve", module, -1.0, 1.0, 0.0, "");
        self.bipolar(p(5), &id("decayCurve"), "D Curve", module, -1.0, 1.0, 0.0, "");
        self.bipolar(p(6), &id("releaseCurve"), "R Curve", module, -1.0, 1.0, 0.0, "");
        self.cont(p(7), &id("velAmount"), "Velocity", module, 0.0, 1.0, 0.5, "");
    }

    fn lfo(&mut self, prefix: &str, module: &'static str, first: ParamId, rate: f32) {
        let p = |o: usize| ParamId::from_index(first as usize + o);
        let id = |leaf: &str| format!("{prefix}.{leaf}");
        self.skewed(p(0), &id("rate"), "Rate", module, 0.01, 40.0, rate, "Hz", 2.0);
        self.choice(p(1), &id("shape"), "Shape", module, 0.0, LFO_SHAPES);
        self.choice(p(2), &id("syncMode"), "Sync", module, 0.0, LFO_SYNC);
        self.choice(p(3), &id("syncDiv"), "Division", module, 2.0, TIME_DIVS);
        self.choice(p(4), &id("retrig"), "Retrigger", module, 1.0, LFO_RETRIG);
        self.cont(p(5), &id("phase"), "Phase", module, 0.0, 360.0, 0.0, "deg");
        self.cont(p(6), &id("smooth"), "Smooth", module, 0.0, 1.0, 0.0, "");
    }
}

fn build() -> Vec<ParamDesc> {
    use ParamId::*;
    let mut b = Builder { descs: Vec::with_capacity(NUM_PARAMS) };

    b.osc("oscA", "Osc A", OscAOn);

    b.osc("oscB", "Osc B", OscBOn);
    // Only osc A sounds in the init patch.
    b.set_default(OscBOn, 0.0);
    b.set_default(OscBTable, 3.0);
    b.choice(OscBNoiseMode, "oscB.noiseMode", "Noise", "Osc B", 0.0, NOISE_MODES);
    b.choice(OscBSyncMode, "oscB.syncMode", "Sync", "Osc B", 0.0, SYNC_MODES);
    b.cont(OscBSyncRatio, "oscB.syncRatio", "Sync Ratio", "Osc B", 1.0, 8.0, 1.0, "x");
    b.cont(OscBFmIndex, "oscB.fmIndex", "FM", "Osc B", 0.0, 1.0, 0.0, "");
    b.cont(OscBAmIndex, "oscB.amIndex", "AM", "Osc B", 0.0, 1.0, 0.0, "");

    b.choice(OscCMode, "oscC.mode", "Mode", "Osc C", 0.0, SUB_MODES);
    b.cont(OscCLevel, "oscC.level", "Level", "Osc C", 0.0, 1.0, 0.0, "");
    b.bipolar(OscCPitch, "oscC.pitch", "Pitch", "Osc C", -24.0, 24.0, -12.0, "st");

    b.choice(FilterOn, "filter.on", "On", "Filter", 1.0, ON_OFF);
    b.choice(FilterType, "filter.type", "Type", "Filter", 1.0, FILTER_TYPES);
    b.skewed(FilterCutoff, "filter.cutoff", "Cutoff", "Filter", 20.0, 20000.0, 20000.0, "Hz", 1000.0);
    b.cont(FilterRes, "filter.res", "Resonance", "Filter", 0.0, 1.0, 0.15, "");
    b.cont(FilterDrive, "filter.drive", "Drive", "Filter", 0.0, 1.0, 0.0, "");
    b.cont(FilterMix, "filter.mix", "Mix", "Filter", 0.0, 1.0, 1.0, "");
    b.cont(FilterKeyTrack, "filter.keyTrack", "Key Track", "Filter", 0.0, 1.0, 0.0, "");
    b.bipolar(FilterEnvAmount, "filter.envAmount", "Env Amt", "Filter", -1.0, 1.0, 0.0, "");
    b.cont(FilterVowel, "filter.vowel", "Vowel", "Filter", 0.0, 1.0, 0.0, "");

    b.env("envAmp", "Amp Env", EnvAmpAttack, 0.005, 0.6, 0.8, 0.25);
    b.env("envMod", "Mod Env", EnvModAttack, 0.01, 0.4, 0.0, 0.3);

    b.lfo("lfo1", "LFO 1", Lfo1Rate, 2.0);
    b.lfo("lfo2", "LFO 2", Lfo2Rate, 0.5);

    b.choice(FxDistOn, "fx.dist.on", "On", "Distortion", 0.0, ON_OFF);
    b.cont(FxDistDrive, "fx.dist.drive", "Drive", "Distortion", 0.0, 1.0, 0.3, "");
    b.cont(FxDistTone, "fx.dist.tone", "Tone", "Distortion", 0.0, 1.0, 0.5, "");
    b.cont(FxDistMix, "fx.dist.mix", "Mix", "Distortion", 0.0, 1.0, 1.0, "");

    b.choice(FxChorusOn, "fx.chorus.on", "On", "Chorus", 0.0, ON_OFF);
    b.skewed(FxChorusRate, "fx.chorus.rate", "Rate", "Chorus", 0.01, 10.0, 0.6, "Hz", 1.0);
    b.cont(FxChorusDepth, "fx.chorus.depth", "Depth", "Chorus", 0.0, 1.0, 0.4, "");
    b.cont(FxChorusMix, "fx.chorus.mix", "Mix", "Chorus", 0.0, 1.0, 0.5, "");

    b.choice(FxDelayOn, "fx.delay.on", "On", "Delay", 0.0, ON_OFF);
    b.choice(FxDelaySyncMode, "fx.delay.syncMode", "Sync", "Delay", 1.0, LFO_SYNC);
    b.choice(FxDelaySyncDiv, "fx.delay.syncDiv", "Division", "Delay", 3.0, TIME_DIVS);
    b.skewed(FxDelayTimeMs, "fx.delay.timeMs", "Time", "Delay", 1.0, 2000.0, 350.0, "ms", 300.0);
    b.cont(FxDelayFeedback, "fx.delay.feedback", "Feedback", "Delay", 0.0, 0.95, 0.35, "");
    b.cont(FxDelayTone, "fx.delay.tone", "Tone", "Delay", 0.0, 1.0, 0.5, "");
    b.cont(FxDelayMix, "fx.delay.mix", "Mix", "Delay", 0.0, 1.0, 0.3, "");
    b.choice(FxDelayPingPong, "fx.delay.pingPong", "Ping Pong", "Delay", 0.0, ON_OFF);

    b.choice(FxRevOn, "fx.rev.on", "On", "Reverb", 0.0, ON_OFF);
    b.cont(FxRevSize, "fx.rev.size", "Size", "Reverb", 0.0, 1.0, 0.5, "");
    b.cont(FxRevDamp, "fx.rev.damp", "Damping", "Reverb", 0.0, 1.0, 0.4, "");
    b.cont(FxRevPreDelay, "fx.rev.preDelay", "Pre Delay", "Reverb", 0.0, 200.0, 10.0, "ms");
    b.cont(FxRevMix, "fx.rev.mix", "Mix", "Reverb", 0.0, 1.0, 0.25, "");

    b.cont(MasterVolume, "master.volume", "Volume", "Master", 0.0, 1.5, 0.8, "");
    b.bipolar(MasterPan, "master.pan", "Pan", "Master", -1.0, 1.0, 0.0, "");
    b.bipolar(MasterTune, "master.tune", "Tune", "Master", -100.0, 100.0, 0.0, "ct");
    b.add(MasterBendRange, "master.bendRange", "Bend Range", "Master", 0.0, 24.0, 2.0, "st", 25, &[], 0.0, false);
    b.add(MasterPolyphony, "master.polyphony", "Polyphony", "Master", 8.0, 16.0, 16.0, "", 9, &[], 0.0, false);
    b.choice(MasterOversampling, "master.oversampling", "Oversampling", "Master", 1.0, OVERSAMPLING);
    b.skewed(MasterGlide, "master.glide", "Glide", "Master", 0.0, 2.0, 0.0, "s", 0.2);

    b.descs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_enum_and_ids_are_unique() {
        assert_eq!(descs().len(), NUM_PARAMS);
        for (i, d) in descs().iter().enumerate() {
            assert_eq!(d.id as usize, i);
            assert_eq!(ParamId::from_id(&d.id_string), Some(d.id));
            assert!(d.default >= d.min && d.default <= d.max, "{} default", d.id_string);
            if !d.choices.is_empty() {
                assert_eq!(d.choices.len(), d.steps, "{}", d.id_string);
            }
        }
        assert_eq!(NUM_PARAMS, 103);
        assert_eq!(ParamId::from_id("oscA.wtPos"), Some(ParamId::OscAWtPos));
        assert_eq!(ParamId::from_id("nope"), None);
    }

    #[test]
    fn normalised_mapping_round_trips_and_honours_skew_centre() {
        for d in descs() {
            for k in 0..=20 {
                let n = k as f32 / 20.0;
                let v = d.from_normalized(n);
                let back = d.to_normalized(v);
                let again = d.from_normalized(back);
                assert!((again - v).abs() <= 1.0e-3 * (d.max - d.min).max(1.0), "{}", d.id_string);
            }
        }
        let cutoff = ParamId::FilterCutoff.desc();
        assert!((cutoff.from_normalized(0.5) - 1000.0).abs() < 1.0);
    }
}
