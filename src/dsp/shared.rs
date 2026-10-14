//! State shared between the audio thread, the editor and the main thread
//! (SPEC 7). One `Arc<SynthShared>` is owned by the plugin; the audio thread
//! renders with a [`super::synth::Synth`] that holds a clone of it.
//!
//! The rules mirror the C++ engine:
//! - The matrix row list is editor/main-thread state behind a mutex the audio
//!   thread never takes. Edits publish into [`SharedSnapshot`], which the
//!   audio thread copies once per block, lock-free.
//! - Wavetables and custom LFO shapes are built off the audio thread and
//!   published by atomic pointer swap. The audio thread keeps its own `Arc`
//!   for the current block, so the last drop of a replaced table never
//!   happens there.
//! - Everything the editor draws from the engine comes through relaxed
//!   atomics in [`Feedback`]; nothing locks.

use arc_swap::{ArcSwap, ArcSwapOption};
use atomic_float::AtomicF32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::any::Any;
use std::sync::{Arc, Mutex};

use super::common::MATRIX_SLOTS;
use super::mod_matrix::{ModMatrix, SharedSnapshot};
use super::params::{default_value, ParamId};
use super::wavetable::Wavetable;

const RELAXED: Ordering = Ordering::Relaxed;

pub struct SynthShared {
    /// Editor-thread row list. Never locked by the audio thread.
    pub matrix: Mutex<ModMatrix>,
    /// Audio-thread side of the matrix: `copy_snapshot` once per block.
    pub matrix_snapshot: Arc<SharedSnapshot>,
    /// Table per oscillator (A, B).
    pub tables: [ArcSwap<Wavetable>; 2],
    /// Custom LFO shapes (LFO 1, 2); `None` until a patch provides one.
    pub lfo_shapes: [ArcSwapOption<Vec<f32>>; 2],
    /// Bumped whenever a table changes, so editor views can refresh.
    pub table_revision: AtomicU32,
    /// Bumped on any matrix edit.
    pub matrix_revision: AtomicU32,
    /// Bumped only when the row set itself changes (add, remove, move, clear,
    /// source or target), as opposed to a value on an existing row.
    pub matrix_structure: AtomicU32,
    pub patch_name: Mutex<String>,
    pub feedback: Feedback,
    /// Tables and shapes replaced by `set_table`/`set_lfo_shape`, kept until
    /// the audio thread has dropped its own reference, so the final free
    /// happens here and never on the audio thread.
    retired: Mutex<Vec<Arc<dyn Any + Send + Sync>>>,
}

impl Default for SynthShared {
    fn default() -> Self {
        let matrix = ModMatrix::new();
        let matrix_snapshot = matrix.shared();
        let table = |id: ParamId| Arc::new(Wavetable::make_factory(default_value(id).round() as usize));
        Self {
            matrix: Mutex::new(matrix),
            matrix_snapshot,
            tables: [
                ArcSwap::new(table(ParamId::OscATable)),
                ArcSwap::new(table(ParamId::OscBTable)),
            ],
            lfo_shapes: [ArcSwapOption::empty(), ArcSwapOption::empty()],
            table_revision: AtomicU32::new(0),
            matrix_revision: AtomicU32::new(0),
            matrix_structure: AtomicU32::new(0),
            patch_name: Mutex::new("Init".to_owned()),
            feedback: Feedback::default(),
            retired: Mutex::new(Vec::new()),
        }
    }
}

impl SynthShared {
    /// Publishes a table for one oscillator (0 = A, 1 = B).
    pub fn set_table(&self, osc: usize, table: Arc<Wavetable>) {
        let old = self.tables[osc.min(1)].swap(table);
        self.retire(old);
        self.table_revision.fetch_add(1, RELAXED);
    }

    pub fn table(&self, osc: usize) -> Arc<Wavetable> {
        self.tables[osc.min(1)].load_full()
    }

    pub fn set_lfo_shape(&self, lfo: usize, shape: Option<Arc<Vec<f32>>>) {
        if let Some(old) = self.lfo_shapes[lfo.min(1)].swap(shape) {
            self.retire(old);
        }
    }

    /// Parks a replaced value, and frees every parked value nobody else holds.
    fn retire(&self, old: Arc<dyn Any + Send + Sync>) {
        if let Ok(mut retired) = self.retired.lock() {
            retired.push(old);
            retired.retain(|r| Arc::strong_count(r) > 1);
        }
    }

    pub fn lfo_shape(&self, lfo: usize) -> Option<Arc<Vec<f32>>> {
        self.lfo_shapes[lfo.min(1)].load_full()
    }

    /// Runs an edit on the matrix row list and records whether the row set
    /// itself changed. Editor and main thread only.
    pub fn edit_matrix<R>(&self, structure: bool, f: impl FnOnce(&mut ModMatrix) -> R) -> R {
        let mut m = self.matrix.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut m);
        self.matrix_revision.fetch_add(1, RELAXED);
        if structure {
            self.matrix_structure.fetch_add(1, RELAXED);
        }
        r
    }

    pub fn patch_name(&self) -> String {
        self.patch_name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    pub fn set_patch_name(&self, name: &str) {
        if let Ok(mut n) = self.patch_name.lock() {
            *n = name.to_owned();
        }
    }
}

