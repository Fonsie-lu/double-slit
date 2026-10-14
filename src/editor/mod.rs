//! Hardware style editor in the look of the sibling synths (Drift, the juno
//! clone): coloured section bars, faders with scales, stepped faders, graphs
//! with drag handles, and four colour themes.
//!
//! Layout: a header (presets, value readout, voice LEDs, CPU, themes), the
//! OSC | WT | FILTER | MATRIX | FX tabs (SPEC 6.2) on the top two rows, and an
//! always-visible bottom half with the AMP and MOD envelopes, both LFOs, the
//! master section and the output scope.
//!
//! Everything is painted on a fixed `BW` x `BH` canvas through [`Cv`] and
//! scaled uniformly to the window; drag the bottom right corner to resize.

mod graphs;
mod tabs;
pub mod theme;
mod widgets;

use nih_plug::prelude::*;
use nih_plug_egui::egui::{self, pos2, vec2, Align2, Color32, FontFamily, Id, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Ui};
use nih_plug_egui::create_egui_editor;
use nih_plug_egui::resizable_window::ResizableWindow;
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use crate::dsp::common::MAX_VOICES;
use crate::dsp::mod_matrix::{
    mod_source_display_name, mod_target_display_name, num_mod_targets, ModCurve, NUM_FIXED_SOURCES,
};
use crate::dsp::params::{ParamId, NUM_PARAMS};
use crate::dsp::presets::{build_factory_preset, factory_preset_name, num_factory_presets};
use crate::dsp::shared::SynthShared;
use crate::dsp::state::apply_patch_data;
use crate::params::DoubleSlitParams;
use theme::{Section, Theme, THEMES};
pub use widgets::{Cv, G};
use widgets::TRAVEL;

/// Canvas size in logical pixels.
pub(crate) const BW: f32 = 1400.0;
pub(crate) const BH: f32 = 905.0;
/// Default window size: the canvas at 100 %.
pub const WIDTH: u32 = BW as u32;
pub const HEIGHT: u32 = BH as u32;

/// Top of the section bar in each of the four rows. Rows 0-1 belong to the
/// active tab, rows 2-3 to the always-visible bottom half.
pub(crate) const BAR_Y: [f32; 4] = [76.0, 284.0, 492.0, 700.0];

/// Label baseline and fader slot (top, bottom) of a row.
pub(crate) fn row(i: usize) -> (f32, (f32, f32)) {
    let bar = BAR_Y[i];
    (bar + 46.0, (bar + 66.0, bar + 66.0 + TRAVEL))
}

/// How long a value set by the GUI is shown before trusting the parameter again.
pub(crate) const PENDING_TIMEOUT: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Osc = 0,
    Wt,
    Filter,
    Matrix,
    Fx,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Osc, Tab::Wt, Tab::Filter, Tab::Matrix, Tab::Fx];

    fn name(self) -> &'static str {
        match self {
            Tab::Osc => "OSC",
            Tab::Wt => "WT",
            Tab::Filter => "FILTER",
            Tab::Matrix => "MATRIX",
            Tab::Fx => "FX",
        }
    }

    fn section(self) -> Section {
        match self {
            Tab::Osc => Section::OscA,
            Tab::Wt => Section::Wt,
            Tab::Filter => Section::Filter,
            Tab::Matrix => Section::Matrix,
            Tab::Fx => Section::Fx,
        }
    }
}

/// What the header readout draws next to the focused control's value.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FocusKind {
    Plain,
}

pub struct Focus {
    pub name: String,
    pub value: String,
    pub kind: FocusKind,
}

pub(crate) struct Pending {
    pub value: f32,
    pub time: f64,
}

/// Overlay lists. Drawn last so they sit on top and take the clicks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Popup {
    Presets { at: Pos2 },
    Choice { param: ParamId, at: Pos2 },
    /// Rows on a target, plus sources to add (right-click on a control).
    Modulation { target: usize, at: Pos2 },
    RowSource { row: usize, at: Pos2 },
    RowTarget { row: usize, at: Pos2 },
}

