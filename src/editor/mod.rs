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
use crate::dsp::library::{self, UserPreset};
use crate::dsp::presets::{build_factory_preset, factory_preset_name, num_factory_presets};
use crate::dsp::shared::SynthShared;
use crate::dsp::state::{apply_patch_data, save_patch, GuiState, PatchInfo};
use crate::params::DoubleSlitParams;
use theme::{Section, Theme, THEMES};
pub use widgets::{Cv, G};

/// Canvas size in logical pixels.
pub(crate) const BW: f32 = 1400.0;
pub(crate) const BH: f32 = 905.0;
/// Default window size: the canvas at 100 %.
pub const WIDTH: u32 = BW as u32;
pub const HEIGHT: u32 = BH as u32;

/// Top of the section bar in each of the four rows. Rows 0-1 belong to the
/// active tab, rows 2-3 to the always-visible bottom half.
pub(crate) const BAR_Y: [f32; 4] = [76.0, 284.0, 492.0, 700.0];

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
    /// New matrix row from an empty slot: pick a source, then a target.
    NewRow { source: Option<usize>, at: Pos2 },
    /// Amount scale after a source chip was dropped on a target; a click on
    /// it adds the row, a click outside cancels (SPEC 4.4).
    DropAmount { source: usize, target: usize, at: Pos2 },
}

/// Preset list: rows per column, column width, columns shown at once, and the
/// IMPORT / EXPORT row under the list.
const PRESET_ROWS: usize = 24;
const PRESET_COL_W: f32 = 256.0;
const PRESET_MAX_COLS: usize = 5;
const PRESET_FOOTER_H: f32 = 30.0;

/// Width of the drop amount scale (-100 … +100 %) in canvas pixels.
const DROP_SCALE_W: f32 = 220.0;
/// Scale position inside the drop popup, relative to its top left corner.
const DROP_SCALE_X: f32 = 10.0;
const DROP_SCALE_Y: f32 = 22.0;
const DROP_SCALE_H: f32 = 14.0;

