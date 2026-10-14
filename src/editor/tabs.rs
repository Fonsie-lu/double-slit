//! The five tabs (SPEC 6.2) and the always-visible bottom half.

use nih_plug_egui::egui::{pos2, vec2, Align2, Id, Pos2, Rect, Sense, Shape, Stroke};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use super::theme::Section;
use super::widgets::{graph_bg, Nums, Steps, G, N10L, N10R};
use super::{palette_sources, row, ControlKind, FocusKind, Popup, BAR_Y, BW};
use crate::dsp::common::FRAME_SIZE;
use crate::dsp::mod_matrix::{
    cc_source_index, mod_source_category, mod_source_display_name, mod_target_display_name, ModCurve,
    ModSource,
};
use crate::dsp::params::{ParamId, CUSTOM_TABLE_CHOICE, FILTER_TYPES};
use crate::dsp::wavetable::Wavetable;

/// Width of each oscillator panel on the OSC tab.
const OSC_W: f32 = 590.0;
/// Width of the envelope and LFO panels, height of their graphs and the
/// centre line of the knob row below them.
const MOD_W: f32 = 504.0;
const GRAPH_H: f32 = 236.0;
const KNOB_ROW: f32 = BAR_Y[2] + 306.0;

// ------------------------------------------------------------------ WT state

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DrawTool {
    #[default]
    Pencil,
    Line,
    Smooth,
}

/// Result of a file dialog running on a helper thread.
type FileJob = Arc<Mutex<Option<Result<String, String>>>>;

#[derive(Default)]
pub struct WtState {
    /// Oscillator being edited (0 = A, 1 = B).
    pub slot: usize,
    pub frame: usize,
    pub tool: DrawTool,
    /// Working copy of the edited table; published to the engine when a
    /// stroke ends. `None` means "reload from the engine".
    pub edit: Option<Wavetable>,
    /// Table revision the working copy was taken at.
    seen_revision: u32,
    /// Last drawn point (sample index, value) during a pencil stroke.
    last: Option<(usize, f32)>,
    /// Start of a line-tool stroke (sample index, value).
    anchor: Option<(usize, f32)>,
    /// First visible frame in the frame list.
    scroll: usize,
    job: Option<(bool, FileJob)>,
    pub message: String,
}

/// `$XDG_DATA_HOME/double-slit/tables`, created on demand (SPEC 5.3).
pub fn tables_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("double-slit").join("tables");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Runs `zenity` for a file name without blocking the GUI.
fn file_dialog(save: bool, suggested: PathBuf) -> FileJob {
    let job: FileJob = Arc::new(Mutex::new(None));
    let out = job.clone();
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("zenity");
        cmd.arg("--file-selection").arg(format!("--filename={}", suggested.display()));
        if save {
            cmd.arg("--save").arg("--confirm-overwrite");
        } else {
            cmd.arg("--file-filter=Wavetables | *.wav *.WAV *.wt");
        }
        let result = match cmd.output() {
            Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned()),
            Ok(_) => Err(String::new()),
            Err(e) => Err(format!("NO FILE DIALOG ({e})")),
        };
        if let Ok(mut j) = out.lock() {
            *j = Some(result);
        }
    });
    job
}

impl G<'_, '_> {
    // ------------------------------------------------------------------ OSC

    /// OSC A on the left and OSC B on the right (SPEC 6.2), each with its
    /// table, pitch, wavetable display and knobs; between them what passes
    /// from A to B, the sub / noise oscillator and the level meters.
    pub(crate) fn osc_tab(&mut self) {
        let th = self.th;
        self.accent = th.accents[Section::OscA as usize];
        self.osc_panel(0, 16.0);
        self.accent = th.accents[Section::OscB as usize];
        self.osc_panel(1, BW - 12.0 - OSC_W);

        let (x0, x1) = (16.0 + OSC_W + 8.0, BW - 12.0 - OSC_W - 8.0);
        let cx = (x0 + x1) * 0.5;
        self.gradient_bar(x0, x1, BAR_Y[0], "A  →  B", Section::OscA, Section::OscB);
        self.accent = th.accents[Section::OscB as usize];
        self.text(self.cv.p(x0 + 4.0, 116.0), Align2::LEFT_CENTER, "SYNC", 9.0, th.legend);
        self.selector(ParamId::OscBSyncMode, x0 + 36.0, 106.0, x1 - x0 - 36.0, "");
        self.knob(ParamId::OscBSyncRatio, cx - 56.0, 160.0, 16.0, "RATIO");
        self.knob(ParamId::OscBFmIndex, cx, 160.0, 16.0, "FM");
        self.knob(ParamId::OscBAmIndex, cx + 56.0, 160.0, 16.0, "AM");

        self.bar(214.0, &[(x0, x1, "SUB / NOISE", Section::OscC)]);
        self.accent = th.accents[Section::OscC as usize];
        self.text(self.cv.p(x0 + 4.0, 254.0), Align2::LEFT_CENTER, "TYPE", 9.0, th.legend);
        self.selector(ParamId::OscCMode, x0 + 36.0, 244.0, x1 - x0 - 36.0, "");
        self.knob(ParamId::OscCLevel, cx - 36.0, 298.0, 16.0, "LEVEL");
        self.knob(ParamId::OscCPitch, cx + 36.0, 298.0, 16.0, "PITCH");

        self.bar(352.0, &[(x0, x1, "LEVELS", Section::Master)]);
        self.osc_meters(x0, x1, 388.0);
    }

    /// One oscillator in the style of Serum's: header with the power switch,
    /// table and pitch, the wavetable display, phase (and noise for B), knobs.
    fn osc_panel(&mut self, osc: usize, x0: f32) {
        let cv = self.cv;
        let section = if osc == 0 { Section::OscA } else { Section::OscB };
        let base = if osc == 0 { ParamId::OscAOn } else { ParamId::OscBOn };
        let p = |o: usize| ParamId::from_index(base.index() + o);
        let w = OSC_W;
        self.osc_header(osc, x0, section);

        self.selector(p(1), x0 + 8.0, 104.0, 290.0, "");
        self.field(p(5), x0 + 306.0, 104.0, 88.0, "OCT");
        self.field(p(6), x0 + 400.0, 104.0, 88.0, "SEMI");
        self.field(p(7), x0 + 494.0, 104.0, w - 502.0, "FINE");

        self.wave_display(osc, cv.r(x0 + 8.0, 132.0, w - 16.0, 234.0));

        let y = 372.0;
        self.field(p(12), x0 + 8.0, y, 110.0, "PHASE");
        self.text(cv.p(x0 + 130.0, y + 10.0), Align2::LEFT_CENTER, "START", 9.0, self.th.legend);
        self.selector(p(13), x0 + 166.0, y, 110.0, "");
        if osc == 1 {
            self.text(cv.p(x0 + 290.0, y + 10.0), Align2::LEFT_CENTER, "NOISE", 9.0, self.th.legend);
            self.selector(ParamId::OscBNoiseMode, x0 + 330.0, y, 110.0, "");
        }

        self.pt.rect_filled(cv.r(x0, 400.0, w, 86.0), cv.round(4.0), self.th.slot.gamma_multiply(0.35));
        let knobs = [(2, "WT POS"), (8, "UNISON"), (9, "DETUNE"), (10, "SPREAD"), (11, "BLEND"), (4, "PAN"), (3, "LEVEL")];
        for (k, (o, label)) in knobs.into_iter().enumerate() {
            self.knob(p(o), x0 + 46.0 + k as f32 * 83.0, 432.0, 17.0, label);
        }
    }