/// Matrix rows summed per target, refreshed every frame for the fader overlays.
#[derive(Clone, Copy, Default, Debug)]
pub struct ModInfo {
    pub pos: f32,
    pub neg: f32,
    pub activity: f32,
}

/// Shared parameter store used by the standalone preview and the tests,
/// which have no plugin wrapper that could apply parameter changes.
pub type PreviewStore = Arc<Mutex<HashMap<ParamPtr, f32>>>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlKind {
    Fader,
    Knob,
    /// Value box dragged vertically (octave, semitone, phase).
    Field,
    Stepper,
    Selector,
    /// Choice as a grid of cells; registered whole under the parameter ID and
    /// per cell as `"<id>#<index>"`.
    List,
    Switch,
    Button,
    Chip,
    /// Envelope breakpoint, registered under its horizontal parameter.
    Handle,
    /// Two-parameter graph, registered under its horizontal parameter.
    Graph,
}

/// Screen rectangle of an interactive control, recorded every frame (tests).
#[derive(Clone, Debug)]
pub struct ControlInfo {
    /// Parameter ID for parameter controls, a fixed key otherwise.
    pub name: String,
    pub kind: ControlKind,
    pub rect: Rect,
}

pub struct EditorState {
    pub(crate) shared: Arc<SynthShared>,
    pub tab: Tab,
    pub(crate) focus: Option<Focus>,
    texture: Option<(u8, TextureHandle)>,
    pub(crate) pending: HashMap<ParamId, Pending>,
    pub(crate) preview_store: Option<PreviewStore>,
    pub controls: Vec<ControlInfo>,
    pub(crate) popup: Option<Popup>,
    /// Modulation source being dragged from a chip.
    pub drag_source: Option<usize>,
    /// Places a dragged source can be dropped, from the previous frame.
    pub(crate) drop_targets: Vec<(Rect, usize)>,
    pub(crate) mod_info: Vec<ModInfo>,
    /// Last factory preset loaded from the header, for prev/next.
    pub preset: Option<usize>,
    /// CC number offered by the MATRIX palette's CC chip.
    pub(crate) cc_pick: u8,
    pub(crate) wt: tabs::WtState,
    /// OSC tab wave displays showing only the current frame instead of the 3D stack.
    pub(crate) wave_flat: [bool; 2],
    /// Envelope (0 = amp, 1 = mod) and LFO shown in the bottom half.
    pub env_page: usize,
    pub lfo_page: usize,
    /// Matrix row being dragged to a new position: (row, current slot).
    pub(crate) row_drag: Option<(usize, usize)>,
    scratch: graphs::Scratch,
}

impl EditorState {
    pub fn new(shared: Arc<SynthShared>) -> Self {
        Self {
            shared,
            tab: start_tab(),
            focus: None,
            texture: None,
            pending: HashMap::new(),
            preview_store: None,
            controls: Vec::new(),
            popup: None,
            drag_source: None,
            drop_targets: Vec::new(),
            mod_info: vec![ModInfo::default(); num_mod_targets()],
            preset: None,
            cc_pick: 74,
            wt: tabs::WtState::default(),
            wave_flat: [false; 2],
            env_page: 0,
            lfo_page: 0,
            row_drag: None,
            scratch: graphs::Scratch::default(),
        }
    }

    /// Read parameter values from `store` instead of the parameters themselves.
    pub fn with_preview_store(mut self, store: PreviewStore) -> Self {
        self.preview_store = Some(store);
        self
    }
}

/// Development aid: `DOUBLE_SLIT_TAB=0..4` opens the editor on a given tab.
fn start_tab() -> Tab {
    std::env::var("DOUBLE_SLIT_TAB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(|i| Tab::ALL.get(i).copied())
        .unwrap_or(Tab::Osc)
}

pub fn create(params: Arc<DoubleSlitParams>, shared: Arc<SynthShared>) -> Option<Box<dyn Editor>> {
    create_egui_editor(
        params.editor_state.clone(),
        EditorState::new(shared),
        |ctx, _| setup_style(ctx),
        move |ctx, setter, state| root(ctx, &params, setter, state),
    )
}

pub fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "panel".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/DejaVuSansCondensed-Bold.ttf"
        ))),
    );
    fonts.font_data.insert(
        "oled".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/DejaVuSansMono-Bold.ttf"
        ))),
    );
    fonts
        .families
        .insert(FontFamily::Name("panel".into()), vec!["panel".to_owned()]);
    fonts
        .families
        .insert(FontFamily::Name("oled".into()), vec!["oled".to_owned()]);
    ctx.set_fonts(fonts);
}

