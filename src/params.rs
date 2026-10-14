//! Host-facing parameters, generated from the engine's table in
//! [`crate::dsp::params`]. The JUCE build had an `AudioProcessorValueTreeState`
//! layout built from the same table; this is its nih-plug equivalent, with a
//! hand-written [`Params`] impl because the parameter set is data, not fields.
//!
//! Continuous parameters become [`FloatParam`]s with the table's skew, discrete
//! and choice parameters become [`IntParam`]s showing the choice names. Values
//! reach the engine once per block in natural units (`plain_value`).

use nih_plug::params::persist::{deserialize_field, serialize_field, PersistentField};
use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use crate::dsp::params::{descs, ParamDesc, ParamId, NUM_PARAMS};

/// Patch state the host does not own: matrix rows, wavetables, custom LFO
/// shapes and the patch name. It travels inside the plugin state next to the
/// parameters, as the JSON the engine's patch format uses.
pub trait ExtraState: Send + Sync {
    /// Serialises everything except the parameter values.
    fn save(&self) -> String;
    /// Restores it. Called on the main thread; implementations publish to the
    /// audio thread lock-free.
    fn load(&self, json: &str);
}

pub enum HostParam {
    Float(FloatParam),
    Int(IntParam),
}

impl HostParam {
    fn new(d: &'static ParamDesc) -> Self {
        if d.steps > 0 {
            let choices = d.choices;
            let min = d.min as i32;
            let mut p = IntParam::new(
                d.name,
                d.default.round() as i32,
                IntRange::Linear { min, max: d.max as i32 },
            );
            if !choices.is_empty() {
                p = p
                    .with_value_to_string(Arc::new(move |v| {
                        choices[(v - min).clamp(0, choices.len() as i32 - 1) as usize].to_owned()
                    }))
                    .with_string_to_value(Arc::new(move |s| {
                        choices
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(s.trim()))
                            .map(|i| i as i32 + min)
                    }));
            } else if !d.unit.is_empty() {
                p = p.with_unit(match d.unit {
                    "oct" => " oct",
                    "st" => " st",
                    other => other,
                });
            }
            HostParam::Int(p)
        } else {
            let factor = d.skew_factor();
            let range = if (factor - 1.0).abs() < 1.0e-6 {
                FloatRange::Linear { min: d.min, max: d.max }
            } else {
                FloatRange::Skewed { min: d.min, max: d.max, factor }
            };
            let p = FloatParam::new(d.name, d.default, range)
                .with_value_to_string(Arc::new(move |v| d.format(v)))
                .with_string_to_value(Arc::new(move |s| parse_value(d, s)));
            HostParam::Float(p)
        }
    }

    pub fn ptr(&self) -> ParamPtr {
        match self {
            HostParam::Float(p) => p.as_ptr(),
            HostParam::Int(p) => p.as_ptr(),
        }
    }

    /// Current value in natural units.
    pub fn value(&self) -> f32 {
        match self {
            HostParam::Float(p) => p.value(),
            HostParam::Int(p) => p.value() as f32,
        }
    }

    /// Normalised value without host modulation.
    pub fn normalized(&self) -> f32 {
        match self {
            HostParam::Float(p) => p.unmodulated_normalized_value(),
            HostParam::Int(p) => p.unmodulated_normalized_value(),
        }
    }

    pub fn default_normalized(&self) -> f32 {
        match self {
            HostParam::Float(p) => p.default_normalized_value(),
            HostParam::Int(p) => p.default_normalized_value(),
        }
    }

    // Host gesture helpers for the editor. `set` must sit between `begin` and `end`.

    pub fn begin(&self, s: &ParamSetter) {
        match self {
            HostParam::Float(p) => s.begin_set_parameter(p),
            HostParam::Int(p) => s.begin_set_parameter(p),
        }
    }

    pub fn set(&self, s: &ParamSetter, normalized: f32) {
        match self {
            HostParam::Float(p) => s.set_parameter_normalized(p, normalized),
            HostParam::Int(p) => s.set_parameter_normalized(p, normalized),
        }
    }

    pub fn end(&self, s: &ParamSetter) {
        match self {
            HostParam::Float(p) => s.end_set_parameter(p),
            HostParam::Int(p) => s.end_set_parameter(p),
        }
    }
}

/// Accepts plain numbers in the parameter's unit, plus "ms"/"kHz"/"%" suffixes.
fn parse_value(d: &ParamDesc, s: &str) -> Option<f32> {
    let t = s.trim().to_ascii_lowercase();
    let (num, scale) = if let Some(n) = t.strip_suffix("khz") {
        (n, 1000.0)
    } else if let Some(n) = t.strip_suffix("ms") {
        (n, if d.unit == "s" { 0.001 } else { 1.0 })
    } else if let Some(n) = t.strip_suffix('%') {
        (n, if d.unit.is_empty() { 0.01 } else { 1.0 })
    } else {
        (t.trim_end_matches(|c: char| c.is_alphabetic()), 1.0)
    };
    num.trim().parse::<f32>().ok().map(|v| d.clamp(v * scale))
}

pub struct DoubleSlitParams {
    pub editor_state: Arc<EguiState>,
    /// Selected GUI colour scheme, an index into `editor::theme::THEMES`.
    /// Persisted with the project but not automatable.
    pub theme: Arc<AtomicU8>,
    /// One host parameter per entry of the engine table, in table order.
    values: Vec<HostParam>,
    extras: Arc<dyn ExtraState>,
}

impl DoubleSlitParams {
    pub fn new(extras: Arc<dyn ExtraState>) -> Self {
        Self {
            editor_state: EguiState::from_size(crate::editor::WIDTH, crate::editor::HEIGHT),
            theme: Arc::new(AtomicU8::new(crate::editor::theme::DEFAULT_THEME)),
            values: descs().iter().map(HostParam::new).collect(),
            extras,
        }
    }

    pub fn get(&self, id: ParamId) -> &HostParam {
        &self.values[id.index()]
    }

    /// Current value in natural units.
    pub fn value(&self, id: ParamId) -> f32 {
        self.values[id.index()].value()
    }

    /// Every value in natural units, in table order.
    pub fn snapshot(&self) -> [f32; NUM_PARAMS] {
        std::array::from_fn(|i| self.values[i].value())
    }
}

unsafe impl Params for DoubleSlitParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        // Grouped by module so hosts show "Osc A / WT Pos" rather than one
        // flat list of 100+ entries.
        descs()
            .iter()
            .zip(&self.values)
            .map(|(d, p)| (d.id_string.clone(), p.ptr(), d.module.to_owned()))
            .collect()
    }

    fn serialize_fields(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        if let Ok(s) = PersistentField::map(&self.editor_state, serialize_field) {
            map.insert("editor-state".to_owned(), s);
        }
        if let Ok(s) = PersistentField::map(&self.theme, serialize_field) {
            map.insert("theme".to_owned(), s);
        }
        map.insert("patch".to_owned(), self.extras.save());
        map
    }

    fn deserialize_fields(&self, serialized: &BTreeMap<String, String>) {
        if let Some(s) = serialized.get("editor-state") {
            match deserialize_field(s) {
                Ok(v) => PersistentField::set(&self.editor_state, v),
                Err(e) => nih_debug_assert_failure!("could not restore editor-state: {}", e),
            }
        }
        if let Some(s) = serialized.get("theme") {
            if let Ok(v) = deserialize_field::<u8>(s) {
                PersistentField::set(&self.theme, v);
            }
        }
        if let Some(s) = serialized.get("patch") {
            self.extras.load(s);
        }
    }
}