    /// Section bar with the power switch and level LED on the left and the
    /// oscillator type on the right.
    fn osc_header(&mut self, osc: usize, x0: f32, section: Section) {
        let cv = self.cv;
        let th = self.th;
        let id = if osc == 0 { ParamId::OscAOn } else { ParamId::OscBOn };
        let bar = cv.r(x0 + 1.5, BAR_Y[0], OSC_W - 3.0, 22.0);
        self.pt.rect_filled(bar, cv.round(3.0), th.bars[section as usize]);

        let hit = cv.r(x0 + 4.0, BAR_Y[0], 96.0, 22.0);
        let resp = self.ui.interact(hit, Id::new(("osc-on", osc)), Sense::click());
        self.register(&id.desc().id_string, ControlKind::Switch, hit);
        let on = self.plain(id) > 0.5;
        if resp.clicked() {
            self.change_plain(id, if on { 0.0 } else { 1.0 });
        }
        if resp.hovered() {
            let v = self.value_text(id);
            self.set_focus(&format!("{} ON", id.desc().module), v, FocusKind::Plain);
        }
        let square = cv.r(x0 + 9.0, BAR_Y[0] + 4.0, 14.0, 14.0);
        self.pt.rect_filled(square, cv.round(2.0), th.bar_text);
        self.pt.rect_filled(square.shrink(2.5 * cv.s), cv.round(1.0), if on { th.led } else { th.led_off });
        let title = if osc == 0 { "OSC A" } else { "OSC B" };
        self.text(cv.p(x0 + 32.0, BAR_Y[0] + 11.5), Align2::LEFT_CENTER, title, 15.0, th.bar_text);
        let level = self.st.shared.feedback.osc_meters[osc].load(Relaxed);
        self.led(cv.p(x0 + 102.0, BAR_Y[0] + 11.0), if on { (level * 2.0).min(1.0) } else { 0.0 });

        let noise = osc == 1 && self.plain(ParamId::OscBNoiseMode) > 0.5;
        let kind = cv.r(x0 + OSC_W - 150.0, BAR_Y[0] + 3.0, 144.0, 16.0);
        self.pt.rect_filled(kind, cv.round(2.0), th.slot);
        let text = if noise { "NOISE" } else { "WAVETABLE" };
        self.text(kind.center(), Align2::CENTER_CENTER, text, 9.5, self.accent);
    }

    /// Section bar fading from one section colour to another.
    fn gradient_bar(&self, x0: f32, x1: f32, y: f32, title: &str, from: Section, to: Section) {
        let cv = self.cv;
        let r = cv.r(x0 + 1.5, y, x1 - x0 - 3.0, 22.0);
        let (a, b) = (self.th.bars[from as usize], self.th.bars[to as usize]);
        let mut mesh = nih_plug_egui::egui::Mesh::default();
        mesh.colored_vertex(r.left_top(), a);
        mesh.colored_vertex(r.right_top(), b);
        mesh.colored_vertex(r.right_bottom(), b);
        mesh.colored_vertex(r.left_bottom(), a);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        self.pt.add(Shape::mesh(mesh));
        self.text(r.center(), Align2::CENTER_CENTER, title, 15.0, self.th.bar_text);
    }

    /// Per-oscillator and output meters (SPEC 6.3 "per-osc level LEDs").
    fn osc_meters(&mut self, x0: f32, x1: f32, y: f32) {
        let cv = self.cv;
        let fb = &self.st.shared.feedback;
        let values = [fb.osc_meters[0].load(Relaxed), fb.osc_meters[1].load(Relaxed), fb.meters[0].load(Relaxed).max(fb.meters[1].load(Relaxed))];
        for (k, (name, v)) in ["OSC A", "OSC B", "OUT"].iter().zip(values).enumerate() {
            let yy = y + k as f32 * 28.0;
            self.text(cv.p(x0 + 4.0, yy), Align2::LEFT_CENTER, name, 9.0, self.th.legend);
            let (m0, m1) = (x0 + 48.0, x1 - 4.0);
            self.pt.rect_filled(cv.r(m0, yy - 4.0, m1 - m0, 8.0), cv.round(2.0), self.th.slot);
            let db = 20.0 * v.max(1.0e-6).log10();
            let t = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            if t > 0.0 {
                let color = if v > 1.0 { self.th.clip } else { self.th.led };
                self.pt.rect_filled(cv.r(m0, yy - 4.0, t * (m1 - m0), 8.0), cv.round(2.0), color);
            }
        }
    }

    // ------------------------------------------------------------------- WT

    pub(crate) fn wt_tab(&mut self) {
        let cv = self.cv;
        self.bar(BAR_Y[0], &[(16.0, BW - 12.0, "WAVETABLE EDITOR", Section::Wt)]);
        self.accent = self.th.accents[Section::Wt as usize];
        self.wt_sync();
        self.wt_toolbar();
        let Some(frames) = self.st.wt.edit.as_ref().map(|t| t.num_frames()) else {
            return;
        };
        self.st.wt.frame = self.st.wt.frame.min(frames - 1);
        self.wt_frame_list(cv.r(24.0, 136.0, 140.0, 330.0));
        self.wt_draw_grid(cv.r(176.0, 136.0, 820.0, 330.0));
        self.wt_morph(cv.r(1010.0, 136.0, 370.0, 330.0));
        self.wt_poll_job();
    }

    /// Takes a fresh working copy when the engine's table changed under us
    /// (preset load, table selector, another slot).
    fn wt_sync(&mut self) {
        let shared = self.st.shared.clone();
        let rev = shared.table_revision.load(Relaxed);
        let stroke = self.st.wt.last.is_some() || self.st.wt.anchor.is_some();
        if self.st.wt.edit.is_none() || (rev != self.st.wt.seen_revision && !stroke) {
            self.st.wt.edit = Some((*shared.table(self.st.wt.slot)).clone());
            self.st.wt.seen_revision = rev;
        }
    }