pub fn root(ctx: &egui::Context, params: &DoubleSlitParams, setter: &ParamSetter, state: &mut EditorState) {
    // Meters, scope, LEDs and graph markers animate without any input.
    ctx.request_repaint();
    ResizableWindow::new("double-slit")
        .min_size(vec2(BW * 0.5, BH * 0.5))
        .show(ctx, params.editor_state.as_ref(), |ui| draw(ui, params, setter, state));
}

fn draw(ui: &mut Ui, p: &DoubleSlitParams, set: &ParamSetter, st: &mut EditorState) {
    let full = ui.max_rect();
    let s = (full.width() / BW).min(full.height() / BH);
    let o = pos2(full.center().x - BW * s * 0.5, full.center().y - BH * s * 0.5);
    let cv = Cv { o, s };

    let theme_index = p.theme.load(Relaxed);
    let th = theme::get(theme_index);
    if st.texture.as_ref().map(|(i, _)| *i) != Some(theme_index) {
        let tex = ui
            .ctx()
            .load_texture("panel-texture", panel_texture(th), egui::TextureOptions::LINEAR_REPEAT);
        st.texture = Some((theme_index, tex));
    }
    if ui.style().visuals.panel_fill != th.outer {
        ui.ctx().style_mut(|s| s.visuals.panel_fill = th.outer);
    }
    st.focus = None;
    st.controls.clear();
    st.drop_targets.clear();
    refresh_mod_info(st);

    let pt = ui.painter().clone();
    let mut g = G { ui, pt, cv, set, p, st, th, accent: th.accents[0] };

    g.panel();
    g.header();
    g.tab_bar();
    // A popup takes the clicks: controls underneath must not react to the
    // click that is meant for a list entry.
    let popup_rect = g.st.popup.map(|pop| g.popup_rect(pop));
    if let Some(r) = popup_rect {
        g.ui.interact(r, Id::new("popup-shield"), Sense::click_and_drag());
    }

    match g.st.tab {
        Tab::Osc => g.osc_tab(),
        Tab::Wt => g.wt_tab(),
        Tab::Filter => g.filter_tab(),
        Tab::Matrix => g.matrix_tab(),
        Tab::Fx => g.fx_tab(),
    }
    g.bottom_half();
    g.readout();
    g.drag_and_drop();
    g.popup();
}

/// Sums the matrix rows per target for the fader overlays. The matrix lock is
/// only ever taken by the GUI and main thread.
fn refresh_mod_info(st: &mut EditorState) {
    for m in st.mod_info.iter_mut() {
        *m = ModInfo::default();
    }
    let shared = st.shared.clone();
    let Ok(matrix) = shared.matrix.lock() else {
        return;
    };
    for (i, row) in matrix.rows().iter().enumerate() {
        let Some(t) = row.target else { continue };
        if row.muted || row.unresolved || t >= st.mod_info.len() {
            continue;
        }
        let info = &mut st.mod_info[t];
        if row.amount >= 0.0 {
            info.pos += row.amount;
        } else {
            info.neg -= row.amount;
        }
        if let Some(c) = matrix.compiled_index_for_row(i) {
            let a = shared.feedback.row_activity.get(c).map(|a| a.load(Relaxed)).unwrap_or(0.0);
            info.activity = info.activity.max(a);
        }
    }
}

/// Procedural powder coat: fine grain plus sparse lighter flecks.
fn panel_texture(th: &Theme) -> egui::ColorImage {
    let n = 256;
    let mut rng = crate::dsp::common::Rng::new(77);
    let mut px = Vec::with_capacity(n * n);
    for _ in 0..n * n {
        let grain = rng.next_bipolar() * th.grain;
        let fleck = if rng.next_float() > 0.985 { th.fleck } else { 0.0 };
        let v = |c: u8| (c as f32 + grain + fleck).clamp(0.0, 255.0) as u8;
        px.push(Color32::from_rgb(v(th.panel.r()), v(th.panel.g()), v(th.panel.b())));
    }
    egui::ColorImage { size: [n, n], pixels: px }
}

