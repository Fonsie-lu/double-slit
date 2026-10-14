//! Headless GUI tests: the editor runs in a real egui context with simulated
//! mouse input against a mock host that applies parameter changes late, like
//! nih-plug's CLAP wrapper does, and checks gesture balance.

use super::*;
use crate::dsp::mod_matrix::{mod_source_id, mod_target_index_for_param, ModSource};
use crate::params::ExtraState;
use nih_plug::wrapper::state::PluginState;
use nih_plug_egui::egui::{Event, Modifiers, PointerButton, RawInput, Vec2};

const SCREEN: Vec2 = Vec2::new(BW, BH);

#[derive(Default)]
struct MockHost {
    /// Values the "plugin" has applied (what the GUI reads back).
    store: PreviewStore,
    /// Values sent by the GUI but not applied yet.
    queue: Mutex<Vec<(ParamPtr, f32)>>,
    open_gestures: Mutex<HashMap<ParamPtr, i32>>,
    errors: Mutex<Vec<String>>,
}

impl MockHost {
    fn flush(&self) {
        let mut store = self.store.lock().unwrap();
        for (ptr, v) in self.queue.lock().unwrap().drain(..) {
            store.insert(ptr, v);
        }
    }
}

impl GuiContext for MockHost {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn request_resize(&self) -> bool {
        false
    }
    unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
        let mut g = self.open_gestures.lock().unwrap();
        let n = g.entry(param).or_default();
        if *n != 0 {
            self.errors.lock().unwrap().push(format!("nested gesture: {}", param.name()));
        }
        *n += 1;
    }
    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        if self.open_gestures.lock().unwrap().get(&param).copied().unwrap_or(0) != 1 {
            self.errors.lock().unwrap().push(format!("set outside gesture: {}", param.name()));
        }
        self.queue.lock().unwrap().push((param, normalized));
    }
    unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
        let mut g = self.open_gestures.lock().unwrap();
        let n = g.entry(param).or_default();
        *n -= 1;
        if *n != 0 {
            self.errors.lock().unwrap().push(format!("unbalanced end: {}", param.name()));
        }
    }
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

struct Harness {
    ctx: egui::Context,
    params: DoubleSlitParams,
    host: MockHost,
    state: EditorState,
    time: f64,
    frame: u64,
    /// Host applies queued values every `latency` frames.
    latency: u64,
    events: Vec<Event>,
    pointer: Pos2,
}

impl Harness {
    fn new(latency: u64) -> Self {
        let ctx = egui::Context::default();
        setup_style(&ctx);
        let host = MockHost::default();
        let state = EditorState::new(Arc::new(SynthShared::default())).with_preview_store(host.store.clone());
        let mut h = Self {
            ctx,
            params: DoubleSlitParams::new(Arc::new(NoExtras)),
            host,
            state,
            time: 0.0,
            frame: 0,
            latency,
            events: Vec::new(),
            pointer: pos2(0.0, 0.0),
        };
        h.state.tab = Tab::Osc;
        h.run(2);
        h
    }

