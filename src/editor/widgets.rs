//! Canvas mapping, parameter I/O and the basic controls: faders, stepped
//! faders, selectors, switches, buttons, LEDs and modulation source chips.

use nih_plug::prelude::*;
use nih_plug_egui::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontFamily, FontId, Id, Painter, Pos2, Rect,
    Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};
use std::f32::consts::{PI, TAU};

use super::theme::Theme;
use super::{ControlInfo, ControlKind, EditorState, FocusKind, Pending, Popup, PENDING_TIMEOUT};
use crate::dsp::mod_matrix::{mod_source_display_name, mod_target, mod_target_index_for_param, ModCurve};
use crate::dsp::params::{table_list_order, ParamId};
use crate::params::DoubleSlitParams;

/// Smallest text size in canvas pixels; smaller requests are raised to it so
/// the dense legends (matrix rows, chips, popups) stay readable.
pub const MIN_FONT: f32 = 11.0;

/// Maps canvas coordinates to screen coordinates.
#[derive(Clone, Copy)]
pub struct Cv {
    pub o: Pos2,
    pub s: f32,
}

impl Cv {
    pub fn p(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.o.x + x * self.s, self.o.y + y * self.s)
    }
    pub fn r(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.p(x, y), vec2(w * self.s, h * self.s))
    }
    pub fn rc(&self, cx: f32, cy: f32, w: f32, h: f32) -> Rect {
        self.r(cx - w * 0.5, cy - h * 0.5, w, h)
    }
    pub fn font(&self, size: f32) -> FontId {
        FontId::new(size.max(MIN_FONT) * self.s, FontFamily::Name("panel".into()))
    }
    pub fn mono(&self, size: f32) -> FontId {
        FontId::new(size.max(MIN_FONT) * self.s, FontFamily::Name("oled".into()))
    }
    pub fn stroke(&self, w: f32, c: Color32) -> Stroke {
        Stroke::new(w * self.s, c)
    }
    pub fn round(&self, r: f32) -> CornerRadius {
        CornerRadius::same((r * self.s) as u8)
    }
    /// Screen position back to canvas coordinates.
    pub fn to_canvas(&self, p: Pos2) -> Pos2 {
        pos2((p.x - self.o.x) / self.s, (p.y - self.o.y) / self.s)
    }
}

/// Labels for the cells of a choice list, in value order.
#[derive(Clone, Copy)]
pub enum Steps {
    Text(&'static [&'static str]),
    /// LFO shape icons, in the order of `params::LfoShape`.
    Lfo,
}

/// Vertical drag across a knob's full range, in canvas pixels.
pub const KNOB_TRAVEL: f32 = 160.0;
/// Vertical drag across a value field's full range, in canvas pixels.
pub const FIELD_TRAVEL: f32 = 120.0;

pub struct G<'a, 'b> {
    pub ui: &'a Ui,
    pub pt: Painter,
    pub cv: Cv,
    pub set: &'a ParamSetter<'b>,
    pub p: &'a DoubleSlitParams,
    pub st: &'a mut EditorState,
    pub th: &'static Theme,
    /// Accent of the section currently being drawn.
    pub accent: Color32,
}

impl G<'_, '_> {
    // ------------------------------------------------------------ text

    pub fn text(&self, pos: Pos2, align: Align2, text: &str, size: f32, color: Color32) {
        self.pt.text(pos, align, text, self.cv.font(size), color);
    }

