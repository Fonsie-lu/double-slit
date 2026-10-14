//! Editor colour schemes. Section colours are indexed in the order of [`Section`].
//!
//! Never hard-code a colour in `editor.rs`: add a field here and fill it in for
//! every theme.

use nih_plug_egui::egui::Color32;

/// Panel sections in the order used by [`Theme::bars`] and [`Theme::accents`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    OscA = 0,
    OscB,
    OscC,
    Wt,
    Filter,
    Env,
    Lfo,
    Matrix,
    Fx,
    Master,
}

pub const SECTIONS: usize = 10;

pub struct Theme {
    pub name: &'static str,
    /// Panel surface, the background around it and the bevel line along its top edge.
    pub panel: Color32,
    pub outer: Color32,
    pub bevel: Color32,
    /// Powder coat texture strength.
    pub grain: f32,
    pub fleck: f32,

    pub text: Color32,
    /// Fader scale ticks.
    pub scale: Color32,
    /// Scale numbers.
    pub legend: Color32,
    /// Unselected stepper labels and icons.
    pub step_off: Color32,
    /// Second legend under buttons, and the selected theme chip.
    pub sublabel: Color32,
    /// Small print in the header.
    pub header: Color32,

    pub bars: [Color32; SECTIONS],
    pub bar_text: Color32,
    /// Fader index lines, selected icons and display traces.
    pub accents: [Color32; SECTIONS],
    /// Fader index line while dragging.
    pub drag_line: Color32,
    /// Modulation ranges, source chips and drag-and-drop feedback (SPEC 6.4).
    pub modulation: Color32,
    /// Warning state: unresolved matrix rows.
    pub warning: Color32,

    pub cap: Color32,
    pub cap_top: Color32,
    pub cap_bottom: Color32,
    pub slot: Color32,

    pub led: Color32,
    pub led_off: Color32,
    /// Meter segments above 0 dBFS.
    pub clip: Color32,

    pub button: Color32,
    pub button_hover: Color32,
    pub button_down: Color32,

