//! Graphs with drag handles (envelopes, filter response), live displays
//! (wavetable, LFOs, scope, spectrum), in the style of the fors Pivot synth:
//! filled shapes, square handles, the live state riding on top.

use nih_plug_egui::egui::{pos2, vec2, Align2, CornerRadius, Id, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};
use std::sync::atomic::Ordering::Relaxed;

use super::widgets::{fill_under, graph_bg, lfo_shape, sample_curve, G};
use super::{ControlKind, FocusKind};
use crate::dsp::fft::Fft;
use crate::dsp::filters::MultiFilter;
use crate::dsp::params::{FilterType, ParamId};
use crate::dsp::shared::AnalyzerRing;
use crate::dsp::wavetable::Wavetable;

const F_MIN: f32 = 20.0;
const F_MAX: f32 = 20_000.0;
const ANALYZER_SIZE: usize = 2048;
/// Frames drawn at most in the 3D wavetable view, and points per frame.
const WAVE_SLICES: usize = 48;
const WAVE_POINTS: usize = 128;

/// GUI-side buffers for the analyser and scope, reused every frame.
pub struct Scratch {
    fft: Fft,
    samples: Vec<f32>,
    wet: Vec<f32>,
    dry: Vec<f32>,
    /// Smoothed magnitudes in dB, so the spectrum moves calmly.
    wet_db: Vec<f32>,
    dry_db: Vec<f32>,
    /// Display frames of each oscillator's table for the 3D view, keyed on
    /// the table's content hash so they are only rebuilt when it changes.
    slices: [(Option<u64>, Vec<Vec<f32>>); 2],
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            fft: Fft::new(ANALYZER_SIZE),
            samples: vec![0.0; ANALYZER_SIZE],
            wet: Vec::new(),
            dry: Vec::new(),
            wet_db: vec![-120.0; ANALYZER_SIZE / 2],
            dry_db: vec![-120.0; ANALYZER_SIZE / 2],
            slices: Default::default(),
        }
    }
}

/// The four parameters of one envelope, plus its curves.
struct EnvIds {
    attack: ParamId,
    decay: ParamId,
    sustain: ParamId,
    release: ParamId,
    curves: [ParamId; 3],
}

fn env_ids(which: usize) -> EnvIds {
    let first = if which == 0 { ParamId::EnvAmpAttack } else { ParamId::EnvModAttack };
    let p = |o: usize| first.offset(o);
    EnvIds {
        attack: p(0),
        decay: p(1),
        sustain: p(2),
        release: p(3),
        curves: [p(4), p(5), p(6)],
    }
}

impl G<'_, '_> {
    fn handle_square(&self, c: Pos2, active: bool) {
        let s = self.cv.s;
        let r = Rect::from_center_size(c, vec2(9.0, 9.0) * s);
        let round = CornerRadius::same((1.5 * s) as u8);
        let fill = if active { self.th.drag_line } else { self.th.text };
        self.pt.rect_filled(r, round, fill);
        self.pt
            .rect_stroke(r, round, Stroke::new(1.2 * s, self.th.slot), StrokeKind::Outside);
    }