impl G<'_, '_> {
    fn panel(&self) {
        let cv = self.cv;
        let rect = cv.r(0.0, 0.0, BW, BH);
        let tex = &self.st.texture.as_ref().unwrap().1;
        let uv = Rect::from_min_max(
            pos2(0.0, 0.0),
            pos2(rect.width() / (256.0 * cv.s), rect.height() / (256.0 * cv.s)),
        );
        self.pt.image(tex.id(), rect, uv, Color32::WHITE);
        self.pt
            .line_segment([cv.p(2.0, 1.0), cv.p(BW - 2.0, 1.0)], cv.stroke(1.5, self.th.bevel));
    }

    fn header(&mut self) {
        let cv = self.cv;
        self.text(cv.p(18.0, 28.0), Align2::LEFT_CENTER, "WAVETABLE SYNTHESIZER", 9.0, self.th.header);
        self.preset_browser();
        self.voice_leds();
        let cpu = self.st.shared.feedback.cpu_load.load(Relaxed);
        self.text(
            cv.p(560.0, 22.0),
            Align2::LEFT_CENTER,
            &format!("CPU {:.0}%", cpu * 100.0),
            8.5,
            self.th.header,
        );
        self.theme_selector();
    }

    /// Patch name with previous / next arrows; clicking the name lists the
    /// factory presets. nih-plug has no host program list, so this is it.
    fn preset_browser(&mut self) {
        let cv = self.cv;
        let count = num_factory_presets();
        let name = self.st.shared.patch_name().to_uppercase();
        let name_rect = cv.r(232.0, 12.0, 256.0, 20.0);
        let resp = self.ui.interact(name_rect, Id::new("preset-name"), Sense::click());
        self.register("preset", ControlKind::Button, name_rect);
        self.pt.rect_filled(name_rect, cv.round(3.0), self.th.oled_bg);
        self.pt.rect_stroke(
            name_rect,
            cv.round(3.0),
            Stroke::new(1.0 * cv.s, if resp.hovered() { self.th.oled_ink } else { self.th.oled_frame }),
            StrokeKind::Inside,
        );
        let head = match self.st.preset {
            Some(i) => format!("{:02}", i + 1),
            None => "--".to_owned(),
        };
        self.pt.text(cv.p(240.0, 22.0), Align2::LEFT_CENTER, &head, cv.mono(9.0), self.th.oled_dim);
        self.pt.text(cv.p(262.0, 22.0), Align2::LEFT_CENTER, &name, cv.mono(10.5), self.th.oled_ink);
        if resp.clicked() {
            self.st.popup = Some(Popup::Presets { at: pos2(232.0, 34.0) });
        }
        if resp.hovered() {
            self.set_focus("FACTORY PRESETS", "CLICK FOR LIST".to_owned(), FocusKind::Plain);
        }
        self.accent = self.th.sublabel;
        if self.button("preset-prev", 206.0, 13.0, 22.0, "<", false, "PREVIOUS PRESET").clicked() {
            let i = self.st.preset.map(|i| (i + count - 1) % count).unwrap_or(count - 1);
            self.load_preset(i);
        }
        if self.button("preset-next", 492.0, 13.0, 22.0, ">", false, "NEXT PRESET").clicked() {
            let i = self.st.preset.map(|i| (i + 1) % count).unwrap_or(0);
            self.load_preset(i);
        }
    }

    /// Resolves a preset off the audio thread: matrix, tables and LFO shapes
    /// are published to the engine, parameters go to the host.
    pub(crate) fn load_preset(&mut self, index: usize) {
        let Some(data) = build_factory_preset(index) else {
            return;
        };
        let values = apply_patch_data(&data, &self.st.shared);
        self.send_all(&values);
        self.st.preset = Some(index);
        self.st.wt.edit = None;
    }

