//! Modulation matrix (SPEC 4). Rows are first-class, persisted state; targets
//! are identified by stable parameter ID strings so patches survive parameter
//! reordering. The audio thread reads a lock-free snapshot published by the
//! editor thread.
//!
//! # Threading design
//!
//! [`ModMatrix`] owns the editable row list and is meant to live behind a
//! `Mutex` that only the GUI / message thread ever locks. Every edit compiles
//! the rows into a [`SharedSnapshot`], which the audio thread holds through an
//! `Arc` ([`ModMatrix::shared`]) and reads with [`SharedSnapshot::copy_snapshot`]
//! without touching the mutex.
//!
//! The shared snapshot is the C++ double buffer, made sound: two buffers of
//! atomics plus an atomic index of the active one. A publish writes the
//! inactive buffer and flips the index. Each buffer also carries a sequence
//! counter (a seqlock), so a reader that was still copying a buffer when two
//! publishes landed back to back sees the counter move and copies again
//! instead of returning a torn row set. In practice the retry never happens:
//! it needs two publishes inside the few nanoseconds of one copy. Reads are
//! lock-free and allocation-free.
//!
//! The audio thread takes a **copy** once per block, never a reference, and bulk
//! row changes publish **once** ([`ModMatrix::set_rows`]) - the same contract as
//! the C++ engine.

use std::collections::HashMap;
use std::sync::atomic::{fence, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use super::common::MATRIX_SLOTS;
use super::params::{ParamId, NUM_PARAMS};

// ---- Sources -------------------------------------------------------------------

/// Fixed modulation sources. MIDI CC sources follow them, see [`cc_source_index`].
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSource {
    None = 0,
    Velocity,
    /// Channel pressure.
    Aftertouch,
    /// Per-note pressure (CLAP note expression / MIDI poly AT).
    PolyAftertouch,
    ModWheel,
    PitchBend,
    NotePitch,
    NoteGate,
    EnvAmp,
    EnvMod,
    Lfo1,
    Lfo2,
    /// New value each note-on.
    RandomNote,
    /// New value each LFO 1 cycle of the voice.
    RandomVoiceCycle,
}

impl ModSource {
    #[inline(always)]
    pub const fn index(self) -> usize {
        self as usize
    }
}

pub const NUM_FIXED_SOURCES: usize = ModSource::RandomVoiceCycle as usize + 1;
pub const NUM_CC_SOURCES: usize = 119;
pub const NUM_MOD_SOURCES: usize = NUM_FIXED_SOURCES + NUM_CC_SOURCES;

/// Source index of MIDI CC `cc` (1..=119, clamped).
pub fn cc_source_index(cc: u8) -> usize {
    NUM_FIXED_SOURCES + cc.clamp(1, 119) as usize - 1
}

struct SourceInfo {
    id: &'static str,
    name: &'static str,
    category: &'static str,
}

const FIXED_SOURCES: [SourceInfo; NUM_FIXED_SOURCES] = [
    SourceInfo { id: "none", name: "None", category: "None" },
    SourceInfo { id: "velocity", name: "Velocity", category: "Note" },
    SourceInfo { id: "aftertouch", name: "Aftertouch", category: "Note" },
    SourceInfo { id: "polyAftertouch", name: "Poly Aftertouch", category: "Note" },
    SourceInfo { id: "modWheel", name: "Mod Wheel", category: "MIDI" },
    SourceInfo { id: "pitchBend", name: "Pitch Bend", category: "MIDI" },
    SourceInfo { id: "notePitch", name: "Note Pitch", category: "Note" },
    SourceInfo { id: "noteGate", name: "Note Gate", category: "Note" },
    SourceInfo { id: "envAmp", name: "AMP Env", category: "Envelopes" },
    SourceInfo { id: "envMod", name: "MOD Env", category: "Envelopes" },
    SourceInfo { id: "lfo1", name: "LFO 1", category: "LFOs" },
    SourceInfo { id: "lfo2", name: "LFO 2", category: "LFOs" },
    SourceInfo { id: "randomNote", name: "Random (note)", category: "Random" },
    SourceInfo { id: "randomVoice", name: "Random (cycle)", category: "Random" },
];

/// SPEC 4.1 asks for aliases on the four common controllers.
fn cc_alias(cc: usize) -> Option<&'static str> {
    match cc {
        1 => Some("Mod Wheel (CC1)"),
        11 => Some("Expression (CC11)"),
        64 => Some("Sustain (CC64)"),
        74 => Some("Brightness (CC74)"),
        _ => None,
    }
}