    pub oled_bg: Color32,
    pub oled_ink: Color32,
    pub oled_dim: Color32,
    pub oled_frame: Color32,
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Tokyo Night (night variant).
pub const TOKYO_NIGHT: Theme = Theme {
    name: "TOKYO NIGHT",
    panel: c(0x1a1b26),
    outer: c(0x16161e),
    bevel: c(0x3b4261),
    grain: 3.0,
    fleck: 7.0,
    text: c(0xc0caf5),
    scale: c(0x565f89),
    legend: c(0x737aa2),
    step_off: c(0x737aa2),
    sublabel: c(0xff9e64),
    header: c(0x737aa2),
    bars: [
        c(0x7dcfff),
        c(0xbb9af7),
        c(0x7aa2f7),
        c(0x1abc9c),
        c(0x9ece6a),
        c(0xe0af68),
        c(0x2ac3de),
        c(0xff9e64),
        c(0xf7768e),
        c(0x73daca),
    ],
    bar_text: c(0x16161e),
    accents: [
        c(0x7dcfff),
        c(0xbb9af7),
        c(0x7aa2f7),
        c(0x1abc9c),
        c(0x9ece6a),
        c(0xe0af68),
        c(0x2ac3de),
        c(0xff9e64),
        c(0xf7768e),
        c(0x73daca),
    ],
    drag_line: c(0xc0caf5),
    modulation: c(0xff9e64),
    warning: c(0xe0af68),
    cap: c(0x24283b),
    cap_top: c(0x414868),
    cap_bottom: c(0x1f2335),
    slot: c(0x16161e),
    led: c(0x9ece6a),
    led_off: c(0x202915),
    clip: c(0xf7768e),
    button: c(0x3b4261),
    button_hover: c(0x414868),
    button_down: c(0x24283b),
    oled_bg: c(0x16161e),
    oled_ink: c(0x7dcfff),
    oled_dim: c(0x565f89),
    oled_frame: c(0x3b4261),
};

/// Catppuccin (Mocha flavour).
pub const CATPPUCCIN: Theme = Theme {
    name: "CATPPUCCIN",
    panel: c(0x1e1e2e),
    outer: c(0x11111b),
    bevel: c(0x45475a),
    grain: 3.0,
    fleck: 7.0,
    text: c(0xcdd6f4),
    scale: c(0x6c7086),
    legend: c(0x7f849c),
    step_off: c(0x7f849c),
    sublabel: c(0xfab387),
    header: c(0x7f849c),
    bars: [
        c(0x89dceb),
        c(0xcba6f7),
        c(0x89b4fa),
        c(0x94e2d5),
        c(0xa6e3a1),
        c(0xf9e2af),
        c(0x74c7ec),
        c(0xfab387),
        c(0xf38ba8),
        c(0xb4befe),
    ],
    bar_text: c(0x11111b),
    accents: [
        c(0x89dceb),
        c(0xcba6f7),
        c(0x89b4fa),
        c(0x94e2d5),
        c(0xa6e3a1),
        c(0xf9e2af),
        c(0x74c7ec),
        c(0xfab387),
        c(0xf38ba8),
        c(0xb4befe),
    ],
    drag_line: c(0xcdd6f4),
    modulation: c(0xfab387),
    warning: c(0xf9e2af),
    cap: c(0x313244),
    cap_top: c(0x45475a),
    cap_bottom: c(0x262637),
    slot: c(0x11111b),
    led: c(0xa6e3a1),
    led_off: c(0x212d20),
    clip: c(0xf38ba8),
    button: c(0x45475a),
    button_hover: c(0x585b70),
    button_down: c(0x313244),
    oled_bg: c(0x11111b),
    oled_ink: c(0x89dceb),
    oled_dim: c(0x6c7086),
    oled_frame: c(0x45475a),
};

/// Dracula.
pub const DRACULA: Theme = Theme {
    name: "DRACULA",
    panel: c(0x282a36),
    outer: c(0x21222c),
    bevel: c(0x44475a),
    grain: 3.0,
    fleck: 7.0,
    text: c(0xf8f8f2),
    scale: c(0x6272a4),
    legend: c(0x7f86b3),
    step_off: c(0x7f86b3),
    sublabel: c(0xffb86c),
    header: c(0x7f86b3),
    bars: [
        c(0x8be9fd),
        c(0xbd93f9),
        c(0xff79c6),
        c(0xa4ffff),
        c(0x50fa7b),
        c(0xf1fa8c),
        c(0xff92df),
        c(0xffb86c),
        c(0xff5555),
        c(0xd6acff),
    ],
    bar_text: c(0x282a36),
    accents: [
        c(0x8be9fd),
        c(0xbd93f9),
        c(0xff79c6),
        c(0xa4ffff),
        c(0x50fa7b),
        c(0xf1fa8c),
        c(0xff92df),
        c(0xffb86c),
        c(0xff5555),
        c(0xd6acff),
    ],
    drag_line: c(0xf8f8f2),
    modulation: c(0xffb86c),
    warning: c(0xf1fa8c),
    cap: c(0x44475a),
    cap_top: c(0x5a5d75),
    cap_bottom: c(0x343746),
    slot: c(0x1e1f29),
    led: c(0x50fa7b),
    led_off: c(0x1f3a2a),
    clip: c(0xff5555),
    button: c(0x44475a),
    button_hover: c(0x5a5d75),
    button_down: c(0x343746),
    oled_bg: c(0x1e1f29),
    oled_ink: c(0x8be9fd),
    oled_dim: c(0x6272a4),
    oled_frame: c(0x44475a),
};

/// Gruvbox (dark, medium contrast).
pub const GRUVBOX: Theme = Theme {
    name: "GRUVBOX",
    panel: c(0x282828),
    outer: c(0x1d2021),
    bevel: c(0x504945),
    grain: 3.0,
    fleck: 7.0,
    text: c(0xebdbb2),
    scale: c(0x665c54),
    legend: c(0xa89984),
    step_off: c(0x928374),
    sublabel: c(0xfe8019),
    header: c(0x928374),
    bars: [
        c(0x8ec07c),
        c(0xd3869b),
        c(0x83a598),
        c(0x689d6a),
        c(0xb8bb26),
        c(0xfabd2f),
        c(0x458588),
        c(0xfe8019),
        c(0xfb4934),
        c(0xd79921),
    ],
    bar_text: c(0x1d2021),
    accents: [
        c(0x8ec07c),
        c(0xd3869b),
        c(0x83a598),
        c(0x689d6a),
        c(0xb8bb26),
        c(0xfabd2f),
        c(0x458588),
        c(0xfe8019),
        c(0xfb4934),
        c(0xd79921),
    ],
    drag_line: c(0xfbf1c7),
    modulation: c(0xfe8019),
    warning: c(0xfabd2f),
    cap: c(0x3c3836),
    cap_top: c(0x504945),
    cap_bottom: c(0x32302f),
    slot: c(0x1d2021),
    led: c(0xb8bb26),
    led_off: c(0x2b2e14),
    clip: c(0xfb4934),
    button: c(0x504945),
    button_hover: c(0x665c54),
    button_down: c(0x3c3836),
    oled_bg: c(0x1d2021),
    oled_ink: c(0xfabd2f),
    oled_dim: c(0x7c6f64),
    oled_frame: c(0x504945),
};

pub const THEMES: [&Theme; 4] = [&TOKYO_NIGHT, &CATPPUCCIN, &DRACULA, &GRUVBOX];

/// Theme used for new plugin instances (index into [`THEMES`]).
pub const DEFAULT_THEME: u8 = 0;

pub fn get(index: u8) -> &'static Theme {
    THEMES[(index as usize).min(THEMES.len() - 1)]
}