    /// Sends every parameter to the host, each as its own gesture.
    pub(crate) fn send_all(&mut self, values: &[f32; NUM_PARAMS]) {
        for id in ParamId::all() {
            let n = id.desc().to_normalized(values[id.index()]);
            self.change(id, n);
        }
    }

    fn voice_leds(&self) {
        let cv = self.cv;
        let fb = &self.st.shared.feedback;
        let count = fb.voice_count.load(Relaxed) as usize;
        let poly = self.p.value(ParamId::MasterPolyphony).round() as usize;
        self.text(cv.p(688.0, 22.0), Align2::RIGHT_CENTER, "VOICES", 8.5, self.th.header);
        for i in 0..MAX_VOICES {
            let rect = cv.r(696.0 + i as f32 * 11.0, 17.0, 9.0, 10.0);
            if i >= poly {
                self.pt.rect_stroke(rect, cv.round(1.5), cv.stroke(0.8, self.th.led_off), StrokeKind::Inside);
                continue;
            }
            let color = if i < count { self.th.led } else { self.th.led_off };
            self.pt.rect_filled(rect, cv.round(1.5), color);
        }
        self.text(
            cv.p(696.0 + MAX_VOICES as f32 * 11.0 + 4.0, 22.0),
            Align2::LEFT_CENTER,
            &format!("{count} / {poly}"),
            8.5,
            self.th.header,
        );
    }

    fn theme_selector(&mut self) {
        let cv = self.cv;
        let current = self.p.theme.load(Relaxed) as usize;
        let chip_w = |name: &str| 14.0 + name.len() as f32 * 5.6;
        let total: f32 = THEMES.iter().map(|t| chip_w(t.name) + 5.0).sum::<f32>() - 5.0;
        let mut x = BW - 12.0 - total;
        self.text(cv.p(x - 10.0, 22.0), Align2::RIGHT_CENTER, "THEME", 8.5, self.th.header);
        for (i, theme) in THEMES.iter().enumerate() {
            let w = chip_w(theme.name);
            let rect = cv.r(x, 15.0, w, 14.0);
            let key = format!("theme:{}", theme.name);
            let resp = self.ui.interact(rect, Id::new(&key), Sense::click());
            self.register(&key, ControlKind::Button, rect);
            let (fill, ink) = if i == current {
                (self.th.sublabel, self.th.bar_text)
            } else if resp.hovered() {
                (self.th.button_hover, self.th.text)
            } else {
                (self.th.button, self.th.text)
            };
            self.pt.rect_filled(rect, cv.round(3.0), fill);
            self.text(rect.center(), Align2::CENTER_CENTER, theme.name, 8.0, ink);
            if resp.clicked() {
                self.p.theme.store(i as u8, Relaxed);
            }
            x += w + 5.0;
        }
    }

    fn tab_bar(&mut self) {
        let cv = self.cv;
        for (i, tab) in Tab::ALL.iter().enumerate() {
            let rect = cv.r(16.0 + i as f32 * 112.0, 44.0, 106.0, 24.0);
            let key = format!("tab:{}", tab.name());
            let resp = self.ui.interact(rect, Id::new(&key), Sense::click());
            self.register(&key, ControlKind::Button, rect);
            let color = self.th.bars[tab.section() as usize];
            let active = self.st.tab == *tab;
            let (fill, ink) = if active {
                (color, self.th.bar_text)
            } else if resp.hovered() {
                (self.th.button_hover, color)
            } else {
                (self.th.button, self.th.text)
            };
            self.pt.rect_filled(rect, cv.round(3.0), fill);
            self.text(rect.center(), Align2::CENTER_CENTER, tab.name(), 12.0, ink);
            if resp.clicked() {
                self.st.tab = *tab;
                self.st.popup = None;
            }
        }
    }