struct SourceNames {
    cc_ids: Vec<String>,
    by_id: HashMap<String, usize>,
}

fn source_names() -> &'static SourceNames {
    static NAMES: OnceLock<SourceNames> = OnceLock::new();
    NAMES.get_or_init(|| {
        let cc_ids: Vec<String> = (1..=NUM_CC_SOURCES).map(|cc| format!("cc.{cc}")).collect();
        let mut by_id = HashMap::new();
        for (i, s) in FIXED_SOURCES.iter().enumerate() {
            by_id.insert(s.id.to_owned(), i);
        }
        for (i, id) in cc_ids.iter().enumerate() {
            by_id.insert(id.clone(), NUM_FIXED_SOURCES + i);
        }
        SourceNames { cc_ids, by_id }
    })
}

/// Stable ID string of a source; "none" when out of range.
pub fn mod_source_id(index: usize) -> &'static str {
    if index >= NUM_MOD_SOURCES {
        return "none";
    }
    if index < NUM_FIXED_SOURCES {
        return FIXED_SOURCES[index].id;
    }
    &source_names().cc_ids[index - NUM_FIXED_SOURCES]
}

pub fn mod_source_display_name(index: usize) -> String {
    if index >= NUM_MOD_SOURCES {
        return "None".to_owned();
    }
    if index < NUM_FIXED_SOURCES {
        return FIXED_SOURCES[index].name.to_owned();
    }
    let cc = index - NUM_FIXED_SOURCES + 1;
    cc_alias(cc).map(str::to_owned).unwrap_or_else(|| format!("CC {cc}"))
}

/// Category label used to group the GUI source palette.
pub fn mod_source_category(index: usize) -> &'static str {
    if index >= NUM_MOD_SOURCES {
        return "None";
    }
    if index < NUM_FIXED_SOURCES {
        return FIXED_SOURCES[index].category;
    }
    "MIDI CC"
}

/// `None` when the ID is unknown.
pub fn mod_source_from_id(id: &str) -> Option<usize> {
    source_names().by_id.get(id).copied()
}

// ---- Curves --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ModCurve {
    #[default]
    Linear = 0,
    Exp,
    Log,
    Switch,
}

impl ModCurve {
    pub const ALL: [ModCurve; 4] = [ModCurve::Linear, ModCurve::Exp, ModCurve::Log, ModCurve::Switch];

    pub fn name(self) -> &'static str {
        match self {
            ModCurve::Linear => "Linear",
            ModCurve::Exp => "Exp",
            ModCurve::Log => "Log",
            ModCurve::Switch => "Switch",
        }
    }

    /// Clamped into the valid range, as patches from other builds may not be.
    pub fn from_index(i: i64) -> ModCurve {
        Self::ALL[i.clamp(0, Self::ALL.len() as i64 - 1) as usize]
    }
}

/// Applies the row curve to a source value in [-1, 1] (or [0, 1] for unipolar sources).
#[inline(always)]
pub fn apply_mod_curve(x: f32, curve: ModCurve) -> f32 {
    match curve {
        ModCurve::Linear => x,
        ModCurve::Exp => x * x.abs(),
        ModCurve::Log => {
            if x < 0.0 {
                -(-x).sqrt()
            } else {
                x.sqrt()
            }
        }
        ModCurve::Switch => {
            if x > 0.5 {
                1.0
            } else if x < -0.5 {
                -1.0
            } else {
                0.0
            }
        }
    }
}

// ---- Targets -------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct ModTargetDesc {
    pub param: ParamId,
    /// Offset applied at amount = +100 %, in natural units (octaves when `log_scale`).
    pub range: f32,
    /// Applied multiplicatively in octaves (cutoff, LFO rate).
    pub log_scale: bool,
}

struct TargetTable {
    targets: Vec<ModTargetDesc>,
    for_param: [Option<usize>; NUM_PARAMS],
}