    fn run(&mut self, frames: usize) {
        for _ in 0..frames {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(self.time),
                events: std::mem::take(&mut self.events),
                ..Default::default()
            };
            let setter = ParamSetter::new(&self.host);
            let (params, state) = (&self.params, &mut self.state);
            let _ = self.ctx.run(raw, |ctx| root(ctx, params, &setter, state));
            self.time += 1.0 / 60.0;
            self.frame += 1;
            if self.latency > 0 && self.frame.is_multiple_of(self.latency) {
                self.host.flush();
            }
        }
    }

    fn settle(&mut self) {
        self.run(3);
        self.host.flush();
        self.run(2);
    }

    fn move_to(&mut self, p: Pos2) {
        self.pointer = p;
        self.events.push(Event::PointerMoved(p));
    }

    fn press(&mut self, button: PointerButton, pressed: bool) {
        self.events.push(Event::PointerButton {
            pos: self.pointer,
            button,
            pressed,
            modifiers: Modifiers::default(),
        });
    }

    fn click_with(&mut self, p: Pos2, button: PointerButton) {
        self.move_to(p);
        self.run(1);
        self.press(button, true);
        self.run(1);
        self.press(button, false);
        self.run(1);
        // Make sure the next click is not seen as a double click.
        self.time += 0.4;
    }

    fn click(&mut self, p: Pos2) {
        self.click_with(p, PointerButton::Primary);
    }

    fn double_click(&mut self, p: Pos2) {
        self.move_to(p);
        self.run(1);
        for _ in 0..2 {
            self.press(PointerButton::Primary, true);
            self.run(1);
            self.press(PointerButton::Primary, false);
            self.run(1);
        }
        self.time += 0.4;
    }

    fn drag(&mut self, from: Pos2, delta: Vec2, steps: usize) {
        self.move_to(from);
        self.run(1);
        self.press(PointerButton::Primary, true);
        self.run(1);
        for k in 1..=steps {
            self.move_to(from + delta * (k as f32 / steps as f32));
            self.run(1);
        }
        self.press(PointerButton::Primary, false);
        self.run(1);
        self.move_to(pos2(2.0, 2.0));
        self.run(1);
        self.time += 0.4;
    }

    fn try_control(&self, name: &str) -> Option<ControlInfo> {
        self.state.controls.iter().find(|c| c.name == name).cloned()
    }

    fn control(&self, name: &str) -> ControlInfo {
        self.try_control(name).unwrap_or_else(|| panic!("control {name} not found"))
    }

    fn controls(&self, kind: ControlKind) -> Vec<ControlInfo> {
        self.state.controls.iter().filter(|c| c.kind == kind).cloned().collect()
    }

    fn id(name: &str) -> ParamId {
        ParamId::from_id(name).unwrap_or_else(|| panic!("param {name} not found"))
    }

    /// Applied normalised value of a parameter.
    fn value(&self, name: &str) -> f32 {
        let hp = self.params.get(Self::id(name));
        if let Some(v) = self.host.store.lock().unwrap().get(&hp.ptr()) {
            return *v;
        }
        hp.default_normalized()
    }

    /// Applied value in natural units.
    fn plain(&self, name: &str) -> f32 {
        Self::id(name).desc().from_normalized(self.value(name))
    }

    fn open_tab(&mut self, tab: Tab) {
        let c = self.control(&format!("tab:{}", tab.name()));
        self.click(c.rect.center());
        assert_eq!(self.state.tab, tab);
    }

    fn rows(&self) -> Vec<crate::dsp::mod_matrix::ModRow> {
        self.state.shared.matrix.lock().unwrap().rows().to_vec()
    }

    fn assert_clean(&self) {
        let errors = self.host.errors.lock().unwrap();
        assert!(errors.is_empty(), "host protocol errors: {errors:?}");
        let open: Vec<_> = self
            .host
            .open_gestures
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, n)| **n != 0)
            .map(|(k, _)| unsafe { k.name() }.to_owned())
            .collect();
        assert!(open.is_empty(), "gestures left open: {open:?}");
    }
}

fn scale_of(c: &ControlInfo, canvas_width: f32) -> f32 {
    c.rect.width() / canvas_width
}

/// Every parameter is reachable from some tab. Resonance and the sustain
/// levels are the vertical axis of the filter graph and the envelope handles.
#[test]
fn every_parameter_has_a_control() {
    let mut h = Harness::new(3);
    let mut seen: std::collections::HashSet<String> = Default::default();
    for tab in Tab::ALL {
        h.open_tab(tab);
        seen.extend(h.state.controls.iter().map(|c| c.name.clone()));
    }
    let missing: Vec<&str> = crate::dsp::params::descs()
        .iter()
        .map(|d| d.id_string.as_str())
        .filter(|id| !seen.contains(*id))
        .collect();
    assert_eq!(missing, ["filter.res", "envAmp.sustain", "envMod.sustain"]);
}

