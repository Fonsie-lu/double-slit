//! Double Slit - a polyphonic wavetable synthesizer (CLAP and VST3).
//!
//! Two layers with a hard boundary, as in the C++ build this was ported from:
//! [`dsp`] holds all DSP and state and knows nothing about nih-plug; this file,
//! [`params`] and [`editor`] adapt it to the host.
//!
//! Threading (SPEC 7): host parameters are copied into the engine at the start
//! of every block, so the engine's parameter array belongs to the audio thread
//! alone. Everything the host does not own (matrix rows, wavetables, LFO
//! shapes) lives in [`dsp::shared::SynthShared`] and is published lock-free.

use nih_plug::prelude::*;
use std::num::NonZeroU32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

pub mod dsp;
pub mod editor;
pub mod params;

use dsp::params::{ParamId, CUSTOM_TABLE_CHOICE};
use dsp::shared::SynthShared;
use dsp::state::{apply_patch_data, load_patch_data, save_patch, PatchInfo};
use dsp::synth::{NoteExpressionKind, Synth};
use dsp::wavetable::Wavetable;
use params::{DoubleSlitParams, ExtraState};

/// Work that must not happen on the audio thread.
pub enum Task {
    /// Build a factory table (FFT mip chain, allocates) and publish it.
    LoadFactoryTable { osc: usize, index: usize },
}

pub struct DoubleSlit {
    params: Arc<DoubleSlitParams>,
    shared: Arc<SynthShared>,
    synth: Synth,
    sample_rate: f32,
    /// Factory table index each oscillator was last asked to load, so a
    /// change of the TABLE parameter (automation, host state, preset) is
    /// noticed once.
    requested_table: [usize; 2],
}

/// Saves and restores the patch state the host does not own, as the engine's
/// patch JSON without its parameter section.
struct PatchExtras(Arc<SynthShared>);

impl ExtraState for PatchExtras {
    fn save(&self) -> String {
        let info = PatchInfo {
            name: self.0.patch_name(),
            ..PatchInfo::default()
        };
        save_patch(&self.0, None, &info, false)
    }

    fn load(&self, json: &str) {
        // Parameters are restored by the host; only the rest is applied here.
        if let Some(data) = load_patch_data(json) {
            let _ = apply_patch_data(&data, &self.0);
        }
    }
}

impl Default for DoubleSlit {
    fn default() -> Self {
        let shared = Arc::new(SynthShared::default());
        let params = Arc::new(DoubleSlitParams::new(Arc::new(PatchExtras(shared.clone()))));
        Self {
            params,
            synth: Synth::new(shared.clone()),
            shared,
            sample_rate: 48_000.0,
            requested_table: [usize::MAX; 2],
        }
    }
}

impl DoubleSlit {
    /// Queues a factory table load when an oscillator's TABLE parameter no
    /// longer matches what was requested. Custom keeps whatever the WT editor
    /// or the patch provided.
    fn check_tables(&mut self, context: &mut impl ProcessContext<Self>) {
        for osc in 0..2 {
            let id = if osc == 0 { ParamId::OscATable } else { ParamId::OscBTable };
            let index = self.params.value(id).round() as usize;
            if index == self.requested_table[osc] {
                continue;
            }
            self.requested_table[osc] = index;
            let loaded = self.shared.tables[osc].load().factory_index();
            if index != CUSTOM_TABLE_CHOICE && loaded != Some(index) {
                context.execute_background(Task::LoadFactoryTable { osc, index });
            }
        }
    }