    /// Width of `text` in the panel font, in canvas pixels.
    pub fn text_w(&self, text: &str, size: f32) -> f32 {
        let font = self.cv.font(size);
        self.ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x) / self.cv.s
    }

    /// Bipolar amount (-1 … +1) as a bar filled from the centre of `rect`.
    pub fn bipolar_fill(&self, rect: Rect, amount: f32) {
        self.pt.rect_filled(rect, self.cv.round(2.0), self.th.slot);
        let mid = rect.center().x;
        let end = mid + amount.clamp(-1.0, 1.0) * rect.width() * 0.5;
        self.pt.rect_filled(
            Rect::from_x_y_ranges(mid.min(end)..=mid.max(end), rect.y_range()),
            self.cv.round(2.0),
            self.th.modulation,
        );
    }

    /// Multi-line label, bottom line sitting on `y`.
    pub fn label(&self, x: f32, y: f32, text: &str) {
        let lines: Vec<&str> = text.split('\n').collect();
        let n = lines.len();
        for (i, line) in lines.iter().enumerate() {
            let yy = y - (n - 1 - i) as f32 * 11.0;
            self.text(self.cv.p(x, yy), Align2::CENTER_BOTTOM, line, 10.0, self.th.text);
        }
    }

    pub fn bar(&self, y: f32, sections: &[(f32, f32, &str, super::theme::Section)]) {
        let cv = self.cv;
        for (a, b, title, section) in sections {
            let color = self.th.bars[*section as usize];
            let rect = cv.r(a + 1.5, y, b - a - 3.0, 22.0);
            self.pt.rect_filled(rect, cv.round(3.0), color);
            self.text(
                cv.p((a + b) * 0.5, y + 11.5),
                Align2::CENTER_CENTER,
                title,
                15.0,
                self.th.bar_text,
            );
        }
    }

    // ------------------------------------------------------- parameter I/O

    pub fn register(&mut self, name: &str, kind: ControlKind, rect: Rect) {
        self.st.controls.push(ControlInfo {
            name: name.to_owned(),
            kind,
            rect,
        });
    }

    pub fn now(&self) -> f64 {
        self.ui.input(|i| i.time)
    }

    /// The parameter's actual normalised value (from the preview store in the
    /// standalone preview and the tests).
    pub fn actual(&self, id: ParamId) -> f32 {
        let hp = self.p.get(id);
        if let Some(store) = &self.st.preview_store {
            if let Some(v) = store.lock().unwrap().get(&hp.ptr()) {
                return *v;
            }
        }
        hp.normalized()
    }

    /// The value to display: a value the GUI just sent wins until the host
    /// has applied it. nih-plug applies GUI changes asynchronously, so reading
    /// the parameter right after setting it returns the old value.
    pub fn shown(&mut self, id: ParamId) -> f32 {
        let actual = self.actual(id);
        let now = self.now();
        match self.st.pending.get(&id) {
            Some(p) if (p.value - actual).abs() > 1e-5 && now - p.time < PENDING_TIMEOUT => p.value,
            Some(_) => {
                self.st.pending.remove(&id);
                actual
            }
            None => actual,
        }
    }

    /// Displayed value in natural units.
    pub fn plain(&mut self, id: ParamId) -> f32 {
        let n = self.shown(id);
        id.desc().from_normalized(n)
    }

    /// Send a normalised value (snapped to the parameter's steps) and remember
    /// it as pending. Must sit between `begin` and `end`.
    pub fn send(&mut self, id: ParamId, normalized: f32) {
        let d = id.desc();
        let v = d.to_normalized(d.from_normalized(normalized.clamp(0.0, 1.0)));
        if (v - self.shown(id)).abs() < 1e-6 {
            return;
        }
        self.p.get(id).set(self.set, v);
        let time = self.now();
        self.st.pending.insert(id, Pending { value: v, time });
    }

    pub fn begin(&self, id: ParamId) {
        self.p.get(id).begin(self.set);
    }

    pub fn end(&self, id: ParamId) {
        self.p.get(id).end(self.set);
    }

    /// A complete single-step change (begin, set, end).
    pub fn change(&mut self, id: ParamId, normalized: f32) {
        self.begin(id);
        self.send(id, normalized);
        self.end(id);
    }

    /// Single-step change in natural units.
    pub fn change_plain(&mut self, id: ParamId, value: f32) {
        let n = id.desc().to_normalized(value);
        self.change(id, n);
    }

    pub fn value_text(&mut self, id: ParamId) -> String {
        let v = self.plain(id);
        id.desc().format(v)
    }

    pub fn set_focus(&mut self, name: &str, value: String, kind: FocusKind) {
        self.st.focus = Some(super::Focus {
            name: name.to_uppercase(),
            value,
            kind,
        });
    }

    fn focus_param(&mut self, id: ParamId, kind: FocusKind) {
        let d = id.desc();
        let text = self.value_text(id);
        self.set_focus(&format!("{} {}", d.module, d.name), text, kind);
    }

    /// Pointer movement since the button went down, scaled down with Shift.
    /// Short graph spans use this instead of per-frame deltas, so the movement
    /// before egui recognises the drag is not lost.
    pub fn drag_offset(&self) -> Vec2 {
        self.ui.input(|i| {
            let fine = if i.modifiers.shift { 0.15 } else { 1.0 };
            match (i.pointer.press_origin(), i.pointer.interact_pos()) {
                (Some(a), Some(b)) => (b - a) * fine,
                _ => Vec2::ZERO,
            }
        })
    }

    // ------------------------------------------------------- modulation

    /// Registers `rect` as a place a dragged modulation source can be dropped
    /// on, and handles right-click (modulation popover) for it.
    pub fn mod_target(&mut self, id: ParamId, rect: Rect, resp: &Response) {
        let Some(target) = mod_target_index_for_param(id) else {
            return;
        };
        self.st.drop_targets.push((rect, target));
        if self.st.drag_source.is_some() && resp.contains_pointer() {
            self.pt.rect_stroke(
                rect.expand(2.0 * self.cv.s),
                self.cv.round(3.0),
                Stroke::new(1.5 * self.cv.s, self.th.modulation),
                StrokeKind::Outside,
            );
        }
        if resp.secondary_clicked() {
            let at = self.cv.to_canvas(rect.right_top());
            self.st.popup = Some(Popup::Modulation { target, at });
        }
    }

    /// Modulation extent on a target as normalised positions (low, high) around
    /// the current value, plus the row activity, or `None` if unmodulated.
    pub fn mod_extent(&mut self, id: ParamId) -> Option<(f32, f32, f32)> {
        let target = mod_target_index_for_param(id)?;
        let info = *self.st.mod_info.get(target)?;
        if info.pos <= 0.0 && info.neg <= 0.0 {
            return None;
        }
        let d = id.desc();
        let t = mod_target(target);
        let v = self.plain(id);
        let apply = |amount: f32| {
            if t.log_scale {
                v * (amount * t.range).exp2()
            } else {
                v + amount * t.range
            }
        };
        let hi = d.to_normalized(d.clamp(apply(info.pos)));
        let lo = d.to_normalized(d.clamp(apply(-info.neg)));
        Some((lo, hi, info.activity))
    }

    // ----------------------------------------------------------- faders

    /// Vertical drag (Shift for fine), wheel, double-click to reset. The drag
    /// position lives in egui temp memory (start value plus movement) because
    /// the parameter itself only updates asynchronously.
    fn continuous_input(&mut self, id: ParamId, rect: Rect, travel: f32, kind: FocusKind) -> Response {
        let eid = Id::new(("fader", id));
        let resp = self.ui.interact(rect, eid, Sense::click_and_drag());
        if resp.drag_started() {
            let start = self.shown(id);
            self.ui.data_mut(|d| d.insert_temp(eid, start));
            self.begin(id);
        }
        if resp.dragged() {
            let fine = self.ui.input(|i| i.modifiers.shift);
            let delta = -resp.drag_delta().y / (travel * self.cv.s) * if fine { 0.15 } else { 1.0 };
            let start = self.shown(id);
            let pos: f32 = self.ui.data(|d| d.get_temp(eid)).unwrap_or(start);
            let pos = (pos + delta).clamp(0.0, 1.0);
            self.ui.data_mut(|d| d.insert_temp(eid, pos));
            self.send(id, pos);
        }
        if resp.drag_stopped() {
            self.end(id);
        }
        if resp.double_clicked() {
            let def = self.p.get(id).default_normalized();
            self.change(id, def);
        }
        if resp.hovered() && !resp.dragged() {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                let steps = id.desc().steps;
                let step = if steps > 1 { 1.0 / (steps - 1) as f32 } else { 0.0 };
                let step = step.max(0.0015 * scroll.abs());
                let v = self.shown(id) + step * scroll.signum();
                self.change(id, v);
            }
        }
        if resp.hovered() || resp.dragged() {
            self.focus_param(id, kind);
        }
        resp
    }

    /// Rotary knob (SPEC 6.3): a 270° track with the value arc from the
    /// bottom left (from the top for bipolar parameters) and the modulation
    /// range on an outer arc. Input as for a fader; the label turns into the
    /// value while the knob is hovered.
    pub fn knob(&mut self, id: ParamId, cx: f32, cy: f32, r: f32, label: &str) {
        let cv = self.cv;
        let s = cv.s;
        let rect = cv.rc(cx, cy + 6.0, 2.0 * r + 22.0, 2.0 * r + 30.0);
        self.register(&id.desc().id_string, ControlKind::Knob, rect);
        let resp = self.continuous_input(id, rect, KNOB_TRAVEL, FocusKind::Plain);
        self.mod_target(id, rect, &resp);
        let active = resp.hovered() || resp.dragged();
        let v = self.shown(id);
        let extent = self.mod_extent(id);
        let text = if active { self.value_text(id) } else { label.to_owned() };

        let c = cv.p(cx, cy);
        let at = |t: f32, radius: f32| {
            let a = (0.75 + 1.5 * t) * PI;
            c + vec2(a.cos(), a.sin()) * radius * s
        };
        let arc = |a: f32, b: f32, radius: f32, width: f32, color: Color32| {
            let (a, b) = (a.min(b), a.max(b));
            let n = ((b - a) * 48.0).ceil().max(1.0) as usize;
            let pts: Vec<Pos2> = (0..=n).map(|k| at(a + (b - a) * k as f32 / n as f32, radius)).collect();
            self.pt.add(Shape::line(pts, Stroke::new(width * s, color)));
        };
        let origin = if id.desc().bipolar { 0.5 } else { 0.0 };
        arc(0.0, 1.0, r + 4.0, 2.5, self.th.slot);
        if (v - origin).abs() > 1e-4 {
            arc(origin, v, r + 4.0, 2.5, self.accent);
        }
        if let Some((lo, hi, activity)) = extent {
            arc(lo, hi, r + 8.0, 2.0, self.th.modulation.gamma_multiply(0.55 + 0.45 * activity.clamp(0.0, 1.0)));
        }
        self.pt.circle_filled(c + vec2(1.5, 3.0) * s, r * s, Color32::from_black_alpha(140));
        self.pt.circle_filled(c, r * s, self.th.cap_bottom);
        self.pt.circle_filled(c - vec2(0.0, 1.0) * s, (r - 2.0) * s, self.th.cap);
        let ink = if resp.dragged() { self.th.drag_line } else { self.th.text };
        self.pt.line_segment([at(v, r * 0.3), at(v, r * 0.85)], Stroke::new(2.2 * s, ink));
        let color = if active { self.accent } else { self.th.text };
        self.text(cv.p(cx, cy + r + 15.0), Align2::CENTER_CENTER, &text, 9.5, color);
    }

    /// Value field ("OCT  +2"): name on the left, value without its unit on
    /// the right. Drag vertically, wheel, double-click resets.
    pub fn field(&mut self, id: ParamId, x: f32, y: f32, w: f32, label: &str) {
        let cv = self.cv;
        let rect = cv.r(x, y, w, 20.0);
        self.register(&id.desc().id_string, ControlKind::Field, rect);
        let resp = self.continuous_input(id, rect, FIELD_TRAVEL, FocusKind::Plain);
        self.mod_target(id, rect, &resp);
        let active = resp.hovered() || resp.dragged();
        self.pt.rect_filled(rect, cv.round(3.0), self.th.slot);
        self.pt.rect_stroke(
            rect,
            cv.round(3.0),
            Stroke::new(1.0 * cv.s, if active { self.accent } else { self.th.scale }),
            StrokeKind::Inside,
        );
        self.text(cv.p(x + 7.0, y + 10.0), Align2::LEFT_CENTER, label, 9.0, self.th.legend);
        let unit = id.desc().unit;
        let value = self.value_text(id);
        // Only a separate unit goes ("0 oct" -> "0"); "6.00 kHz" keeps its prefix.
        let value = value.strip_suffix(&format!(" {unit}")).unwrap_or(&value).trim_end();
        let value = if unit == "deg" { format!("{value}°") } else { value.to_owned() };
        self.text(cv.p(x + w - 7.0, y + 10.0), Align2::RIGHT_CENTER, &value, 10.0, self.accent);
        if let Some((lo, hi, activity)) = self.mod_extent(id) {
            let color = self.th.modulation.gamma_multiply(0.55 + 0.45 * activity.clamp(0.0, 1.0));
            let x_of = |n: f32| x + 3.0 + n * (w - 6.0);
            self.pt.rect_filled(
                Rect::from_two_pos(cv.p(x_of(lo), y + 16.0), cv.p(x_of(hi), y + 18.0)),
                cv.round(1.0),
                color,
            );
        }
    }

    /// Choice parameter as a grid of cells, `cols` per row in value order
    /// (Serum's mode lists and toggles). Clicking a cell selects its value.
    #[allow(clippy::too_many_arguments)]
    pub fn segments(&mut self, id: ParamId, x: f32, y: f32, w: f32, cell_h: f32, cols: usize, steps: Steps) {
        let cv = self.cv;
        let d = id.desc();
        let count = d.steps.max(2);
        let cell_w = w / cols as f32;
        let whole = cv.r(x, y, w, count.div_ceil(cols) as f32 * cell_h);
        self.register(&d.id_string, ControlKind::List, whole);
        let current = (self.plain(id) - d.min).round() as usize;
        for i in 0..count {
            let rect = cv
                .r(x + (i % cols) as f32 * cell_w, y + (i / cols) as f32 * cell_h, cell_w, cell_h)
                .shrink(1.0 * cv.s);
            let key = format!("{}#{i}", d.id_string);
            let resp = self.ui.interact(rect, Id::new(&key), Sense::click());
            self.register(&key, ControlKind::List, rect);
            if resp.clicked() {
                self.change_plain(id, d.min + i as f32);
            }
            if resp.hovered() {
                let name = format!("{} {}", d.module, d.name);
                self.set_focus(&name, d.format(d.min + i as f32), FocusKind::Plain);
            }
            let selected = i == current;
            let fill = if selected { self.th.slot } else { self.th.slot.gamma_multiply(0.45) };
            self.pt.rect_filled(rect, cv.round(2.0), fill);
            if selected || resp.hovered() {
                let edge = if selected { self.accent } else { self.th.scale };
                self.pt.rect_stroke(rect, cv.round(2.0), Stroke::new(1.0 * cv.s, edge), StrokeKind::Inside);
            }
            let ink = if selected { self.accent } else { self.th.step_off };
            match steps {
                Steps::Text(t) => self.text(rect.center(), Align2::CENTER_CENTER, t.get(i).copied().unwrap_or(""), 9.0, ink),
                Steps::Lfo => {
                    let icon = Rect::from_center_size(rect.center(), vec2(rect.width() - 14.0 * cv.s, rect.height() * 0.45));
                    let pts = sample_curve(icon, 1.0, |ph| lfo_shape(i, ph));
                    self.pt.add(Shape::line(pts, Stroke::new(1.4 * cv.s, ink)));
                }
            }
        }
    }

    /// Round drag handle for a modulation source, as on Serum's modulator
    /// tabs. Dropping it on a modulatable control adds a matrix row there.
    pub fn source_handle(&mut self, source: usize, c: Pos2, ink: Color32) {
        let s = self.cv.s;
        let key = format!("chip:{}", crate::dsp::mod_matrix::mod_source_id(source));
        let rect = Rect::from_center_size(c, vec2(20.0, 20.0) * s);
        let resp = self.ui.interact(rect, Id::new(&key), Sense::click_and_drag());
        self.register(&key, ControlKind::Chip, rect);
        if resp.drag_started() {
            self.st.drag_source = Some(source);
        }
        let hot = resp.hovered() || resp.dragged();
        if hot {
            self.pt.circle_filled(c, 8.0 * s, self.th.modulation);
            let name = mod_source_display_name(source).to_uppercase();
            self.set_focus("DRAG ONTO A CONTROL TO MODULATE IT", name, FocusKind::Plain);
        }
        let ink = if hot { self.th.bar_text } else { ink };
        // A ring with an arrowhead: "this drags out".
        let pts: Vec<Pos2> = (0..=20)
            .map(|k| {
                let a = (0.15 + 1.55 * k as f32 / 20.0) * PI;
                c + vec2(a.cos(), a.sin()) * 5.5 * s
            })
            .collect();
        let end = pts[pts.len() - 1];
        self.pt.add(Shape::line(pts, Stroke::new(1.6 * s, ink)));
        self.pt.add(Shape::convex_polygon(
            vec![end + vec2(-3.0, -2.0) * s, end + vec2(3.0, -2.0) * s, end + vec2(0.0, 2.5) * s],
            ink,
            Stroke::NONE,
        ));
    }

    /// Box showing a choice parameter's value with arrows either side. Arrows
    /// and the wheel step; clicking the name opens the full list.
    pub fn selector(&mut self, id: ParamId, x: f32, y: f32, w: f32, label: &str) {
        self.selector_sized(id, x, y, w, label, 9.5);
    }

    /// [`Self::selector`] with the value set at `size`.
    #[allow(clippy::too_many_arguments)]
    pub fn selector_sized(&mut self, id: ParamId, x: f32, y: f32, w: f32, label: &str, size: f32) {
        let cv = self.cv;
        let d = id.desc();
        let rect = cv.r(x, y, w, 20.0);
        let eid = Id::new(("sel", id));
        let resp = self.ui.interact(rect.shrink2(vec2(14.0 * cv.s, 0.0)), eid, Sense::click());
        self.register(&d.id_string, ControlKind::Selector, rect);
        let current = self.plain(id).round();
        let order = choice_order(id);
        let pos = order.iter().position(|&c| c as f32 == current).unwrap_or(0) as f32;
        let left = self.ui.interact(Rect::from_min_size(rect.min, vec2(14.0 * cv.s, rect.height())), eid.with("l"), Sense::click());
        let right = self.ui.interact(
            Rect::from_min_size(rect.right_top() - vec2(14.0 * cv.s, 0.0), vec2(14.0 * cv.s, rect.height())),
            eid.with("r"),
            Sense::click(),
        );
        let mut next = None;
        if left.clicked() {
            next = Some(pos - 1.0);
        }
        if right.clicked() {
            next = Some(pos + 1.0);
        }
        if resp.clicked() {
            self.st.popup = Some(Popup::Choice { param: id, at: cv.to_canvas(rect.left_bottom()) });
        }
        let hovered = resp.hovered() || left.hovered() || right.hovered();
        if hovered {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                next = Some(pos - scroll.signum());
            }
            self.focus_param(id, FocusKind::Plain);
        }
        if let Some(p) = next {
            let p = p.clamp(0.0, order.len() as f32 - 1.0) as usize;
            self.change_plain(id, order[p] as f32);
        }
        self.mod_target(id, rect, &resp);

        if !label.is_empty() {
            self.text(cv.p(x + w * 0.5, y - 3.0), Align2::CENTER_BOTTOM, label, 10.0, self.th.text);
        }
        self.pt.rect_filled(rect, cv.round(3.0), self.th.slot);
        self.pt.rect_stroke(
            rect,
            cv.round(3.0),
            Stroke::new(1.0 * cv.s, if hovered { self.accent } else { self.th.scale }),
            StrokeKind::Inside,
        );
        let text = self.value_text(id).to_uppercase();
        self.text(rect.center(), Align2::CENTER_CENTER, &text, size, self.accent);
        for (cx, dir) in [(x + 7.0, -1.0), (x + w - 7.0, 1.0)] {
            let c = cv.p(cx, y + 10.0);
            self.pt.add(Shape::convex_polygon(
                vec![c + vec2(3.0 * dir, 0.0) * cv.s, c + vec2(-3.0 * dir, -3.5) * cv.s, c + vec2(-3.0 * dir, 3.5) * cv.s],
                self.th.legend,
                Stroke::NONE,
            ));
        }
    }

    pub fn led(&self, c: Pos2, level: f32) {
        let s = self.cv.s;
        if level > 0.0 {
            self.pt
                .circle_filled(c, 6.0 * s, self.th.led.gamma_multiply(0.25 * level));
        }
        self.pt.circle_filled(c, 3.2 * s, self.th.led_off);
        if level > 0.0 {
            self.pt.circle_filled(c, 3.2 * s, self.th.led.gamma_multiply(level.min(1.0)));
        }
        self.pt
            .circle_stroke(c, 3.2 * s, Stroke::new(0.8 * s, self.th.slot));
    }

    /// Rectangular push button with an optional coloured legend below.
    #[allow(clippy::too_many_arguments)]
    pub fn button(&mut self, key: &str, x: f32, y: f32, w: f32, text: &str, active: bool, help: &str) -> Response {
        let cv = self.cv;
        let rect = cv.r(x, y, w, 18.0);
        let resp = self.ui.interact(rect, Id::new(("btn", key)), Sense::click());
        self.register(key, ControlKind::Button, rect);
        let down = resp.is_pointer_button_down_on();
        // Active reads like a selected cell of a choice list.
        let face = if active {
            self.th.slot
        } else if down {
            self.th.button_down
        } else if resp.hovered() {
            self.th.button_hover
        } else {
            self.th.button
        };
        let rect = if down { rect.translate(vec2(0.0, cv.s)) } else { rect };
        self.pt.rect_filled(
            rect.translate(vec2(0.0, 2.0 * cv.s)),
            cv.round(2.0),
            Color32::from_black_alpha(140),
        );
        self.pt.rect_filled(rect, cv.round(2.0), face);
        if active {
            self.pt.rect_stroke(rect, cv.round(2.0), Stroke::new(1.0 * cv.s, self.accent), StrokeKind::Inside);
        }
        let ink = if active { self.accent } else { self.th.text };
        self.text(rect.center(), Align2::CENTER_CENTER, text, 8.5, ink);
        if resp.hovered() && !help.is_empty() {
            self.set_focus(help, String::new(), FocusKind::Plain);
        }
        resp
    }

    /// Draggable modulation source chip (SPEC 4.4). Dropping it on a
    /// modulatable control adds a matrix row there.
    pub fn source_chip(&mut self, source: usize, rect: Rect) {
        let key = format!("chip:{}", crate::dsp::mod_matrix::mod_source_id(source));
        let resp = self.ui.interact(rect, Id::new(&key), Sense::click_and_drag());
        self.register(&key, ControlKind::Chip, rect);
        if resp.drag_started() {
            self.st.drag_source = Some(source);
        }
        let name = mod_source_display_name(source).to_uppercase();
        let fill = if resp.hovered() || resp.dragged() {
            self.th.modulation
        } else {
            self.th.modulation.gamma_multiply(0.75)
        };
        self.pt.rect_filled(rect, self.cv.round(3.0), fill);
        self.pt.text(rect.center(), Align2::CENTER_CENTER, &name, self.cv.font(8.0), self.th.bar_text);
        if resp.hovered() {
            self.set_focus("DRAG ONTO A CONTROL TO MODULATE IT", name, FocusKind::Plain);
        }
    }

    /// Curve icon used in the matrix rows.
    pub fn curve_icon(&self, rect: Rect, curve: ModCurve, color: Color32) {
        let pts = sample_curve(rect, 1.0, |ph| {
            let x = ph * 2.0 - 1.0;
            crate::dsp::mod_matrix::apply_mod_curve(x, curve)
        });
        self.pt.add(Shape::line(pts, Stroke::new(1.2 * self.cv.s, color)));
    }
}