/// Every fader, knob and value field on the OSC tab and the bottom half
/// follows a drag by the full distance, even though the host applies values late.
#[test]
fn faders_follow_drag_with_host_latency() {
    use crate::editor::widgets::{FIELD_TRAVEL, KNOB_TRAVEL};
    let mut h = Harness::new(4);
    let mut checked = 0;
    for (kind, width, travel) in [(ControlKind::Fader, 28.0, TRAVEL), (ControlKind::Knob, 0.0, KNOB_TRAVEL), (ControlKind::Field, 0.0, FIELD_TRAVEL)] {
        for c in h.controls(kind) {
            // Knobs and fields come in several sizes; the harness canvas is unscaled.
            let s = if width > 0.0 { scale_of(&c, width) } else { 1.0 };
            let before = h.value(&c.name);
            let up = before < 0.5;
            let px = 0.4 * travel * s;
            h.drag(c.rect.center(), vec2(0.0, if up { -px } else { px }), 12);
            h.settle();
            let change = h.value(&c.name) - before;
            match Harness::id(&c.name).desc().steps {
                0 => {
                    let expected = if up { 0.4 } else { -0.4 };
                    assert!((change - expected).abs() < 0.08, "{}: moved {change:.3}, expected {expected:.3}", c.name);
                }
                _ => assert!(if up { change > 0.2 } else { change < -0.2 }, "{}: stepped control moved {change:.3}", c.name),
            }
            checked += 1;
        }
    }
    assert!(checked > 40, "only {checked} controls");
    h.assert_clean();
}

#[test]
fn double_click_resets_and_scroll_changes() {
    let mut h = Harness::new(2);
    let c = h.control("oscA.level");
    let default = h.value("oscA.level");
    h.move_to(c.rect.center());
    h.run(1);
    h.events.push(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -60.0),
        modifiers: Modifiers::default(),
    });
    h.run(1);
    h.settle();
    assert!(h.value("oscA.level") < default - 0.02);
    h.double_click(c.rect.center());
    h.settle();
    assert!((h.value("oscA.level") - default).abs() < 1e-4);
    h.assert_clean();
}

/// Clicking the top and bottom detent of every stepped fader selects the end values.
#[test]
fn steppers_click_positions() {
    let mut h = Harness::new(3);
    for c in h.controls(ControlKind::Stepper) {
        let s = scale_of(&c, 54.0);
        h.click(pos2(c.rect.center().x, c.rect.top() + 16.0 * s));
        h.settle();
        assert!((h.value(&c.name) - 1.0).abs() < 1e-4, "{} top", c.name);
        h.click(pos2(c.rect.center().x, c.rect.bottom() - 16.0 * s));
        h.settle();
        assert!(h.value(&c.name).abs() < 1e-4, "{} bottom", c.name);
    }
    h.assert_clean();
}

/// Selector arrows step through the choices; the name opens the full list.
#[test]
fn table_selector_steps_and_lists() {
    let mut h = Harness::new(2);
    let c = h.control("oscA.table");
    let s = scale_of(&c, 290.0);
    let before = h.plain("oscA.table");
    h.click(c.rect.right_center() - vec2(6.0 * s, 0.0));
    h.settle();
    assert_eq!(h.plain("oscA.table"), before + 1.0);
    h.click(c.rect.center());
    assert!(matches!(h.state.popup, Some(Popup::Choice { .. })));
    let item = h.control("choice:oscA.table:0");
    h.click(item.rect.center());
    h.settle();
    assert_eq!(h.plain("oscA.table"), 0.0);
    assert!(h.state.popup.is_none());
    h.assert_clean();
}

#[test]
fn switches_toggle() {
    let mut h = Harness::new(2);
    let c = h.control("oscB.on");
    assert_eq!(c.kind, ControlKind::Switch);
    h.click(c.rect.center());
    h.settle();
    assert_eq!(h.plain("oscB.on"), 1.0);
    h.click(c.rect.center());
    h.settle();
    assert_eq!(h.plain("oscB.on"), 0.0);
    h.assert_clean();
}

/// The wave display scrubs the WT position (sideways or up), switches between
/// 3D and 2D, and its pencil opens the table in the WT editor.
#[test]
fn wave_display_scrubs_and_opens_the_editor() {
    let mut h = Harness::new(3);
    let wave = h.control("wave:1");
    assert_eq!(wave.kind, ControlKind::Graph);
    let w = wave.rect.width();
    h.drag(wave.rect.center(), vec2(0.15 * w, -0.15 * w), 10);
    h.settle();
    assert!((h.value("oscB.wtPos") - 0.3).abs() < 0.03, "{}", h.value("oscB.wtPos"));

    let view = h.control("wave-view:1");
    h.click(view.rect.center());
    assert!(h.state.wave_flat[1] && !h.state.wave_flat[0]);
    h.click(view.rect.center());
    assert!(!h.state.wave_flat[1]);
    // The click on the button does not reach the display underneath.
    assert!((h.value("oscB.wtPos") - 0.3).abs() < 0.03);

    let edit = h.control("wave-edit:1");
    h.click(edit.rect.center());
    assert_eq!(h.state.tab, Tab::Wt);
    assert_eq!(h.state.wt.slot, 1);
    h.assert_clean();
}