fn target_table() -> &'static TargetTable {
    static TABLE: OnceLock<TargetTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        use ParamId::*;
        let mut t = TargetTable { targets: Vec::new(), for_param: [None; NUM_PARAMS] };
        let mut add = |param: ParamId, range: f32, log_scale: bool| {
            t.for_param[param as usize] = Some(t.targets.len());
            t.targets.push(ModTargetDesc { param, range, log_scale });
        };
        add(OscAWtPos, 100.0, false);
        add(OscAOctave, 6.0, false);
        add(OscASemi, 24.0, false);
        add(OscAFine, 1200.0, false);
        add(OscAUniDetune, 100.0, false);
        add(OscAPhase, 360.0, false);
        add(OscALevel, 1.0, false);
        add(OscAPan, 1.0, false);

        add(OscBWtPos, 100.0, false);
        add(OscBOctave, 6.0, false);
        add(OscBSemi, 24.0, false);
        add(OscBFine, 1200.0, false);
        add(OscBUniDetune, 100.0, false);
        add(OscBPhase, 360.0, false);
        add(OscBLevel, 1.0, false);
        add(OscBPan, 1.0, false);
        add(OscBFmIndex, 1.0, false);
        add(OscBAmIndex, 1.0, false);

        add(OscCLevel, 1.0, false);
        add(OscCPitch, 24.0, false);

        add(FilterCutoff, 6.0, true); // octaves
        add(FilterRes, 1.0, false);
        add(FilterDrive, 1.0, false);
        add(FilterMix, 1.0, false);
        add(FilterVowel, 1.0, false);

        add(EnvAmpAttack, 2.0, false);
        add(EnvAmpDecay, 2.0, false);
        add(EnvAmpSustain, 1.0, false);
        add(EnvAmpRelease, 2.0, false);
        add(EnvModAttack, 2.0, false);
        add(EnvModDecay, 2.0, false);
        add(EnvModSustain, 1.0, false);
        add(EnvModRelease, 2.0, false);

        add(Lfo1Rate, 4.0, true); // octaves
        add(Lfo2Rate, 4.0, true);

        add(FxDistDrive, 1.0, false);
        add(FxChorusDepth, 1.0, false);
        add(FxDelayTimeMs, 500.0, false);
        add(FxRevMix, 1.0, false);

        add(MasterVolume, 1.0, false);
        add(MasterPan, 1.0, false);
        add(MasterTune, 1200.0, false);
        t
    })
}

pub fn num_mod_targets() -> usize {
    target_table().targets.len()
}

/// Contiguous target array for the audio thread.
pub fn mod_targets() -> &'static [ModTargetDesc] {
    &target_table().targets
}

/// The target at `index`, clamped into range.
pub fn mod_target(index: usize) -> &'static ModTargetDesc {
    let all = &target_table().targets;
    &all[index.min(all.len() - 1)]
}

/// `None` when the parameter is not a modulation target.
pub fn mod_target_index_for_param(param: ParamId) -> Option<usize> {
    target_table().for_param[param as usize]
}

/// Target index from a parameter ID string; `None` when unknown or not a target.
pub fn mod_target_from_id(param_id: &str) -> Option<usize> {
    ParamId::from_id(param_id).and_then(mod_target_index_for_param)
}

/// Parameter ID string of a target; "" when out of range.
pub fn mod_target_id(index: usize) -> &'static str {
    match target_table().targets.get(index) {
        Some(t) => &t.param.desc().id_string,
        None => "",
    }
}

/// "Module Name", e.g. "Filter Cutoff"; "-" when out of range.
pub fn mod_target_display_name(index: usize) -> String {
    match target_table().targets.get(index) {
        Some(t) => {
            let d = t.param.desc();
            format!("{} {}", d.module, d.name)
        }
        None => "-".to_owned(),
    }
}

// ---- Rows ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct ModRow {
    pub source: usize,
    /// `None` for an unknown target (kept as an unresolved row).
    pub target: Option<usize>,
    /// -1 … +1 (displayed as -100 … +100 %).
    pub amount: f32,
    pub curve: ModCurve,
    pub muted: bool,
    pub unresolved: bool,
    /// As loaded, so unresolved rows can be shown and re-saved.
    pub source_id_raw: String,
    pub target_id_raw: String,
}

