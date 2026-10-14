//! The synthesis engine: all DSP and state, independent of nih-plug and the
//! editor so it can be tested on its own. Nothing in here may depend on
//! `nih_plug`; the plugin layer in `crate::params` and `crate::lib` adapts it.

pub mod common;
pub mod envelope;
pub mod fft;
pub mod filters;
pub mod fx;
pub mod lfo;
pub mod mod_matrix;
pub mod params;
pub mod presets;
pub mod resampler;
pub mod shared;
pub mod state;
pub mod wavetable;
pub mod oscillator;
pub mod synth;
pub mod voice;
