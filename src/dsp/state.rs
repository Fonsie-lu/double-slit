//! Patch serialisation (SPEC 5): versioned JSON holding every parameter by
//! stable ID, the modulation matrix, embedded wavetables (deduplicated by
//! content hash), custom LFO shapes and GUI state.
//!
//! The JSON shape is the one the C++ engine wrote, so existing patches load:
//! `{"schema", "product", "name", "params": {id: value}, "matrix",
//! "tablePool": {hash: table}, "tables": {"oscA": hash | {"factory": n}},
//! "lfoShapes": {"lfo1": [..]}, "gui": {"tab", "zoom"}}`.

use serde_json::{json, Map, Value};
use std::sync::Arc;

use super::mod_matrix::{ModMatrix, ModRow};
use super::params::{clamp_param, defaults, descs, ParamId, NUM_PARAMS};
use super::shared::SynthShared;
use super::wavetable::Wavetable;

pub const PATCH_SCHEMA_VERSION: i64 = 1;

/// Product tag written into every patch. Loading ignores it, so patches from
/// the C++ build (tagged "opus-serum") still load.
const PRODUCT: &str = "double-slit";

const SLOT_NAMES: [&str; 2] = ["oscA", "oscB"];
const SHAPE_NAMES: [&str; 2] = ["lfo1", "lfo2"];

#[derive(Clone, Debug, PartialEq)]
pub struct GuiState {
    pub tab: usize,
    pub zoom: f32,
}

