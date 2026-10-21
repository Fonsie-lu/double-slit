//! Factory presets (SPEC 5.2): 20+ patches exercising every feature block,
//! built in code. Building one is pure: it yields a [`PatchData`] value and
//! touches no engine, so the editor can resolve a preset while audio runs.

use std::sync::Arc;

use super::common::MATRIX_SLOTS;
use super::mod_matrix::{mod_source_id, mod_target_id, mod_target_index_for_param, ModCurve, ModRow, ModSource, NUM_MOD_SOURCES};
use super::params::{clamp_param, ParamId};
use super::state::PatchData;
use super::wavetable::Wavetable;

struct Row {
    source: ModSource,
    target: ParamId,
    amount: f32,
    curve: ModCurve,
}

struct Preset {
    name: &'static str,
    params: &'static [(ParamId, f32)],
    rows: Vec<Row>,
}

fn row(source: ModSource, target: ParamId, amount: f32) -> Row {
    Row { source, target, amount, curve: ModCurve::Linear }
}

fn row_curve(source: ModSource, target: ParamId, amount: f32, curve: ModCurve) -> Row {
    Row { source, target, amount, curve }
}

fn presets() -> Vec<Preset> {
    use ModSource as S;
    use ParamId::*;
    vec![
        Preset { name: "Init Saw", params: &[(OscATable, 2.0), (OscBOn, 0.0)], rows: vec![] },
        Preset {
            name: "Supersaw Wide",
            params: &[
                (OscATable, 5.0), (OscAUniVoices, 7.0), (OscAUniDetune, 28.0), (OscAUniSpread, 0.9),
                (OscBOn, 1.0), (OscBTable, 5.0), (OscBUniVoices, 5.0), (OscBUniDetune, 18.0),
                (OscBSemi, -12.0), (OscBLevel, 0.4), (FilterCutoff, 9000.0), (EnvAmpAttack, 0.02),
                (EnvAmpRelease, 0.6), (FxChorusOn, 1.0), (FxRevOn, 1.0), (FxRevMix, 0.2),
            ],
            rows: vec![row(S::ModWheel, FilterCutoff, 0.4)],
        },
        Preset {
            name: "PWM Pad",
            params: &[
                (OscATable, 6.0), (OscAUniVoices, 4.0), (OscAUniDetune, 12.0), (EnvAmpAttack, 0.6),
                (EnvAmpRelease, 1.4), (FilterType, 1.0), (FilterCutoff, 4200.0), (FilterRes, 0.2),
                (Lfo1Rate, 0.25), (FxRevOn, 1.0), (FxRevMix, 0.35), (FxRevSize, 0.7),
            ],
            rows: vec![row(S::Lfo1, OscAWtPos, 0.6)],
        },
        Preset {
            name: "Sub Bass",
            params: &[
                (OscATable, 0.0), (OscCMode, 0.0), (OscCLevel, 0.8), (OscCPitch, -12.0),
                (FilterType, 1.0), (FilterCutoff, 700.0), (EnvAmpDecay, 0.4), (EnvAmpSustain, 0.6),
                (EnvAmpRelease, 0.12), (MasterPolyphony, 8.0),
            ],
            rows: vec![row(S::EnvMod, FilterCutoff, 0.35)],
        },
        Preset {
            name: "FM Growl",
            params: &[
                (OscATable, 0.0), (OscBOn, 1.0), (OscBTable, 10.0), (OscBFmIndex, 0.45),
                (OscBSemi, 7.0), (FilterType, 7.0), (FilterCutoff, 2400.0), (FilterRes, 0.35),
                (FxDistOn, 1.0), (FxDistDrive, 0.35),
            ],
            rows: vec![row(S::Lfo1, OscBFmIndex, 0.5), row(S::EnvMod, FilterCutoff, 0.5)],
        },
        Preset {
            name: "Ladder Acid",
            params: &[
                (OscATable, 2.0), (FilterType, 7.0), (FilterCutoff, 500.0), (FilterRes, 0.7),
                (FilterDrive, 0.4), (FilterEnvAmount, 0.6), (EnvModAttack, 0.001), (EnvModDecay, 0.25),
                (EnvModSustain, 0.0), (EnvAmpDecay, 0.3), (EnvAmpSustain, 0.7), (MasterVolume, 1.5),
                (MasterPolyphony, 8.0), (FxDistOn, 1.0), (FxDistDrive, 0.6), (FxDistMix, 0.6),
                (FxDelayOn, 1.0), (FxDelayMix, 0.22),
            ],
            rows: vec![row(S::Velocity, FilterCutoff, 0.3)],
        },
        Preset {
            name: "Comb Pluck",
            params: &[
                (OscATable, 3.0), (FilterType, 8.0), (FilterCutoff, 900.0), (FilterRes, 0.75),
                (FilterKeyTrack, 1.0), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.35), (EnvAmpSustain, 0.0),
                (EnvAmpRelease, 0.25),
            ],
            rows: vec![row(S::Velocity, FilterRes, 0.25)],
        },
        Preset {
            name: "Formant Vox",
            params: &[
                (OscATable, 13.0), (FilterType, 10.0), (FilterCutoff, 1000.0), (FilterRes, 0.5),
                (FilterVowel, 0.25), (EnvAmpAttack, 0.15), (EnvAmpRelease, 0.5), (Lfo2Rate, 0.2),
            ],
            rows: vec![row(S::Lfo2, FilterVowel, 0.8), row(S::ModWheel, OscAWtPos, 1.0)],
        },
        Preset {
            name: "Noise Sweep",
            params: &[
                (OscAOn, 0.0), (OscBOn, 1.0), (OscBNoiseMode, 1.0), (OscBLevel, 0.8), (FilterType, 4.0),
                (FilterCutoff, 1200.0), (FilterRes, 0.6), (EnvAmpAttack, 0.3), (EnvAmpRelease, 1.0),
                (Lfo1Rate, 0.15), (FxRevOn, 1.0), (FxRevMix, 0.4),
            ],
            rows: vec![row(S::Lfo1, FilterCutoff, 0.8)],
        },
        Preset {
            name: "Bell Keys",
            params: &[
                (OscATable, 9.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBSemi, 12.0), (OscBLevel, 0.3),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 1.2), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.8),
                (FilterCutoff, 12000.0), (FxRevOn, 1.0), (FxRevMix, 0.28),
            ],
            rows: vec![row(S::Velocity, OscAWtPos, 0.5)],
        },
        Preset {
            name: "Organ Tones",
            params: &[
                (OscATable, 7.0), (OscAUniVoices, 2.0), (OscAUniDetune, 6.0), (EnvAmpAttack, 0.005),
                (EnvAmpSustain, 1.0), (EnvAmpRelease, 0.08), (FilterCutoff, 14000.0), (FxChorusOn, 1.0),
                (FxChorusRate, 5.0), (FxChorusDepth, 0.25),
            ],
            rows: vec![row(S::ModWheel, OscAWtPos, 1.0)],
        },
        Preset {
            name: "Wavefold Lead",
            params: &[
                (OscATable, 11.0), (OscAUniVoices, 3.0), (OscAUniDetune, 10.0), (FilterType, 0.0),
                (FilterCutoff, 7000.0), (EnvAmpAttack, 0.01), (EnvAmpRelease, 0.2), (MasterPolyphony, 8.0),
                (FxDelayOn, 1.0), (FxDelayMix, 0.25), (FxDelayPingPong, 1.0),
            ],
            rows: vec![row(S::Aftertouch, OscAWtPos, 0.7)],
        },
        Preset {
            name: "Sync Screech",
            params: &[
                (OscATable, 2.0), (OscBOn, 1.0), (OscBTable, 12.0), (OscBSyncMode, 1.0),
                (OscBSyncRatio, 3.0), (OscBLevel, 0.7), (FilterCutoff, 9000.0), (EnvModDecay, 0.5),
            ],
            rows: vec![row(S::EnvMod, OscBWtPos, 0.9), row(S::Lfo1, OscBFine, 0.15)],
        },
        Preset {
            name: "Vowel Morph Pad",
            params: &[
                (OscATable, 13.0), (OscAUniVoices, 5.0), (OscAUniDetune, 14.0), (EnvAmpAttack, 0.9),
                (EnvAmpRelease, 1.8), (FilterCutoff, 6000.0), (Lfo1Rate, 0.1), (FxRevOn, 1.0),
                (FxRevSize, 0.85), (FxRevMix, 0.45),
            ],
            rows: vec![row_curve(S::Lfo1, OscAWtPos, 1.0, ModCurve::Log)],
        },
        Preset {
            name: "S&H Bass",
            params: &[
                (OscATable, 3.0), (FilterType, 1.0), (FilterCutoff, 800.0), (FilterRes, 0.4),
                (Lfo2Shape, 5.0), (Lfo2SyncMode, 1.0), (Lfo2SyncDiv, 4.0), (EnvAmpSustain, 0.9),
                (MasterPolyphony, 8.0),
            ],
            rows: vec![row_curve(S::Lfo2, FilterCutoff, 0.5, ModCurve::Switch)],
        },
        Preset {
            name: "Tempo Gate Pluck",
            params: &[
                (OscATable, 8.0), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.12), (EnvAmpSustain, 0.4),
                (Lfo1SyncMode, 1.0), (Lfo1SyncDiv, 4.0), (Lfo1Shape, 4.0), (FxDelayOn, 1.0),
                (FxDelaySyncDiv, 3.0), (FxDelayMix, 0.3),
            ],
            rows: vec![row(S::Lfo1, OscALevel, -0.6)],
        },
        Preset {
            name: "Reverb Bloom",
            params: &[
                (OscATable, 9.0), (EnvAmpAttack, 1.5), (EnvAmpRelease, 2.5), (FilterCutoff, 5000.0),
                (FxRevOn, 1.0), (FxRevSize, 0.95), (FxRevMix, 0.6), (FxRevPreDelay, 40.0),
            ],
            rows: vec![row(S::EnvAmp, FxRevMix, 0.3)],
        },
        Preset {
            name: "Dub Chord Delay",
            params: &[
                (OscATable, 6.0), (OscBOn, 1.0), (OscBTable, 2.0), (OscBSemi, 7.0), (OscBLevel, 0.5),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.2), (EnvAmpSustain, 0.0), (FxDelayOn, 1.0),
                (FxDelayFeedback, 0.6), (FxDelayMix, 0.45), (FxDelayTone, 0.35), (FxDelayPingPong, 1.0),
            ],
            rows: vec![row(S::ModWheel, FxDelayTimeMs, 0.3)],
        },
        Preset {
            name: "Chorus Strings",
            params: &[
                (OscATable, 5.0), (OscAUniVoices, 6.0), (OscAUniDetune, 20.0), (OscAUniSpread, 0.8),
                (EnvAmpAttack, 0.35), (EnvAmpRelease, 0.9), (FilterCutoff, 6500.0), (FxChorusOn, 1.0),
                (FxChorusDepth, 0.6), (FxChorusMix, 0.6), (FxRevOn, 1.0), (FxRevMix, 0.25),
            ],
            rows: vec![row(S::Lfo2, OscAUniDetune, 0.3)],
        },
        Preset {
            name: "Distorted Reese",
            params: &[
                (OscATable, 2.0), (OscBOn, 1.0), (OscBTable, 2.0), (OscBFine, 18.0), (OscAFine, -18.0),
                (FilterType, 1.0), (FilterCutoff, 1200.0), (FilterDrive, 0.5), (FxDistOn, 1.0),
                (FxDistDrive, 0.5), (FxDistTone, 0.4), (MasterPolyphony, 8.0),
            ],
            rows: vec![row(S::Lfo1, FilterCutoff, 0.3), row(S::ModWheel, FxDistDrive, 0.5)],
        },
        Preset {
            name: "Pressure Swell",
            params: &[
                (OscATable, 10.0), (EnvAmpAttack, 0.05), (EnvAmpSustain, 0.9), (EnvAmpRelease, 0.7),
                (FilterCutoff, 3000.0), (FilterRes, 0.3),
            ],
            rows: vec![row(S::PolyAftertouch, FilterCutoff, 0.7), row(S::PolyAftertouch, OscAWtPos, 0.5)],
        },
        Preset {
            name: "Trance Detune Stack",
            params: &[
                (OscATable, 5.0), (OscAUniVoices, 16.0), (OscAUniDetune, 35.0), (OscAUniSpread, 1.0),
                (OscAUniBlend, 0.85), (MasterOversampling, 2.0), (FilterCutoff, 11000.0),
                (EnvAmpAttack, 0.01), (EnvAmpRelease, 0.5), (FxRevOn, 1.0), (FxRevMix, 0.22),
            ],
            rows: vec![row(S::EnvMod, OscAUniDetune, 0.4)],
        },
        Preset {
            name: "Random Pluck",
            params: &[
                (OscATable, 8.0), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.25), (EnvAmpSustain, 0.0),
                (FilterType, 1.0), (FilterCutoff, 2500.0), (FilterKeyTrack, 0.6),
            ],
            rows: vec![row(S::RandomNote, OscAWtPos, 0.6), row(S::RandomVoiceCycle, OscAPan, 0.5)],
        },
        Preset {
            name: "Shape Shifter Lead",
            params: &[
                (OscATable, 15.0), (OscAUniVoices, 3.0), (OscAUniDetune, 8.0), (OscBOn, 0.0),
                (FilterType, 1.0), (FilterCutoff, 6000.0), (FilterRes, 0.25), (EnvAmpAttack, 0.005),
                (EnvAmpRelease, 0.25), (MasterPolyphony, 8.0), (MasterGlide, 0.06), (Lfo1Rate, 5.5),
                (FxDelayOn, 1.0), (FxDelayMix, 0.2),
            ],
            rows: vec![row(S::ModWheel, OscAWtPos, 1.0), row(S::Lfo1, OscAFine, 0.05)],
        },
        Preset {
            name: "Reso Sweep Bass",
            params: &[
                (OscATable, 16.0), (OscBOn, 0.0), (OscCMode, 0.0), (OscCLevel, 0.5), (OscCPitch, -12.0),
                (FilterType, 1.0), (FilterCutoff, 3500.0), (EnvModAttack, 0.0), (EnvModDecay, 0.3),
                (EnvModSustain, 0.0), (EnvAmpDecay, 0.4), (EnvAmpSustain, 0.7), (EnvAmpRelease, 0.12),
                (MasterPolyphony, 8.0),
            ],
            rows: vec![row(S::EnvMod, OscAWtPos, 0.8), row(S::Velocity, OscAWtPos, 0.2)],
        },
        Preset {
            name: "Tube Drive Keys",
            params: &[
                (OscATable, 17.0), (OscAWtPos, 15.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBOctave, 1.0),
                (OscBLevel, 0.25), (EnvAmpAttack, 0.002), (EnvAmpDecay, 1.4), (EnvAmpSustain, 0.3),
                (EnvAmpRelease, 0.4), (FilterType, 0.0), (FilterCutoff, 8000.0), (FilterKeyTrack, 0.5),
                (FxChorusOn, 1.0), (FxChorusMix, 0.35), (FxRevOn, 1.0), (FxRevMix, 0.18),
            ],
            rows: vec![row(S::Velocity, OscAWtPos, 0.7), row(S::EnvAmp, OscAWtPos, 0.25)],
        },
        Preset {
            name: "Harmonic Rise Pad",
            params: &[
                (OscATable, 18.0), (OscAUniVoices, 5.0), (OscAUniDetune, 12.0), (OscAUniSpread, 0.8),
                (OscBOn, 1.0), (OscBTable, 18.0), (OscBOctave, -1.0), (OscBLevel, 0.4), (EnvAmpAttack, 1.2),
                (EnvAmpSustain, 1.0), (EnvAmpRelease, 2.0), (EnvModAttack, 3.0), (EnvModDecay, 2.0),
                (EnvModSustain, 0.6), (FilterCutoff, 9000.0), (FxRevOn, 1.0), (FxRevSize, 0.8),
                (FxRevMix, 0.4),
            ],
            rows: vec![row(S::EnvMod, OscAWtPos, 0.9), row(S::EnvMod, OscBWtPos, 0.6), row(S::ModWheel, FilterCutoff, -0.3)],
        },
        Preset {
            name: "Phase Dist Pluck",
            params: &[
                (OscATable, 19.0), (OscBOn, 0.0), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.45),
                (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.3), (EnvModAttack, 0.0), (EnvModDecay, 0.2),
                (EnvModSustain, 0.0), (FilterType, 0.0), (FilterCutoff, 5000.0), (FilterKeyTrack, 0.7),
                (FxDelayOn, 1.0), (FxDelaySyncMode, 1.0), (FxDelaySyncDiv, 7.0), (FxDelayMix, 0.25),
                (FxDelayPingPong, 1.0),
            ],
            rows: vec![row_curve(S::EnvMod, OscAWtPos, 0.9, ModCurve::Exp), row(S::Velocity, OscAWtPos, 0.1)],
        },
        Preset {
            name: "FM Glass Piano",
            params: &[
                (OscATable, 20.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBOctave, 2.0), (OscBLevel, 0.15),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 2.0), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.6),
                (EnvModAttack, 0.0), (EnvModDecay, 0.6), (EnvModSustain, 0.1), (FilterCutoff, 14000.0),
                (FxChorusOn, 1.0), (FxChorusMix, 0.25), (FxRevOn, 1.0), (FxRevMix, 0.3),
            ],
            rows: vec![row(S::EnvMod, OscAWtPos, 0.55), row(S::Velocity, OscAWtPos, 0.35)],
        },
        Preset {
            name: "Crushed Chip Lead",
            params: &[
                (OscATable, 21.0), (OscAWtPos, 45.0), (OscBOn, 1.0), (OscBTable, 21.0), (OscBWtPos, 80.0),
                (OscBOctave, -1.0), (OscBLevel, 0.4), (EnvAmpAttack, 0.0), (EnvAmpSustain, 0.8),
                (EnvAmpRelease, 0.1), (Lfo1SyncMode, 1.0), (Lfo1SyncDiv, 4.0), (Lfo1Shape, 4.0),
                (MasterPolyphony, 8.0), (MasterGlide, 0.03), (FxDelayOn, 1.0), (FxDelaySyncMode, 1.0),
                (FxDelaySyncDiv, 3.0), (FxDelayMix, 0.3), (FxDelayPingPong, 1.0),
            ],
            rows: vec![row_curve(S::Lfo1, OscAWtPos, 0.3, ModCurve::Switch), row(S::ModWheel, OscAWtPos, 0.5)],
        },
        Preset {
            name: "Spectral Drift",
            params: &[
                (OscATable, 22.0), (OscAUniVoices, 4.0), (OscAUniDetune, 10.0), (OscAUniSpread, 1.0),
                (OscBOn, 1.0), (OscBTable, 22.0), (OscBWtPos, 60.0), (OscBSemi, 7.0), (OscBLevel, 0.45),
                (OscBPan, 0.3), (EnvAmpAttack, 1.8), (EnvAmpSustain, 1.0), (EnvAmpRelease, 3.0),
                (Lfo1Rate, 0.07), (Lfo2Rate, 0.11), (FilterCutoff, 7000.0), (FxRevOn, 1.0),
                (FxRevSize, 0.95), (FxRevMix, 0.5), (FxRevPreDelay, 30.0),
            ],
            rows: vec![row(S::Lfo1, OscAWtPos, 0.8), row(S::Lfo2, OscBWtPos, -0.8), row(S::Lfo2, OscAPan, 0.4)],
        },
        // Drums are tuned for middle C: the pitch tracks the key like any patch.
        Preset {
            name: "808 Kick",
            params: &[
                (OscATable, 0.0), (OscAOctave, -2.0), (OscASemi, -3.0), (OscAPhaseMode, 2.0), (OscBOn, 0.0),
                (FilterOn, 0.0), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.9), (EnvAmpSustain, 0.0),
                (EnvAmpRelease, 0.9), (EnvAmpDecayCurve, 0.4), (EnvAmpReleaseCurve, 0.4), (EnvModAttack, 0.0),
                (EnvModDecay, 0.12), (EnvModSustain, 0.0), (EnvModRelease, 0.12), (EnvModDecayCurve, 0.8),
                (EnvModReleaseCurve, 0.8), (FxDistOn, 1.0), (FxDistDrive, 0.15), (FxDistMix, 0.5),
            ],
            rows: vec![row(S::EnvMod, OscAOctave, 0.35), row(S::Velocity, FxDistDrive, 0.2)],
        },
        Preset {
            name: "808 Snare",
            params: &[
                (OscATable, 0.0), (OscAOctave, -1.0), (OscASemi, 5.0), (OscALevel, 0.0), (OscAPhaseMode, 2.0),
                (OscBOn, 1.0), (OscBTable, 0.0), (OscBSemi, 4.0), (OscBLevel, 0.0), (OscBPhaseMode, 2.0),
                (OscCMode, 2.0), (OscCLevel, 0.6), (FilterType, 2.0), (FilterCutoff, 160.0), (FilterRes, 0.0),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.28), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.28),
                (EnvAmpDecayCurve, 0.5), (EnvAmpReleaseCurve, 0.5), (EnvModAttack, 0.0), (EnvModDecay, 0.11),
                (EnvModSustain, 0.0), (EnvModRelease, 0.11), (EnvModDecayCurve, 0.5),
            ],
            rows: vec![
                row(S::EnvMod, OscALevel, 0.9), row(S::EnvMod, OscBLevel, 0.5), row(S::EnvMod, OscAOctave, 0.04),
                row(S::Velocity, OscCLevel, 0.3),
            ],
        },
        Preset {
            name: "808 Clap",
            // One short noise burst; the 11 ms delay turns it into the flurry
            // of hits and the small room adds the tail.
            params: &[
                (OscAOn, 0.0), (OscBOn, 0.0), (OscCMode, 2.0), (OscCLevel, 1.0), (FilterType, 2.0),
                (FilterCutoff, 800.0), (FilterRes, 0.25), (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.012),
                (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.012), (MasterVolume, 1.5), (FxDistOn, 1.0),
                (FxDistDrive, 0.5), (FxDelayOn, 1.0), (FxDelaySyncMode, 0.0), (FxDelayTimeMs, 11.0), (FxDelayFeedback, 0.55),
                (FxDelayTone, 0.6), (FxDelayMix, 0.5), (FxRevOn, 1.0), (FxRevSize, 0.35), (FxRevDamp, 0.6),
                (FxRevMix, 0.45),
            ],
            rows: vec![row(S::Velocity, FilterCutoff, 0.15), row(S::RandomNote, FilterCutoff, 0.05)],
        },
        Preset {
            name: "808 Tom",
            params: &[
                (OscATable, 0.0), (OscAOctave, -1.0), (OscAPhaseMode, 2.0), (OscBOn, 0.0), (OscCMode, 3.0),
                (OscCLevel, 0.05), (FilterType, 0.0), (FilterCutoff, 3000.0), (FilterRes, 0.0),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 0.55), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.55),
                (EnvAmpDecayCurve, 0.4), (EnvAmpReleaseCurve, 0.4), (EnvModAttack, 0.0), (EnvModDecay, 0.25),
                (EnvModSustain, 0.0), (EnvModRelease, 0.25), (EnvModDecayCurve, 0.6),
            ],
            rows: vec![row(S::EnvMod, OscAOctave, 0.1), row(S::Velocity, OscAOctave, 0.02)],
        },
        Preset {
            name: "303 Bass",
            params: &[
                (OscATable, 2.0), (OscAOctave, -1.0), (OscBOn, 0.0), (FilterType, 7.0), (FilterCutoff, 500.0),
                (FilterRes, 0.82), (FilterDrive, 0.35), (FilterEnvAmount, 0.55), (FilterKeyTrack, 0.3),
                (EnvModAttack, 0.0), (EnvModDecay, 0.22), (EnvModSustain, 0.0), (EnvModDecayCurve, 0.4),
                (EnvAmpAttack, 0.002), (EnvAmpDecay, 0.6), (EnvAmpSustain, 0.6), (EnvAmpRelease, 0.04),
                (MasterVolume, 1.4), (MasterPolyphony, 8.0), (MasterGlide, 0.07), (FxDistOn, 1.0),
                (FxDistDrive, 0.6), (FxDistMix, 0.6),
            ],
            rows: vec![
                row(S::Velocity, FilterCutoff, 0.25), row(S::Velocity, FilterRes, 0.1),
                row(S::ModWheel, FilterCutoff, 0.5),
            ],
        },
        Preset {
            name: "Hypersaw",
            params: &[
                (OscATable, 5.0), (OscAUniVoices, 16.0), (OscAUniDetune, 45.0), (OscAUniSpread, 1.0),
                (OscAUniBlend, 0.9), (OscAPhaseMode, 1.0), (OscBOn, 1.0), (OscBTable, 2.0), (OscBOctave, -1.0),
                (OscBUniVoices, 16.0), (OscBUniDetune, 38.0), (OscBUniSpread, 1.0), (OscBUniBlend, 0.9),
                (OscBLevel, 0.5), (OscBPhaseMode, 1.0), (FilterType, 0.0), (FilterCutoff, 12000.0),
                (EnvAmpAttack, 0.01), (EnvAmpSustain, 1.0), (EnvAmpRelease, 0.45), (FxChorusOn, 1.0),
                (FxChorusMix, 0.3), (FxDelayOn, 1.0), (FxDelaySyncMode, 1.0), (FxDelaySyncDiv, 7.0),
                (FxDelayMix, 0.18), (FxDelayPingPong, 1.0), (FxRevOn, 1.0), (FxRevSize, 0.8), (FxRevMix, 0.28),
            ],
            rows: vec![row(S::ModWheel, OscAUniDetune, 0.4), row(S::ModWheel, OscBUniDetune, 0.4)],
        },
        Preset {
            name: "Growl Bass",
            params: &[
                (OscATable, 10.0), (OscAOctave, -1.0), (OscAUniVoices, 2.0), (OscAUniDetune, 8.0),
                (OscBOn, 1.0), (OscBTable, 20.0), (OscBOctave, -1.0), (OscBLevel, 0.5), (OscCMode, 0.0),
                (OscCLevel, 0.45), (OscCPitch, -12.0), (FilterType, 10.0), (FilterCutoff, 1200.0),
                (FilterRes, 0.55), (FilterMix, 0.85), (EnvAmpAttack, 0.003), (EnvAmpSustain, 1.0),
                (EnvAmpRelease, 0.12), (Lfo1SyncMode, 1.0), (Lfo1SyncDiv, 3.0), (Lfo1Retrig, 2.0),
                (MasterPolyphony, 8.0), (FxDistOn, 1.0), (FxDistDrive, 0.6), (FxDistTone, 0.55),
            ],
            rows: vec![
                row(S::Lfo1, OscAWtPos, 0.8), row(S::Lfo1, OscBWtPos, 0.6), row(S::Lfo1, FilterVowel, 0.9),
                row(S::ModWheel, FxDistDrive, 0.3),
            ],
        },
        Preset {
            name: "Fog Horn",
            // Both oscillators sit two semitones flat and the amp envelope
            // bends them up to pitch: the horn groans in and sags out.
            params: &[
                (OscATable, 3.0), (OscAOctave, -2.0), (OscASemi, -2.0), (OscBOn, 1.0), (OscBTable, 2.0),
                (OscBOctave, -2.0), (OscBSemi, -2.0), (OscBFine, 9.0), (OscBLevel, 0.6), (OscCMode, 0.0),
                (OscCLevel, 0.3), (FilterType, 1.0), (FilterCutoff, 420.0), (FilterRes, 0.3),
                (FilterEnvAmount, 0.25), (EnvModAttack, 0.5), (EnvModDecay, 1.0), (EnvModSustain, 0.6),
                (EnvModRelease, 1.2), (EnvAmpAttack, 0.35), (EnvAmpSustain, 1.0), (EnvAmpRelease, 1.6),
                (Lfo2Rate, 4.5), (MasterPolyphony, 8.0), (FxDistOn, 1.0), (FxDistDrive, 0.25),
                (FxRevOn, 1.0), (FxRevSize, 0.95), (FxRevDamp, 0.7), (FxRevMix, 0.4),
            ],
            rows: vec![
                row(S::EnvAmp, OscASemi, 0.08), row(S::EnvAmp, OscBSemi, 0.08), row(S::Lfo2, OscAFine, 0.01),
            ],
        },
        // Acoustic instruments.
        Preset {
            name: "Acoustic Piano",
            // A long decay with a steep curve drops fast after the hammer and
            // then rings on; the filter closes as the note dies.
            params: &[
                (OscATable, 5.0), (OscAWtPos, 55.0), (OscAPhaseMode, 2.0), (OscBOn, 1.0), (OscBTable, 1.0),
                (OscBOctave, 1.0), (OscBFine, 3.0), (OscBLevel, 0.35), (OscBPhaseMode, 2.0),
                (FilterType, 0.0), (FilterCutoff, 1100.0), (FilterRes, 0.05), (FilterKeyTrack, 0.8),
                (EnvModAttack, 0.0), (EnvModDecay, 1.8), (EnvModSustain, 0.0), (EnvModDecayCurve, 0.6),
                (EnvAmpAttack, 0.001), (EnvAmpDecay, 6.0), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.35),
                (EnvAmpDecayCurve, 0.75), (EnvAmpVelAmount, 0.8), (FxRevOn, 1.0), (FxRevSize, 0.55),
                (FxRevMix, 0.18),
            ],
            rows: vec![row(S::EnvMod, FilterCutoff, 0.45), row(S::Velocity, FilterCutoff, 0.3)],
        },
        Preset {
            name: "Electric Piano",
            // The FM table's index decays with the mod envelope, as on a DX tine piano.
            params: &[
                (OscATable, 20.0), (OscAWtPos, 10.0), (OscAPhaseMode, 2.0), (OscBOn, 1.0), (OscBTable, 0.0),
                (OscBLevel, 0.35), (OscBPhaseMode, 2.0), (FilterCutoff, 9000.0), (EnvModAttack, 0.0),
                (EnvModDecay, 0.7), (EnvModSustain, 0.0), (EnvModDecayCurve, 0.5), (EnvAmpAttack, 0.001),
                (EnvAmpDecay, 3.5), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.3), (EnvAmpDecayCurve, 0.5),
                (EnvAmpVelAmount, 0.7), (Lfo1Rate, 4.0), (Lfo1Retrig, 0.0), (FxChorusOn, 1.0),
                (FxChorusMix, 0.35), (FxRevOn, 1.0), (FxRevMix, 0.15),
            ],
            rows: vec![
                row(S::EnvMod, OscAWtPos, 0.4), row(S::Velocity, OscAWtPos, 0.3), row(S::Lfo1, MasterPan, 0.25),
            ],
        },
        Preset {
            name: "Nylon Guitar",
            params: &[
                (OscATable, 5.0), (OscAWtPos, 40.0), (OscAPhaseMode, 2.0), (OscBOn, 0.0), (FilterType, 0.0),
                (FilterCutoff, 1200.0), (FilterRes, 0.1), (FilterKeyTrack, 0.6), (FilterEnvAmount, 0.45),
                (EnvModAttack, 0.0), (EnvModDecay, 0.25), (EnvModSustain, 0.0), (EnvModRelease, 0.25),
                (EnvAmpAttack, 0.0), (EnvAmpDecay, 3.0), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.25),
                (EnvAmpDecayCurve, 0.4), (EnvAmpVelAmount, 0.7), (FxChorusOn, 1.0), (FxChorusMix, 0.2),
                (FxRevOn, 1.0), (FxRevSize, 0.45), (FxRevMix, 0.18),
            ],
            rows: vec![row(S::Velocity, FilterCutoff, 0.3)],
        },
        Preset {
            name: "Steel Guitar",
            // Plucked string: a short noise-and-saw burst excites a comb filter
            // tuned to the note (key track 1, middle C at 261.6 Hz). The comb
            // has no damping, so every overtone rings on: a bright steel string.
            params: &[
                (OscATable, 2.0), (OscALevel, 0.0), (OscAPhaseMode, 2.0), (OscBOn, 0.0), (OscCMode, 3.0),
                (OscCLevel, 0.0), (FilterType, 8.0), (FilterCutoff, 261.63), (FilterRes, 1.0),
                (FilterKeyTrack, 1.0), (EnvModAttack, 0.0), (EnvModDecay, 0.02), (EnvModSustain, 0.0),
                (EnvModRelease, 0.02), (EnvAmpAttack, 0.0), (EnvAmpDecay, 3.0), (EnvAmpSustain, 0.0),
                (EnvAmpRelease, 0.25), (EnvAmpDecayCurve, 0.4), (EnvAmpVelAmount, 0.7), (MasterVolume, 1.5),
                (FxDistOn, 1.0), (FxDistDrive, 0.35), (FxDistTone, 0.4), (FxChorusOn, 1.0),
                (FxChorusMix, 0.2), (FxRevOn, 1.0), (FxRevSize, 0.45), (FxRevMix, 0.18),
            ],
            rows: vec![
                row(S::EnvMod, OscALevel, 1.0), row(S::EnvMod, OscCLevel, 0.8), row(S::Velocity, OscCLevel, 0.2),
            ],
        },
        Preset {
            name: "Concert Harp",
            params: &[
                (OscATable, 1.0), (OscAPhaseMode, 2.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBOctave, 1.0),
                (OscBLevel, 0.25), (OscBPhaseMode, 2.0), (FilterType, 0.0), (FilterCutoff, 1800.0),
                (FilterRes, 0.1), (FilterKeyTrack, 0.6), (EnvModAttack, 0.0), (EnvModDecay, 0.4),
                (EnvModSustain, 0.0), (EnvAmpAttack, 0.003), (EnvAmpDecay, 3.0), (EnvAmpSustain, 0.0),
                (EnvAmpRelease, 1.5), (EnvAmpDecayCurve, 0.5), (EnvAmpReleaseCurve, 0.5), (FxChorusOn, 1.0),
                (FxChorusMix, 0.15), (FxRevOn, 1.0), (FxRevSize, 0.8), (FxRevMix, 0.35),
            ],
            rows: vec![row(S::EnvMod, FilterCutoff, 0.35), row(S::Velocity, FilterCutoff, 0.25)],
        },
        Preset {
            name: "Xylophone",
            // Sounds an octave above the key, like the real instrument; the bar's
            // overtone at three times the pitch fades first, with a mallet tick.
            params: &[
                (OscATable, 0.0), (OscAOctave, 1.0), (OscAPhaseMode, 2.0), (OscBOn, 1.0), (OscBTable, 0.0),
                (OscBOctave, 2.0), (OscBSemi, 7.0), (OscBLevel, 0.0), (OscBPhaseMode, 2.0), (OscCMode, 2.0),
                (OscCLevel, 0.0), (FilterType, 0.0), (FilterCutoff, 9000.0), (EnvModAttack, 0.0),
                (EnvModDecay, 0.08), (EnvModSustain, 0.0), (EnvModRelease, 0.08), (EnvAmpAttack, 0.0),
                (EnvAmpDecay, 0.45), (EnvAmpSustain, 0.0), (EnvAmpRelease, 0.45), (EnvAmpDecayCurve, 0.5),
                (EnvAmpReleaseCurve, 0.5), (EnvAmpVelAmount, 0.7), (FxRevOn, 1.0), (FxRevSize, 0.5),
                (FxRevMix, 0.2),
            ],
            rows: vec![row(S::EnvMod, OscBLevel, 0.45), row(S::EnvMod, OscCLevel, 0.12)],
        },
        Preset {
            name: "Marimba",
            params: &[
                (OscATable, 0.0), (OscAPhaseMode, 2.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBOctave, 2.0),
                (OscBLevel, 0.0), (OscBPhaseMode, 2.0), (FilterType, 0.0), (FilterCutoff, 6000.0),
                (EnvModAttack, 0.0), (EnvModDecay, 0.15), (EnvModSustain, 0.0), (EnvModRelease, 0.15),
                (EnvAmpAttack, 0.002), (EnvAmpDecay, 1.1), (EnvAmpSustain, 0.0), (EnvAmpRelease, 1.1),
                (EnvAmpDecayCurve, 0.5), (EnvAmpReleaseCurve, 0.5), (EnvAmpVelAmount, 0.7), (FxRevOn, 1.0),
                (FxRevSize, 0.6), (FxRevMix, 0.22),
            ],
            rows: vec![row(S::EnvMod, OscBLevel, 0.35), row(S::Velocity, OscBLevel, 0.1)],
        },
        Preset {
            name: "French Horn",
            params: &[
                (OscATable, 2.0), (OscAOctave, -1.0), (OscBOn, 1.0), (OscBTable, 2.0), (OscBOctave, -1.0),
                (OscBFine, 6.0), (OscBLevel, 0.6), (FilterType, 1.0), (FilterCutoff, 650.0), (FilterRes, 0.1),
                (FilterKeyTrack, 0.5), (FilterEnvAmount, 0.3), (EnvModAttack, 0.08), (EnvModDecay, 0.4),
                (EnvModSustain, 0.5), (EnvModRelease, 0.25), (EnvAmpAttack, 0.06), (EnvAmpSustain, 0.9),
                (EnvAmpRelease, 0.3), (MasterVolume, 0.6), (Lfo1Rate, 5.0), (FxRevOn, 1.0), (FxRevSize, 0.75), (FxRevMix, 0.3),
            ],
            rows: vec![
                row(S::Velocity, FilterCutoff, 0.35), row(S::ModWheel, FilterCutoff, 0.4),
                row(S::Lfo1, OscAFine, 0.006), row(S::Lfo1, OscBFine, 0.006),
            ],
        },
        Preset {
            name: "Flute",
            params: &[
                (OscATable, 1.0), (OscBOn, 1.0), (OscBTable, 0.0), (OscBOctave, 1.0), (OscBLevel, 0.15),
                (OscCMode, 3.0), (OscCLevel, 0.06), (FilterType, 0.0), (FilterCutoff, 2600.0),
                (FilterKeyTrack, 0.6), (EnvAmpAttack, 0.08), (EnvAmpSustain, 1.0), (EnvAmpRelease, 0.15),
                (Lfo1Rate, 5.2), (FxRevOn, 1.0), (FxRevSize, 0.65), (FxRevMix, 0.28),
            ],
            rows: vec![
                row(S::Lfo1, OscAFine, 0.01), row(S::Lfo1, OscALevel, 0.05), row(S::ModWheel, OscCLevel, 0.15),
            ],
        },
    ]
}