impl Default for ModRow {
    fn default() -> Self {
        Self {
            source: 0,
            target: None,
            amount: 0.5,
            curve: ModCurve::Linear,
            muted: false,
            unresolved: false,
            source_id_raw: String::new(),
            target_id_raw: String::new(),
        }
    }
}

/// One row as the audio thread sees it: resolved, unmuted, non-zero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompiledRow {
    pub source: usize,
    pub target: usize,
    pub amount: f32,
    pub curve: ModCurve,
}

/// Plain copy of the compiled rows; the audio thread keeps one per block.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub count: usize,
    pub rows: [CompiledRow; MATRIX_SLOTS],
}

impl Snapshot {
    pub fn rows(&self) -> &[CompiledRow] {
        &self.rows[..self.count]
    }
}

/// One published buffer: a sequence counter plus every compiled field as an atomic.
#[derive(Default)]
struct AtomicBuffer {
    /// Odd while being written.
    seq: AtomicU32,
    count: AtomicU32,
    /// source, target, amount bits, curve.
    rows: [[AtomicU32; 4]; MATRIX_SLOTS],
}

/// The audio-thread side of the matrix: lock-free, allocation-free reads.
#[derive(Default)]
pub struct SharedSnapshot {
    active: AtomicUsize,
    buffers: [AtomicBuffer; 2],
}

impl SharedSnapshot {
    /// Writes `rows` into the inactive buffer and makes it the active one.
    /// Single writer: only called by the owning [`ModMatrix`].
    fn publish(&self, count: usize, rows: &[CompiledRow; MATRIX_SLOTS]) {
        let inactive = 1 - self.active.load(Ordering::Relaxed);
        let b = &self.buffers[inactive];
        b.seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        b.count.store(count as u32, Ordering::Relaxed);
        for (slot, row) in b.rows.iter().zip(rows.iter()).take(count) {
            slot[0].store(row.source as u32, Ordering::Relaxed);
            slot[1].store(row.target as u32, Ordering::Relaxed);
            slot[2].store(row.amount.to_bits(), Ordering::Relaxed);
            slot[3].store(row.curve as u32, Ordering::Relaxed);
        }
        b.seq.fetch_add(1, Ordering::Release);
        self.active.store(inactive, Ordering::Release);
    }

    /// Lock-free, allocation-free copy of the current row set.
    ///
    /// Copy, not a reference: the editor publishes into a double buffer, so a
    /// reference held across a whole block would only be safe until the second
    /// publish, and publishes can arrive back to back. The audio thread calls
    /// this once per block; the sequence check guarantees a whole row set.
    pub fn copy_snapshot(&self, dest: &mut Snapshot) {
        loop {
            let b = &self.buffers[self.active.load(Ordering::Acquire)];
            let s1 = b.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let count = (b.count.load(Ordering::Relaxed) as usize).min(MATRIX_SLOTS);
            for (row, slot) in dest.rows.iter_mut().zip(b.rows.iter()).take(count) {
                row.source = slot[0].load(Ordering::Relaxed) as usize;
                row.target = slot[1].load(Ordering::Relaxed) as usize;
                row.amount = f32::from_bits(slot[2].load(Ordering::Relaxed));
                row.curve = ModCurve::from_index(slot[3].load(Ordering::Relaxed) as i64);
            }
            fence(Ordering::Acquire);
            if b.seq.load(Ordering::Relaxed) == s1 {
                dest.count = count;
                return;
            }
        }
    }
}

/// The editable row list (editor / message thread) plus its published snapshot.
pub struct ModMatrix {
    rows: Vec<ModRow>,
    compiled_index_by_row: Vec<Option<usize>>,
    shared: Arc<SharedSnapshot>,
}

impl Default for ModMatrix {
    fn default() -> Self {
        Self::new()
    }
}

impl ModMatrix {
    pub fn new() -> Self {
        let mut m = Self {
            rows: Vec::with_capacity(MATRIX_SLOTS),
            compiled_index_by_row: Vec::with_capacity(MATRIX_SLOTS),
            shared: Arc::new(SharedSnapshot::default()),
        };
        m.publish();
        m
    }

    /// The audio thread's handle; read it with [`SharedSnapshot::copy_snapshot`].
    pub fn shared(&self) -> Arc<SharedSnapshot> {
        self.shared.clone()
    }

