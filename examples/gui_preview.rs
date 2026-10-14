//! Opens the editor in a standalone window without a plugin host. Parameter
//! changes go into a local store so every control can be operated and checked.
//!
//!   cargo run --release --example gui_preview
//!   DOUBLE_SLIT_THEME=0..3 DOUBLE_SLIT_TAB=0..4 cargo run --release --example gui_preview
//!
//! With `DOUBLE_SLIT_DEMO=1` the engine plays a repeating chord in a
//! background thread at real-time pace (no audio output), following every
//! edit made in the window, so the live displays can be checked.
use baseview::{Size, WindowOpenOptions, WindowScalePolicy};
use double_slit::dsp::params::{ParamId, NUM_PARAMS};
use double_slit::dsp::shared::SynthShared;
use double_slit::dsp::synth::Synth;
use double_slit::editor::{self, EditorState, PreviewStore};
use double_slit::params::{DoubleSlitParams, ExtraState};
use egui_baseview::{EguiWindow, GraphicsConfig};
use nih_plug::prelude::*;
use nih_plug::wrapper::state::PluginState;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct PreviewContext {
    store: PreviewStore,
}

impl GuiContext for PreviewContext {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn request_resize(&self) -> bool {
        false
    }
    unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {}
    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        self.store.lock().unwrap().insert(param, normalized);
    }
    unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}
    fn get_state(&self) -> PluginState {
        unimplemented!()
    }
    fn set_state(&self, _state: PluginState) {}
}

struct NoExtras;
impl ExtraState for NoExtras {
    fn save(&self) -> String {
        String::new()
    }
    fn load(&self, _json: &str) {}
}

fn main() {
    let params = Arc::new(DoubleSlitParams::new(Arc::new(NoExtras)));
    if let Some(i) = std::env::var("DOUBLE_SLIT_THEME").ok().and_then(|v| v.parse().ok()) {
        params.theme.store(i, std::sync::atomic::Ordering::Relaxed);
    }
    let shared = Arc::new(SynthShared::default());
    let store = PreviewStore::default();
    if std::env::var("DOUBLE_SLIT_DEMO").is_ok() {
        let (shared, params, store) = (shared.clone(), params.clone(), store.clone());
        std::thread::spawn(move || demo(shared, params, store));
    }
    let settings = WindowOpenOptions {
        title: "Double Slit Preview".into(),
        size: Size::new(editor::WIDTH as f64, editor::HEIGHT as f64),
        scale: WindowScalePolicy::ScaleFactor(1.0),
        gl_config: Some(Default::default()),
    };
    let context = PreviewContext { store: store.clone() };
    EguiWindow::open_blocking(
        settings,
        GraphicsConfig::default(),
        EditorState::new(shared).with_preview_store(store),
        |ctx, _queue, _state| editor::setup_style(ctx),
        move |ctx, _queue, state| {
            let setter = ParamSetter::new(&context);
            editor::root(ctx, &params, &setter, state);
        },
    );
}

/// Plays a chord for 1.5 s out of every 2.5 s, with the window's current
/// parameter values, and publishes what the plugin would publish.
fn demo(shared: Arc<SynthShared>, params: Arc<DoubleSlitParams>, store: PreviewStore) {
    const SR: f32 = 48_000.0;
    const BLOCK: usize = 256;
    let mut synth = Synth::new(shared.clone());
    synth.prepare(SR as f64, BLOCK);
    shared.feedback.gui_attached.store(true, std::sync::atomic::Ordering::Relaxed);
    let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
    let chord = [48u8, 55, 60, 64];
    let start = Instant::now();
    let mut rendered = 0usize;
    let mut held = false;
    loop {
        let values: [f32; NUM_PARAMS] = {
            let store = store.lock().unwrap();
            std::array::from_fn(|i| {
                let id = ParamId::from_index(i);
                let hp = params.get(id);
                let n = store.get(&hp.ptr()).copied().unwrap_or_else(|| hp.normalized());
                id.desc().from_normalized(n)
            })
        };
        synth.set_params(&values);
        let t = (rendered as f32 / SR) % 2.5;
        if t < 1.5 && !held {
            chord.iter().for_each(|&n| synth.note_on(0, n, 0.8, -1));
            held = true;
        } else if t >= 1.5 && held {
            chord.iter().for_each(|&n| synth.note_off(0, n, -1));
            held = false;
        }
        l.fill(0.0);
        r.fill(0.0);
        synth.process(&mut l, &mut r);
        rendered += BLOCK;
        let due = Duration::from_secs_f32(rendered as f32 / SR);
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
    }
}
