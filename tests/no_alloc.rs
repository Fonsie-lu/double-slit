//! The audio path never allocates (SPEC 7 / 9). Every factory preset renders
//! notes, events and expression changes with the allocator armed to abort;
//! block sizes vary from 1 sample to more than the prepared maximum.

use assert_no_alloc::assert_no_alloc;
use double_slit::dsp::presets::{build_factory_preset, num_factory_presets};
use double_slit::dsp::shared::SynthShared;
use double_slit::dsp::state::apply_patch_data;
use double_slit::dsp::synth::{NoteExpressionKind, Synth};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// nih-plug's `assert_process_allocs` feature already installs the checking
// global allocator, so it is active in this test binary too.

#[test]
fn rendering_every_preset_does_not_allocate() {
    for i in 0..num_factory_presets() {
        let shared = Arc::new(SynthShared::default());
        shared.feedback.gui_attached.store(true, Ordering::Relaxed);
        let params = apply_patch_data(&build_factory_preset(i).unwrap(), &shared);
        let mut synth = Synth::new(shared);
        synth.prepare(48_000.0, 512);
        synth.set_params(&params);
        let mut l = vec![0.0_f32; 700];
        let mut r = vec![0.0_f32; 700];
        assert_no_alloc(|| {
            for (k, block) in [1usize, 32, 128, 511, 700, 64].into_iter().cycle().take(60).enumerate() {
                match k {
                    0 => {
                        for note in [48u8, 55, 60, 64, 67, 71, 72, 76] {
                            synth.note_on(0, note, 0.7, note as i32);
                        }
                    }
                    10 => synth.set_note_expression(NoteExpressionKind::Tuning, 0, 60, 60, 0.5),
                    15 => synth.set_control_change(74, 0.8),
                    20 => synth.set_pitch_bend(0.3),
                    30 => synth.all_notes_off(),
                    _ => {}
                }
                synth.set_params(&params);
                synth.process(&mut l[..block], &mut r[..block]);
            }
        });
    }
}