    /// Publishes the working copy (mips regenerate here, on the GUI thread)
    /// and switches the oscillator's table parameter to Custom.
    fn wt_publish(&mut self) {
        let slot = self.st.wt.slot;
        let Some(table) = self.st.wt.edit.as_mut() else {
            return;
        };
        table.refresh();
        let published = Arc::new(table.clone());
        self.st.shared.set_table(slot, published);
        self.st.wt.seen_revision = self.st.shared.table_revision.load(Relaxed);
        let id = if slot == 0 { ParamId::OscATable } else { ParamId::OscBTable };
        self.change_plain(id, CUSTOM_TABLE_CHOICE as f32);
    }

    fn wt_toolbar(&mut self) {
        let y = BAR_Y[0] + 32.0;
        let mut x = 24.0;
        for (slot, name) in [(0usize, "OSC A"), (1, "OSC B")] {
            if self.button(&format!("wt:slot{slot}"), x, y, 54.0, name, self.st.wt.slot == slot, "EDIT TABLE OF").clicked() && self.st.wt.slot != slot {
                self.st.wt.slot = slot;
                self.st.wt.edit = None;
                self.st.wt.frame = 0;
                self.st.wt.scroll = 0;
            }
            x += 58.0;
        }
        x += 16.0;
        for (tool, name) in [(DrawTool::Pencil, "PENCIL"), (DrawTool::Line, "LINE"), (DrawTool::Smooth, "SMOOTH")] {
            if self.button(&format!("wt:{name}"), x, y, 58.0, name, self.st.wt.tool == tool, "DRAW TOOL").clicked() {
                self.st.wt.tool = tool;
            }
            x += 62.0;
        }
        x += 16.0;
        let frame = self.st.wt.frame;
        let actions: [(&str, &str); 6] = [
            ("ADD", "ADD A FRAME (COPY OF CURRENT)"),
            ("DUP", "DUPLICATE FRAME"),
            ("DEL", "DELETE FRAME"),
            ("UP", "MOVE FRAME UP"),
            ("DOWN", "MOVE FRAME DOWN"),
            ("NORMALISE", "NORMALISE FRAME TO FULL SCALE"),
        ];
        for (name, help) in actions {
            let w = if name == "NORMALISE" { 76.0 } else { 44.0 };
            if self.button(&format!("wt:{name}"), x, y, w, name, false, help).clicked() {
                if let Some(t) = self.st.wt.edit.as_mut() {
                    let n = t.num_frames();
                    match name {
                        "ADD" => {
                            let copy = t.frame_copy(frame);
                            t.add_frame(&copy);
                            self.st.wt.frame = t.num_frames() - 1;
                        }
                        "DUP" => {
                            t.duplicate_frame(frame);
                            self.st.wt.frame = (frame + 1).min(t.num_frames() - 1);
                        }
                        "DEL" => {
                            t.remove_frame(frame);
                            self.st.wt.frame = frame.min(t.num_frames() - 1);
                        }
                        "UP" if frame > 0 => {
                            t.move_frame(frame, frame - 1);
                            self.st.wt.frame = frame - 1;
                        }
                        "DOWN" if frame + 1 < n => {
                            t.move_frame(frame, frame + 1);
                            self.st.wt.frame = frame + 1;
                        }
                        "NORMALISE" => {
                            let data = t.mutable_frame(frame);
                            let peak = data.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
                            if peak > 1.0e-6 {
                                data.iter_mut().for_each(|v| *v /= peak);
                            }
                        }
                        _ => {}
                    }
                }
                self.wt_publish();
            }
            x += w + 4.0;
        }
        x += 16.0;
        if self.button("wt:import", x, y, 60.0, "IMPORT", false, "IMPORT .WAV OR .WT").clicked() && self.st.wt.job.is_none() {
            self.st.wt.job = Some((false, file_dialog(false, tables_dir().join(""))));
        }
        x += 64.0;
        for (ext, w) in [("wav", 76.0), ("wt", 70.0)] {
            let label = format!("EXPORT {}", ext.to_uppercase());
            if self.button(&format!("wt:export-{ext}"), x, y, w, &label, false, "EXPORT TABLE").clicked() && self.st.wt.job.is_none() {
                let name = self.st.wt.edit.as_ref().map(|t| t.name().to_owned()).unwrap_or_default();
                let file = tables_dir().join(format!("{name}.{ext}"));
                self.st.wt.job = Some((true, file_dialog(true, file)));
            }
            x += w + 4.0;
        }
        let name = self.st.wt.edit.as_ref().map(|t| t.name().to_uppercase()).unwrap_or_default();
        let text = if self.st.wt.message.is_empty() { name } else { self.st.wt.message.clone() };
        self.text(self.cv.p(BW - 20.0, y + 9.0), Align2::RIGHT_CENTER, &text, 9.0, self.th.legend);
    }

    fn wt_poll_job(&mut self) {
        let Some((save, job)) = self.st.wt.job.clone() else {
            return;
        };
        let result = job.lock().ok().and_then(|mut j| j.take());
        let Some(result) = result else {
            return;
        };
        self.st.wt.job = None;
        match result {
            Ok(path) if !path.is_empty() => {
                if save {
                    let Some(t) = self.st.wt.edit.as_ref() else { return };
                    let bytes = if path.to_ascii_lowercase().ends_with(".wt") { t.write_wt() } else { t.write_wav(44_100.0) };
                    self.st.wt.message = match std::fs::write(&path, bytes) {
                        Ok(()) => format!("SAVED {}", short(&path)),
                        Err(e) => format!("SAVE FAILED: {e}"),
                    };
                } else {
                    self.wt_import(&path);
                }
            }
            Ok(_) => {}
            Err(e) => self.st.wt.message = e,
        }
    }

    fn wt_import(&mut self, path: &str) {
        let Ok(bytes) = std::fs::read(path) else {
            self.st.wt.message = format!("CANNOT READ {}", short(path));
            return;
        };
        let stem = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Imported".to_owned());
        let parsed = Wavetable::parse_wt(&bytes).or_else(|| Wavetable::parse_wav(&bytes).map(|f| (f, stem)));
        match parsed {
            Some((frames, name)) if !frames.is_empty() => {
                let mut t = Wavetable::default();
                t.set_frames(&frames, &name);
                self.st.wt.edit = Some(t);
                self.st.wt.frame = 0;
                self.st.wt.message = format!("IMPORTED {} ({} FRAMES)", short(path), frames.len());
                self.wt_publish();
            }
            _ => self.st.wt.message = format!("NOT A WAVETABLE: {}", short(path)),
        }
    }