    /// Convenience for single-threaded callers (tests, offline rendering).
    pub fn copy_snapshot(&self, dest: &mut Snapshot) {
        self.shared.copy_snapshot(dest);
    }

    // ---- Editor-thread API ------------------------------------------------------

    pub fn rows(&self) -> &[ModRow] {
        &self.rows
    }

    pub fn num_rows(&self) -> usize {
        self.rows.len()
    }

    /// Adds a row; `false` when all slots are taken. A `None` target makes an
    /// unresolved row.
    pub fn add_row(&mut self, source: usize, target: Option<usize>, amount: f32, curve: ModCurve) -> bool {
        if self.rows.len() >= MATRIX_SLOTS {
            return false;
        }
        let source = source.min(NUM_MOD_SOURCES - 1);
        self.rows.push(ModRow {
            source,
            target,
            amount: amount.clamp(-1.0, 1.0),
            curve,
            muted: false,
            unresolved: target.is_none(),
            source_id_raw: mod_source_id(source).to_owned(),
            target_id_raw: target.map(mod_target_id).unwrap_or("").to_owned(),
        });
        self.publish();
        true
    }

    pub fn remove_row(&mut self, index: usize) {
        if index < self.rows.len() {
            self.rows.remove(index);
            self.publish();
        }
    }

    pub fn move_row(&mut self, from: usize, to: usize) {
        let n = self.rows.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let row = self.rows.remove(from);
        self.rows.insert(to, row);
        self.publish();
    }

    pub fn set_amount(&mut self, index: usize, amount: f32) {
        if let Some(row) = self.rows.get_mut(index) {
            row.amount = amount.clamp(-1.0, 1.0);
            self.publish();
        }
    }

    pub fn set_curve(&mut self, index: usize, curve: ModCurve) {
        if let Some(row) = self.rows.get_mut(index) {
            row.curve = curve;
            self.publish();
        }
    }

    pub fn set_muted(&mut self, index: usize, muted: bool) {
        if let Some(row) = self.rows.get_mut(index) {
            row.muted = muted;
            self.publish();
        }
    }