impl Default for GuiState {
    fn default() -> Self {
        Self { tab: 0, zoom: 1.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatchInfo {
    pub name: String,
    pub gui: GuiState,
}

impl Default for PatchInfo {
    fn default() -> Self {
        Self {
            name: "Init".to_owned(),
            gui: GuiState::default(),
        }
    }
}

/// A patch resolved into plain values: everything an engine needs, produced
/// without touching one. The plugin builds this off the audio thread and then
/// hands the parameters to the host rather than writing them into the engine
/// while the audio thread is reading it.
#[derive(Clone)]
pub struct PatchData {
    pub params: [f32; NUM_PARAMS],
    pub rows: Vec<ModRow>,
    /// After [`load_patch_data`] or a factory preset both slots are `Some`;
    /// `None` in a hand-built value means "keep the current table".
    pub tables: [Option<Arc<Wavetable>>; 2],
    pub lfo_shapes: [Option<Arc<Vec<f32>>>; 2],
    pub info: PatchInfo,
}

impl Default for PatchData {
    fn default() -> Self {
        Self {
            params: defaults(),
            rows: Vec::new(),
            tables: [None, None],
            lfo_shapes: [None, None],
            info: PatchInfo::default(),
        }
    }
}

/// Factory table for an oscillator table-choice value; "Custom" and anything
/// out of range clamp onto the factory list.
fn factory_table(choice: f32) -> Arc<Wavetable> {
    Arc::new(Wavetable::make_factory(choice as usize))
}

fn table_param(slot: usize) -> ParamId {
    if slot == 0 {
        ParamId::OscATable
    } else {
        ParamId::OscBTable
    }
}

/// Serialises the engine state to JSON text.
///
/// `params` supplies the parameter values; the engine does not own them (the
/// host does), so the caller passes the host's values. With `None` the
/// "params" section is left out entirely: the plugin stores host-owned
/// parameters itself and uses this for everything else.
pub fn save_patch(
    shared: &SynthShared,
    params: Option<&[f32; NUM_PARAMS]>,
    info: &PatchInfo,
    pretty: bool,
) -> String {
    let mut root = Map::new();
    root.insert("schema".into(), json!(PATCH_SCHEMA_VERSION));
    root.insert("product".into(), json!(PRODUCT));
    root.insert("name".into(), json!(info.name));

    if let Some(values) = params {
        let obj: Map<String, Value> = descs()
            .iter()
            .zip(values.iter())
            .map(|(d, v)| (d.id_string.clone(), json!(*v as f64)))
            .collect();
        root.insert("params".into(), Value::Object(obj));
    }

    let matrix = shared.matrix.lock().unwrap_or_else(|e| e.into_inner()).to_json();
    root.insert("matrix".into(), matrix);

    // Custom wavetables are embedded so patches are self-contained, and
    // deduplicated by content hash so A == B costs one copy (SPEC 5.1/5.3).
    // An untouched factory table is stored as its index instead: embedding it
    // cost ~180 kB of base64 per slot for samples we can rebuild from nothing.
    let mut pool = Map::new();
    let mut refs = Map::new();
    for (slot, name) in SLOT_NAMES.iter().enumerate() {
        let table = shared.table(slot);
        if let Some(index) = table.factory_index() {
            refs.insert((*name).into(), json!({ "factory": index }));
            continue;
        }
        let key = table.content_hash().to_string();
        if !pool.contains_key(&key) {
            pool.insert(key.clone(), table.to_json());
        }
        refs.insert((*name).into(), json!(key));
    }
    root.insert("tablePool".into(), Value::Object(pool));
    root.insert("tables".into(), Value::Object(refs));

    let mut shapes = Map::new();
    for (i, name) in SHAPE_NAMES.iter().enumerate() {
        if let Some(points) = shared.lfo_shape(i).filter(|p| !p.is_empty()) {
            let arr: Vec<Value> = points.iter().map(|v| json!(*v as f64)).collect();
            shapes.insert((*name).into(), Value::Array(arr));
        }
    }
    root.insert("lfoShapes".into(), Value::Object(shapes));

    root.insert(
        "gui".into(),
        json!({ "tab": info.gui.tab, "zoom": info.gui.zoom as f64 }),
    );

    let root = Value::Object(root);
    if pretty {
        serde_json::to_string_pretty(&root).unwrap_or_default()
    } else {
        root.to_string()
    }
}

/// Parses a patch into plain values. Returns `None` on malformed JSON, so a
/// failed parse leaves the caller's data alone.
///
/// Forwards compatible: newer schemas still load and unknown members,
/// including unknown parameters, are ignored. A missing "params" section
/// yields the defaults; unknown matrix source/target IDs keep their row,
/// flagged as unresolved.
pub fn load_patch_data(json_text: &str) -> Option<PatchData> {
    let root: Value = serde_json::from_str(json_text).ok()?;
    if !root.is_object() {
        return None;
    }

    let mut out = PatchData::default();
    out.info.name = root["name"].as_str().unwrap_or("Init").to_owned();

    let params = &root["params"];
    for d in descs() {
        let value = params[&d.id_string].as_f64().map_or(d.default, |v| v as f32);
        out.params[d.id.index()] = clamp_param(d.id, value);
    }

    out.rows = ModMatrix::rows_from_json(&root["matrix"]);

    let pool = &root["tablePool"];
    let refs = &root["tables"];
    for (slot, name) in SLOT_NAMES.iter().enumerate() {
        let r = &refs[*name];
        let table = if r["factory"].is_number() {
            let index = r["factory"].as_f64().unwrap_or(0.0).max(0.0) as usize;
            Some(Arc::new(Wavetable::make_factory(index)))
        } else if let Some(entry) = r.as_str().map(|key| &pool[key]).filter(|e| e.is_object()) {
            embedded(entry)
        } else if r.is_object() {
            // Inline table (older or hand-written patches).
            embedded(r)
        } else {
            None
        };
        let fallback = || factory_table(out.params[table_param(slot).index()]);
        out.tables[slot] = Some(table.unwrap_or_else(fallback));
    }

    let shapes = &root["lfoShapes"];
    for (i, name) in SHAPE_NAMES.iter().enumerate() {
        let Some(arr) = shapes[*name].as_array().filter(|a| a.len() >= 2) else {
            continue;
        };
        let points: Vec<f32> = arr
            .iter()
            .map(|v| (v.as_f64().unwrap_or(0.0) as f32).clamp(-1.0, 1.0))
            .collect();
        out.lfo_shapes[i] = Some(Arc::new(points));
    }

    let gui = &root["gui"];
    out.info.gui.tab = gui["tab"].as_f64().map_or(0, |t| t.clamp(0.0, 4.0) as usize);
    out.info.gui.zoom = gui["zoom"].as_f64().map_or(1.0, |z| z as f32).clamp(0.5, 3.0);
    Some(out)
}

/// An embedded table, or `None` when the entry carries no sample data (the
/// caller then falls back to the factory table the parameter names).
fn embedded(entry: &Value) -> Option<Arc<Wavetable>> {
    entry["data"].is_string().then(|| Arc::new(Wavetable::from_json(entry)))
}

/// Publishes resolved data into the shared engine state - rows in a single
/// publish, tables and LFO shapes by atomic swap, the patch name - and
/// returns the parameter values. The engine does not own parameters, so the
/// caller sends them to the host (plugin) or straight into a synth (tests).
pub fn apply_patch_data(data: &PatchData, shared: &SynthShared) -> [f32; NUM_PARAMS] {
    shared.edit_matrix(true, |m| m.set_rows(&data.rows));
    for slot in 0..2 {
        if let Some(table) = &data.tables[slot] {
            shared.set_table(slot, table.clone());
        }
        shared.set_lfo_shape(slot, data.lfo_shapes[slot].clone());
    }
    shared.set_patch_name(&data.info.name);
    data.params
}

/// Loads a patch into the shared state. Returns the info and parameter values,
/// or `None` on malformed JSON (nothing is touched then).
pub fn load_patch(json_text: &str, shared: &SynthShared) -> Option<(PatchInfo, [f32; NUM_PARAMS])> {
    let data = load_patch_data(json_text)?;
    let params = apply_patch_data(&data, shared);
    Some((data.info, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::common::{FRAME_SIZE, TWO_PI};
    use crate::dsp::mod_matrix::{cc_source_index, mod_target_from_id, ModCurve, ModSource};
    use crate::dsp::params::{default_value, CUSTOM_TABLE_CHOICE};
    use crate::dsp::presets::{build_factory_preset, factory_preset_name, num_factory_presets};

    fn param_dump(params: &[f32; NUM_PARAMS]) -> String {
        descs()
            .iter()
            .map(|d| format!("{}={};", d.id_string, params[d.id.index()]))
            .collect()
    }

    fn matrix_dump(shared: &SynthShared) -> String {
        shared.matrix.lock().unwrap().to_json().to_string()
    }

    /// The single-threaded equivalent of the C++ `applyFactoryPreset`.
    fn apply_preset(i: usize, shared: &SynthShared) -> [f32; NUM_PARAMS] {
        apply_patch_data(&build_factory_preset(i).unwrap(), shared)
    }

    #[test]
    fn round_trip_reproduces_params_and_matrix() {
        let shared = SynthShared::default();
        let mut params = defaults();
        params[ParamId::FilterCutoff.index()] = 1234.5;
        params[ParamId::OscAUniVoices.index()] = 7.0;
        params[ParamId::EnvAmpRelease.index()] = 3.25;
        params[ParamId::MasterOversampling.index()] = 2.0;
        shared.edit_matrix(true, |m| {
            m.add_row(ModSource::Lfo1.index(), mod_target_from_id("filter.cutoff"), -0.6, ModCurve::Log);
            m.add_row(cc_source_index(11), mod_target_from_id("oscA.wtPos"), 0.35, ModCurve::Linear);
        });

        let info = PatchInfo {
            name: "Test Patch".into(),
            gui: GuiState { tab: 3, zoom: 1.5 },
        };
        let json = save_patch(&shared, Some(&params), &info, false);

        let loaded = SynthShared::default();
        let (loaded_info, loaded_params) = load_patch(&json, &loaded).unwrap();
        assert_eq!(param_dump(&loaded_params), param_dump(&params));
        assert_eq!(matrix_dump(&loaded), matrix_dump(&shared));
        assert_eq!(loaded_info.name, "Test Patch");
        assert_eq!(loaded_info.gui.tab, 3);
        assert_eq!(loaded_info.gui.zoom, 1.5);
        assert_eq!(loaded.patch_name(), "Test Patch");
    }

    #[test]
    fn embedded_wavetable_survives_round_trip() {
        let shared = SynthShared::default();
        let frames: Vec<Vec<f32>> = (0..4)
            .map(|f| {
                (0..FRAME_SIZE)
                    .map(|i| {
                        let p = i as f32 / FRAME_SIZE as f32;
                        (TWO_PI * p * (f + 1) as f32).sin() * (0.5 + 0.1 * f as f32)
                    })
                    .collect()
            })
            .collect();
        let mut table = Wavetable::default();
        table.set_frames(&frames, "Drawn Table");
        // Not a factory table, so it must be embedded.
        assert_eq!(table.factory_index(), None);
        let table = Arc::new(table);
        shared.set_table(0, table.clone());
        let mut params = defaults();
        params[ParamId::OscATable.index()] = CUSTOM_TABLE_CHOICE as f32;

        let json = save_patch(&shared, Some(&params), &PatchInfo::default(), false);
        let loaded = SynthShared::default();
        load_patch(&json, &loaded).unwrap();

        let restored = loaded.table(0);
        assert_eq!(restored.name(), "Drawn Table");
        assert_eq!(restored.num_frames(), 4);
        assert_eq!(restored.content_hash(), table.content_hash());
        for f in 0..4 {
            for i in (0..FRAME_SIZE).step_by(97) {
                assert!((restored.frame_data(f)[i] - table.frame_data(f)[i]).abs() < 1.0e-6);
            }
        }
    }

    #[test]
    fn identical_tables_are_deduplicated_in_the_pool() {
        let shared = SynthShared::default();
        // An edited table has to be embedded; two slots sharing it cost one copy.
        let mut edited = Wavetable::make_factory(6);
        let second = edited.frame_copy(1);
        edited.set_frame(0, &second);
        let edited = Arc::new(edited);
        shared.set_table(0, edited.clone());
        shared.set_table(1, edited);

        let json = save_patch(&shared, Some(&defaults()), &PatchInfo::default(), false);
        let root: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(root["tablePool"].as_object().unwrap().len(), 1);
        assert_eq!(root["tables"]["oscA"].as_str(), root["tables"]["oscB"].as_str());
        assert!(root["tables"]["oscA"].is_string());
    }

    #[test]
    fn untouched_factory_tables_are_stored_by_index_not_embedded() {
        let shared = SynthShared::default();
        // Digi Vowel (17 frames) and Analog Saws (16 frames).
        shared.set_table(0, Arc::new(Wavetable::make_factory(13)));
        shared.set_table(1, Arc::new(Wavetable::make_factory(5)));

        let json = save_patch(&shared, Some(&defaults()), &PatchInfo::default(), false);
        let root: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(root["tablePool"].as_object().unwrap().len(), 0);
        assert_eq!(root["tables"]["oscA"]["factory"].as_i64(), Some(13));
        assert_eq!(root["tables"]["oscB"]["factory"].as_i64(), Some(5));
        // Embedding these cost ~360 kB of base64 for samples we can just rebuild.
        assert!(json.len() < 32 * 1024, "{} bytes", json.len());

        let loaded = SynthShared::default();
        load_patch(&json, &loaded).unwrap();
        assert_eq!(loaded.table(0).content_hash(), shared.table(0).content_hash());
        assert_eq!(loaded.table(1).content_hash(), shared.table(1).content_hash());
    }

    #[test]
    fn custom_lfo_shape_persists() {
        let shared = SynthShared::default();
        let points = Arc::new(vec![0.0, 0.5, 1.0, -1.0, -0.25]);
        shared.set_lfo_shape(1, Some(points.clone()));

        let json = save_patch(&shared, Some(&defaults()), &PatchInfo::default(), false);
        let loaded = SynthShared::default();
        load_patch(&json, &loaded).unwrap();

        let restored = loaded.lfo_shape(1).expect("shape restored");
        assert_eq!(restored.len(), points.len());
        for (a, b) in restored.iter().zip(points.iter()) {
            assert!((a - b).abs() < 1.0e-6);
        }
        assert!(loaded.lfo_shape(0).is_none());
    }

    #[test]
    fn resolved_patch_data_matches_loading_into_an_engine() {
        let source = SynthShared::default();
        let mut params = apply_preset(4, &source);
        params[ParamId::FilterCutoff.index()] = 987.0;
        source.set_lfo_shape(0, Some(Arc::new(vec![0.0, 1.0, -1.0, 0.5])));

        let info = PatchInfo {
            name: "Resolved".into(),
            ..PatchInfo::default()
        };
        let json = save_patch(&source, Some(&params), &info, false);

        let data = load_patch_data(&json).unwrap();
        assert_eq!(data.info.name, "Resolved");
        assert_eq!(data.params[ParamId::FilterCutoff.index()], 987.0);
        assert!(data.tables[0].is_some());
        assert_eq!(data.lfo_shapes[0].as_ref().map(|s| s.len()), Some(4));
        assert!(data.lfo_shapes[1].is_none());

        let via_data = SynthShared::default();
        let data_params = apply_patch_data(&data, &via_data);
        let via_load = SynthShared::default();
        let (_, load_params) = load_patch(&json, &via_load).unwrap();

        assert_eq!(param_dump(&data_params), param_dump(&load_params));
        assert_eq!(matrix_dump(&via_data), matrix_dump(&via_load));
        assert_eq!(via_data.table(0).content_hash(), via_load.table(0).content_hash());
    }

    #[test]
    fn malformed_patch_data_is_rejected() {
        let data = load_patch_data("{\"schema\":1,\"name\":\"Keep\"}").unwrap();
        assert_eq!(data.info.name, "Keep");
        assert!(load_patch_data("not json").is_none());
        assert!(load_patch_data("").is_none());
        assert!(load_patch_data("[1, 2]").is_none());

        let shared = SynthShared::default();
        assert!(load_patch("this is not json", &shared).is_none());
        assert_eq!(shared.patch_name(), "Init", "a failed load touches nothing");
    }

    #[test]
    fn missing_parameters_fall_back_to_defaults() {
        let shared = SynthShared::default();
        let (info, params) =
            load_patch("{\"schema\":1,\"name\":\"Sparse\",\"params\":{\"oscA.level\":0.5}}", &shared).unwrap();
        assert_eq!(params[ParamId::OscALevel.index()], 0.5);
        assert_eq!(params[ParamId::FilterCutoff.index()], default_value(ParamId::FilterCutoff));
        assert_eq!(info.name, "Sparse");
    }

    #[test]
    fn patch_without_params_section_round_trips_the_rest() {
        let shared = SynthShared::default();
        apply_preset(2, &shared);
        let json = save_patch(&shared, None, &PatchInfo::default(), false);
        let root: Value = serde_json::from_str(&json).unwrap();
        assert!(root.get("params").is_none());

        let loaded = SynthShared::default();
        let (_, params) = load_patch(&json, &loaded).unwrap();
        assert_eq!(param_dump(&params), param_dump(&defaults()));
        assert_eq!(matrix_dump(&loaded), matrix_dump(&shared));
    }

    #[test]
    fn unknown_ids_are_kept_as_unresolved_rows_and_resaved() {
        let json = r#"{"schema":1,"name":"Future","params":{"future.param":1.0},
            "matrix":[{"source":"lfo1","target":"future.target","amount":0.25,"curve":0,"muted":false,"order":0}]}"#;
        let shared = SynthShared::default();
        load_patch(json, &shared).unwrap();
        let saved = save_patch(&shared, None, &PatchInfo::default(), false);
        let rows = ModMatrix::rows_from_json(&serde_json::from_str::<Value>(&saved).unwrap()["matrix"]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].unresolved);
        assert_eq!(rows[0].source_id_raw, "lfo1");
        assert_eq!(rows[0].target_id_raw, "future.target");
        assert_eq!(rows[0].amount, 0.25);
    }

    #[test]
    fn presets_round_trip_through_patch_format() {
        for i in 0..num_factory_presets() {
            let shared = SynthShared::default();
            let params = apply_preset(i, &shared);
            let info = PatchInfo {
                name: factory_preset_name(i).into(),
                ..PatchInfo::default()
            };
            let json = save_patch(&shared, Some(&params), &info, false);

            let loaded = SynthShared::default();
            let (_, loaded_params) = load_patch(&json, &loaded).unwrap();
            assert_eq!(param_dump(&loaded_params), param_dump(&params), "{}", factory_preset_name(i));
            assert_eq!(matrix_dump(&loaded), matrix_dump(&shared), "{}", factory_preset_name(i));
        }
    }
}
