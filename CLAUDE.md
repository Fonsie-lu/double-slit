# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Double Slit (vendor Fonsie Audio) is a Serum-inspired polyphonic wavetable synth plugin, CLAP + VST3, built on nih-plug with a custom-painted nih_plug_egui editor (egui 0.31). It is a Rust port of the C++/JUCE "opus-serum" (in git history before the `nih-plug-port` branch). `SPEC.md` is the original requirements document; comments cite its sections ("SPEC 4.3") — keep that convention. `README.md` lists the deviations. The GUI follows the sibling projects `~/tools/drift-synth` and `~/tools/juno-clone`.

## Commands

```sh
cargo test                                   # engine + headless editor + tests/no_alloc.rs
cargo test --lib dsp::filters                # one module's tests
cargo test --lib editor::tests::matrix       # editor tests by name filter
cargo clippy --all-targets
cargo run --release --example bench          # SPEC 9 performance cases (print % of one core)

RUSTFLAGS="-C target-cpu=x86-64-v3" cargo xtask bundle double_slit --release   # -> target/bundled/
./install.sh                                 # build, symlink into ~/.clap and ~/.vst3 (--help for options)

cargo run --release --example gui_preview    # editor without a host
DOUBLE_SLIT_DEMO=1 DOUBLE_SLIT_TAB=0..4 DOUBLE_SLIT_THEME=0..3 cargo run --release --example gui_preview
```

`[profile.test]` uses opt-level 2; DSP tests render seconds of audio. ALSA/JACK dev headers are not installed, so there is no nih-plug standalone; use `gui_preview` (with `DOUBLE_SLIT_DEMO=1` the engine plays a chord in a thread, no audio out) for the GUI and the tests/bench for audio.

Screenshot workflow: start the preview with `timeout 7 ... &`, wait ~5 s, `import -window "Double Slit Preview" out.png`, then crop with `magick out.png -crop 1506x975+0+197 +repage crop.png` (the window manager makes the window 1506x1369 and the panel is centred), then Read the PNG.

## Architecture

Two layers with a hard boundary:

- **`src/dsp/`** — all DSP and state. **No nih-plug or egui imports**; that keeps the engine testable on its own.
- **`src/lib.rs`, `src/params.rs`, `src/editor/`** — the plugin, host parameters and GUI.

### Parameters: `dsp/params.rs` is the single source of truth

`ParamId` (103 entries, `repr(usize)`) plus the table built in `build()` drive the engine's flat `[f32; NUM_PARAMS]`, the host parameters (`src/params.rs` generates a `FloatParam` or `IntParam` per entry and implements `Params` by hand), patch serialisation and the modulation targets. **The order of the builder calls must match the enum** (debug assertion). Adding a parameter: enum entry → builder call in the same position → consume it in the DSP → optionally a mod target in `mod_matrix.rs` → a GUI control (the `every_parameter_has_a_control` test lists the deliberate exceptions). Values are natural units everywhere; host normalisation is `ParamDesc::to_normalized`/`from_normalized` with the JUCE-style skew centre. The `id_string`s (`"oscA.wtPos"`) are the stable automation and patch keys: never rename one.

### Threading (SPEC 7)

- Audio thread: `lib.rs::process` copies all host values into the engine (`Synth::set_params`) each block, splits the block at event times, and renders. Allocation-free: `tests/no_alloc.rs` proves it with nih-plug's aborting allocator (it only works in tests that link the crate).
- `SynthShared` (`dsp/shared.rs`, one `Arc`): matrix row list behind a `Mutex` only the GUI/main thread takes, published into a lock-free double-buffered `SharedSnapshot` the audio thread copies once per block; tables and LFO shapes in `ArcSwap`, always replaced through `set_table`/`set_lfo_shape`, which park the old `Arc` so it is never freed on the audio thread; GUI feedback (`Feedback`) as relaxed atomics plus analyser rings.
- Factory tables are built off the audio thread: `process` notices a changed TABLE parameter and queues `Task::LoadFactoryTable` on nih-plug's background executor; `initialize` builds them directly. Custom (index 14) keeps whatever the WT editor or a patch published.
- State: host parameters are saved by nih-plug; `serialize_fields` adds the editor size, theme and `"patch"` = `state::save_patch(shared, None, ..)` (the patch JSON without its params). Restoring applies it through `apply_patch_data`.

### Engine

`Synth` owns the parameter array, voice pool (16 + 4 slots for 10 ms steal fades), global LFOs, oversampling (1/2/4×, oscillators + filter only; envelopes, LFOs and matrix at the base rate), the FX chain and the denormal guard. `Voice` runs 2 oscillators + sub/noise → filter → VCA with per-sample matrix evaluation. Modulation is soft-limited against the headroom to the range edge (`Voice::modulated`), never around the centre. Wavetables: fixed 2048-sample frames, ≤256 frames, 11-level FFT mip chain; factory frames RMS-normalised; `factory_index()` is `None` after any edit, which makes patches embed the samples. Patches are the C++ JSON format (schema 1); unresolved matrix rows are kept and re-saved verbatim. Factory presets are built in code (`dsp/presets.rs`, pure functions returning `PatchData`); `tests/no_alloc.rs` renders every one, so a new preset is covered automatically.

### Editor (`src/editor/`)

Painted on a fixed 1400×905 canvas (`BW`/`BH`) through `Cv`, scaled to the window. `mod.rs`: state, header, tabs, popups, drag-and-drop; `widgets.rs`: `G` (param I/O, faders, steppers, selectors, switches, buttons, chips); `graphs.rs`: envelope/filter graphs with handles, wave display, LFO, scope, spectrum; `tabs.rs`: the five tabs (rows 0–1) and the bottom half (rows 2–3). `row(i)`/`graph_rows(i)` give row geometry; x positions are hand-placed.

- Parameter values reach the plugin asynchronously. Read through `G::shown`/`plain` (pending values win until the host applies them) and write only through `send` inside `begin`/`end`, or `change`/`change_plain`. Faders track drags in egui temp memory; graph handles use `drag_offset` (offset from the press origin) because egui swallows the first ~6 px of a drag, so test drags on handles must be longer than that.
- Modulation: every modulatable control calls `mod_target`, which records it as a drop target for source chips (`drag_and_drop`) and opens the popover on right-click. `mod_info` sums matrix rows per target each frame for the range overlays.
- Popups are drawn last and shielded with a background interact so the click on an entry does not reach controls underneath.
- Never hard-code colours; add a `Theme` field (`theme.rs`) and fill it for all four themes (Tokyo Night default, Catppuccin, Dracula, Gruvbox — same set as the sibling synths). `Section` indexes bar/accent colours; `modulation` and `warning` are the matrix colours.
- `src/editor_tests.rs` (compiled as `editor::tests` via `#[path]`) runs the real editor headless against a mock host that applies values late and checks gesture balance; every control registers its rect in `EditorState::controls` under its parameter ID (or a fixed key).

## Conventions

- No allocation, lock or libm `exp2`/`sin`/`tanh`/`powf` on the audio path: use `fast_exp2`, `fast_sin01`, `fast_tanh`, `fast_pow`, `midi_to_hz_fast` from `dsp/common.rs`.
- Randomness is seeded from the voice/engine (`Rng`), never the clock: repeat renders are bit-identical (tested).
- The unison inner loop (`oscillator.rs`) is the hottest code; run the bench after touching it. The AVX2 path must match the scalar one (tested).
- No vendor name or logo on the panel.