    pub fn set_source(&mut self, index: usize, source: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row.source = source.min(NUM_MOD_SOURCES - 1);
            row.source_id_raw = mod_source_id(row.source).to_owned();
            row.unresolved = row.target.is_none();
            self.publish();
        }
    }

    pub fn set_target(&mut self, index: usize, target: Option<usize>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.target = target;
            row.target_id_raw = target.map(mod_target_id).unwrap_or("").to_owned();
            row.unresolved = target.is_none();
            self.publish();
        }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.publish();
    }

    /// Rows (by index) that modulate a given target, for the control popover.
    pub fn rows_for_target(&self, target: usize) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].target == Some(target)).collect()
    }

    /// Summed absolute depth on a target, used to draw the modulation range.
    pub fn depth_for_target(&self, target: usize) -> f32 {
        let depth: f32 = self
            .rows
            .iter()
            .filter(|r| r.target == Some(target) && !r.muted && !r.unresolved)
            .map(|r| r.amount.abs())
            .sum();
        depth.min(1.0)
    }

    /// Replaces every row in one go, publishing once. Loading row by row would
    /// publish once per row, and the audio thread only tolerates one publish per
    /// block. Extra rows beyond [`MATRIX_SLOTS`] are dropped.
    pub fn set_rows(&mut self, new_rows: &[ModRow]) {
        self.rows.clear();
        self.rows.extend(new_rows.iter().take(MATRIX_SLOTS).cloned());
        self.publish();
    }

    /// Row index -> compiled index, for GUI trigger feedback.
    pub fn compiled_index_for_row(&self, row: usize) -> Option<usize> {
        self.compiled_index_by_row.get(row).copied().flatten()
    }

    /// JSON array of row objects, the same shape the C++ build wrote, so
    /// patches stay compatible.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let source = if row.source_id_raw.is_empty() {
                        mod_source_id(row.source).to_owned()
                    } else {
                        row.source_id_raw.clone()
                    };
                    let target = if row.target_id_raw.is_empty() {
                        row.target.map(mod_target_id).unwrap_or("").to_owned()
                    } else {
                        row.target_id_raw.clone()
                    };
                    json!({
                        "source": source,
                        "target": target,
                        "amount": row.amount as f64,
                        "curve": row.curve as i64,
                        "muted": row.muted,
                        "order": i,
                    })
                })
                .collect(),
        )
    }

    /// Parses rows without touching a live matrix, for off-thread patch loading.
    /// Unknown IDs keep the row but flag it unresolved (SPEC 5.2) instead of
    /// dropping it.
    pub fn rows_from_json(json: &Value) -> Vec<ModRow> {
        let Some(arr) = json.as_array() else {
            return Vec::new();
        };
        arr.iter()
            .take(MATRIX_SLOTS)
            .map(|j| {
                let source_id_raw = j.get("source").and_then(Value::as_str).unwrap_or("").to_owned();
                let target_id_raw = j.get("target").and_then(Value::as_str).unwrap_or("").to_owned();
                let source = mod_source_from_id(&source_id_raw);
                let target = mod_target_from_id(&target_id_raw);
                ModRow {
                    source: source.unwrap_or(0),
                    target,
                    amount: (j.get("amount").and_then(Value::as_f64).unwrap_or(0.5) as f32).clamp(-1.0, 1.0),
                    curve: ModCurve::from_index(j.get("curve").and_then(json_int).unwrap_or(0)),
                    muted: j.get("muted").and_then(Value::as_bool).unwrap_or(false),
                    unresolved: source.is_none() || target.is_none(),
                    source_id_raw,
                    target_id_raw,
                }
            })
            .collect()
    }

    pub fn from_json(&mut self, json: &Value) {
        let rows = Self::rows_from_json(json);
        self.set_rows(&rows);
    }

    /// Recompiles into the inactive buffer and flips it in.
    fn publish(&mut self) {
        let mut compiled = [CompiledRow::default(); MATRIX_SLOTS];
        let mut count = 0;
        self.compiled_index_by_row.clear();
        self.compiled_index_by_row.resize(self.rows.len(), None);
        for (i, row) in self.rows.iter().enumerate() {
            if count >= MATRIX_SLOTS {
                break;
            }
            let Some(target) = row.target else { continue };
            if row.muted || row.unresolved || row.source == 0 || row.amount == 0.0 {
                continue;
            }
            self.compiled_index_by_row[i] = Some(count);
            compiled[count] = CompiledRow { source: row.source, target, amount: row.amount, curve: row.curve };
            count += 1;
        }
        self.shared.publish(count, &compiled);
    }
}