/// Envelope handles move their times horizontally, the decay handle moves sustain too.
#[test]
fn envelope_handles_drag_times_and_sustain() {
    let mut h = Harness::new(3);
    let attack = h.control("envAmp.attack");
    assert_eq!(attack.kind, ControlKind::Handle);
    let s = attack.rect.width() / 18.0;
    let zone = 0.3 * (220.0 - 16.0) * s;
    let before = h.value("envAmp.attack");
    h.drag(attack.rect.center(), vec2(0.25 * zone, 0.0), 10);
    h.settle();
    assert!((h.value("envAmp.attack") - before - 0.25).abs() < 0.03);

    let decay = h.control("envAmp.decay");
    let (d0, s0) = (h.value("envAmp.decay"), h.value("envAmp.sustain"));
    let height = (BAR_Y[2] + 186.0 - (BAR_Y[2] + 56.0) - 16.0) * s;
    h.drag(decay.rect.center(), vec2(0.2 * zone, 0.2 * height), 10);
    h.settle();
    assert!((h.value("envAmp.decay") - d0 - 0.2).abs() < 0.03);
    assert!((h.value("envAmp.sustain") - s0 + 0.2).abs() < 0.03);
    h.assert_clean();
}

/// Dragging the filter graph moves cutoff on a log axis and resonance vertically.
#[test]
fn filter_graph_drags_cutoff_and_resonance() {
    let mut h = Harness::new(3);
    h.open_tab(Tab::Filter);
    let g = h.control("filter.cutoff");
    assert_eq!(g.kind, ControlKind::Graph);
    let before = h.plain("filter.cutoff");
    let r0 = h.value("filter.res");
    let s = scale_of(&g, 870.0);
    let (w, ht) = ((870.0 - 20.0) * s, (360.0 - 24.0) * s);
    h.drag(g.rect.center(), vec2(-0.2 * w, -0.3 * ht), 10);
    h.settle();
    let expected = before / 1000.0_f32.powf(0.2);
    let after = h.plain("filter.cutoff");
    assert!((after / expected - 1.0).abs() < 0.08, "cutoff {before} -> {after}, expected {expected}");
    assert!((h.value("filter.res") - r0 - 0.3).abs() < 0.03);
    // A type button selects its type.
    let ladder = h.control("ftype:7");
    h.click(ladder.rect.center());
    h.settle();
    assert_eq!(h.plain("filter.type"), 7.0);
    h.assert_clean();
}

/// Dragging a source chip onto a fader adds a matrix row (+50 %), and the
/// fader then shows a modulation range (SPEC 4.4).
#[test]
fn dropping_a_source_on_a_fader_adds_a_row() {
    let mut h = Harness::new(2);
    let chip = h.control(&format!("chip:{}", mod_source_id(ModSource::Lfo1.index())));
    let fader = h.control("oscA.wtPos");
    h.drag(chip.rect.center(), fader.rect.center() - chip.rect.center(), 12);
    let rows = h.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, ModSource::Lfo1.index());
    assert_eq!(rows[0].target, mod_target_index_for_param(ParamId::OscAWtPos));
    assert!((rows[0].amount - 0.5).abs() < 1e-6);
    assert!(h.state.drag_source.is_none());
    let target = mod_target_index_for_param(ParamId::OscAWtPos).unwrap();
    assert!(h.state.mod_info[target].pos > 0.4);
    // Dropping on empty panel adds nothing.
    h.drag(chip.rect.center(), vec2(0.0, -200.0), 8);
    assert_eq!(h.rows().len(), 1);
    h.assert_clean();
}