    /// Small value display in the header: the control under the mouse with
    /// its value, or the patch name.
    fn readout(&mut self) {
        let cv = self.cv;
        let rect = cv.r(586.0, 44.0, 802.0, 24.0);
        self.pt.rect_filled(rect, cv.round(3.0), self.th.oled_bg);
        self.pt
            .rect_stroke(rect, cv.round(3.0), Stroke::new(1.0 * cv.s, self.th.oled_frame), StrokeKind::Inside);
        let (name, value) = match &self.st.focus {
            Some(f) => (f.name.clone(), f.value.clone()),
            None => ("PATCH".to_owned(), self.st.shared.patch_name().to_uppercase()),
        };
        self.pt.text(cv.p(596.0, 56.0), Align2::LEFT_CENTER, &name, cv.mono(9.0), self.th.oled_dim);
        self.pt.text(cv.p(1378.0, 56.0), Align2::RIGHT_CENTER, &value, cv.mono(11.0), self.th.oled_ink);
    }

    /// Finishes a source-chip drag: a drop on a modulatable control adds a
    /// matrix row there with +50 % (SPEC 4.4).
    fn drag_and_drop(&mut self) {
        let Some(source) = self.st.drag_source else {
            return;
        };
        let (pos, released) = self
            .ui
            .input(|i| (i.pointer.interact_pos(), i.pointer.any_released() || !i.pointer.any_down()));
        if let Some(pos) = pos {
            let name = mod_source_display_name(source).to_uppercase();
            let rect = Rect::from_center_size(pos + vec2(0.0, -14.0) * self.cv.s, vec2(80.0, 16.0) * self.cv.s);
            self.pt.rect_filled(rect, self.cv.round(3.0), self.th.modulation);
            self.pt
                .text(rect.center(), Align2::CENTER_CENTER, &name, self.cv.font(8.0), self.th.bar_text);
        }
        if released {
            self.st.drag_source = None;
            let target = pos.and_then(|p| self.st.drop_targets.iter().rev().find(|(r, _)| r.contains(p)).map(|(_, t)| *t));
            if let Some(target) = target {
                self.st
                    .shared
                    .edit_matrix(true, |m| m.add_row(source, Some(target), 0.5, ModCurve::Linear));
            }
        }
    }

    // ------------------------------------------------------------ popups

    fn popup_rect(&self, pop: Popup) -> Rect {
        let (at, w, h) = match pop {
            Popup::Presets { at } => (at, 256.0, num_factory_presets() as f32 * 16.0 + 8.0),
            Popup::Choice { param, at } => {
                let (w, row, _) = choice_metrics(param);
                (at, w + 8.0, param.desc().choices.len().max(1) as f32 * row + 8.0)
            }
            Popup::Modulation { at, .. } => (at, 230.0, 8.0 + 16.0 * (6.0 + NUM_FIXED_SOURCES as f32 / 2.0)),
            Popup::RowSource { at, .. } => (at, 260.0, 8.0 + 16.0 * ((NUM_FIXED_SOURCES as f32 + 4.0) / 2.0).ceil()),
            Popup::RowTarget { at, .. } => (at, 420.0, 8.0 + 16.0 * (num_mod_targets() as f32 / 2.0).ceil()),
        };
        let x = at.x.min(BW - w - 4.0);
        let y = at.y.min(BH - h - 4.0);
        self.cv.r(x, y, w, h)
    }