    fn wt_frame_list(&mut self, rect: Rect) {
        let cv = self.cv;
        let s = cv.s;
        graph_bg(self, rect);
        let Some(table) = self.st.wt.edit.as_ref() else { return };
        let frames = table.num_frames();
        let row_h = 44.0 * s;
        let visible = ((rect.height() / row_h) as usize).max(1);
        let resp = self.ui.interact(rect, Id::new("wt-list"), Sense::click());
        self.register("wt:list", ControlKind::Graph, rect);
        if resp.hovered() {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll < 0.0 {
                self.st.wt.scroll = (self.st.wt.scroll + 1).min(frames.saturating_sub(visible));
            } else if scroll > 0.0 {
                self.st.wt.scroll = self.st.wt.scroll.saturating_sub(1);
            }
        }
        // Keep the selected frame in view.
        if self.st.wt.frame < self.st.wt.scroll {
            self.st.wt.scroll = self.st.wt.frame;
        } else if self.st.wt.frame >= self.st.wt.scroll + visible {
            self.st.wt.scroll = self.st.wt.frame + 1 - visible;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let i = self.st.wt.scroll + ((p.y - rect.top()) / row_h) as usize;
                if i < frames {
                    self.st.wt.frame = i;
                }
            }
        }
        let table = self.st.wt.edit.as_ref().unwrap();
        for k in 0..visible.min(frames - self.st.wt.scroll) {
            let i = self.st.wt.scroll + k;
            let cell = Rect::from_min_size(rect.min + vec2(0.0, k as f32 * row_h), vec2(rect.width(), row_h)).shrink(3.0 * s);
            let selected = i == self.st.wt.frame;
            self.pt.rect_filled(cell, cv.round(2.0), if selected { self.th.button_hover } else { self.th.slot });
            let data = table.display_frame(i, 64);
            let inner = cell.shrink2(vec2(26.0 * s, 4.0 * s)).translate(vec2(10.0 * s, 0.0));
            let pts: Vec<Pos2> = data
                .iter()
                .enumerate()
                .map(|(j, v)| pos2(inner.left() + inner.width() * j as f32 / 63.0, inner.center().y - v * inner.height() * 0.5))
                .collect();
            self.pt.add(Shape::line(pts, Stroke::new(1.0 * s, if selected { self.accent } else { self.th.step_off })));
            self.pt.text(cell.left_center() + vec2(4.0 * s, 0.0), Align2::LEFT_CENTER, format!("{}", i + 1), cv.font(8.0), self.th.legend);
        }
    }

    /// Single-frame editor: pencil, line and smooth tools on a grid. Edits go
    /// into the working copy and are published when the stroke ends.
    fn wt_draw_grid(&mut self, rect: Rect) {
        let cv = self.cv;
        let s = cv.s;
        graph_bg(self, rect);
        let inner = rect.shrink(8.0 * s);
        for k in 0..=8 {
            let x = inner.left() + inner.width() * k as f32 / 8.0;
            self.pt.line_segment([pos2(x, inner.top()), pos2(x, inner.bottom())], Stroke::new(0.5 * s, self.th.scale.gamma_multiply(0.6)));
        }
        for k in 0..=4 {
            let y = inner.top() + inner.height() * k as f32 / 4.0;
            let w = if k == 2 { 1.0 } else { 0.5 };
            self.pt.line_segment([pos2(inner.left(), y), pos2(inner.right(), y)], Stroke::new(w * s, self.th.scale.gamma_multiply(0.6)));
        }
        let to_sample = |p: Pos2| -> (usize, f32) {
            let i = (((p.x - inner.left()) / inner.width()) * FRAME_SIZE as f32).clamp(0.0, (FRAME_SIZE - 1) as f32) as usize;
            let v = (1.0 - 2.0 * (p.y - inner.top()) / inner.height()).clamp(-1.0, 1.0);
            (i, v)
        };
        let resp = self.ui.interact(rect, Id::new("wt-grid"), Sense::click_and_drag());
        self.register("wt:grid", ControlKind::Graph, rect);
        let frame = self.st.wt.frame;
        let tool = self.st.wt.tool;
        let pointer = resp.interact_pointer_pos();
        let pressed = resp.drag_started() || resp.clicked();
        if let (true, Some(pointer)) = (resp.dragged() || pressed, pointer) {
            let (i, v) = to_sample(pointer);
            let start = if pressed || self.st.wt.last.is_none() && self.st.wt.anchor.is_none() {
                self.ui.input(|inp| inp.pointer.press_origin()).map(to_sample).unwrap_or((i, v))
            } else {
                self.st.wt.last.unwrap_or((i, v))
            };
            if let Some(t) = self.st.wt.edit.as_mut() {
                let data = t.mutable_frame(frame);
                match tool {
                    DrawTool::Pencil => {
                        draw_segment(data, start, (i, v));
                        self.st.wt.last = Some((i, v));
                    }
                    DrawTool::Line => {
                        if self.st.wt.anchor.is_none() {
                            self.st.wt.anchor = Some(start);
                        }
                        self.st.wt.last = Some((i, v));
                    }
                    DrawTool::Smooth => {
                        smooth_around(data, i, 48);
                        self.st.wt.last = Some((i, v));
                    }
                }
            }
        }
        let released = resp.drag_stopped() || resp.clicked();
        if released {
            if let (DrawTool::Line, Some(a), Some(b)) = (tool, self.st.wt.anchor, self.st.wt.last) {
                if let Some(t) = self.st.wt.edit.as_mut() {
                    draw_segment(t.mutable_frame(frame), a, b);
                }
            }
            if self.st.wt.last.is_some() {
                self.wt_publish();
            }
            self.st.wt.last = None;
            self.st.wt.anchor = None;
        }
        if resp.hovered() {
            let name = match tool {
                DrawTool::Pencil => "PENCIL: DRAW THE WAVE",
                DrawTool::Line => "LINE: DRAG A STRAIGHT SEGMENT",
                DrawTool::Smooth => "SMOOTH: BRUSH OVER EDGES",
            };
            self.set_focus(name, format!("FRAME {}", frame + 1), FocusKind::Plain);
        }

        let Some(table) = self.st.wt.edit.as_ref() else { return };
        let data = table.frame_data(frame);
        let n = 512;
        let mid = inner.center().y;
        let pts: Vec<Pos2> = (0..n)
            .map(|k| {
                let v = data[k * FRAME_SIZE / n];
                pos2(inner.left() + inner.width() * k as f32 / (n - 1) as f32, mid - v.clamp(-1.0, 1.0) * inner.height() * 0.5)
            })
            .collect();
        super::widgets::fill_under(&self.pt, &pts, mid, self.accent.gamma_multiply(0.2));
        self.pt.add(Shape::line(pts, Stroke::new(1.6 * s, self.accent)));
        if let (Some(a), Some(b)) = (self.st.wt.anchor, self.st.wt.last) {
            let p = |(i, v): (usize, f32)| pos2(inner.left() + inner.width() * i as f32 / FRAME_SIZE as f32, mid - v * inner.height() * 0.5);
            self.pt.line_segment([p(a), p(b)], Stroke::new(1.5 * s, self.th.drag_line));
        }
    }

    /// All frames stacked with an offset, the selected one highlighted.
    fn wt_morph(&mut self, rect: Rect) {
        let s = self.cv.s;
        graph_bg(self, rect);
        let Some(table) = self.st.wt.edit.as_ref() else { return };
        let frames = table.num_frames();
        let shown = frames.min(40);
        let inner = rect.shrink(12.0 * s);
        let step = vec2(inner.width() * 0.3, -inner.height() * 0.55) / shown.max(1) as f32;
        let w = inner.width() * 0.7;
        let h = inner.height() * 0.2;
        let base = pos2(inner.left(), inner.bottom() - h);
        for k in 0..shown {
            let f = if shown == frames { k } else { k * (frames - 1) / (shown - 1).max(1) };
            let o = base + step * k as f32;
            let data = table.display_frame(f, 64);
            let pts: Vec<Pos2> = data
                .iter()
                .enumerate()
                .map(|(j, v)| pos2(o.x + w * j as f32 / 63.0, o.y - v * h * 0.5))
                .collect();
            let selected = f == self.st.wt.frame;
            let color = if selected { self.th.drag_line } else { self.accent.gamma_multiply(0.25 + 0.5 * k as f32 / shown as f32) };
            self.pt.add(Shape::line(pts, Stroke::new(if selected { 1.8 } else { 0.9 } * s, color)));
        }
        let caption = format!("{} FRAMES", frames);
        self.text(rect.left_top() + vec2(6.0, 4.0) * s, Align2::LEFT_TOP, &caption, 8.0, self.th.legend);
    }

    // --------------------------------------------------------------- FILTER

    pub(crate) fn filter_tab(&mut self) {
        let cv = self.cv;
        self.bar(BAR_Y[0], &[(16.0, BW - 12.0, "FILTER", Section::Filter)]);
        self.accent = self.th.accents[Section::Filter as usize];
        let (l, sl) = row(0);
        self.switch(ParamId::FilterOn, 70.0, l, "ON");
        // Type list: one button per type (the C++ build's icon row).
        let current = self.plain(ParamId::FilterType).round() as usize;
        for (i, name) in FILTER_TYPES.iter().enumerate() {
            let y = BAR_Y[0] + 108.0 + i as f32 * 22.0;
            if self.button(&format!("ftype:{i}"), 28.0, y, 84.0, &name.to_uppercase(), i == current, "FILTER TYPE").clicked() {
                self.change_plain(ParamId::FilterType, i as f32);
            }
        }
        let list = cv.r(28.0, BAR_Y[0] + 108.0, 84.0, FILTER_TYPES.len() as f32 * 22.0);
        self.register("filter.type", ControlKind::Selector, list);
        let graph = cv.r(136.0, BAR_Y[0] + 32.0, 870.0, 360.0);
        self.filter_graph(graph);

        let faders = [
            (ParamId::FilterDrive, "DRIVE", N10L),
            (ParamId::FilterMix, "MIX", Nums::None),
            (ParamId::FilterKeyTrack, "KEY\nTRACK", Nums::None),
            (ParamId::FilterEnvAmount, "ENV\nAMOUNT", Nums::None),
            (ParamId::FilterVowel, "VOWEL", N10R),
        ];
        for (i, (id, label, nums)) in faders.into_iter().enumerate() {
            self.fader(id, 1060.0 + i as f32 * 66.0, l, sl, label, nums, FocusKind::Plain);
        }
        let (rl, _) = row(1);
        let lines = [
            ("TYPE", self.value_text(ParamId::FilterType).to_uppercase()),
            ("CUTOFF", self.value_text(ParamId::FilterCutoff)),
            ("RESONANCE", self.value_text(ParamId::FilterRes)),
            ("ENV AMOUNT", self.value_text(ParamId::FilterEnvAmount)),
        ];
        for (k, (name, value)) in lines.iter().enumerate() {
            let y = rl - 4.0 + k as f32 * 26.0;
            self.text(cv.p(1046.0, y), Align2::LEFT_CENTER, name, 10.0, self.th.legend);
            self.pt.text(cv.p(1380.0, y), Align2::RIGHT_CENTER, value, cv.mono(13.0), self.accent);
        }
        self.text(
            cv.p(1046.0, rl + 106.0),
            Align2::LEFT_CENTER,
            "DRAG THE CURVE: SIDEWAYS = CUTOFF, UP/DOWN = RESONANCE",
            8.0,
            self.th.legend,
        );
    }

    // --------------------------------------------------------------- MATRIX

    pub(crate) fn matrix_tab(&mut self) {
        let cv = self.cv;
        self.bar(BAR_Y[0], &[(16.0, 336.0, "SOURCES", Section::Matrix), (336.0, BW - 12.0, "MODULATION MATRIX", Section::Matrix)]);
        self.accent = self.th.accents[Section::Matrix as usize];

        // Palette: every source as a draggable chip, grouped by category.
        let mut y = BAR_Y[0] + 30.0;
        let mut last_cat = "";
        let mut col = 0;
        for src in palette_sources(self.st.cc_pick) {
            let cat = mod_source_category(src);
            if cat != last_cat {
                if col == 1 {
                    y += 20.0;
                }
                col = 0;
                self.text(cv.p(26.0, y + 7.0), Align2::LEFT_CENTER, &cat.to_uppercase(), 8.0, self.th.legend);
                y += 14.0;
                last_cat = cat;
            }
            let rect = cv.r(26.0 + col as f32 * 152.0, y, 146.0, 17.0);
            self.source_chip(src, rect);
            col += 1;
            if col == 2 {
                col = 0;
                y += 20.0;
            }
        }
        // CC picker for the last chip.
        let y = BAR_Y[0] + 348.0;
        self.text(cv.p(26.0, y + 9.0), Align2::LEFT_CENTER, "CC CHIP", 9.0, self.th.text);
        if self.button("cc:down", 96.0, y, 22.0, "-", false, "CC NUMBER").clicked() {
            self.st.cc_pick = (self.st.cc_pick.max(2) - 1).max(1);
        }
        self.pt.text(cv.p(141.0, y + 9.0), Align2::CENTER_CENTER, format!("CC {}", self.st.cc_pick), cv.mono(9.0), self.accent);
        if self.button("cc:up", 164.0, y, 22.0, "+", false, "CC NUMBER").clicked() {
            self.st.cc_pick = (self.st.cc_pick + 1).min(119);
        }
        if self.button("matrix:add", 26.0, BAR_Y[0] + 374.0, 140.0, "+ ADD ROW", false, "NEW ROW: LFO 1 -> CUTOFF").clicked() {
            let target = crate::dsp::mod_matrix::mod_target_index_for_param(ParamId::FilterCutoff);
            self.st.shared.edit_matrix(true, |m| m.add_row(ModSource::Lfo1.index(), target, 0.5, ModCurve::Linear));
        }
        if self.button("matrix:clear", 176.0, BAR_Y[0] + 374.0, 140.0, "CLEAR ALL", false, "REMOVE EVERY ROW").clicked() {
            self.st.shared.edit_matrix(true, |m| m.clear());
        }

        self.matrix_rows();
    }

    fn matrix_rows(&mut self) {
        let cv = self.cv;
        let s = cv.s;
        let rows = self.st.shared.matrix.lock().map(|m| m.rows().to_vec()).unwrap_or_default();
        let compiled: Vec<Option<usize>> = {
            let m = self.st.shared.matrix.lock().unwrap_or_else(|e| e.into_inner());
            (0..rows.len()).map(|i| m.compiled_index_for_row(i)).collect()
        };
        let top = BAR_Y[0] + 32.0;
        for (x, h) in [(370.0, "SOURCE"), (566.0, "TARGET"), (800.0, "AMOUNT"), (1052.0, "CURVE"), (1130.0, "MUTE")] {
            self.text(cv.p(x, top + 6.0), Align2::LEFT_CENTER, h, 8.5, self.th.legend);
        }
        if rows.is_empty() {
            self.text(
                cv.p(860.0, top + 170.0),
                Align2::CENTER_CENTER,
                "DRAG A SOURCE CHIP ONTO ANY FADER, GRAPH OR DISPLAY - OR RIGHT-CLICK A CONTROL",
                10.0,
                self.th.legend,
            );
            return;
        }
        let row_h = 21.0;
        for (i, r) in rows.iter().enumerate() {
            let y = top + 16.0 + i as f32 * row_h;
            let rect = cv.r(346.0, y, BW - 362.0, row_h - 2.0);
            self.pt.rect_filled(rect, cv.round(2.0), self.th.slot.gamma_multiply(0.5));
            let ink = if r.unresolved { self.th.warning } else if r.muted { self.th.step_off } else { self.th.text };

            // Reorder handle.
            let handle = cv.r(350.0, y + 2.0, 12.0, row_h - 6.0);
            let hresp = self.ui.interact(handle, Id::new(("row-handle", i)), Sense::drag());
            self.register(&format!("row:{i}:handle"), ControlKind::Button, handle);
            for k in 0..3 {
                let yy = handle.top() + (4.0 + k as f32 * 4.0) * s;
                self.pt.line_segment([pos2(handle.left(), yy), pos2(handle.right(), yy)], Stroke::new(1.0 * s, self.th.legend));
            }
            if hresp.drag_started() {
                self.st.row_drag = Some((i, i));
            }
            if hresp.dragged() {
                let dy = self.drag_offset().y / (row_h * s);
                let to = (i as f32 + dy).round().clamp(0.0, (rows.len() - 1) as f32) as usize;
                self.st.row_drag = Some((i, to));
            }
            if hresp.drag_stopped() {
                if let Some((from, to)) = self.st.row_drag.take() {
                    if from != to {
                        self.st.shared.edit_matrix(true, |m| m.move_row(from, to));
                    }
                }
            }

            let src_name = if r.unresolved && !r.source_id_raw.is_empty() {
                r.source_id_raw.clone()
            } else {
                mod_source_display_name(r.source)
            };
            if self.cell(&format!("row:{i}:source"), 368.0, y, 180.0, &src_name.to_uppercase(), ink) {
                self.st.popup = Some(Popup::RowSource { row: i, at: pos2(368.0, y + row_h) });
            }
            self.text(cv.p(556.0, y + 9.5), Align2::CENTER_CENTER, ">", 10.0, self.th.legend);
            let tgt_name = match r.target {
                Some(t) if !r.unresolved => mod_target_display_name(t),
                _ => r.target_id_raw.clone(),
            };
            if self.cell(&format!("row:{i}:target"), 566.0, y, 220.0, &tgt_name.to_uppercase(), ink) {
                self.st.popup = Some(Popup::RowTarget { row: i, at: pos2(566.0, y + row_h) });
            }
            self.amount_bar(&format!("row:{i}:amount"), i, r.amount, 800.0, y + 3.0, 240.0);
            // Curve: click cycles through the four shapes.
            let curve_rect = cv.r(1052.0, y + 1.0, 60.0, row_h - 4.0);
            let cresp = self.ui.interact(curve_rect, Id::new(("row-curve", i)), Sense::click());
            self.register(&format!("row:{i}:curve"), ControlKind::Button, curve_rect);
            self.pt.rect_filled(curve_rect, cv.round(2.0), if cresp.hovered() { self.th.button_hover } else { self.th.button });
            self.curve_icon(curve_rect.shrink2(vec2(6.0, 4.0) * s), r.curve, self.accent);
            if cresp.clicked() {
                let next = ModCurve::ALL[(r.curve as usize + 1) % ModCurve::ALL.len()];
                self.st.shared.edit_matrix(false, |m| m.set_curve(i, next));
            }
            if cresp.hovered() {
                self.set_focus("CURVE", r.curve.name().to_uppercase(), FocusKind::Plain);
            }
            let mute = self.button(&format!("row:{i}:mute"), 1130.0, y + 1.0, 44.0, if r.muted { "MUTED" } else { "ON" }, r.muted, "MUTE ROW");
            if mute.clicked() {
                self.st.shared.edit_matrix(false, |m| m.set_muted(i, !r.muted));
            }
            let activity = compiled[i]
                .and_then(|c| self.st.shared.feedback.row_activity.get(c))
                .map(|a| a.load(Relaxed))
                .unwrap_or(0.0);
            self.led(cv.p(1196.0, y + 10.0), activity.clamp(0.0, 1.0));
            if self.button(&format!("row:{i}:delete"), 1352.0, y + 1.0, 24.0, "X", false, "DELETE ROW").clicked() {
                self.st.shared.edit_matrix(true, |m| m.remove_row(i));
            }
            if r.unresolved {
                self.text(cv.p(1216.0, y + 9.5), Align2::LEFT_CENTER, "UNRESOLVED", 8.0, self.th.warning);
            }
        }
        if let Some((from, to)) = self.st.row_drag {
            if from != to {
                let y = top + 16.0 + to as f32 * row_h - 1.0;
                self.pt.line_segment([cv.p(346.0, y), cv.p(BW - 16.0, y)], cv.stroke(2.0, self.th.modulation));
            }
        }
    }

    /// Clickable text cell of a matrix row.
    fn cell(&mut self, key: &str, x: f32, y: f32, w: f32, text: &str, ink: nih_plug_egui::egui::Color32) -> bool {
        let cv = self.cv;
        let rect = cv.r(x, y + 1.0, w, 17.0);
        let resp = self.ui.interact(rect, Id::new(("cell", key)), Sense::click());
        self.register(key, ControlKind::Button, rect);
        self.pt.rect_filled(rect, cv.round(2.0), if resp.hovered() { self.th.button_hover } else { self.th.button });
        self.pt.text(rect.left_center() + vec2(6.0 * cv.s, 0.0), Align2::LEFT_CENTER, text, cv.mono(8.5), ink);
        resp.clicked()
    }

    // ------------------------------------------------------------------- FX

    pub(crate) fn fx_tab(&mut self) {
        self.bar(
            BAR_Y[0],
            &[
                (16.0, 250.0, "DISTORTION", Section::Fx),
                (250.0, 484.0, "CHORUS", Section::Fx),
                (484.0, 968.0, "DELAY", Section::Fx),
                (968.0, BW - 12.0, "REVERB", Section::Fx),
            ],
        );
        self.bar(BAR_Y[1], &[(16.0, BW - 12.0, "SPECTRUM", Section::Master)]);
        self.accent = self.th.accents[Section::Fx as usize];
        let (l, sl) = row(0);
        for (x0, first) in [(16.0, ParamId::FxDistOn), (250.0, ParamId::FxChorusOn)] {
            let p = |o: usize| ParamId::from_index(first.index() + o);
            let labels: [&str; 3] = if first == ParamId::FxDistOn { ["DRIVE", "TONE", "MIX"] } else { ["RATE", "DEPTH", "MIX"] };
            self.switch(p(0), x0 + 34.0, l, "ON");
            self.fader(p(1), x0 + 90.0, l, sl, labels[0], N10L, FocusKind::Plain);
            self.fader(p(2), x0 + 136.0, l, sl, labels[1], Nums::None, FocusKind::Plain);
            self.fader(p(3), x0 + 182.0, l, sl, labels[2], N10R, FocusKind::Plain);
        }
        let x0 = 484.0;
        self.switch(ParamId::FxDelayOn, x0 + 34.0, l, "ON");
        self.stepper(ParamId::FxDelaySyncMode, x0 + 104.0, l, sl, "SYNC", Steps::Text(&["FREE", "TEMPO"]));
        self.selector(ParamId::FxDelaySyncDiv, x0 + 124.0, sl.0 + 44.0, 64.0, "DIV");
        self.fader(ParamId::FxDelayTimeMs, x0 + 226.0, l, sl, "TIME", N10L, FocusKind::Plain);
        self.fader(ParamId::FxDelayFeedback, x0 + 272.0, l, sl, "FEED-\nBACK", Nums::None, FocusKind::Plain);
        self.fader(ParamId::FxDelayTone, x0 + 318.0, l, sl, "TONE", Nums::None, FocusKind::Plain);
        self.fader(ParamId::FxDelayMix, x0 + 364.0, l, sl, "MIX", Nums::None, FocusKind::Plain);
        self.switch(ParamId::FxDelayPingPong, x0 + 430.0, l, "PING\nPONG");
        let x0 = 968.0;
        self.switch(ParamId::FxRevOn, x0 + 34.0, l, "ON");
        self.fader(ParamId::FxRevSize, x0 + 90.0, l, sl, "SIZE", N10L, FocusKind::Plain);
        self.fader(ParamId::FxRevDamp, x0 + 136.0, l, sl, "DAMP", Nums::None, FocusKind::Plain);
        self.fader(ParamId::FxRevPreDelay, x0 + 182.0, l, sl, "PRE\nDELAY", Nums::None, FocusKind::Plain);
        self.fader(ParamId::FxRevMix, x0 + 228.0, l, sl, "MIX", N10R, FocusKind::Plain);
        self.accent = self.th.accents[Section::Master as usize];
        let r = self.cv.r(24.0, BAR_Y[1] + 32.0, BW - 48.0, 156.0);
        self.spectrum_view(r);
    }

    // ---------------------------------------------------------- bottom half

    /// The always-visible bottom half, laid out after Serum 2: an envelope
    /// and an LFO panel, each with tabs that double as drag handles for their
    /// source, large graphs and a knob row; the master section with the
    /// scope; the performance sources along the bottom edge.
    pub(crate) fn bottom_half(&mut self) {
        let th = self.th;
        let y = BAR_Y[2];
        let lfo_x = 16.0 + MOD_W + 8.0;
        let master_x = lfo_x + MOD_W + 8.0;
        self.accent = th.accents[Section::Env as usize];
        self.env_panel(16.0, y);
        self.accent = th.accents[Section::Lfo as usize];
        self.lfo_panel(lfo_x, y);
        self.accent = th.accents[Section::Master as usize];
        self.master_panel(master_x, y);
        self.accent = th.accents[Section::Matrix as usize];
        self.quick_sources(y + 362.0);
    }

    /// Modulator tabs: click a name to show that modulator, drag the round
    /// handle onto a control to modulate it. Returns the selected page.
    fn mod_tabs(&mut self, x0: f32, y: f32, key: &str, page: usize, tabs: [(&str, ModSource); 2], section: Section) -> usize {
        let cv = self.cv;
        let th = self.th;
        let w = MOD_W / 2.0;
        let mut page = page;
        for (k, (name, src)) in tabs.into_iter().enumerate() {
            let rect = cv.r(x0 + k as f32 * w + 1.5, y, w - 3.0, 22.0);
            let tab_key = format!("modtab:{key}:{k}");
            let resp = self.ui.interact(rect, Id::new(&tab_key), Sense::click());
            self.register(&tab_key, ControlKind::Button, rect);
            if resp.clicked() {
                page = k;
            }
            let selected = page == k;
            let fill = if selected {
                th.bars[section as usize]
            } else if resp.hovered() {
                th.button_hover
            } else {
                th.button
            };
            self.pt.rect_filled(rect, cv.round(3.0), fill);
            let ink = if selected { th.bar_text } else { th.text };
            self.text(rect.left_center() + vec2(10.0 * cv.s, 0.5 * cv.s), Align2::LEFT_CENTER, name, 13.0, ink);
            self.source_handle(src.index(), rect.right_center() - vec2(14.0 * cv.s, 0.0), ink);
        }
        page
    }

    /// Panel behind a knob row.
    fn knob_panel(&self, x0: f32, w: f32) {
        let cv = self.cv;
        self.pt.rect_filled(cv.r(x0, KNOB_ROW - 28.0, w, 78.0), cv.round(4.0), self.th.slot.gamma_multiply(0.35));
    }

    /// Label under a non-knob control in a knob row.
    fn row_label(&self, cx: f32, text: &str) {
        self.text(self.cv.p(cx, KNOB_ROW + 30.0), Align2::CENTER_CENTER, text, 9.5, self.th.text);
    }

    fn env_panel(&mut self, x0: f32, y: f32) {
        let tabs = [("AMP ENV", ModSource::EnvAmp), ("MOD ENV", ModSource::EnvMod)];
        let page = self.st.env_page;
        self.st.env_page = self.mod_tabs(x0, y, "env", page, tabs, Section::Env);
        let which = self.st.env_page;
        self.env_graph(which, self.cv.r(x0 + 8.0, y + 30.0, MOD_W - 16.0, GRAPH_H));

        let first = if which == 0 { ParamId::EnvAmpAttack } else { ParamId::EnvModAttack };
        let p = |o: usize| ParamId::from_index(first.index() + o);
        self.knob_panel(x0, MOD_W);
        let knobs = ["ATTACK", "DECAY", "SUSTAIN", "RELEASE", "A CURVE", "D CURVE", "R CURVE", "VELOCITY"];
        for (o, label) in knobs.into_iter().enumerate() {
            self.knob(p(o), x0 + 36.0 + o as f32 * 62.0, KNOB_ROW, 15.0, label);
        }
    }

    fn lfo_panel(&mut self, x0: f32, y: f32) {
        let cv = self.cv;
        let tabs = [("LFO 1", ModSource::Lfo1), ("LFO 2", ModSource::Lfo2)];
        let page = self.st.lfo_page;
        self.st.lfo_page = self.mod_tabs(x0, y, "lfo", page, tabs, Section::Lfo);
        let which = self.st.lfo_page;
        let first = if which == 0 { ParamId::Lfo1Rate } else { ParamId::Lfo2Rate };
        let p = |o: usize| ParamId::from_index(first.index() + o);

        self.text(cv.p(x0 + 10.0, y + 38.0), Align2::LEFT_CENTER, "TRIGGER", 9.0, self.th.legend);
        self.segments(p(4), x0 + 8.0, y + 46.0, 88.0, 22.0, 1, Steps::Text(&["FREE", "VOICE", "NOTE"]));
        self.text(cv.p(x0 + 10.0, y + 126.0), Align2::LEFT_CENTER, "SHAPE", 9.0, self.th.legend);
        self.segments(p(1), x0 + 8.0, y + 134.0, 88.0, 26.0, 2, Steps::Lfo);
        self.lfo_graph(which, cv.r(x0 + 104.0, y + 30.0, MOD_W - 112.0, GRAPH_H));

        self.knob_panel(x0, MOD_W);
        let tempo = self.plain(p(2)) > 0.5;
        self.segments(p(2), x0 + 12.0, KNOB_ROW - 10.0, 80.0, 20.0, 2, Steps::Text(&["HZ", "BPM"]));
        self.row_label(x0 + 52.0, "SYNC");
        self.knob(p(0), x0 + 140.0, KNOB_ROW, 15.0, "RATE");
        self.selector(p(3), x0 + 184.0, KNOB_ROW - 10.0, 96.0, "");
        self.row_label(x0 + 232.0, "DIVISION");
        // The control that does not apply in the current sync mode is dimmed.
        let off = if tempo { cv.r(x0 + 114.0, KNOB_ROW - 26.0, 52.0, 66.0) } else { cv.r(x0 + 182.0, KNOB_ROW - 12.0, 100.0, 52.0) };
        self.pt.rect_filled(off, cv.round(3.0), self.th.panel.gamma_multiply(0.6));
        self.knob(p(5), x0 + 340.0, KNOB_ROW, 15.0, "PHASE");
        self.knob(p(6), x0 + 420.0, KNOB_ROW, 15.0, "SMOOTH");
    }

    fn master_panel(&mut self, x0: f32, y: f32) {
        let cv = self.cv;
        let w = BW - 12.0 - x0;
        self.bar(y, &[(x0, x0 + w, "MASTER", Section::Master)]);
        self.scope(cv.r(x0 + 10.0, y + 33.0, w - 20.0, 132.0));
        self.field(ParamId::MasterPolyphony, x0 + 8.0, y + 178.0, (w - 24.0) / 2.0, "VOICES");
        self.field(ParamId::MasterBendRange, x0 + 16.0 + (w - 24.0) / 2.0, y + 178.0, (w - 24.0) / 2.0, "BEND");
        self.text(cv.p(x0 + 10.0, y + 218.0), Align2::LEFT_CENTER, "OVERSAMPLING", 9.0, self.th.legend);
        self.segments(ParamId::MasterOversampling, x0 + 112.0, y + 208.0, w - 120.0, 20.0, 3, Steps::Text(&["1X", "2X", "4X"]));

        self.knob_panel(x0, w);
        let knobs = [(ParamId::MasterVolume, "VOLUME"), (ParamId::MasterPan, "PAN"), (ParamId::MasterTune, "TUNE"), (ParamId::MasterGlide, "GLIDE")];
        for (k, (id, label)) in knobs.into_iter().enumerate() {
            self.knob(id, x0 + w * (0.125 + 0.25 * k as f32), KNOB_ROW, 15.0, label);
        }
    }

    /// Performance sources as chips along the bottom edge (the envelope and
    /// LFO handles sit on their tabs).
    fn quick_sources(&mut self, y: f32) {
        let cv = self.cv;
        let skip = [ModSource::EnvAmp.index(), ModSource::EnvMod.index(), ModSource::Lfo1.index(), ModSource::Lfo2.index()];
        let sources: Vec<usize> = palette_sources(self.st.cc_pick)
            .into_iter()
            .filter(|s| !skip.contains(s))
            .chain([cc_source_index(74)])
            .take(14)
            .collect();
        let mut seen = Vec::new();
        let sources: Vec<usize> = sources.into_iter().filter(|s| if seen.contains(s) { false } else { seen.push(*s); true }).collect();
        self.text(cv.p(20.0, y + 21.0), Align2::LEFT_CENTER, "SOURCES", 10.0, self.th.text);
        let per_row = sources.len().div_ceil(2).max(1);
        let (x0, x1) = (96.0, BW - 12.0);
        let cell = (x1 - x0) / per_row as f32;
        for (k, src) in sources.into_iter().enumerate() {
            let rect = cv.r(x0 + (k % per_row) as f32 * cell, y + (k / per_row) as f32 * 22.0, cell - 6.0, 18.0);
            self.source_chip(src, rect);
        }
    }
}

/// Straight segment between two (sample, value) points, inclusive.
fn draw_segment(data: &mut [f32], a: (usize, f32), b: (usize, f32)) {
    let (lo, hi) = if a.0 <= b.0 { (a, b) } else { (b, a) };
    let span = (hi.0 - lo.0).max(1) as f32;
    for i in lo.0..=hi.0.min(data.len() - 1) {
        let t = (i - lo.0) as f32 / span;
        data[i] = lo.1 + (hi.1 - lo.1) * t;
    }
}

/// Three-tap smoothing over a window around `centre`, wrapping at the frame edge.
fn smooth_around(data: &mut [f32], centre: usize, radius: usize) {
    let n = data.len();
    let src: Vec<f32> = data.to_vec();
    for k in 0..=2 * radius {
        let i = (centre + n + k - radius) % n;
        let w = 1.0 - (k as f32 - radius as f32).abs() / (radius as f32 + 1.0);
        let avg = (src[(i + n - 2) % n] + src[(i + n - 1) % n] + src[i] + src[(i + 1) % n] + src[(i + 2) % n]) / 5.0;
        data[i] = src[i] + (avg - src[i]) * w;
    }
}

fn short(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_uppercase())
        .unwrap_or_default()
}