/// Amount on the drop scale under canvas x, snapped to whole percent.
fn drop_amount(at: Pos2, x: f32) -> f32 {
    let mid = at.x + DROP_SCALE_X + DROP_SCALE_W * 0.5;
    (((x - mid) / (DROP_SCALE_W * 0.5)).clamp(-1.0, 1.0) * 100.0).round() / 100.0
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
    /// Last preset loaded from the header, for prev/next: factory presets
    /// first, then `user_presets`.
    pub preset: Option<usize>,
    /// The user preset library, re-read when the list opens and after an
    /// export or import.
    pub(crate) user_presets: Vec<UserPreset>,
    pub(crate) presets_dir: std::path::PathBuf,
    /// Running export (`true`) or import dialog.
    pub(crate) preset_job: Option<(bool, tabs::FileJob)>,
    pub(crate) preset_message: String,
    /// First preset list column shown, when there are more than fit.
    pub(crate) preset_scroll: usize,
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
            user_presets: library::list(&tabs::presets_dir()),
            presets_dir: tabs::presets_dir(),
            preset_job: None,
            preset_message: String::new(),
            preset_scroll: 0,
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
        self.preset_poll_job();
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
    /// factory and user presets. nih-plug has no host program list, so this is it.
    fn preset_browser(&mut self) {
        let cv = self.cv;
        let count = self.num_presets();
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
            self.refresh_user_presets();
            self.st.popup = Some(Popup::Presets { at: pos2(232.0, 34.0) });
        }
        if resp.hovered() {
            self.set_focus("PRESETS", "CLICK FOR LIST, IMPORT, EXPORT".to_owned(), FocusKind::Plain);
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

    /// Factory presets plus the user library.
    pub(crate) fn num_presets(&self) -> usize {
        num_factory_presets() + self.st.user_presets.len()
    }

    fn preset_name(&self, index: usize) -> &str {
        match index.checked_sub(num_factory_presets()) {
            None => factory_preset_name(index),
            Some(u) => self.st.user_presets.get(u).map_or("", |p| p.name.as_str()),
        }
    }

    pub(crate) fn refresh_user_presets(&mut self) {
        self.st.user_presets = library::list(&self.st.presets_dir);
    }

    /// Resolves a preset off the audio thread: matrix, tables and LFO shapes
    /// are published to the engine, parameters go to the host.
    pub(crate) fn load_preset(&mut self, index: usize) {
        let data = match index.checked_sub(num_factory_presets()) {
            None => build_factory_preset(index),
            Some(u) => self.st.user_presets.get(u).and_then(|p| library::load(&p.path)),
        };
        let Some(data) = data else {
            self.st.preset_message = format!("CANNOT LOAD {}", self.preset_name(index).to_uppercase());
            self.refresh_user_presets();
            return;
        };
        let values = apply_patch_data(&data, &self.st.shared);
        self.send_all(&values);
        self.st.preset = Some(index);
        self.st.wt.edit = None;
    }

    /// The current sound as a self-contained patch (SPEC 5.1): parameter
    /// values as the editor shows them, so pending edits are included.
    fn current_patch_json(&mut self, name: &str) -> String {
        let mut params = [0.0; NUM_PARAMS];
        for id in ParamId::all() {
            params[id.index()] = self.plain(id);
        }
        let info = PatchInfo { name: name.to_owned(), gui: GuiState { tab: self.st.tab as usize, ..GuiState::default() } };
        save_patch(&self.st.shared, Some(&params), &info, true)
    }

    fn preset_export_dialog(&mut self) {
        let name = library::file_name(&self.st.shared.patch_name());
        let suggested = self.st.presets_dir.join(format!("{name}.json"));
        let _ = std::fs::create_dir_all(&self.st.presets_dir);
        self.st.preset_job = Some((true, tabs::file_dialog(None, suggested)));
    }

    fn preset_import_dialog(&mut self) {
        let open = tabs::OpenFilter { filter: "Double Slit patches | *.json *.JSON", multiple: true };
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        self.st.preset_job = Some((false, tabs::file_dialog(Some(open), home.join(""))));
    }

    fn preset_poll_job(&mut self) {
        let Some((export, job)) = self.st.preset_job.clone() else {
            return;
        };
        let Some(result) = job.lock().ok().and_then(|mut j| j.take()) else {
            return;
        };
        self.st.preset_job = None;
        match result {
            Ok(text) if !text.is_empty() => {
                if export {
                    self.preset_export(std::path::Path::new(&text));
                } else {
                    let files: Vec<std::path::PathBuf> =
                        text.lines().filter(|l| !l.is_empty()).map(std::path::PathBuf::from).collect();
                    self.preset_import(&files);
                }
            }
            Ok(_) => {}
            Err(e) => self.st.preset_message = e,
        }
    }

    /// Writes the current sound to `path` (".json" added when missing). The
    /// file name becomes the patch name; inside the library the export shows
    /// up as a user preset and becomes the selected one.
    pub(crate) fn preset_export(&mut self, path: &std::path::Path) {
        let path = if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")) {
            path.to_path_buf()
        } else {
            path.with_extension("json")
        };
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        let json = self.current_patch_json(&name);
        match library::write_patch(dir, &name, &json) {
            Ok(written) => {
                self.st.shared.set_patch_name(&name);
                self.st.preset_message = format!("EXPORTED {}", tabs::short(&written.to_string_lossy()));
                self.refresh_user_presets();
                self.st.preset = self
                    .st
                    .user_presets
                    .iter()
                    .position(|p| p.path == written)
                    .map(|u| num_factory_presets() + u)
                    .or(self.st.preset);
            }
            Err(e) => self.st.preset_message = format!("EXPORT FAILED: {e}"),
        }
    }

    /// Merges patch files into the library (`library::import`).
    pub(crate) fn preset_import(&mut self, files: &[std::path::PathBuf]) {
        let factory: Vec<&str> = (0..num_factory_presets()).map(factory_preset_name).collect();
        let report = library::import(&self.st.presets_dir, files, &factory);
        self.st.preset_message = report.summary();
        self.refresh_user_presets();
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
        let widths: Vec<f32> = THEMES.iter().map(|t| 14.0 + self.text_w(t.name, 8.0)).collect();
        let total: f32 = widths.iter().map(|w| w + 5.0).sum::<f32>() - 5.0;
        let mut x = BW - 12.0 - total;
        self.text(cv.p(x - 10.0, 22.0), Align2::RIGHT_CENTER, "THEME", 8.5, self.th.header);
        for (i, theme) in THEMES.iter().enumerate() {
            let w = widths[i];
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

    /// Finishes a source-chip drag: a drop on a modulatable control opens the
    /// amount scale with the pointer on +50 % (SPEC 4.4).
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
            if let (Some(target), Some(pos)) = (target, pos) {
                let p = self.cv.to_canvas(pos);
                let at = pos2(
                    p.x - DROP_SCALE_X - DROP_SCALE_W * 0.75,
                    p.y - DROP_SCALE_Y - DROP_SCALE_H * 0.5,
                );
                self.st.popup = Some(Popup::DropAmount { source, target, at });
            }
        }
    }

    // ------------------------------------------------------------ popups

    fn popup_rect(&self, pop: Popup) -> Rect {
        let (at, w, h) = match pop {
            Popup::Presets { at } => {
                let (cols, rows) = self.preset_grid();
                (at, cols.min(PRESET_MAX_COLS) as f32 * PRESET_COL_W + 8.0, (rows + 1) as f32 * 16.0 + 8.0 + PRESET_FOOTER_H)
            }
            Popup::Choice { param, at } => {
                let (w, row, _) = choice_metrics(param);
                (at, w + 8.0, param.desc().choices.len().max(1) as f32 * row + 8.0)
            }
            Popup::Modulation { at, .. } => (at, 272.0, 8.0 + 16.0 * (6.0 + NUM_FIXED_SOURCES as f32 / 2.0)),
            Popup::RowSource { at, .. } => (at, 260.0, 8.0 + 16.0 * ((NUM_FIXED_SOURCES as f32 + 4.0) / 2.0).ceil()),
            Popup::RowTarget { at, .. } => (at, 420.0, 8.0 + 16.0 * (num_mod_targets() as f32 / 2.0).ceil()),
            Popup::NewRow { source: None, at } => (at, 260.0, 8.0 + 16.0 * ((NUM_FIXED_SOURCES as f32 + 4.0) / 2.0).ceil()),
            Popup::NewRow { source: Some(_), at } => (at, 420.0, 24.0 + 16.0 * (num_mod_targets() as f32 / 2.0).ceil()),
            Popup::DropAmount { at, .. } => (at, DROP_SCALE_W + 2.0 * DROP_SCALE_X, DROP_SCALE_Y + DROP_SCALE_H + 10.0),
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
            Popup::Presets { .. } => close = self.preset_list(origin, rect),
            Popup::Choice { param, .. } => {
                let d = param.desc();
                let current = self.plain(param).round() as usize;
                let (w, row, font) = choice_metrics(param);
                for (n, i) in widgets::choice_order(param).into_iter().enumerate() {
                    let key = format!("choice:{}:{i}", d.id_string);
                    let at = origin + vec2(0.0, n as f32 * row);
                    if self.popup_item_sized(&key, at, w, row - 1.0, &d.choices[i].to_uppercase(), current == i, font) {
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
            Popup::DropAmount { source, target, .. } => close = self.drop_amount_scale(source, target, rect),
            Popup::NewRow { source: None, at } => {
                for (k, src) in palette_sources(self.st.cc_pick).iter().enumerate() {
                    let item = origin + vec2((k % 2) as f32 * 126.0, (k / 2) as f32 * 16.0);
                    if self.popup_item(&format!("newsrc:{k}"), item, 122.0, &mod_source_display_name(*src).to_uppercase(), false) {
                        self.st.popup = Some(Popup::NewRow { source: Some(*src), at });
                    }
                }
            }
            Popup::NewRow { source: Some(src), .. } => {
                let title = format!("{} >  PICK A TARGET", mod_source_display_name(src).to_uppercase());
                self.pt
                    .text(cv.p(origin.x + 4.0, origin.y + 6.0), Align2::LEFT_CENTER, &title, cv.mono(8.5), self.th.oled_dim);
                for t in 0..num_mod_targets() {
                    let item = origin + vec2((t % 2) as f32 * 206.0, 16.0 + (t / 2) as f32 * 16.0);
                    if self.popup_item(&format!("newtgt:{t}"), item, 202.0, &mod_target_display_name(t).to_uppercase(), false) {
                        self.st.shared.edit_matrix(true, |m| m.add_row(src, Some(t), 0.5, ModCurve::Linear));
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

    /// Columns of the preset list (factory columns, then at least one user
    /// column) and the rows of the longest column.
    fn preset_grid(&self) -> (usize, usize) {
        let (factory, user) = (num_factory_presets(), self.st.user_presets.len());
        let cols = factory.div_ceil(PRESET_ROWS) + user.div_ceil(PRESET_ROWS).max(1);
        (cols, factory.max(user).clamp(1, PRESET_ROWS))
    }

    /// The preset list: FACTORY and USER columns under their titles, IMPORT /
    /// EXPORT and the last result below. The wheel scrolls the columns when
    /// there are more than fit. Returns whether to close the popup.
    fn preset_list(&mut self, origin: Pos2, rect: Rect) -> bool {
        let cv = self.cv;
        let factory = num_factory_presets();
        let factory_cols = factory.div_ceil(PRESET_ROWS);
        let (cols, rows) = self.preset_grid();
        let shown = cols.min(PRESET_MAX_COLS);
        if cols > shown && self.ui.rect_contains_pointer(rect) {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                let s = self.st.preset_scroll as isize - scroll.signum() as isize;
                self.st.preset_scroll = s.clamp(0, (cols - shown) as isize) as usize;
            }
        }
        self.st.preset_scroll = self.st.preset_scroll.min(cols - shown);

        let mut close = false;
        for col in self.st.preset_scroll..self.st.preset_scroll + shown {
            let x = origin.x + (col - self.st.preset_scroll) as f32 * PRESET_COL_W;
            let (first, last, title) = if col < factory_cols {
                (col * PRESET_ROWS, factory, (col == 0).then_some("FACTORY"))
            } else {
                let c = col - factory_cols;
                (factory + c * PRESET_ROWS, self.num_presets(), (c == 0).then_some("USER"))
            };
            if let Some(title) = title {
                self.pt.text(cv.p(x + 5.0, origin.y + 7.0), Align2::LEFT_CENTER, title, cv.mono(8.5), self.th.oled_dim);
            }
            if title == Some("USER") && first == last {
                let hint = "EXPORT OR IMPORT TO ADD";
                self.pt.text(cv.p(x + 5.0, origin.y + 23.0), Align2::LEFT_CENTER, hint, cv.mono(8.5), self.th.oled_dim);
            }
            for i in first..last.min(first + PRESET_ROWS) {
                let label = format!("{:02}  {}", i + 1, self.preset_name(i).to_uppercase());
                let at = pos2(x, origin.y + 16.0 + (i - first) as f32 * 16.0);
                if self.popup_item(&format!("preset:{i}"), at, PRESET_COL_W - 8.0, &label, self.st.preset == Some(i)) {
                    self.load_preset(i);
                    close = true;
                }
            }
        }

        let y = origin.y + 16.0 + rows as f32 * 16.0 + 6.0;
        let busy = self.st.preset_job.is_some();
        if self.button("preset-import", origin.x, y, 70.0, "IMPORT", false, "IMPORT PATCH FILES").clicked() && !busy {
            self.preset_import_dialog();
        }
        if self.button("preset-export", origin.x + 74.0, y, 70.0, "EXPORT", false, "EXPORT THE CURRENT SOUND").clicked() && !busy {
            self.preset_export_dialog();
        }
        let message = self.st.preset_message.clone();
        self.pt.text(cv.p(origin.x + 154.0, y + 9.0), Align2::LEFT_CENTER, &message, cv.mono(8.5), self.th.oled_dim);
        close
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
            self.amount_bar(&format!("pop-amount:{i}"), *i, row.amount, origin.x + 128.0, y + 1.0, 104.0);
            if self.popup_item(&format!("pop-remove:{i}"), pos2(origin.x + 242.0, y), 18.0, "X", false) {
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
            let at = pos2(origin.x + (k % 2) as f32 * 132.0, y + (k / 2) as f32 * 16.0);
            if self.popup_item(&format!("pop-add:{src}"), at, 128.0, &mod_source_display_name(src).to_uppercase(), false) {
                self.st
                    .shared
                    .edit_matrix(true, |m| m.add_row(src, Some(target), 0.5, ModCurve::Linear));
                close = true;
            }
        }
        close
    }

    /// Second step of a chip drop: the pointer picks the amount on a bipolar
    /// scale, a primary click adds the row. Returns true once it was added.
    fn drop_amount_scale(&mut self, source: usize, target: usize, rect: Rect) -> bool {
        let cv = self.cv;
        // The clamped popup position, so the scale sits where it is drawn.
        let at = cv.to_canvas(rect.min);
        let (pointer, pressed) = self.ui.input(|i| (i.pointer.hover_pos(), i.pointer.primary_pressed()));
        let amount = pointer.map_or(0.5, |p| drop_amount(at, cv.to_canvas(p).x));

        let title = format!(
            "{} > {}",
            mod_source_display_name(source).to_uppercase(),
            mod_target_display_name(target).to_uppercase()
        );
        self.pt.text(cv.p(at.x + DROP_SCALE_X, at.y + 10.0), Align2::LEFT_CENTER, &title, cv.mono(9.5), self.th.oled_dim);
        let value = format!("{:+.0}%", amount * 100.0);
        self.pt.text(
            cv.p(at.x + DROP_SCALE_X + DROP_SCALE_W, at.y + 10.0),
            Align2::RIGHT_CENTER,
            &value,
            cv.mono(9.5),
            self.th.oled_ink,
        );

        let bar = cv.r(at.x + DROP_SCALE_X, at.y + DROP_SCALE_Y, DROP_SCALE_W, DROP_SCALE_H);
        self.register("drop:scale", ControlKind::Fader, bar);
        self.bipolar_fill(bar, amount);
        let end = bar.center().x + amount * bar.width() * 0.5;
        for k in 0..=4 {
            let x = bar.left() + bar.width() * k as f32 * 0.25;
            let h = if k == 2 { 0.0 } else { 0.35 };
            self.pt.line_segment(
                [pos2(x, bar.top() + bar.height() * h), pos2(x, bar.bottom())],
                Stroke::new(1.0 * cv.s, self.th.scale),
            );
        }
        self.pt.line_segment([pos2(end, bar.top() - 2.0 * cv.s), pos2(end, bar.bottom() + 2.0 * cv.s)], cv.stroke(2.0, self.th.oled_ink));

        let clicked = pressed && pointer.is_some_and(|p| rect.contains(p));
        if clicked {
            self.st
                .shared
                .edit_matrix(true, |m| m.add_row(source, Some(target), amount, ModCurve::Linear));
        }
        clicked
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
        self.bipolar_fill(rect, amount);
        let mid = rect.center().x;
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