/// Integers may arrive as JSON floats from other writers.
fn json_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// The audio thread copies the compiled rows once per block; do the same here.
    fn compiled(matrix: &ModMatrix) -> Snapshot {
        let mut snap = Snapshot::default();
        matrix.copy_snapshot(&mut snap);
        snap
    }

    fn target(id: &str) -> Option<usize> {
        mod_target_from_id(id)
    }

    #[test]
    fn set_rows_replaces_the_whole_set_at_once() {
        let mut matrix = ModMatrix::new();
        matrix.add_row(ModSource::Lfo1.index(), target("filter.cutoff"), 0.5, ModCurve::Linear);
        let replacement: Vec<ModRow> = (0..3)
            .map(|i| ModRow {
                source: ModSource::EnvMod.index(),
                target: target("oscA.wtPos"),
                amount: 0.25 * (i + 1) as f32,
                ..ModRow::default()
            })
            .collect();
        matrix.set_rows(&replacement);
        assert_eq!(matrix.num_rows(), 3);
        assert_eq!(compiled(&matrix).count, 3);
        for i in 0..3 {
            assert_eq!(matrix.rows()[i].amount, 0.25 * (i + 1) as f32);
        }
    }

    #[test]
    fn set_rows_clamps_to_the_slot_limit() {
        let mut matrix = ModMatrix::new();
        let too_many = vec![
            ModRow {
                source: ModSource::Lfo1.index(),
                target: target("filter.cutoff"),
                amount: 0.4,
                ..ModRow::default()
            };
            MATRIX_SLOTS + 7
        ];
        matrix.set_rows(&too_many);
        assert_eq!(matrix.num_rows(), MATRIX_SLOTS);
        assert_eq!(compiled(&matrix).count, MATRIX_SLOTS);
    }

    /// Bulk row changes land in a single publish while a reader copies once per
    /// block; every copy must be a whole, valid row set. (The C++ test cycled
    /// the factory presets' row sets; these are generated to cover the same
    /// range of sizes, sources, targets and amounts.)
    #[test]
    fn snapshot_stays_self_consistent_while_row_sets_are_applied() {
        let row_sets: Vec<Vec<ModRow>> = (0..23)
            .map(|set| {
                (0..(set % (MATRIX_SLOTS + 1)))
                    .map(|i| ModRow {
                        source: 1 + (set * 7 + i * 3) % (NUM_MOD_SOURCES - 1),
                        target: Some((set + i * 5) % num_mod_targets()),
                        amount: (((set * 13 + i * 29) % 41) as f32 / 20.0 - 1.0).clamp(-1.0, 1.0),
                        curve: ModCurve::ALL[(set + i) % 4],
                        ..ModRow::default()
                    })
                    .collect()
            })
            .collect();

        let mut matrix = ModMatrix::new();
        let shared = matrix.shared();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (mut bad, mut seen) = (0, 0);
                let mut snap = Snapshot::default();
                while !stop.load(Ordering::Relaxed) {
                    shared.copy_snapshot(&mut snap);
                    if snap.count > MATRIX_SLOTS {
                        bad += 1;
                        continue;
                    }
                    for row in snap.rows() {
                        if row.source >= NUM_MOD_SOURCES
                            || row.target >= num_mod_targets()
                            || !row.amount.is_finite()
                            || row.amount.abs() > 1.0
                        {
                            bad += 1;
                        }
                    }
                    seen += 1;
                }
                (bad, seen)
            })
        };
        for pass in 0..20_000 {
            matrix.set_rows(&row_sets[pass % row_sets.len()]);
        }
        stop.store(true, Ordering::Relaxed);
        let (bad, seen) = reader.join().unwrap();
        assert!(seen > 0);
        assert_eq!(bad, 0);
    }

    #[test]
    fn targets_resolve_by_parameter_id_string() {
        let t = target("oscA.wtPos").expect("oscA.wtPos is a target");
        assert_eq!(mod_target(t).param, ParamId::OscAWtPos);
        assert_eq!(mod_target_id(t), "oscA.wtPos");
        assert_eq!(target("does.not.exist"), None);
        // A parameter that is deliberately not a target.
        assert_eq!(mod_target_index_for_param(ParamId::FilterType), None);
    }

    #[test]
    fn source_ids_round_trip() {
        for i in 0..NUM_MOD_SOURCES {
            assert_eq!(mod_source_from_id(mod_source_id(i)), Some(i), "{i}");
        }
        assert_eq!(mod_source_from_id("cc.74"), Some(cc_source_index(74)));
        assert_eq!(mod_source_from_id("nonsense"), None);
        assert_eq!(mod_source_display_name(cc_source_index(1)), "Mod Wheel (CC1)");
        assert_eq!(mod_source_display_name(cc_source_index(20)), "CC 20");
    }

    #[test]
    fn compiled_snapshot_skips_muted_and_unresolved_rows() {
        let mut matrix = ModMatrix::new();
        let cutoff = target("filter.cutoff");
        assert!(matrix.add_row(ModSource::Lfo1.index(), cutoff, 0.5, ModCurve::Linear));
        assert!(matrix.add_row(ModSource::Velocity.index(), cutoff, -0.25, ModCurve::Linear));
        assert_eq!(compiled(&matrix).count, 2);

        matrix.set_muted(1, true);
        assert_eq!(compiled(&matrix).count, 1);
        // Muted rows stay in the list.
        assert_eq!(matrix.num_rows(), 2);

        matrix.set_muted(1, false);
        matrix.set_amount(1, 0.0);
        // Zero amount contributes nothing.
        assert_eq!(compiled(&matrix).count, 1);
        assert_eq!(matrix.compiled_index_for_row(0), Some(0));
        assert_eq!(matrix.compiled_index_for_row(1), None);
    }

    #[test]
    fn slot_limit_is_enforced() {
        let mut matrix = ModMatrix::new();
        let t = target("filter.cutoff");
        for i in 0..MATRIX_SLOTS {
            assert!(matrix.add_row(ModSource::Lfo1.index(), t, 0.1, ModCurve::Linear), "{i}");
        }
        assert!(!matrix.add_row(ModSource::Lfo1.index(), t, 0.1, ModCurve::Linear));
        assert_eq!(matrix.num_rows(), MATRIX_SLOTS);
    }

    #[test]
    fn multiple_rows_on_one_target_sum() {
        let mut matrix = ModMatrix::new();
        let t = target("oscA.level");
        matrix.add_row(ModSource::Velocity.index(), t, 0.5, ModCurve::Linear);
        matrix.add_row(ModSource::ModWheel.index(), t, 0.25, ModCurve::Linear);
        let snap = compiled(&matrix);
        assert_eq!(snap.count, 2);
        let sum: f32 = snap
            .rows()
            .iter()
            .map(|r| apply_mod_curve(1.0, r.curve) * r.amount * mod_target(r.target).range)
            .sum();
        assert!((sum - 0.75).abs() < 1.0e-6);
        assert!((matrix.depth_for_target(t.unwrap()) - 0.75).abs() < 1.0e-6);
    }

    #[test]
    fn curves_shape_the_source() {
        let near = |a: f32, b: f32| (a - b).abs() < 1.0e-6;
        assert!(near(apply_mod_curve(0.5, ModCurve::Linear), 0.5));
        assert!(near(apply_mod_curve(0.5, ModCurve::Exp), 0.25));
        assert!(near(apply_mod_curve(0.25, ModCurve::Log), 0.5));
        assert!(near(apply_mod_curve(0.75, ModCurve::Switch), 1.0));
        assert!(near(apply_mod_curve(0.25, ModCurve::Switch), 0.0));
        // Bipolar sources keep their sign.
        assert!(near(apply_mod_curve(-0.5, ModCurve::Exp), -0.25));
        assert!(near(apply_mod_curve(-0.25, ModCurve::Log), -0.5));
        assert!(near(apply_mod_curve(-0.75, ModCurve::Switch), -1.0));
    }

    #[test]
    fn unknown_ids_are_kept_as_unresolved_rows() {
        let rows = json!([
            { "source": "lfo1", "target": "filter.cutoff", "amount": 0.5 },
            { "source": "lfo2", "target": "oscZ.mystery", "amount": 0.75 },
            { "source": "futureSource", "target": "filter.res", "amount": -0.5 },
        ]);
        let mut matrix = ModMatrix::new();
        matrix.from_json(&rows);
        assert_eq!(matrix.num_rows(), 3);
        assert!(!matrix.rows()[0].unresolved);
        assert!(matrix.rows()[1].unresolved);
        assert!(matrix.rows()[2].unresolved);
        // Unresolved rows are not audible.
        assert_eq!(compiled(&matrix).count, 1);

        // Round-tripping keeps the original IDs so the patch can move to a newer build.
        let json = matrix.to_json();
        assert_eq!(json[1]["target"], "oscZ.mystery");
        assert_eq!(json[2]["source"], "futureSource");
    }

    #[test]
    fn json_round_trip_preserves_rows() {
        let mut matrix = ModMatrix::new();
        matrix.add_row(ModSource::EnvMod.index(), target("filter.cutoff"), -0.8, ModCurve::Exp);
        matrix.add_row(cc_source_index(74), target("oscB.fmIndex"), 0.4, ModCurve::Switch);
        matrix.set_muted(1, true);

        let mut restored = ModMatrix::new();
        restored.from_json(&matrix.to_json());
        assert_eq!(restored.num_rows(), 2);
        let r = restored.rows();
        assert_eq!(r[0].source, ModSource::EnvMod.index());
        assert_eq!(r[0].target, target("filter.cutoff"));
        assert!((r[0].amount + 0.8).abs() < 1.0e-6);
        assert_eq!(r[0].curve, ModCurve::Exp);
        assert_eq!(r[1].source, cc_source_index(74));
        assert!(r[1].muted);
    }

    #[test]
    fn row_reordering_is_stable() {
        let mut matrix = ModMatrix::new();
        let a = target("oscA.level");
        let b = target("oscB.level");
        matrix.add_row(ModSource::Lfo1.index(), a, 0.1, ModCurve::Linear);
        matrix.add_row(ModSource::Lfo2.index(), b, 0.2, ModCurve::Linear);
        matrix.move_row(1, 0);
        assert_eq!(matrix.rows()[0].target, b);
        assert_eq!(matrix.rows()[1].target, a);
    }
}