/// Everything the editor reads from the audio thread. Relaxed atomics only;
/// tearing between fields is invisible at display rates.
pub struct Feedback {
    /// The editor sets this while it is open: the engine then captures the
    /// analyser taps and keeps voice LFOs and the MOD envelope running for
    /// their displays even when nothing listens to them.
    pub gui_attached: AtomicBool,
    pub voice_count: AtomicU32,
    /// Output peak per channel (decaying on the audio thread).
    pub meters: [AtomicF32; 2],
    /// Peak output of each oscillator across all voices, for the level LEDs.
    pub osc_meters: [AtomicF32; 2],
    /// Frame actually played by each oscillator (max across voices), as a
    /// 0..1 position in the table.
    pub display_frame: [AtomicF32; 2],
    /// AMP and MOD envelope of the newest voice: level and stage
    /// (`envelope::Stage as u8`, 0 = idle).
    pub env_level: [AtomicF32; 2],
    pub env_stage: [AtomicU8; 2],
    /// LFO 1 and 2 outputs (newest voice, or the free-running LFO).
    pub lfo_value: [AtomicF32; 2],
    /// Cutoff actually used by the newest voice, for the response curve.
    pub cutoff_hz: AtomicF32,
    /// Absolute contribution of each compiled matrix row, for the row pulse.
    pub row_activity: [AtomicF32; MATRIX_SLOTS],
    /// Post-FX output and pre-filter voice mix, for the analyser and scope.
    pub analyzer_wet: AnalyzerRing,
    pub analyzer_dry: AnalyzerRing,
    /// Smoothed fraction of the real-time budget used by `process()`.
    pub cpu_load: AtomicF32,
    /// Engine sample rate, for the analyser's frequency axis.
    pub sample_rate: AtomicF32,
}

impl Default for Feedback {
    fn default() -> Self {
        Self {
            gui_attached: AtomicBool::new(false),
            voice_count: AtomicU32::new(0),
            meters: std::array::from_fn(|_| AtomicF32::new(0.0)),
            osc_meters: std::array::from_fn(|_| AtomicF32::new(0.0)),
            display_frame: std::array::from_fn(|_| AtomicF32::new(0.0)),
            env_level: std::array::from_fn(|_| AtomicF32::new(0.0)),
            env_stage: std::array::from_fn(|_| AtomicU8::new(0)),
            lfo_value: std::array::from_fn(|_| AtomicF32::new(0.0)),
            cutoff_hz: AtomicF32::new(20_000.0),
            row_activity: std::array::from_fn(|_| AtomicF32::new(0.0)),
            analyzer_wet: AnalyzerRing::default(),
            analyzer_dry: AnalyzerRing::default(),
            cpu_load: AtomicF32::new(0.0),
            sample_rate: AtomicF32::new(48_000.0),
        }
    }
}

/// Single-producer ring buffer feeding the GUI analyser and scope. Samples are
/// atomics so a reader racing the writer sees old or new values, never torn ones.
pub struct AnalyzerRing {
    data: Box<[AtomicF32]>,
    write: AtomicUsize,
}

impl AnalyzerRing {
    pub const SIZE: usize = 4096;

    pub fn push(&self, samples: &[f32]) {
        let mut w = self.write.load(RELAXED);
        for &s in samples {
            self.data[w].store(s, RELAXED);
            w = (w + 1) & (Self::SIZE - 1);
        }
        self.write.store(w, Ordering::Release);
    }

    /// Copies the most recent `dest.len()` samples in chronological order.
    pub fn copy_latest(&self, dest: &mut [f32]) {
        let n = dest.len().min(Self::SIZE);
        let w = self.write.load(Ordering::Acquire);
        for (i, d) in dest.iter_mut().take(n).enumerate() {
            let idx = (w + Self::SIZE - n + i) & (Self::SIZE - 1);
            *d = self.data[idx].load(RELAXED);
        }
    }

    pub fn clear(&self) {
        for d in self.data.iter() {
            d.store(0.0, RELAXED);
        }
    }
}

impl Default for AnalyzerRing {
    fn default() -> Self {
        Self {
            data: (0..Self::SIZE).map(|_| AtomicF32::new(0.0)).collect(),
            write: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_ring_returns_latest_in_order() {
        let ring = AnalyzerRing::default();
        let input: Vec<f32> = (0..5000).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = [0.0; 4];
        ring.copy_latest(&mut out);
        assert_eq!(out, [4996.0, 4997.0, 4998.0, 4999.0]);
    }

    #[test]
    fn defaults_load_the_default_tables() {
        let s = SynthShared::default();
        assert_eq!(s.table(0).factory_index(), Some(2));
        assert_eq!(s.table(1).factory_index(), Some(3));
        assert!(s.lfo_shape(0).is_none());
    }
}