    /// Drag handle for one or two parameters on a graph: horizontal movement
    /// changes `x` by one full range per `span.x` screen pixels, vertical
    /// movement `y` per `span.y`. Double-click resets, the wheel moves `x`.
    /// Returns whether it is hovered or dragged.
    pub(crate) fn xy_handle(&mut self, hit: Rect, x: ParamId, y: Option<ParamId>, span: Vec2, kind: ControlKind) -> bool {
        let eid = Id::new(("xy", x));
        let resp = self.ui.interact(hit, eid, Sense::click_and_drag());
        self.register(&x.desc().id_string, kind, hit);
        self.mod_target(x, hit, &resp);
        if resp.drag_started() {
            let start = (self.shown(x), y.map(|y| self.shown(y)).unwrap_or(0.0));
            self.ui.data_mut(|d| d.insert_temp(eid, start));
            self.begin(x);
            if let Some(y) = y {
                self.begin(y);
            }
        }
        if resp.dragged() {
            let d = self.drag_offset();
            let current = (self.shown(x), y.map(|y| self.shown(y)).unwrap_or(0.0));
            let (x0, y0): (f32, f32) = self.ui.data(|dm| dm.get_temp(eid)).unwrap_or(current);
            self.send(x, x0 + d.x / span.x);
            if let Some(y) = y {
                self.send(y, y0 - d.y / span.y);
            }
        }
        if resp.drag_stopped() {
            self.end(x);
            if let Some(y) = y {
                self.end(y);
            }
        }
        if resp.double_clicked() {
            let def = self.p.get(x).default_normalized();
            self.change(x, def);
            if let Some(y) = y {
                let def = self.p.get(y).default_normalized();
                self.change(y, def);
            }
        }
        if resp.hovered() && !resp.dragged() {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                let v = self.shown(x) + scroll * 0.0015;
                self.change(x, v);
            }
        }
        let active = resp.hovered() || resp.dragged();
        if active {
            let dx = x.desc();
            let (name, value) = match y {
                Some(y) => (
                    format!("{} {} / {}", dx.module, dx.name, y.desc().name),
                    format!("{} / {}", self.value_text(x), self.value_text(y)),
                ),
                None => (format!("{} {}", dx.module, dx.name), self.value_text(x)),
            };
            self.set_focus(&name, value, FocusKind::Plain);
        }
        active
    }

    /// Envelope as a filled shape with three handles: attack time (top),
    /// decay time and sustain level (corner), release time (end). Segment
    /// shapes follow the curve parameters exactly as `envelope.rs` does, and a
    /// dot rides along with the newest voice.
    pub(crate) fn env_graph(&mut self, which: usize, rect: Rect) {
        let s = self.cv.s;
        let ids = env_ids(which);
        graph_bg(self, rect);
        let inner = rect.shrink2(vec2(8.0, 8.0) * s);
        let (w, h) = (inner.width(), inner.height());
        let zone = 0.3 * w;
        let sus_end = inner.left() + 0.66 * w;
        let rel_zone = inner.right() - sus_end;
        let y_of = |l: f32| inner.bottom() - l * h;
        let hit = |c: Pos2| Rect::from_center_size(c, vec2(18.0, 18.0) * s);

        let geometry = |g: &mut Self| {
            let xa = inner.left() + g.shown(ids.attack) * zone;
            let xd = xa + g.shown(ids.decay) * zone;
            let xr = sus_end + g.shown(ids.release) * rel_zone;
            (xa, xd, xr, g.plain(ids.sustain))
        };
        let (xa, xd, xr, sus) = geometry(self);
        // Release first so attack and decay win where they overlap.
        let a_r = self.xy_handle(hit(pos2(xr, y_of(0.0))), ids.release, None, vec2(rel_zone, 1.0), ControlKind::Handle);
        let a_a = self.xy_handle(hit(pos2(xa, y_of(1.0))), ids.attack, None, vec2(zone, 1.0), ControlKind::Handle);
        let a_d = self.xy_handle(hit(pos2(xd, y_of(sus))), ids.decay, Some(ids.sustain), vec2(zone, h), ControlKind::Handle);
        let (xa, xd, xr, sus) = geometry(self);

        let exp = |g: &mut Self, id: ParamId, sign: f32| (sign * 2.0 * g.plain(id)).exp2();
        let (ea, ed, er) = (exp(self, ids.curves[0], 1.0), exp(self, ids.curves[1], -1.0), exp(self, ids.curves[2], -1.0));
        let shape = |t: f32, e: f32| t.clamp(0.0, 1.0).powf(e);
        let n = 24;
        let mut pts = Vec::with_capacity(4 * n + 4);
        for k in 0..=n {
            let u = k as f32 / n as f32;
            pts.push(pos2(inner.left() + u * (xa - inner.left()), y_of(shape(u, ea))));
        }
        for k in 1..=n {
            let u = k as f32 / n as f32;
            pts.push(pos2(xa + u * (xd - xa), y_of(1.0 + (sus - 1.0) * shape(u, ed))));
        }
        pts.push(pos2(sus_end, y_of(sus)));
        for k in 1..=n {
            let u = k as f32 / n as f32;
            pts.push(pos2(sus_end + u * (xr - sus_end), y_of(sus * (1.0 - shape(u, er)))));
        }
        pts.push(pos2(inner.right(), y_of(0.0)));

        fill_under(&self.pt, &pts, inner.bottom(), self.accent.gamma_multiply(0.55));
        for (x, l) in [(xa, 1.0), (xd, sus), (sus_end, sus)] {
            self.pt.line_segment(
                [pos2(x, y_of(l)), pos2(x, inner.bottom())],
                Stroke::new(2.0 * s, self.th.slot.gamma_multiply(0.8)),
            );
        }
        self.pt.add(Shape::line(pts, Stroke::new(1.6 * s, self.accent)));

        let fb = &self.st.shared.feedback;
        let stage = fb.env_stage[which].load(Relaxed);
        if stage != 0 {
            let level = fb.env_level[which].load(Relaxed).clamp(0.0, 1.0);
            // Place the dot from the stage plus how far the level has travelled within it.
            let x = match stage {
                1 => inner.left() + (xa - inner.left()) * level,
                2 => xa + (xd - xa) * ((1.0 - level) / (1.0 - sus).max(1.0e-3)).clamp(0.0, 1.0),
                3 => (xd + sus_end) * 0.5,
                _ => sus_end + (xr - sus_end) * (1.0 - (level / sus.max(1.0e-3)).clamp(0.0, 1.0)),
            };
            let at = pos2(x, y_of(level));
            self.pt.circle_filled(at, 4.0 * s, self.th.drag_line);
            self.pt.circle_stroke(at, 4.0 * s, Stroke::new(1.2 * s, self.th.slot));
        }
        self.handle_square(pos2(xr, y_of(0.0)), a_r);
        self.handle_square(pos2(xa, y_of(1.0)), a_a);
        self.handle_square(pos2(xd, y_of(sus)), a_d);
    }

    /// Two cycles of the LFO shape (the drawn shape for Custom), shifted by
    /// the phase parameter, with the live output as a dot on the right.
    pub(crate) fn lfo_graph(&mut self, which: usize, rect: Rect) {
        let s = self.cv.s;
        let (shape_id, phase_id) = if which == 0 {
            (ParamId::Lfo1Shape, ParamId::Lfo1Phase)
        } else {
            (ParamId::Lfo2Shape, ParamId::Lfo2Phase)
        };
        graph_bg(self, rect);
        let inner = rect.shrink2(vec2(8.0, 10.0) * s);
        let shape = self.plain(shape_id).round() as usize;
        let offset = self.plain(phase_id) / 360.0;
        let custom = self.st.shared.lfo_shape(which);
        let f = |ph: f32| {
            let ph = (ph + offset).fract();
            match (&custom, shape) {
                (Some(points), 6) if points.len() >= 2 => {
                    let pos = ph * (points.len() - 1) as f32;
                    let i = (pos as usize).min(points.len() - 2);
                    points[i] + (points[i + 1] - points[i]) * (pos - i as f32)
                }
                _ => lfo_shape(shape, ph),
            }
        };
        let mid = inner.center().y;
        self.pt.line_segment(
            [pos2(inner.left(), mid), pos2(inner.right(), mid)],
            Stroke::new(0.8 * s, self.th.scale),
        );
        let pts = sample_curve(inner, 2.0, f);
        fill_under(&self.pt, &pts, mid, self.accent.gamma_multiply(0.2));
        self.pt.add(Shape::line(pts, Stroke::new(1.6 * s, self.accent)));
        let live = self.st.shared.feedback.lfo_value[which].load(Relaxed).clamp(-1.0, 1.0);
        let at = pos2(inner.right() + 4.0 * s, mid - live * inner.height() * 0.5);
        self.pt.circle_filled(at, 3.5 * s, self.th.drag_line);
        self.pt.circle_stroke(at, 3.5 * s, Stroke::new(1.0 * s, self.th.slot));
    }

    /// Filter response with a cutoff / resonance handle: drag sideways for
    /// cutoff on a log axis, up and down for resonance. Behind it the output
    /// spectrum (filled) and the pre-filter spectrum (line); while notes sound
    /// a second trace shows the cutoff the newest voice actually gets.
    pub(crate) fn filter_graph(&mut self, rect: Rect) {
        let s = self.cv.s;
        let inner = rect.shrink2(vec2(10.0, 12.0) * s);
        let span = (F_MAX / F_MIN).ln();
        let t_of = |hz: f32| (hz.max(F_MIN) / F_MIN).ln() / span;
        let hz_of = |t: f32| F_MIN * (t.clamp(0.0, 1.0) * span).exp();
        let cutoff_id = ParamId::FilterCutoff;
        let res_id = ParamId::FilterRes;

        let eid = Id::new("filter-graph");
        let resp = self.ui.interact(rect, eid, Sense::click_and_drag());
        self.register(&cutoff_id.desc().id_string, ControlKind::Graph, rect);
        self.mod_target(cutoff_id, rect, &resp);
        if resp.drag_started() {
            let start = (t_of(self.plain(cutoff_id)), self.shown(res_id));
            self.ui.data_mut(|d| d.insert_temp(eid, start));
            self.begin(cutoff_id);
            self.begin(res_id);
        }
        if resp.dragged() {
            let d = self.drag_offset();
            let current = (t_of(self.plain(cutoff_id)), self.shown(res_id));
            let (t0, r0): (f32, f32) = self.ui.data(|dm| dm.get_temp(eid)).unwrap_or(current);
            let t = (t0 + d.x / inner.width()).clamp(0.0, 1.0);
            let r = (r0 - d.y / inner.height()).clamp(0.0, 1.0);
            let n = cutoff_id.desc().to_normalized(hz_of(t));
            self.send(cutoff_id, n);
            self.send(res_id, r);
        }
        if resp.drag_stopped() {
            self.end(cutoff_id);
            self.end(res_id);
        }
        if resp.double_clicked() {
            for id in [cutoff_id, res_id] {
                let def = self.p.get(id).default_normalized();
                self.change(id, def);
            }
        }
        if resp.hovered() && !resp.dragged() {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                let t = t_of(self.plain(cutoff_id)) + scroll * 0.0008;
                self.change_plain(cutoff_id, hz_of(t));
            }
        }
        let active = resp.hovered() || resp.dragged();
        if active {
            let v = format!("{} / {}", self.value_text(cutoff_id), self.value_text(res_id));
            self.set_focus("FILTER CUTOFF / RESONANCE", v, FocusKind::Plain);
        }

        graph_bg(self, rect);
        for hz in [50.0_f32, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
            let x = inner.left() + inner.width() * t_of(hz);
            let decade = hz == 100.0 || hz == 1000.0 || hz == 10_000.0;
            self.pt.line_segment(
                [pos2(x, inner.top()), pos2(x, inner.bottom())],
                Stroke::new((if decade { 1.0 } else { 0.6 }) * s, self.th.scale),
            );
            if decade {
                let label = if hz >= 1000.0 { format!("{}k", hz / 1000.0) } else { format!("{hz}") };
                self.text(pos2(x + 3.0 * s, inner.bottom() - 2.0 * s), Align2::LEFT_BOTTOM, &label, 8.0, self.th.legend);
            }
        }
        self.spectrum(inner, t_of);

        let ft = FilterType::from_value(self.plain(ParamId::FilterType));
        let cutoff = self.plain(cutoff_id);
        let res = self.plain(res_id);
        let vowel = self.plain(ParamId::FilterVowel);
        let on = self.plain(ParamId::FilterOn) > 0.5;
        let trace = |cutoff: f32| -> Vec<Pos2> {
            (0..160)
                .map(|i| {
                    let t = i as f32 / 159.0;
                    let mag = if on { MultiFilter::magnitude_response(ft, cutoff, res, hz_of(t), vowel) } else { 1.0 };
                    let db = 20.0 * mag.max(1.0e-5).log10();
                    // +24 dB at the top for resonant peaks, -48 dB at the bottom.
                    let y = (1.0 - (db + 48.0) / 72.0).clamp(0.0, 1.0);
                    pos2(inner.left() + inner.width() * t, inner.top() + inner.height() * y)
                })
                .collect()
        };
        let pts = trace(cutoff);
        fill_under(&self.pt, &pts, inner.bottom(), self.accent.gamma_multiply(0.22));
        self.pt.add(Shape::line(pts, Stroke::new(2.0 * s, self.accent)));
        if on && self.st.shared.feedback.voice_count.load(Relaxed) > 0 {
            let live = self.st.shared.feedback.cutoff_hz.load(Relaxed).clamp(F_MIN, F_MAX);
            self.pt.add(Shape::line(trace(live), Stroke::new(1.0 * s, self.th.drag_line.gamma_multiply(0.7))));
        }
        let hx = inner.left() + inner.width() * t_of(cutoff);
        let hy = inner.bottom() - res * inner.height();
        self.pt.line_segment(
            [pos2(hx, inner.top()), pos2(hx, inner.bottom())],
            Stroke::new(1.0 * s, self.accent.gamma_multiply(0.6)),
        );
        self.handle_square(pos2(hx, hy), active);
        // Modulation range of the cutoff as a bar along the top edge.
        if let Some((lo, hi, activity)) = self.mod_extent(cutoff_id) {
            let d = cutoff_id.desc();
            let x_of = |n: f32| inner.left() + inner.width() * t_of(d.from_normalized(n));
            let color = self.th.modulation.gamma_multiply(0.55 + 0.45 * activity.clamp(0.0, 1.0));
            self.pt.rect_filled(
                Rect::from_x_y_ranges(x_of(lo).min(x_of(hi))..=x_of(lo).max(x_of(hi)), inner.top() - 6.0 * s..=inner.top() - 3.0 * s),
                CornerRadius::same(1),
                color,
            );
        }
    }

    /// Analyser behind the filter curve: post-FX output filled, pre-filter
    /// voice mix as a thin line (SPEC 6.2 "input vs output, subtle").
    fn spectrum(&mut self, inner: Rect, t_of: impl Fn(f32) -> f32) {
        if self.st.shared.feedback.voice_count.load(Relaxed) == 0 && self.st.scratch.wet_db[8] < -110.0 {
            return;
        }
        let sr = self.st.shared.feedback.sample_rate.load(Relaxed).max(8000.0);
        let shared = self.st.shared.clone();
        let sc = &mut self.st.scratch;
        for (ring, mags, db) in [
            (&shared.feedback.analyzer_wet, &mut sc.wet, &mut sc.wet_db),
            (&shared.feedback.analyzer_dry, &mut sc.dry, &mut sc.dry_db),
        ] {
            ring_spectrum(ring, &sc.fft, &mut sc.samples, mags, db);
        }
        let bins = ANALYZER_SIZE / 2;
        let curve = |db: &[f32]| -> Vec<Pos2> {
            (1..bins)
                .step_by(2)
                .filter_map(|b| {
                    let hz = b as f32 * sr / ANALYZER_SIZE as f32;
                    if !(F_MIN..=F_MAX).contains(&hz) {
                        return None;
                    }
                    let y = (1.0 - (db[b] + 90.0) / 90.0).clamp(0.0, 1.0);
                    Some(pos2(inner.left() + inner.width() * t_of(hz), inner.top() + inner.height() * y))
                })
                .collect()
        };
        let wet = curve(&self.st.scratch.wet_db);
        let dry = curve(&self.st.scratch.dry_db);
        fill_under(&self.pt, &wet, inner.bottom(), self.th.legend.gamma_multiply(0.18));
        self.pt.add(Shape::line(dry, Stroke::new(0.8 * self.cv.s, self.th.legend.gamma_multiply(0.5))));
    }

    /// Wavetable display of an oscillator (SPEC 6.2, 6.3): every frame of
    /// the table stacked in depth with the current one highlighted, or only
    /// the current frame in 2D. While notes sound it follows the frame
    /// actually being played, so the table breathes with the sound. Drag
    /// sideways or up to scrub the WT position; drop a source to modulate it.
    /// The pencil opens the table in the WT editor.
    pub(crate) fn wave_display(&mut self, osc: usize, rect: Rect) {
        let s = self.cv.s;
        let pos_id = if osc == 0 { ParamId::OscAWtPos } else { ParamId::OscBWtPos };
        let eid = Id::new(("wave", osc));
        let resp = self.ui.interact(rect, eid, Sense::click_and_drag());
        self.register(&format!("wave:{osc}"), ControlKind::Graph, rect);
        self.mod_target(pos_id, rect, &resp);
        if resp.drag_started() {
            let start = self.shown(pos_id);
            self.ui.data_mut(|d| d.insert_temp(eid, start));
            self.begin(pos_id);
        }
        if resp.dragged() {
            let start: f32 = self.ui.data(|d| d.get_temp(eid)).unwrap_or(0.0);
            let d = self.drag_offset();
            self.send(pos_id, start + (d.x - d.y) / rect.width());
        }
        if resp.drag_stopped() {
            self.end(pos_id);
        }
        if resp.double_clicked() {
            let def = self.p.get(pos_id).default_normalized();
            self.change(pos_id, def);
        }
        if resp.hovered() || resp.dragged() {
            let v = self.value_text(pos_id);
            let name = self.st.shared.table(osc).name().to_uppercase();
            self.set_focus(&format!("{name} WT POS"), v, FocusKind::Plain);
        }

        graph_bg(self, rect);
        let table = self.st.shared.table(osc);
        let frames = table.num_frames();
        let playing = self.st.shared.feedback.voice_count.load(Relaxed) > 0;
        let pos = if playing {
            self.st.shared.feedback.display_frame[osc].load(Relaxed)
        } else {
            self.plain(pos_id) / 100.0
        };
        let current = ((pos * (frames.saturating_sub(1)) as f32).round() as usize).min(frames - 1);
        let noise = if osc == 1 { self.plain(ParamId::OscBNoiseMode).round() as usize } else { 0 };
        let inner = rect.shrink2(vec2(14.0, 16.0) * s);
        let caption = if noise > 0 {
            self.noise_view(inner, noise == 2);
            if noise == 2 { "PINK NOISE".to_owned() } else { "WHITE NOISE".to_owned() }
        } else {
            if self.st.wave_flat[osc] || frames < 2 {
                self.wave_flat_view(&table, inner, current);
            } else {
                self.wave_stack_view(osc, &table, inner, current);
            }
            format!("{}  {}/{}", table.name().to_uppercase(), current + 1, frames)
        };
        self.text(rect.left_top() + vec2(8.0, 6.0) * s, Align2::LEFT_TOP, &caption, 9.0, self.th.legend);

        // Position along the bottom edge, with the modulation range.
        let y = rect.bottom() - 6.0 * s;
        let (x0, x1) = (rect.left() + 8.0 * s, rect.right() - 8.0 * s);
        self.pt.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.0 * s, self.th.scale));
        if let Some((lo, hi, activity)) = self.mod_extent(pos_id) {
            let color = self.th.modulation.gamma_multiply(0.55 + 0.45 * activity.clamp(0.0, 1.0));
            self.pt.line_segment([pos2(x0 + (x1 - x0) * lo, y), pos2(x0 + (x1 - x0) * hi, y)], Stroke::new(2.5 * s, color));
        }
        self.pt.circle_filled(pos2(x0 + (x1 - x0) * pos.clamp(0.0, 1.0), y), 3.0 * s, self.th.wave_current);

        if noise == 0 {
            let c = self.cv.to_canvas(rect.right_top());
            let flat = self.st.wave_flat[osc];
            let help = if flat { "SHOW ALL FRAMES" } else { "SHOW THE CURRENT FRAME ONLY" };
            if self.button(&format!("wave-view:{osc}"), c.x - 78.0, c.y + 6.0, 28.0, if flat { "3D" } else { "2D" }, false, help).clicked() {
                self.st.wave_flat[osc] = !flat;
            }
            let edit = self.button(&format!("wave-edit:{osc}"), c.x - 44.0, c.y + 6.0, 36.0, "", false, "EDIT THIS TABLE IN THE WT EDITOR");
            self.pencil(edit.rect.center());
            if edit.clicked() {
                self.st.wt.slot = osc;
                self.st.wt.edit = None;
                self.st.tab = super::Tab::Wt;
            }
        }
    }

    /// All frames (up to [`WAVE_SLICES`]) in an oblique projection, front
    /// frame at the bottom left, the last frame at the back. The current
    /// frame is drawn on top in its own colour.
    fn wave_stack_view(&mut self, osc: usize, table: &Wavetable, inner: Rect, current: usize) {
        let s = self.cv.s;
        let frames = table.num_frames();
        let n = frames.min(WAVE_SLICES);
        let hash = table.content_hash();
        if self.st.scratch.slices[osc].0 != Some(hash) {
            let slices = (0..n)
                .map(|k| table.display_frame(k * (frames - 1) / (n - 1), WAVE_POINTS))
                .collect();
            self.st.scratch.slices[osc] = (Some(hash), slices);
        }

        let (w, h) = (inner.width(), inner.height());
        let depth = vec2(0.26 * w, -0.42 * h);
        let wave_w = w - depth.x;
        let amp = 0.27 * h;
        let front = pos2(inner.left(), inner.bottom() - amp);
        let point = |d: f32, i: usize, v: f32| {
            front + depth * d + vec2(wave_w * i as f32 / (WAVE_POINTS - 1) as f32, -v * amp)
        };
        let line = |data: &[f32], d: f32| -> Vec<Pos2> { data.iter().enumerate().map(|(i, v)| point(d, i, *v)).collect() };

        // Floor of the stack.
        let floor = self.th.scale.gamma_multiply(0.6);
        let corners = [front, front + vec2(wave_w, 0.0), front + vec2(wave_w, 0.0) + depth, front + depth];
        for k in 0..4 {
            self.pt.line_segment([corners[k], corners[(k + 1) % 4]], Stroke::new(0.8 * s, floor));
        }

        let slices = std::mem::take(&mut self.st.scratch.slices[osc].1);
        for (k, data) in slices.iter().enumerate().rev() {
            let d = k as f32 / (n - 1) as f32;
            let pts = line(data, d);
            fill_under(&self.pt, &pts, front.y + depth.y * d, self.accent.gamma_multiply(0.04));
            let near = 1.0 - d;
            self.pt.add(Shape::line(pts, Stroke::new(0.9 * s, self.accent.gamma_multiply(0.18 + 0.32 * near))));
        }
        self.st.scratch.slices[osc].1 = slices;

        let d = current as f32 / (frames - 1) as f32;
        let pts = line(&table.display_frame(current, WAVE_POINTS), d);
        fill_under(&self.pt, &pts, front.y + depth.y * d, self.th.wave_current.gamma_multiply(0.22));
        self.pt.add(Shape::line(pts, Stroke::new(2.0 * s, self.th.wave_current)));
        // Where the current frame sits along the depth axis.
        let edge = front + vec2(wave_w, 0.0);
        self.pt.line_segment([edge + depth * d, edge + depth * d + vec2(-6.0, 0.0) * s], Stroke::new(1.2 * s, self.th.wave_current));
    }

    /// The current frame large, with neighbouring frames faintly behind it.
    fn wave_flat_view(&mut self, table: &Wavetable, inner: Rect, current: usize) {
        let s = self.cv.s;
        let frames = table.num_frames();
        let mid = inner.center().y;
        let amp = inner.height() * 0.45;
        let line = |data: &[f32]| -> Vec<Pos2> {
            data.iter()
                .enumerate()
                .map(|(i, v)| pos2(inner.left() + inner.width() * i as f32 / (WAVE_POINTS - 1) as f32, mid - v * amp))
                .collect()
        };
        self.pt.line_segment([pos2(inner.left(), mid), pos2(inner.right(), mid)], Stroke::new(0.8 * s, self.th.scale));
        for k in [-2isize, -1, 1, 2] {
            let f = current as isize + k * (frames as isize / 16).max(1);
            if f >= 0 && (f as usize) < frames {
                let pts = line(&table.display_frame(f as usize, WAVE_POINTS));
                self.pt.add(Shape::line(pts, Stroke::new(0.8 * s, self.accent.gamma_multiply(0.3))));
            }
        }
        let pts = line(&table.display_frame(current, WAVE_POINTS));
        fill_under(&self.pt, &pts, mid, self.th.wave_current.gamma_multiply(0.22));
        self.pt.add(Shape::line(pts, Stroke::new(2.0 * s, self.th.wave_current)));
    }

    /// Osc B in noise mode: a fixed burst of white or (smoother) pink noise.
    fn noise_view(&mut self, inner: Rect, pink: bool) {
        let s = self.cv.s;
        let mut rng = crate::dsp::common::Rng::new(if pink { 2 } else { 1 });
        let n = 240;
        let mut v = 0.0;
        let pts: Vec<Pos2> = (0..n)
            .map(|i| {
                let white = rng.next_bipolar();
                v = if pink { 0.6 * v + 0.4 * white * 1.8 } else { white };
                let t = i as f32 / (n - 1) as f32;
                // A soft envelope so the burst reads as a sound, not a box.
                let env = (t * std::f32::consts::PI).sin().powf(0.4);
                pos2(inner.left() + inner.width() * t, inner.center().y - v.clamp(-1.0, 1.0) * env * inner.height() * 0.45)
            })
            .collect();
        self.pt.add(Shape::line(pts, Stroke::new(1.0 * s, self.accent)));
    }

    /// Pencil icon for the edit button.
    fn pencil(&self, c: Pos2) {
        let s = self.cv.s;
        let tip = c + vec2(-6.0, 6.0) * s;
        let (along, across) = (vec2(1.0, -1.0) * (0.707 * s), vec2(1.0, 1.0) * (0.707 * s));
        let ink = self.th.text;
        self.pt.add(Shape::convex_polygon(vec![tip, tip + along * 4.5 + across * 2.0, tip + along * 4.5 - across * 2.0], ink, Stroke::NONE));
        self.pt.line_segment([tip + along * 5.5, tip + along * 15.0], Stroke::new(4.0 * s, ink));
    }

    /// Output oscilloscope from the post-FX analyser ring, triggered on a
    /// rising zero crossing so periodic sounds stand still.
    pub(crate) fn scope(&mut self, rect: Rect) {
        let s = self.cv.s;
        self.pt
            .rect_filled(rect.expand(3.0 * s), self.cv.round(3.0), self.th.oled_frame);
        self.pt.rect_filled(rect, CornerRadius::ZERO, self.th.oled_bg);
        let mid = rect.center().y;
        self.pt.line_segment(
            [pos2(rect.left(), mid), pos2(rect.right(), mid)],
            Stroke::new(0.6 * s, self.th.oled_dim),
        );
        self.pt
            .text(rect.left_top() + vec2(5.0, 4.0) * s, Align2::LEFT_TOP, "OUTPUT", self.cv.mono(7.5), self.th.oled_dim);
        let window = 1024;
        let shared = self.st.shared.clone();
        let buf = &mut self.st.scratch.samples;
        shared.feedback.analyzer_wet.copy_latest(&mut buf[..2 * window]);
        let start = (1..window).find(|&i| buf[i - 1] <= 0.0 && buf[i] > 0.0).unwrap_or(0);
        let seg = &buf[start..start + window];
        let peak = seg.iter().fold(0.05_f32, |m, v| m.max(v.abs()));
        let pts: Vec<Pos2> = seg
            .iter()
            .enumerate()
            .step_by(2)
            .map(|(i, v)| {
                pos2(
                    rect.left() + rect.width() * i as f32 / window as f32,
                    mid - (v / peak.max(0.5)).clamp(-1.0, 1.0) * rect.height() * 0.42,
                )
            })
            .collect();
        self.pt.add(Shape::line(pts, Stroke::new(1.2 * s, self.th.oled_ink)));
        // Stereo peak meter along the bottom edge.
        let fb = &self.st.shared.feedback;
        for (k, m) in fb.meters.iter().enumerate() {
            let gain = m.load(Relaxed);
            let db = 20.0 * gain.max(1.0e-6).log10();
            let t = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            let y = rect.bottom() - (7.0 - 3.0 * k as f32) * s;
            let color = if gain > 1.0 { self.th.clip } else { self.th.oled_ink.gamma_multiply(0.7) };
            self.pt.line_segment(
                [pos2(rect.left() + 4.0 * s, y), pos2(rect.left() + 4.0 * s + t * (rect.width() - 8.0 * s), y)],
                Stroke::new(2.0 * s, color),
            );
        }
    }

    /// Wide spectrum view (FX tab): output filled, voice mix as a line.
    pub(crate) fn spectrum_view(&mut self, rect: Rect) {
        let s = self.cv.s;
        self.pt
            .rect_filled(rect.expand(3.0 * s), self.cv.round(3.0), self.th.oled_frame);
        self.pt.rect_filled(rect, CornerRadius::ZERO, self.th.oled_bg);
        let inner = rect.shrink2(vec2(8.0, 10.0) * s);
        let span = (F_MAX / F_MIN).ln();
        let t_of = |hz: f32| (hz.max(F_MIN) / F_MIN).ln() / span;
        for hz in [100.0_f32, 1000.0, 10_000.0] {
            let x = inner.left() + inner.width() * t_of(hz);
            self.pt
                .line_segment([pos2(x, inner.top()), pos2(x, inner.bottom())], Stroke::new(0.6 * s, self.th.oled_dim));
        }
        self.pt
            .text(rect.left_top() + vec2(6.0, 4.0) * s, Align2::LEFT_TOP, "SPECTRUM  OUT / VOICES", self.cv.mono(8.0), self.th.oled_dim);
        self.spectrum(inner, t_of);
    }
}

/// Hann-windowed magnitude spectrum of the newest ring samples, smoothed in dB.
fn ring_spectrum(ring: &AnalyzerRing, fft: &Fft, samples: &mut [f32], mags: &mut Vec<f32>, db: &mut [f32]) {
    ring.copy_latest(&mut samples[..ANALYZER_SIZE]);
    fft.magnitudes(&samples[..ANALYZER_SIZE], mags);
    let norm = 2.0 / (ANALYZER_SIZE as f32 * 0.5);
    for (d, m) in db.iter_mut().zip(mags.iter()) {
        let v = 20.0 * (m * norm).max(1.0e-6).log10();
        *d = if v > *d { v } else { *d * 0.85 + v * 0.15 };
    }
}