/// Right-click on a control opens its modulation popover; sources can be added there.
#[test]
fn right_click_popover_adds_and_removes_rows() {
    let mut h = Harness::new(2);
    let fader = h.control("oscA.level");
    h.click_with(fader.rect.center(), PointerButton::Secondary);
    assert!(matches!(h.state.popup, Some(Popup::Modulation { .. })));
    let add = h.control(&format!("pop-add:{}", ModSource::ModWheel.index()));
    h.click(add.rect.center());
    let rows = h.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, ModSource::ModWheel.index());
    h.click_with(fader.rect.center(), PointerButton::Secondary);
    let remove = h.control("pop-remove:0");
    h.click(remove.rect.center());
    assert!(h.rows().is_empty());
}

/// The MATRIX tab edits rows: amount, curve, mute, source, delete.
#[test]
fn matrix_rows_are_editable() {
    let mut h = Harness::new(2);
    h.open_tab(Tab::Matrix);
    let add = h.control("matrix:add");
    h.click(add.rect.center());
    h.click(add.rect.center());
    assert_eq!(h.rows().len(), 2);

    let bar = h.control("row:0:amount");
    h.drag(bar.rect.center(), vec2(-bar.rect.width() * 0.25, 0.0), 8);
    assert!((h.rows()[0].amount - 0.0).abs() < 0.06, "{}", h.rows()[0].amount);

    let curve = h.control("row:0:curve");
    h.click(curve.rect.center());
    assert_eq!(h.rows()[0].curve, ModCurve::Exp);

    let mute = h.control("row:1:mute");
    h.click(mute.rect.center());
    assert!(h.rows()[1].muted);

    let src = h.control("row:1:source");
    h.click(src.rect.center());
    let entry = h.control("rowsrc:0");
    h.click(entry.rect.center());
    assert_eq!(h.rows()[1].source, 1);

    let del = h.control("row:0:delete");
    h.click(del.rect.center());
    assert_eq!(h.rows().len(), 1);
    assert!(h.rows()[0].muted);
}

/// Next / previous load factory presets: parameters go to the host, the name to the header.
#[test]
fn preset_buttons_load_presets() {
    let mut h = Harness::new(2);
    let next = h.control("preset-next");
    h.click(next.rect.center());
    h.settle();
    assert_eq!(h.state.preset, Some(0));
    h.click(next.rect.center());
    h.settle();
    assert_eq!(h.state.preset, Some(1));
    let data = crate::dsp::presets::build_factory_preset(1).unwrap();
    for id in ParamId::all() {
        let want = id.desc().to_normalized(data.params[id.index()]);
        assert!((h.value(&id.desc().id_string) - want).abs() < 1e-4, "{}", id.desc().id_string);
    }
    assert_eq!(h.state.shared.patch_name(), crate::dsp::presets::factory_preset_name(1));
    h.assert_clean();
}

/// Drawing on the WT grid edits the frame, publishes the table and selects Custom.
#[test]
fn drawing_a_wavetable_publishes_a_custom_table() {
    let mut h = Harness::new(2);
    h.open_tab(Tab::Wt);
    let grid = h.control("wt:grid");
    let rev = h.state.shared.table_revision.load(Relaxed);
    let from = grid.rect.left_center() + vec2(grid.rect.width() * 0.2, -grid.rect.height() * 0.3);
    h.drag(from, vec2(grid.rect.width() * 0.3, 0.0), 10);
    h.settle();
    assert!(h.state.shared.table_revision.load(Relaxed) > rev);
    assert_eq!(h.plain("oscA.table"), crate::dsp::params::CUSTOM_TABLE_CHOICE as f32);
    let table = h.state.shared.table(0);
    assert!(table.factory_index().is_none());
    let i = crate::dsp::common::FRAME_SIZE * 35 / 100;
    assert!(table.frame_data(0)[i] > 0.3, "{}", table.frame_data(0)[i]);

    let add = h.control("wt:ADD");
    h.click(add.rect.center());
    assert_eq!(h.state.shared.table(0).num_frames(), 2);
    h.assert_clean();
}

/// Every theme chip switches the scheme and every scheme renders all controls.
#[test]
fn theme_selector_switches_and_all_themes_render() {
    let mut h = Harness::new(2);
    let before = h.state.controls.len();
    for (i, theme) in THEMES.iter().enumerate() {
        let chip = h.control(&format!("theme:{}", theme.name));
        h.click(chip.rect.center());
        h.run(2);
        assert_eq!(h.params.theme.load(Relaxed) as usize, i);
        assert_eq!(h.state.controls.len(), before);
    }
}