/// Names only; the full list is rebuilt on demand, which is cheap next to the
/// wavetables a preset needs anyway.
const NAMES: [&str; 48] = [
    "Init Saw",
    "Supersaw Wide",
    "PWM Pad",
    "Sub Bass",
    "FM Growl",
    "Ladder Acid",
    "Comb Pluck",
    "Formant Vox",
    "Noise Sweep",
    "Bell Keys",
    "Organ Tones",
    "Wavefold Lead",
    "Sync Screech",
    "Vowel Morph Pad",
    "S&H Bass",
    "Tempo Gate Pluck",
    "Reverb Bloom",
    "Dub Chord Delay",
    "Chorus Strings",
    "Distorted Reese",
    "Pressure Swell",
    "Trance Detune Stack",
    "Random Pluck",
    "Shape Shifter Lead",
    "Reso Sweep Bass",
    "Tube Drive Keys",
    "Harmonic Rise Pad",
    "Phase Dist Pluck",
    "FM Glass Piano",
    "Crushed Chip Lead",
    "Spectral Drift",
    "808 Kick",
    "808 Snare",
    "808 Clap",
    "808 Tom",
    "303 Bass",
    "Hypersaw",
    "Growl Bass",
    "Fog Horn",
    "Acoustic Piano",
    "Electric Piano",
    "Nylon Guitar",
    "Steel Guitar",
    "Concert Harp",
    "Xylophone",
    "Marimba",
    "French Horn",
    "Flute",
];

