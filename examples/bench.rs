//! Performance check for SPEC 9: 16 voices, 2 osc x 8 unison, 4x
//! oversampling, 48 kHz / 128-sample blocks must stay well under one core.
//!
//!   cargo run --release --example bench

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use double_slit::dsp::common::MATRIX_SLOTS;
use double_slit::dsp::mod_matrix::{mod_target_from_id, ModCurve, ModSource};
use double_slit::dsp::params::{FilterType, ParamId};
use double_slit::dsp::shared::SynthShared;
use double_slit::dsp::synth::Synth;

struct Case {
    name: &'static str,
    oversampling: f32,
    unison: f32,
    voices: usize,
    block: usize,
    rate: f64,
    fx: bool,
}

/// Fraction of one core, and the output peak.
fn run(c: &Case) -> (f64, f32) {
    let shared = Arc::new(SynthShared::default());
    let mut synth = Synth::new(shared.clone());
    synth.prepare(c.rate, c.block);
    synth.set_param(ParamId::MasterOversampling, c.oversampling);
    synth.set_param(ParamId::MasterPolyphony, c.voices as f32);
    synth.set_param(ParamId::OscAUniVoices, c.unison);
    synth.set_param(ParamId::OscBOn, 1.0);
    synth.set_param(ParamId::OscBUniVoices, c.unison);
    synth.set_param(ParamId::FilterType, FilterType::LadderLp as usize as f32);
    synth.set_param(ParamId::FilterRes, 0.6);
    synth.set_param(ParamId::FilterDrive, 0.3);
    synth.set_param(ParamId::EnvAmpSustain, 1.0);
    synth.set_param(ParamId::EnvAmpRelease, 2.0);
    if c.fx {
        for id in [ParamId::FxDistOn, ParamId::FxChorusOn, ParamId::FxDelayOn, ParamId::FxRevOn] {
            synth.set_param(id, 1.0);
        }
    }
    // Worst case: every slot in the matrix busy and modulating per sample.
    shared.edit_matrix(true, |m| {
        for i in 0..MATRIX_SLOTS {
            let source = if i % 2 == 0 { ModSource::Lfo1 } else { ModSource::EnvMod };
            let target = ["filter.cutoff", "oscA.wtPos", "oscB.fine"][i % 3];
            m.add_row(source.index(), mod_target_from_id(target), 0.3, ModCurve::Linear);
        }
    });
    // Worst case: editor open, analyser capturing.
    shared.feedback.gui_attached.store(true, Ordering::Relaxed);

    for i in 0..c.voices {
        synth.note_on(0, 36 + i as u8 * 2, 0.9, i as i32);
    }

    let mut left = vec![0.0; c.block];
    let mut right = vec![0.0; c.block];
    // Two seconds of audio per pass.
    let blocks = (c.rate * 2.0 / c.block as f64) as usize;

    // Half a second before timing anything: first-touch page faults, and time
    // for the CPU frequency governor to ramp.
    for _ in 0..(c.rate * 0.5 / c.block as f64) as usize {
        synth.process(&mut left, &mut right);
    }

    // Best of three: scheduling and thermal noise can only make a pass slower,
    // so the minimum is the honest estimate of what the code costs.
    let audio_seconds = (blocks * c.block) as f64 / c.rate;
    let mut best = f64::MAX;
    let mut peak: f32 = 0.0;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..blocks {
            left.fill(0.0);
            right.fill(0.0);
            synth.process(&mut left, &mut right);
            peak = left.iter().fold(peak, |m, v| m.max(v.abs()));
        }
        best = best.min(start.elapsed().as_secs_f64() / audio_seconds);
    }
    (best, peak)
}

fn main() {
    let cases = [
        Case { name: "SPEC 9: 16 voices, 2x8 unison, 4x OS, 48k/128", oversampling: 2.0, unison: 8.0, voices: 16, block: 128, rate: 48_000.0, fx: true },
        Case { name: "16 voices, 2x8 unison, 2x OS, 48k/128", oversampling: 1.0, unison: 8.0, voices: 16, block: 128, rate: 48_000.0, fx: true },
        Case { name: "16 voices, 2x1 unison, 1x OS, 48k/128", oversampling: 0.0, unison: 1.0, voices: 16, block: 128, rate: 48_000.0, fx: true },
        Case { name: "16 voices, 2x16 unison, 4x OS, 48k/128", oversampling: 2.0, unison: 16.0, voices: 16, block: 128, rate: 48_000.0, fx: true },
        Case { name: "16 voices, 2x8 unison, 4x OS, 192k/128", oversampling: 2.0, unison: 8.0, voices: 16, block: 128, rate: 192_000.0, fx: true },
        Case { name: "16 voices, 2x8 unison, 4x OS, 48k/1 sample", oversampling: 2.0, unison: 8.0, voices: 16, block: 1, rate: 48_000.0, fx: true },
    ];

    println!("{:<52} {:>10} {:>8}", "case", "cpu/core", "peak");
    let mut failed = false;
    for (i, c) in cases.iter().enumerate() {
        let (load, peak) = run(c);
        println!("{:<52} {:>9.1}% {:>8.3}", c.name, load * 100.0, peak);
        if i == 0 && load > 0.25 {
            failed = true;
        }
    }
    println!("{}", if failed { "SPEC 9 BUDGET EXCEEDED" } else { "SPEC 9 budget met" });
    if failed {
        std::process::exit(1);
    }
}