    fn popup(&mut self) {
        let Some(pop) = self.st.popup else {
            return;
        };
        let rect = self.popup_rect(pop);
        let cv = self.cv;
        self.pt
            .rect_filled(rect.translate(vec2(2.0, 3.0) * cv.s), cv.round(4.0), Color32::from_black_alpha(140));
        self.pt.rect_filled(rect, cv.round(4.0), self.th.oled_bg);
        self.pt
            .rect_stroke(rect, cv.round(4.0), Stroke::new(1.0 * cv.s, self.th.oled_frame), StrokeKind::Inside);
        let origin = cv.to_canvas(rect.min) + vec2(4.0, 4.0);
        let mut close = false;

        match pop {
            Popup::Presets { .. } => {
                for i in 0..num_factory_presets() {
                    let label = format!("{:02}  {}", i + 1, factory_preset_name(i).to_uppercase());
                    if self.popup_item(&format!("preset:{i}"), origin + vec2(0.0, i as f32 * 16.0), 248.0, &label, self.st.preset == Some(i)) {
                        self.load_preset(i);
                        close = true;
                    }
                }
            }
            Popup::Choice { param, .. } => {
                let d = param.desc();
                let current = self.plain(param).round() as usize;
                let (w, row, font) = choice_metrics(param);
                for (i, c) in d.choices.iter().enumerate() {
                    let key = format!("choice:{}:{i}", d.id_string);
                    let at = origin + vec2(0.0, i as f32 * row);
                    if self.popup_item_sized(&key, at, w, row - 1.0, &c.to_uppercase(), current == i, font) {
                        self.change_plain(param, i as f32);
                        close = true;
                    }
                }
            }
            Popup::Modulation { target, .. } => close = self.modulation_popover(target, origin),
            Popup::RowSource { row, .. } => {
                let sources = palette_sources(self.st.cc_pick);
                for (k, src) in sources.iter().enumerate() {
                    let at = origin + vec2((k % 2) as f32 * 126.0, (k / 2) as f32 * 16.0);
                    if self.popup_item(&format!("rowsrc:{k}"), at, 122.0, &mod_source_display_name(*src).to_uppercase(), false) {
                        self.st.shared.edit_matrix(true, |m| m.set_source(row, *src));
                        close = true;
                    }
                }
            }
            Popup::RowTarget { row, .. } => {
                for t in 0..num_mod_targets() {
                    let at = origin + vec2((t % 2) as f32 * 206.0, (t / 2) as f32 * 16.0);
                    if self.popup_item(&format!("rowtgt:{t}"), at, 202.0, &mod_target_display_name(t).to_uppercase(), false) {
                        self.st.shared.edit_matrix(true, |m| m.set_target(row, Some(t)));
                        close = true;
                    }
                }
            }
        }

        // A click anywhere outside closes the popup.
        let outside = self.ui.input(|i| {
            i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !rect.contains(p))
        });
        if close || outside {
            self.st.popup = None;
        }
    }

    pub(crate) fn popup_item(&mut self, key: &str, at: Pos2, w: f32, text: &str, selected: bool) -> bool {
        self.popup_item_sized(key, at, w, 15.0, text, selected, 8.5)
    }

    #[allow(clippy::too_many_arguments)]
    fn popup_item_sized(&mut self, key: &str, at: Pos2, w: f32, h: f32, text: &str, selected: bool, font: f32) -> bool {
        let cv = self.cv;
        let rect = cv.r(at.x, at.y, w, h);
        let resp = self.ui.interact(rect, Id::new(("popup", key)), Sense::click());
        self.register(key, ControlKind::Button, rect);
        if resp.hovered() || selected {
            let fill = if resp.hovered() { self.th.button_hover } else { self.th.button };
            self.pt.rect_filled(rect, cv.round(2.0), fill);
        }
        let ink = if selected { self.th.oled_ink } else { self.th.text };
        self.pt
            .text(rect.left_center() + vec2(5.0 * cv.s, 0.0), Align2::LEFT_CENTER, text, cv.mono(font), ink);
        resp.clicked()
    }

    /// Rows modulating one target with amount, curve and remove, plus the
    /// sources that can be added (the C++ build's knob popover).
    fn modulation_popover(&mut self, target: usize, origin: Pos2) -> bool {
        let cv = self.cv;
        let title = format!("MODULATION  {}", mod_target_display_name(target).to_uppercase());
        self.pt.text(cv.p(origin.x + 4.0, origin.y + 6.0), Align2::LEFT_CENTER, &title, cv.mono(8.5), self.th.oled_dim);
        let rows: Vec<(usize, crate::dsp::mod_matrix::ModRow)> = {
            let m = self.st.shared.matrix.lock().unwrap_or_else(|e| e.into_inner());
            m.rows_for_target(target).into_iter().map(|i| (i, m.rows()[i].clone())).collect()
        };
        let mut y = origin.y + 16.0;
        for (i, row) in rows.iter().take(4) {
            self.pt.text(
                cv.p(origin.x + 4.0, y + 7.0),
                Align2::LEFT_CENTER,
                mod_source_display_name(row.source).to_uppercase(),
                cv.mono(8.5),
                self.th.text,
            );
            self.amount_bar(&format!("pop-amount:{i}"), *i, row.amount, origin.x + 96.0, y + 1.0, 96.0);
            if self.popup_item(&format!("pop-remove:{i}"), pos2(origin.x + 200.0, y), 18.0, "X", false) {
                self.st.shared.edit_matrix(true, |m| m.remove_row(*i));
            }
            y += 16.0;
        }
        y += 4.0;
        self.pt
            .text(cv.p(origin.x + 4.0, y + 7.0), Align2::LEFT_CENTER, "ADD SOURCE", cv.mono(8.5), self.th.oled_dim);
        y += 16.0;
        let mut close = false;
        for src in 1..NUM_FIXED_SOURCES {
            let k = src - 1;
            let at = pos2(origin.x + (k % 2) as f32 * 112.0, y + (k / 2) as f32 * 16.0);
            if self.popup_item(&format!("pop-add:{src}"), at, 108.0, &mod_source_display_name(src).to_uppercase(), false) {
                self.st
                    .shared
                    .edit_matrix(true, |m| m.add_row(src, Some(target), 0.5, ModCurve::Linear));
                close = true;
            }
        }
        close
    }

    /// Bipolar amount bar for a matrix row; drag horizontally, double-click resets to 0.
    pub(crate) fn amount_bar(&mut self, key: &str, row: usize, amount: f32, x: f32, y: f32, w: f32) {
        let cv = self.cv;
        let rect = cv.r(x, y, w, 13.0);
        let eid = Id::new(("amount", key));
        let resp = self.ui.interact(rect, eid, Sense::click_and_drag());
        self.register(key, ControlKind::Fader, rect);
        if resp.drag_started() {
            self.ui.data_mut(|d| d.insert_temp(eid, amount));
        }
        if resp.dragged() {
            let start: f32 = self.ui.data(|d| d.get_temp(eid)).unwrap_or(amount);
            let v = (start + self.drag_offset().x / (rect.width() * 0.5)).clamp(-1.0, 1.0);
            self.st.shared.edit_matrix(false, |m| m.set_amount(row, v));
        }
        if resp.double_clicked() {
            self.st.shared.edit_matrix(false, |m| m.set_amount(row, 0.0));
        }
        if resp.hovered() {
            self.set_focus("MODULATION AMOUNT", format!("{:+.0}%", amount * 100.0), FocusKind::Plain);
        }
        self.pt.rect_filled(rect, cv.round(2.0), self.th.slot);
        let mid = rect.center().x;
        let end = mid + amount * rect.width() * 0.5;
        self.pt.rect_filled(
            Rect::from_x_y_ranges(mid.min(end)..=mid.max(end), rect.y_range()),
            cv.round(2.0),
            self.th.modulation,
        );
        self.pt
            .line_segment([pos2(mid, rect.top()), pos2(mid, rect.bottom())], Stroke::new(1.0 * cv.s, self.th.scale));
        self.pt.text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("{:+.0}%", amount * 100.0),
            cv.mono(8.0),
            self.th.text,
        );
    }
}

/// Sources offered in palettes and choosers: every fixed source, the four
/// pre-mapped CC aliases (SPEC 4.1) and the currently picked CC.
/// Item width, row pitch and font size of a choice list. The wavetable lists
/// are as wide as their selector on the OSC tab and set larger.
fn choice_metrics(param: ParamId) -> (f32, f32, f32) {
    match param {
        ParamId::OscATable | ParamId::OscBTable => (tabs::TABLE_SELECTOR_W, 24.0, 12.5),
        _ => (142.0, 16.0, 8.5),
    }
}

pub(crate) fn palette_sources(cc_pick: u8) -> Vec<usize> {
    use crate::dsp::mod_matrix::cc_source_index;
    let mut v: Vec<usize> = (1..NUM_FIXED_SOURCES).collect();
    for cc in [1u8, 11, 64, 74] {
        v.push(cc_source_index(cc));
    }
    let picked = cc_source_index(cc_pick);
    if !v.contains(&picked) {
        v.push(picked);
    }
    v
}

#[cfg(test)]
#[path = "../editor_tests.rs"]
mod tests;