    fn handle_event(&mut self, event: NoteEvent<()>) {
        let s = &mut self.synth;
        match event {
            NoteEvent::NoteOn { channel, note, velocity, voice_id, .. } => {
                s.note_on(channel, note, velocity, voice_id.unwrap_or(-1))
            }
            NoteEvent::NoteOff { channel, note, voice_id, .. } => s.note_off(channel, note, voice_id.unwrap_or(-1)),
            NoteEvent::Choke { channel, note, voice_id, .. } => {
                s.note_off(channel, note, voice_id.unwrap_or(-1));
            }
            NoteEvent::PolyPressure { channel, note, pressure, .. } => s.set_poly_pressure(channel, note, pressure),
            NoteEvent::PolyVolume { channel, note, voice_id, gain, .. } => {
                s.set_note_expression(NoteExpressionKind::Volume, channel, note, voice_id.unwrap_or(-1), gain)
            }
            NoteEvent::PolyPan { channel, note, voice_id, pan, .. } => {
                // nih-plug: -1 .. 1; the engine takes 0 .. 1 with 0.5 at centre.
                s.set_note_expression(NoteExpressionKind::Pan, channel, note, voice_id.unwrap_or(-1), 0.5 + 0.5 * pan)
            }
            NoteEvent::PolyTuning { channel, note, voice_id, tuning, .. } => {
                s.set_note_expression(NoteExpressionKind::Tuning, channel, note, voice_id.unwrap_or(-1), tuning)
            }
            NoteEvent::PolyVibrato { channel, note, voice_id, vibrato, .. } => {
                s.set_note_expression(NoteExpressionKind::Vibrato, channel, note, voice_id.unwrap_or(-1), vibrato)
            }
            NoteEvent::MidiChannelPressure { pressure, .. } => s.set_channel_pressure(pressure),
            NoteEvent::MidiPitchBend { value, .. } => s.set_pitch_bend(value * 2.0 - 1.0),
            NoteEvent::MidiCC { cc, value, .. } => match cc {
                120 => s.all_sound_off(),
                123 => s.all_notes_off(),
                1 => {
                    s.set_mod_wheel(value);
                    s.set_control_change(cc, value);
                }
                _ => s.set_control_change(cc, value),
            },
            _ => {}
        }
    }
}

impl Plugin for DoubleSlit {
    const NAME: &'static str = "Double Slit";
    const VENDOR: &'static str = "Fonsie Audio";
    const URL: &'static str = "https://example.com/double-slit";
    const EMAIL: &'static str = "beat.weber@bithawk.ch";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let shared = self.shared.clone();
        Box::new(move |task| match task {
            Task::LoadFactoryTable { osc, index } => {
                shared.set_table(osc, Arc::new(Wavetable::make_factory(index)));
            }
        })
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.shared.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;
        self.synth
            .prepare(buffer_config.sample_rate as f64, buffer_config.max_buffer_size as usize);
        // Not real-time: factory tables can be built right here.
        for osc in 0..2 {
            let id = if osc == 0 { ParamId::OscATable } else { ParamId::OscBTable };
            let index = self.params.value(id).round() as usize;
            self.requested_table[osc] = index;
            if index != CUSTOM_TABLE_CHOICE && self.shared.tables[osc].load().factory_index() != Some(index) {
                self.shared.set_table(osc, Arc::new(Wavetable::make_factory(index)));
            }
        }
        self.synth.set_params(&self.params.snapshot());
        true
    }

    fn reset(&mut self) {
        self.synth.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let start = Instant::now();
        let num_samples = buffer.samples();
        self.shared
            .feedback
            .gui_attached
            .store(self.params.editor_state.is_open(), Relaxed);
        if let Some(bpm) = context.transport().tempo {
            self.synth.set_tempo(bpm as f32);
        }
        self.check_tables(context);
        self.synth.set_params(&self.params.snapshot());

        let channels = buffer.as_slice();
        let (left, right) = channels.split_at_mut(1);
        let (left, right) = (&mut *left[0], &mut *right[0]);
        left.fill(0.0);
        right.fill(0.0);

        // Render in segments split on event times for tight note timing.
        let mut pos = 0;
        while let Some(event) = context.next_event() {
            let at = (event.timing() as usize).min(num_samples);
            if at > pos {
                self.synth.process(&mut left[pos..at], &mut right[pos..at]);
                pos = at;
            }
            self.handle_event(event);
        }
        if pos < num_samples {
            self.synth.process(&mut left[pos..], &mut right[pos..]);
        }

        let budget = num_samples as f32 / self.sample_rate;
        if budget > 0.0 {
            let feedback = &self.shared.feedback;
            let load = start.elapsed().as_secs_f32() / budget;
            let smoothed = feedback.cpu_load.load(Relaxed) * 0.9 + load * 0.1;
            feedback.cpu_load.store(smoothed, Relaxed);
        }
        ProcessStatus::KeepAlive
    }
}

impl ClapPlugin for DoubleSlit {
    const CLAP_ID: &'static str = "ch.fonsie.double-slit";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("A polyphonic wavetable synthesizer");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Instrument,
        ClapFeature::Synthesizer,
        ClapFeature::Stereo,
    ];
}

impl Vst3Plugin for DoubleSlit {
    const VST3_CLASS_ID: [u8; 16] = *b"FonsieDoubleSlit";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Instrument,
        Vst3SubCategory::Synth,
        Vst3SubCategory::Stereo,
    ];
}

nih_export_clap!(DoubleSlit);
nih_export_vst3!(DoubleSlit);