pub fn num_factory_presets() -> usize {
    NAMES.len()
}

/// Empty for an out-of-range index.
pub fn factory_preset_name(index: usize) -> &'static str {
    NAMES.get(index).copied().unwrap_or("")
}

/// Resolves a preset into plain values without touching an engine: defaults,
/// the preset's overrides, its matrix rows, and the factory tables its table
/// parameters name. `None` for an out-of-range index.
pub fn build_factory_preset(index: usize) -> Option<PatchData> {
    let preset = presets().into_iter().nth(index)?;
    let mut out = PatchData::default();
    out.info.name = preset.name.to_owned();

    for &(id, value) in preset.params {
        out.params[id.index()] = clamp_param(id, value);
    }

    for r in preset.rows.iter().take(MATRIX_SLOTS) {
        let source = r.source.index().min(NUM_MOD_SOURCES - 1);
        let target = mod_target_index_for_param(r.target);
        out.rows.push(ModRow {
            source,
            target,
            amount: r.amount.clamp(-1.0, 1.0),
            curve: r.curve,
            muted: false,
            unresolved: target.is_none(),
            source_id_raw: mod_source_id(source).to_owned(),
            target_id_raw: target.map(mod_target_id).unwrap_or("").to_owned(),
        });
    }

    for slot in 0..2 {
        let id = if slot == 0 { ParamId::OscATable } else { ParamId::OscBTable };
        out.tables[slot] = Some(Arc::new(Wavetable::make_factory(out.params[id.index()] as usize)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::mod_matrix::{mod_target_from_id, ModMatrix};
    use crate::dsp::params::{default_value, NUM_PARAMS};
    use crate::dsp::shared::SynthShared;
    use crate::dsp::state::apply_patch_data;

    #[test]
    fn names_match_the_preset_list() {
        let list = presets();
        assert_eq!(list.len(), NAMES.len());
        for (p, n) in list.iter().zip(NAMES.iter()) {
            assert_eq!(p.name, *n);
        }
    }

    #[test]
    fn at_least_twenty_presets_with_unique_names() {
        assert!(num_factory_presets() >= 20);
        let mut names: Vec<&str> = Vec::new();
        for i in 0..num_factory_presets() {
            let name = factory_preset_name(i);
            assert!(!name.is_empty(), "{i}");
            assert!(!names.contains(&name), "{name}");
            names.push(name);
        }
        assert_eq!(factory_preset_name(num_factory_presets()), "");
    }

    /// The plugin resolves presets off the audio thread while audio runs, so this
    /// has to be pure: parameters come back as values, not as writes into an engine.
    #[test]
    fn building_a_preset_touches_no_engine() {
        for i in 0..num_factory_presets() {
            let data = build_factory_preset(i).unwrap();
            let name = factory_preset_name(i);
            assert_eq!(data.info.name, name);
            assert!(data.rows.len() <= MATRIX_SLOTS, "{name}");
            for id in ParamId::all() {
                let v = data.params[id.index()];
                assert!(v.is_finite(), "{}", id.desc().id_string);
                assert_eq!(v, clamp_param(id, v), "{}", id.desc().id_string);
            }
            for slot in 0..2 {
                let id = if slot == 0 { ParamId::OscATable } else { ParamId::OscBTable };
                let table = data.tables[slot].as_ref().unwrap_or_else(|| panic!("{name}"));
                assert_eq!(table.factory_index(), Some(data.params[id.index()] as usize));
            }
            for r in &data.rows {
                assert!(!r.unresolved, "{name} / {}", r.target_id_raw);
            }
        }
        assert!(build_factory_preset(num_factory_presets()).is_none());
    }

    /// Single-threaded equivalent of the C++ `PresetsUseValidMatrixRows`, read
    /// back through a live matrix after applying the preset.
    #[test]
    fn presets_use_valid_matrix_rows() {
        for i in 0..num_factory_presets() {
            let shared = SynthShared::default();
            apply_patch_data(&build_factory_preset(i).unwrap(), &shared);
            let matrix = shared.matrix.lock().unwrap();
            for r in matrix.rows() {
                assert!(!r.unresolved, "{}", factory_preset_name(i));
                assert!(r.target.is_some(), "{}", factory_preset_name(i));
                assert!(r.source > 0, "{}", factory_preset_name(i));
            }
        }
    }

    #[test]
    fn applying_resolved_data_twice_is_stable() {
        // C++ `ApplyingResolvedDataMatchesApplyingThePreset`: building is
        // deterministic, so two independent applications end up identical.
        for i in 0..num_factory_presets() {
            let a = SynthShared::default();
            let b = SynthShared::default();
            let pa = apply_patch_data(&build_factory_preset(i).unwrap(), &a);
            let pb = apply_patch_data(&build_factory_preset(i).unwrap(), &b);
            assert_eq!(pa, pb, "{}", factory_preset_name(i));
            let ja = a.matrix.lock().unwrap().to_json();
            let jb = b.matrix.lock().unwrap().to_json();
            assert_eq!(ja, jb, "{}", factory_preset_name(i));
            assert_eq!(a.table(0).content_hash(), b.table(0).content_hash());
        }
    }

    #[test]
    fn applying_a_preset_resets_previous_state() {
        let shared = SynthShared::default();
        shared.edit_matrix(true, |m: &mut ModMatrix| {
            m.add_row(ModSource::Lfo1.index(), mod_target_from_id("oscA.pan"), 1.0, ModCurve::Linear)
        });
        // "Init Saw" has no matrix rows; the returned values replace whatever
        // the caller had (the C++ test set cutoff to 60 Hz first).
        let params: [f32; NUM_PARAMS] = apply_patch_data(&build_factory_preset(0).unwrap(), &shared);
        assert_eq!(shared.matrix.lock().unwrap().num_rows(), 0);
        assert_eq!(params[ParamId::FilterCutoff.index()], default_value(ParamId::FilterCutoff));
    }

    /// Every preset renders finite, audible, non-exploding audio: notes 60
    /// and 67 for 600 blocks of 128 at 48 kHz, then 200 blocks of release.
    #[test]
    fn every_preset_renders_finite_audio() {
        use crate::dsp::synth::Synth;
        use std::sync::Arc;
        for i in 0..num_factory_presets() {
            let shared = Arc::new(SynthShared::default());
            let params = apply_patch_data(&build_factory_preset(i).unwrap(), &shared);
            let mut synth = Synth::new(shared);
            synth.prepare(48_000.0, 128);
            synth.set_tempo(128.0);
            synth.set_params(&params);
            synth.note_on(0, 60, 0.8, -1);
            synth.note_on(0, 67, 0.8, -1);
            let (mut l, mut r) = ([0.0_f32; 128], [0.0_f32; 128]);
            let mut peak = 0.0_f32;
            for block in 0..800 {
                if block == 600 {
                    synth.note_off(0, 60, -1);
                    synth.note_off(0, 67, -1);
                }
                synth.process(&mut l, &mut r);
                for v in l.iter().chain(r.iter()) {
                    assert!(v.is_finite(), "{} produced a non-finite sample", factory_preset_name(i));
                    peak = peak.max(v.abs());
                }
            }
            let name = factory_preset_name(i);
            assert!(peak > 0.01 && peak < 6.0, "{name}: peak {peak}");
        }
    }

    /// Building and applying a preset leaves an engine in the same state as
    /// applying it directly: params, matrix and tables.
    #[test]
    fn applying_resolved_data_matches_applying_the_preset() {
        use crate::dsp::synth::Synth;
        use std::sync::Arc;
        for i in [0, 5, num_factory_presets() - 1] {
            let a = Arc::new(SynthShared::default());
            let b = Arc::new(SynthShared::default());
            let mut sa = Synth::new(a.clone());
            let mut sb = Synth::new(b.clone());
            sa.set_params(&apply_patch_data(&build_factory_preset(i).unwrap(), &a));
            let data = build_factory_preset(i).unwrap();
            sb.set_params(&apply_patch_data(&data, &b));
            assert_eq!(sa.params(), sb.params());
            assert_eq!(a.matrix.lock().unwrap().to_json(), b.matrix.lock().unwrap().to_json());
            assert_eq!(a.table(0).content_hash(), b.table(0).content_hash());
        }
    }
}
