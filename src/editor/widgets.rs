//! Canvas mapping, parameter I/O and the basic controls: faders, stepped
//! faders, selectors, switches, buttons, LEDs and modulation source chips.

use nih_plug::prelude::*;
use nih_plug_egui::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontFamily, FontId, Id, Painter, Pos2, Rect,
    Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};
use std::f32::consts::TAU;

use super::theme::Theme;
use super::{ControlInfo, ControlKind, EditorState, FocusKind, Pending, Popup, PENDING_TIMEOUT};
use crate::dsp::mod_matrix::{mod_source_display_name, mod_target, mod_target_index_for_param, ModCurve};
use crate::dsp::params::ParamId;
use crate::params::DoubleSlitParams;

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
        FontId::new(size * self.s, FontFamily::Name("panel".into()))
    }
    pub fn mono(&self, size: f32) -> FontId {
        FontId::new(size * self.s, FontFamily::Name("oled".into()))
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

/// Scale legends next to a fader, top to bottom.
#[derive(Clone, Copy)]
pub enum Nums {
    None,
    Left(&'static str, &'static str, &'static str),
    Right(&'static str, &'static str, &'static str),
}

pub const N10L: Nums = Nums::Left("10", "5", "0");
pub const N10R: Nums = Nums::Right("10", "5", "0");
pub const BIPOLAR_L: Nums = Nums::Left("+", "0", "-");

/// Labels for the positions of a stepped fader, bottom position first.
#[derive(Clone, Copy)]
pub enum Steps {
    Text(&'static [&'static str]),
    /// LFO shape icons, in the order of `params::LfoShape`.
    Lfo,
}

/// Fader slot travel in canvas pixels.
pub const TRAVEL: f32 = 110.0;

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

    fn scale_ticks(&self, x: f32, slot: (f32, f32), nums: Nums) {
        let cv = self.cv;
        let (y0, y1) = slot;
        for i in 0..11 {
            let y = y1 - i as f32 / 10.0 * (y1 - y0);
            let major = i % 5 == 0;
            let w = if major { 15.0 } else { 12.0 };
            self.pt.line_segment(
                [cv.p(x - w, y), cv.p(x + w, y)],
                cv.stroke(if major { 1.2 } else { 0.9 }, self.th.scale),
            );
        }
        let (side, legends) = match nums {
            Nums::None => return,
            Nums::Left(a, b, c) => (-1.0, [a, b, c]),
            Nums::Right(a, b, c) => (1.0, [a, b, c]),
        };
        let align = if side < 0.0 { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
        for (k, text) in legends.iter().enumerate() {
            let y = y0 + k as f32 * 0.5 * (y1 - y0);
            self.text(cv.p(x + side * 18.0, y), align, text, 9.0, self.th.legend);
        }
    }

    fn slot(&self, x: f32, slot: (f32, f32)) {
        let cv = self.cv;
        self.pt.rect_filled(
            cv.r(x - 2.5, slot.0 - 8.0, 5.0, slot.1 - slot.0 + 16.0),
            cv.round(2.0),
            self.th.slot,
        );
    }

    pub fn cap(&self, x: f32, y: f32, w: f32, active: bool) {
        let cv = self.cv;
        let body = cv.rc(x, y, w, 30.0);
        let round = cv.round(2.0);
        self.pt.rect_filled(
            body.translate(vec2(1.5 * cv.s, 3.0 * cv.s)),
            round,
            Color32::from_black_alpha(140),
        );
        self.pt.rect_filled(body, round, self.th.cap);
        self.pt
            .rect_filled(cv.rc(x, y - 8.0, w - 2.0, 10.0), cv.round(1.0), self.th.cap_top);
        self.pt
            .rect_filled(cv.rc(x, y + 8.0, w - 2.0, 10.0), cv.round(1.0), self.th.cap_bottom);
        self.pt.line_segment(
            [cv.p(x - w * 0.45, y), cv.p(x + w * 0.45, y)],
            cv.stroke(2.2, if active { self.th.drag_line } else { self.accent }),
        );
    }

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

    /// Continuous fader for any parameter, with its modulation range drawn
    /// next to the slot and drop / right-click modulation handling.
    #[allow(clippy::too_many_arguments)]
    pub fn fader(&mut self, id: ParamId, x: f32, label_y: f32, slot: (f32, f32), label: &str, nums: Nums, kind: FocusKind) {
        let (y0, y1) = slot;
        let rect = self.cv.r(x - 14.0, y0 - 16.0, 28.0, y1 - y0 + 32.0);
        self.register(&id.desc().id_string, ControlKind::Fader, rect);
        let resp = self.continuous_input(id, rect, y1 - y0, kind);
        self.mod_target(id, rect, &resp);

        self.label(x, label_y, label);
        self.scale_ticks(x, slot, nums);
        self.slot(x, slot);
        if let Some((lo, hi, activity)) = self.mod_extent(id) {
            let cv = self.cv;
            let (a, b) = (y1 - lo * (y1 - y0), y1 - hi * (y1 - y0));
            let color = self.th.modulation.gamma_multiply(0.55 + 0.45 * activity.clamp(0.0, 1.0));
            self.pt.rect_filled(
                Rect::from_two_pos(cv.p(x + 5.0, a), cv.p(x + 8.0, b)),
                cv.round(1.0),
                color,
            );
        }
        let v = self.shown(id);
        self.cap(x, y1 - v * (y1 - y0), 20.0, resp.dragged());
    }

    /// Fader with detents; positions are labelled with text or icons on the
    /// left, lowest value at the bottom.
    #[allow(clippy::too_many_arguments)]
    pub fn stepper(&mut self, id: ParamId, x: f32, label_y: f32, slot: (f32, f32), label: &str, steps: Steps) {
        let cv = self.cv;
        let count = id.desc().steps.max(2);
        let (y0, y1) = slot;
        let last = (count - 1) as f32;
        let pos_y = |i: usize| y1 - i as f32 / last * (y1 - y0);
        let rect = cv.r(x - 40.0, y0 - 16.0, 54.0, y1 - y0 + 32.0);
        let eid = Id::new(("step", id));
        let resp = self.ui.interact(rect, eid, Sense::click_and_drag());
        self.register(&id.desc().id_string, ControlKind::Stepper, rect);

        let current = (self.shown(id) * last).round() as usize;
        let mut target = None;
        if resp.drag_started() {
            self.ui.data_mut(|d| d.insert_temp(eid, current as f32));
            self.begin(id);
        }
        if resp.dragged() {
            let step_px = (y1 - y0) / last * cv.s;
            let mut acc: f32 = self.ui.data(|d| d.get_temp(eid)).unwrap_or(current as f32);
            acc = (acc - resp.drag_delta().y / step_px).clamp(0.0, last);
            self.ui.data_mut(|d| d.insert_temp(eid, acc));
            self.send(id, acc.round() / last);
        }
        if resp.drag_stopped() {
            self.end(id);
        }
        if resp.clicked() {
            if let Some(ptr) = resp.interact_pointer_pos() {
                let local_y = (ptr.y - cv.o.y) / cv.s;
                target = (0..count).min_by(|a, b| (pos_y(*a) - local_y).abs().total_cmp(&(pos_y(*b) - local_y).abs()));
            }
        }
        if resp.hovered() && !resp.dragged() {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll > 0.0 && current + 1 < count {
                target = Some(current + 1);
            } else if scroll < 0.0 && current > 0 {
                target = Some(current - 1);
            }
        }
        if let Some(i) = target {
            self.change(id, i as f32 / last);
        }
        if resp.hovered() || resp.dragged() {
            self.focus_param(id, FocusKind::Plain);
        }

        let shown = (self.shown(id) * last).round() as usize;
        self.label(x, label_y, label);
        for i in 0..count {
            let y = pos_y(i);
            self.pt
                .line_segment([cv.p(x - 9.0, y), cv.p(x + 12.0, y)], cv.stroke(1.2, self.th.scale));
            self.step_label(steps, i, x - 14.0, y, i == shown);
        }
        self.slot(x, slot);
        self.cap(x, pos_y(shown), 20.0, resp.dragged());
    }

    fn step_label(&self, steps: Steps, i: usize, right: f32, y: f32, selected: bool) {
        let cv = self.cv;
        let color = if selected { self.accent } else { self.th.step_off };
        match steps {
            Steps::Text(t) => {
                self.text(cv.p(right, y), Align2::RIGHT_CENTER, t.get(i).copied().unwrap_or(""), 9.0, color);
            }
            Steps::Lfo => {
                let icon = Rect::from_min_max(cv.p(right - 20.0, y - 4.5), cv.p(right - 1.0, y + 4.5));
                let pts = sample_curve(icon, 2.0, |ph| lfo_shape(i, ph));
                self.pt.add(Shape::line(pts, Stroke::new(1.1 * cv.s, color)));
            }
        }
    }

    /// Box showing a choice parameter's value with arrows either side. Arrows
    /// and the wheel step; clicking the name opens the full list.
    pub fn selector(&mut self, id: ParamId, x: f32, y: f32, w: f32, label: &str) {
        let cv = self.cv;
        let d = id.desc();
        let rect = cv.r(x, y, w, 20.0);
        let eid = Id::new(("sel", id));
        let resp = self.ui.interact(rect.shrink2(vec2(14.0 * cv.s, 0.0)), eid, Sense::click());
        self.register(&d.id_string, ControlKind::Selector, rect);
        let current = self.plain(id).round();
        let left = self.ui.interact(Rect::from_min_size(rect.min, vec2(14.0 * cv.s, rect.height())), eid.with("l"), Sense::click());
        let right = self.ui.interact(
            Rect::from_min_size(rect.right_top() - vec2(14.0 * cv.s, 0.0), vec2(14.0 * cv.s, rect.height())),
            eid.with("r"),
            Sense::click(),
        );
        let mut next = None;
        if left.clicked() {
            next = Some(current - 1.0);
        }
        if right.clicked() {
            next = Some(current + 1.0);
        }
        if resp.clicked() {
            self.st.popup = Some(Popup::Choice { param: id, at: cv.to_canvas(rect.left_bottom()) });
        }
        let hovered = resp.hovered() || left.hovered() || right.hovered();
        if hovered {
            let scroll = self.ui.input(|i| i.raw_scroll_delta.y);
            if scroll != 0.0 {
                next = Some(current - scroll.signum());
            }
            self.focus_param(id, FocusKind::Plain);
        }
        if let Some(v) = next {
            self.change_plain(id, v.clamp(d.min, d.max));
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
        self.text(rect.center(), Align2::CENTER_CENTER, &text, 9.5, self.accent);
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
        let face = if active {
            self.accent
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
        let ink = if active { self.th.bar_text } else { self.th.text };
        self.text(rect.center(), Align2::CENTER_CENTER, text, 8.5, ink);
        if resp.hovered() && !help.is_empty() {
            self.set_focus(help, String::new(), FocusKind::Plain);
        }
        resp
    }

    /// On/off switch for a two-choice parameter: LED above a push button.
    pub fn switch(&mut self, id: ParamId, x: f32, label_y: f32, label: &str) {
        let on = self.plain(id) > 0.5;
        self.label(x, label_y, label);
        self.led(self.cv.p(x, label_y + 14.0), if on { 1.0 } else { 0.0 });
        let resp = self.button(&id.desc().id_string, x - 14.0, label_y + 26.0, 28.0, if on { "ON" } else { "OFF" }, false, "");
        // Registered as a switch so tests can tell it from plain buttons.
        if let Some(c) = self.st.controls.last_mut() {
            c.kind = ControlKind::Switch;
        }
        if resp.clicked() {
            self.change_plain(id, if on { 0.0 } else { 1.0 });
        }
        if resp.hovered() {
            self.focus_param(id, FocusKind::Plain);
        }
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
