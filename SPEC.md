# SPEC — "ox-serum" Wavetable Synthesizer (CLAP, Linux)

A Serum-inspired wavetable software synthesizer distributed as a CLAP plugin with GUI, targeting Bitwig Studio on Linux.

---

## 1. Overview

| Item                   | Value                                                              |
| ---------------------- | ------------------------------------------------------------------ |
| Product name           | opus-serum                                                         |
| Type                   | Polyphonic wavetable synthesizer                                   |
| Format                 | CLAP 1.2.x (primary), CLAP with GUI extension                      |
| Platforms              | Linux x86_64 (x64 build); architecture-agnostic core               |
| Host validation target | Bitwig Studio ≥ 5.x on Linux                                       |
| Sample rates           | 44.1 – 192 kHz, arbitrary buffer sizes (including 1-sample blocks) |
| Latency                | 0 (no reported latency; no internal lookahead)                     |
| Channels               | Stereo out, MIDI in (CLAP events), no audio in                     |

Non-goals (v1): VST2/VST3/LV2 builds, Windows/macOS, sample playback/ROMpler mode, FX beyond the filter section, preset browser GUI (presets via host program list only).

## 2. Technology Choices

| Layer            | Choice                                                                                                                             | Rationale                                                                                                            |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Language         | C++20                                                                                                                              | Ecosystem maturity for DSP; CLAP C ABI via thin wrapper                                                              |
| Plugin framework | [clap-juce-extensions](https://github.com/free-audio/clap-juce-extensions) wrapping JUCE 9                                         | One codebase → CLAP; JUCE handles GUI, parameter, state plumbing; CLAP is a first-class target, Linux well supported |
| DSP              | Custom, block-based; no external DSP lib                                                                                           | Full control, no license risk                                                                                        |
| Wavetable format | Custom binary (`.wt`) + import of Serum `.wav` tables (one frame per wav file / single-file frame concatenation, as Serum exports) | Interoperability with existing Serum table ecosystem                                                                 |
| Build            | CMake ≥ 3.22, presets for `Release`/`Debug`                                                                                        | Standard, CI-friendly                                                                                                |
| Testing          | GoogleTest (DSP core, state, modulation math) + manual Bitwig validation checklist                                                 | Deterministic core tests; GUI verified in host                                                                       |

Build output: `build/ox-serum.clap` — a single shared library placed in `~/.clap/`. Bitwig on Linux scans `~/.clap` by default.

## 3. Signal Architecture

```
MIDI → Voice Allocator → [Osc A] ┐
                       → [Osc B] ├→ Mixer (A/B level, phase, unison) → Filter → VCA → FX-lite → Out
Wavetable positions ← Mod Matrix (see §7)
```

### 3.1 Voices

- 8–16 voice polyphony, configurable; voice stealing: oldest.
- Per voice: 2 oscillators, 1 filter, 2 envelopes (AMP, MOD), 2 LFOs.
- Global: master volume, master tuning (master tune ± 100 cent, pitch-bend range per patch).
- Oversampling: 1x / 2x / 4x, applied to oscillator + filter chain (selectable; 2x default). Anti-aliased via mipmapped wavetables (see §5) — oversampling mainly protects the filter self-oscillation and FM/unison paths.

### 3.2 Oscillators (A and B)

- Wavetable playback with:
  - Frame (wavetable position) parameter 0–100%, modulatable.
  - Morph within frame is NOT in v1 (frames only, like Serum's WT position).
  - Unison: 1–16 voices, detune ± 100 cent, stereo spread, blend.
  - Phase: free / random / 0°; osc B can hard-sync to osc A or to itself (sync ratio).
  - Octave (-3…+3), semitone (-12…+12), fine (± 100 cent).
- Osc B additionally: noise mode (white/pink) and AM/FM from osc A (index modulatable).
- Sub/noise auxiliary oscillator (C): sine sub or noise, level only.

### 3.3 Wavetables

- Internal factory tables: at minimum basic shapes (sine, saw, square, triangle), classic analog set, 4–6 morphing digital sets.
- Max 256 frames × 2048 samples per frame, float32.
- Mipmap generation: per frame, band-limited octave-reduced copies (like Serum) to avoid aliasing at higher pitches; mip selected per note by fundamental frequency.
- Editor (GUI, "WT" tab):
  - Draw/edit single-frame waveform with pencil + smooth + line tools on a grid.
  - Frame list with add/duplicate/delete/move.
  - Morph view showing all frames stacked.
  - Import: Serum-style WAV tables, single WAV with concatenated frames, raw float32/fmt chunks.
  - Export: `.wt` (custom) and WAV.
  - All edits are per-patch state (table embedded in preset, deduplicated by content hash in the preset pool).

### 3.4 Filter (own tab, "Filter")

- Per voice, stereo. Types:
  - Low-pass 12/24 dB, high-pass 12/24 dB, band-pass 12 dB, notch, peak — state-variable topology (TPT/zero-delay-delay-free, e.g. Chamberlin-derived SVF or ZDF multimode à la Vadim Zavalishin).
  - Ladder LP model (Moog-style, non-linear, 4-pole).
  - Comb (positive/negative feedback).
  - Formant (vowel morph: 3 parallel BPFs with vowel table morphing).
- Cutoff, resonance, drive, mix (dry/wet), key tracking (0–100%), env amount (bipolar).
- Cutoff and resonance are the primary modulation targets.
- Filter drives the VCA; AMP envelope always active; MOD envelope routable via matrix.

### 3.5 Envelopes & LFOs

- 2 envelopes: ADSR (AMP), ADSR (MOD), each with curve knobs (attack/decay shape).
- 2 LFOs: rate (Hz or tempo-sync: 1/1…1/16, dotted/triplet), retrigger modes (free, voice, note), shapes: sine, tri, saw down/up, square, S&H random, custom drawable (v1: sine/tri/saw/square/S&H only; drawable LFO is v1.1).
- Envelope/LFO output values exposed as modulation sources (§7).

### 3.6 FX-lite (post filter, v1)

- Distortion (soft clip/tanh), chorus (2-voice), delay (synced, feedback, mix), reverb (simple FDN or Schroeder).
- Fixed chain order: Distortion → Chorus → Delay → Reverb. Bypass per module.
- Rationale: Serum's full FX rack is out of scope; these four cover "usable preset" needs. Expansion is a v2 item.

## 4. Modulation System (drag & drop matrix)

### 4.1 Sources

`Velocity`, `Aftertouch` (channel + per-note/CLAP poly key), `Mod Wheel`, `Pitch Bend` (bipolar), `Note Pitch`, `Note Gate`, `AMP Env`, `MOD Env`, `LFO 1`, `LFO 2`, `MIDI CC 1–119` (CC 1/11/64/74 pre-mapped aliases), `Random (per note)`, `Random (per voice cycle)`.

### 4.2 Targets

- Osc A/B: WT position, pitch (fine, bipolar), unison detune, phase, AM/FM index, level.
- Osc C: level, pitch.
- Filter: cutoff, resonance, drive, mix.
- Envelopes: all ADSR amounts (bipolar scaling).
- LFOs: rate.
- FX-lite: each module's primary macro (drive / depth / time / mix), master volume.
- Global: pan (per-voice), pitch (master).

### 4.3 Matrix semantics

- Each row: `source → target, amount (bipolar −100…+100 %), curve (linear, exp, log, switch)`.
- 16 slots v1; unlimited list v1.1.
- Multiple sources to one target sum; per-row curve shapes the source.
- Range clamping at target with soft-clip (tanh) to avoid hard discontinuities.
- Sample-accurate: modulation values are computed per-sample for audio-rate sources (LFO audio mode, env), per-block otherwise.

### 4.4 Drag & drop UX

- The Matrix tab renders rows; each source chip is draggable (mouse down + drag) onto any parameter control anywhere in the GUI (osc, filter, FX panels). Drop creates a new matrix row with default amount 50 % (bipolar targets: +50).
- Dropping onto an already-modulated parameter opens a mini-popover listing existing rows for that target (edit amount, curve, remove).
- Modulated parameters show a colored arc/ring around the knob (Tokyo Night accent, §6) indicating live modulation range; the arc animates with the modulator value when the source is active.
- Matrix tab shows the same rows as a table: source, target, amount slider, curve dropdown, mute toggle, delete. Rows are reorderable by drag.
- Right-click any parameter → "Modulation → <source>" context menu as keyboard/trackpad-friendly alternative.

## 5. State / Persistence

### 5.1 Patch format

- Single JSON (clap-juce-extensions/JUCE `ValueTree` → XML stream in CLAP state chunk), versioned (`"schema": 1`).
- Contains: all parameter values (by param ID), matrix rows, wavetable data (base64 or raw chunk; dedup by 64-bit content hash), LFO shapes, GUI tab state, FX settings.
- CLAP state: full chunk via `clap_plugin_state` (Bitwig saves patches in project + `.clappreset` export).

### 5.2 Modulation matrix persistence (explicit requirement)

- Matrix rows are first-class state: each row persisted with source ID, target parameter ID, amount, curve, muted flag, user order index.
- Target identity is by stable parameter ID string (e.g. `oscA.wtPos`), never by pointer/index — survives parameter reordering between versions.
- Unknown source/target IDs on load → row kept but flagged `unresolved`, shown grayed in matrix tab; never silently dropped.
- Factory presets: 20+ showing each feature; stored as CLAP presets inside the binary resource or shipped alongside in the clap preset dir.

### 5.3 Wavetable persistence

- User tables saved under `$XDG_DATA_HOME/ox-serum/tables/` when exported; tables used by a patch are embedded in the patch so patches are self-contained.

## 6. GUI

### 6.1 Framework & window

- JUCE OpenGL-accelerated component rendering (software fallback automatic).
- Resizable: 100 %, 125 %, 150 %, 200 % scale presets + free zoom via drag on corner handle; aspect locked. Base size 1200×750 px (Serum-like layout density, but minimal).
- All custom-drawn (no native widgets) for consistent look.

### 6.2 Layout

- Top bar: preset name (click → host preset via Bitwig), master volume, voice count meter, CPU meter, oversampling selector, zoom.
- Tab bar: `OSC` | `WT` (wavetable editor) | `FILTER` | `MATRIX` | `FX` — exactly the tabs the user requested.
- Bottom strip (always visible): AMP env, MOD env, LFO 1, LFO 2 mini-views + master out meter.

- **OSC tab**: two oscillator panels side by side (A left, B right), each with wavetable thumbnail display (current frame highlighted, drag on display = scrub position), unison/detune/spread/phase controls, octave/semitone/fine, level/pan. Osc C sub-panel below.
- **WT tab**: full-width wavetable editor per §3.3 — frame list on left, big draw grid center, morph stack right, import/export buttons.
- **FILTER tab**: type selector (icon row), big cutoff/resonance knobs, drive, mix, key-track, env amount, plus a real-time frequency-response curve display and an animated spectrum analyzer behind it (input vs output, subtle).
- **MATRIX tab**: the drag & drop matrix per §4.4 — table of rows + a "sources palette" (all sources as draggable chips grouped by category) on the left.
- **FX tab**: four module panels in fixed chain order, each with on/off, 3–4 knobs, and a small level LED.

### 6.3 Visual feedback (requirement)

- Every knob: arc shows value; modulation arc overlay in accent color; double-click resets to default; scroll = fine adjust; shift+scroll = finer.
- Wavetable displays animate the actual playback frame per voice (max of active voices) — the table "breathes" with the sound.
- Filter tab: live response curve + spectrum.
- Matrix rows pulse briefly when their source fires (env/LFO trigger dot).
- Master/voice meters on top bar; per-osc level LEDs.
- No decorative animation beyond the above; minimal by design.

### 6.4 Tokyo Night color scheme

Using the canonical Tokyo Night palette ( enkia/tokyo-night / folke variant), dark theme:

| Role                 | Hex       | Usage                                                 |
| -------------------- | --------- | ----------------------------------------------------- |
| Background           | `#1a1b26` | Main panel background                                 |
| Background dark      | `#16161e` | Recessed areas (displays, editor grid)                |
| Background highlight | `#292e42` | Panels, rows, button surfaces                         |
| Foreground           | `#a9b1d6` | Primary text, knob tracks                             |
| Foreground bright    | `#c0caf5` | Emphasized text, active values                        |
| Comment/muted        | `#565f89` | Labels, disabled state, grid lines                    |
| Blue                 | `#7aa2f7` | Primary accent — value arcs, selection, links         |
| Cyan                 | `#7dcfff` | Osc A accent, LFO 1                                   |
| Purple               | `#bb9af7` | Osc B accent, LFO 2                                   |
| Green                | `#9ece6a` | Meters, OK states, FX LEDs                            |
| Orange               | `#ff9e64` | Modulation arcs (the "this is being modulated" color) |
| Red/magenta          | `#f7768e` | Record/edit-active states, clipping indicators        |
| Yellow               | `#e0af68` | Warning, unresolved matrix rows                       |

- Typography: Inter (or system sans fallback) for labels; monospace (JetBrains Mono fallback) for value readouts.
- Knob style: thin arc track (`#565f89`), value arc (`#7aa2f7`), modulation arc (`#ff9e64`), center dot; no skeuomorphism, no gradients beyond a 2-stop subtle panel shade.
- Selection/hover: `#292e42` fill + 1px `#7aa2f7` border.
- Theme is compile-time constant in v1 (no user theming); palette defined once in `Theme.h`.

## 7. CLAP Integration Details

- Plugin entry: `clap_entry`; id `org.ox-serum.ox-serum`; features `["instrument", "synthesizer", "stereo"]`.
- Uses: `clap_plugin_state` (full patch), `clap_plugin_params` (all automatable params, per §8), `clap_plugin_gui` (X11 via JUCE; Wayland through XWayland — Bitwig runs either), `clap_plugin_noteports` (1 MIDI input port, 16 channels), `clap_plugin_audioports` (1 stereo out), `clap_plugin_voice_info` (report polyphony so Bitwig voice management works), `clap_plugin_tail` (reverb/delay tail seconds), `clap_plugin_render` (offline render support for bounce).
- Parameter changes from host: applied sample-accurately where cheap (levels, pan), per-block otherwise (filter cutoff smoothing per-block with 5 ms one-pole ramp minimum).
- Note expressions supported (v1): volume, pan, tuning, vibrato — mapped into the per-voice mod chain.
- Threading: audio callback lock-free (SPSC FIFO for GUI→audio messages, e.g. wavetable swaps applied via atomic pointer swap on table boundary); GUI never calls into DSP directly.
- Wavetable swap safety: new table prepared on GUI thread, published via `std::shared_ptr` atomic exchange; voices keep a snapshot reference for the current block.

## 8. Parameter List (automatable, stable IDs)

Groups: `oscA.*`, `oscB.*`, `oscC.*`, `filter.*`, `envAmp.*`, `envMod.*`, `lfo1.*`, `lfo2.*`, `fx.dist.*`, `fx.chorus.*`, `fx.delay.*`, `fx.rev.*`, `master.*`.

Count estimate ≈ 120 params. All floats with `PARAM_REQUIRES_PROCESS`-style flags per CLAP; discrete params (filter type, LFO shape, sync mode) as enums with `stepped` flag. Every parameter has: name, module, default, min/max, unit, automation curve hint.

## 9. Performance Targets

- 16 voices, 2 osc × 8 unison, 4x oversampling, 48 kHz / 128 samples: < 25 % of one core on a modern x86_64 (Ryzen/Intel post-2018).
- Wavetable mipmap generation for 256×2048 table: < 100 ms, done off-thread.
- GUI render: steady 60 fps at 150 % scale on integrated GPUs (OpenGL path, no per-frame allocations).
- No allocations, locks, or syscalls on the audio thread after `start_processing` (pre-allocated voice pool, pre-rendered mipmaps, lock-free FIFOs).

## 10. Testing & Acceptance

### Automated

- DSP unit tests: wavetable mip generation correctness (band-limit check), SVF stability at Nyquist/2, envelope edge cases (zero attack), modulation summation/clamping, state round-trip (save → load → identical parameter + matrix dump), unresolved-row handling.
- CLAP validation with `clap-validator` in CI.
- Determinism: same render twice → bit-identical output.

### Manual (Bitwig on Linux)

1. `cmake -B build && cmake --build build` produces `ox-serum.clap`; copy to `~/.clap/`; Bitwig scans and lists it.
2. Load on an instrument track; play MIDI; no xruns at 128 samples.
3. Automate `oscA.wtPos` and `filter.cutoff` from Bitwig — smooth, sample-accurate.
4. Drag LFO 1 chip onto cutoff → arc appears, sound modulates; save project, reopen → row restored (matrix persistence acceptance).
5. Save/load preset with custom drawn wavetable → table identical after reload.
6. GUI: resize/zoom, all 5 tabs, Tokyo Night palette matches §6.4, meters animate, no rendering glitches on 125 % scale.
7. Offline bounce of a project renders identically to real-time capture.

## 11. Milestones

| M   | Scope                                                                                      | Exit criteria                                              |
| --- | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------- |
| M1  | Skeleton: CLAP builds, loads in Bitwig, GUI window, Tokyo Night theme shell, master volume | Acceptance 1, 6 (shell only)                               |
| M2  | Mono synth core: osc A/B wavetable playback + mipmaps, filter, envelopes, state save/load  | Sound, acceptance 3 (partial), state round-trip tests pass |
| M3  | Polyphony, unison, LFOs, osc C, noise                                                      | Full synth engine                                          |
| M4  | Modulation matrix + drag & drop + persistence                                              | Acceptance 4                                               |
| M5  | WT editor tab (draw, import/export)                                                        | Acceptance 5                                               |
| M6  | FX-lite, filter tab visual feedback, polish, factory presets                               | All acceptance items                                       |

## 12. Risks / Open Decisions

- **clap-juce-extensions vs raw CLAP**: extensions wrapper is chosen for velocity; if JUCE 8's native CLAP support (in progress upstream) lands stable, switch — parameter/state code is wrapper-agnostic.
- **Serum `.wav` table import edge cases** (non-standard frame counts, loop metadata): accept per-frame WAVs first; concatenated single-file import best-effort in M5.
- **Wayland-native GUI**: JUCE currently X11; Bitwig on Wayland compositors works via XWayland — acceptable for v1, native Wayland is a v2 consideration.
- **Voice-stealing audibility with unison 16**: may need per-voice gain ramp on steal — verify in M3, add 10 ms fade-out if clicks are audible.