/// Polyline of `f(phase)` across `rect`, `cycles` periods wide, `f` in -1..1.
pub fn sample_curve(rect: Rect, cycles: f32, f: impl Fn(f32) -> f32) -> Vec<Pos2> {
    let n = 48;
    (0..=n)
        .map(|k| {
            let t = k as f32 / n as f32;
            // The last point ends the final cycle instead of wrapping to its start.
            let ph = if k == n { 0.9999 } else { (t * cycles).fract() };
            pos2(rect.left() + rect.width() * t, rect.center().y - f(ph) * rect.height() * 0.5)
        })
        .collect()
}

/// LFO shape by enum index, in the order of `params::LfoShape`. Custom draws
/// a placeholder squiggle; the LFO graph shows the real custom shape.
pub fn lfo_shape(index: usize, ph: f32) -> f32 {
    match index {
        0 => (ph * TAU).sin(),
        1 => 1.0 - 4.0 * (ph - 0.5).abs(),
        2 => 2.0 * ph - 1.0,
        3 => 1.0 - 2.0 * ph,
        4 => {
            if ph < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        5 => [0.6, -0.4, 0.9, -0.8][((ph * 4.0) as usize).min(3)],
        _ => 0.7 * (ph * TAU).sin() * (1.0 - ph) + 0.3 * (ph * TAU * 3.0).sin(),
    }
}

/// Recessed background behind the panel graphs.
pub fn graph_bg(g: &G, rect: Rect) {
    g.pt.rect_filled(rect, g.cv.round(3.0), g.th.slot.gamma_multiply(0.55));
}

/// Fill between a polyline and a horizontal baseline. The area is usually not
/// convex, so it is built as a strip of quads.
pub fn fill_under(pt: &Painter, pts: &[Pos2], base: f32, color: Color32) {
    let mut mesh = egui::Mesh::default();
    for w in pts.windows(2) {
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(w[0], color);
        mesh.colored_vertex(w[1], color);
        mesh.colored_vertex(pos2(w[1].x, base), color);
        mesh.colored_vertex(pos2(w[0].x, base), color);
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i, i + 2, i + 3);
    }
    pt.add(Shape::mesh(mesh));
}

/// Choice values in the order selectors step and lists show them: the
/// wavetable lists put Custom last, every other choice is in value order.
pub(crate) fn choice_order(id: ParamId) -> Vec<usize> {
    match id {
        ParamId::OscATable | ParamId::OscBTable => table_list_order().collect(),
        _ => (0..id.desc().choices.len()).collect(),
    }
}
