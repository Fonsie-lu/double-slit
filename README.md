# Double Slit

Named after the double slit experiment, in honor of two wave sources interaction with each other.
A Serum-inspired polyphonic wavetable synthesizer for Linux, built with
[nih-plug](https://github.com/robbert-vdh/nih-plug). Exports **CLAP** and **VST3**.
[`SPEC.md`](SPEC.md) is the original requirements document; this README records
where the implementation differs from it.

| Item         | Value                                                      |
| ------------ | ---------------------------------------------------------- |
| Product name | Double Slit                                                |
| Vendor       | Fonsie Audio                                               |
| CLAP id      | `ch.fonsie.double-slit`                                    |
| Features     | `instrument`, `synthesizer`, `stereo`                      |
| Parameters   | 103, grouped by module, natural ranges in the host         |
| GUI          | egui, 1400 × 905 design surface, freely resizable (corner) |

- Two wavetable oscillators (22 factory tables plus a custom slot), unison 1-16
  with detune, spread and blend, hard sync, FM/AM from osc A, noise, and a
  sub / noise oscillator C
- Per-voice multimode filter: TPT SVF (LP/HP 12/24, BP, notch, peak), ladder,
  comb ±, formant with vowel morph
- AMP and MOD envelopes with curve shaping, two LFOs (tempo sync, retrigger
  modes, drawable custom shape from patches)
- 16-slot modulation matrix with drag-and-drop source chips
- FX chain: distortion → chorus → delay → reverb
- 1×/2×/4× oversampling of oscillators and filter, mip-mapped tables

## Building

```sh
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo xtask bundle double_slit --release
```

Artifacts land in `target/bundled/` (`Double Slit.clap`, `Double Slit.vst3`).
The unison loop has an AVX2+FMA path chosen at run time, so a build without
`target-cpu` still runs anywhere x86_64; `x86-64-v3` is about 20 % faster on the
heavy cases. To build and install in one step:

```sh
./install.sh               # symlink into ~/.clap and ~/.vst3; later rebuilds need no reinstall
./install.sh --copy        # copy instead of symlink
./install.sh --system      # copy to /usr/lib/clap and /usr/lib/vst3, via sudo
./install.sh --uninstall   # remove (combine with --system for the system folders)
```

`--clap` or `--vst3` limits the install to one format, and `--no-build` installs
the existing bundles. `TARGET_CPU= ./install.sh` builds without `target-cpu`.

`vendor/vst3-com` is nih-plug's `vst3-com` with one macro fixed (a trailing
semicolon rustc warns about and will reject in a future release), wired in by
`[patch]` in `Cargo.toml`; it goes once upstream has the fix.

## Development

```sh
cargo test                         # engine, editor (headless) and allocation tests
cargo clippy --all-targets
cargo run --release --example bench         # SPEC 9 performance cases

# Editor in a plain window, no host. DOUBLE_SLIT_DEMO=1 plays a chord through
# the engine (no audio output) so the live displays move.
cargo run --release --example gui_preview
DOUBLE_SLIT_DEMO=1 DOUBLE_SLIT_TAB=2 DOUBLE_SLIT_THEME=0 cargo run --release --example gui_preview
DOUBLE_SLIT_PRESET=13 cargo run --release --example gui_preview   # start with a factory preset
```

`tests/no_alloc.rs` renders every factory preset with nih-plug's checking
allocator armed; any allocation on the audio path aborts the test.

## Interface

Laid out after Serum 2 throughout: module headers with a power square and a
type tag, rotary knobs with value and modulation arcs, value fields, cell lists
for choices, and large graphs. The top half shows the OSC | WT | FILTER |
MATRIX | FX tabs; the bottom half always shows an envelope, an LFO, the master
section with the output scope and the performance modulation sources.

- Knobs, value fields and graph handles: drag (Shift for fine), mouse wheel,
  double-click resets. A hovered knob shows its value in place of its name.
- Choices are cell lists (click a cell) or, for long lists (wavetable, tempo
  division), boxes with arrows; clicking the name opens the full list. A
  module's power square and title in its header toggle it.
- OSC: OSC A on the left and OSC B on the right, each with a power switch,
  table browser, OCT / SEMI / FINE fields, a large wavetable display, phase
  start and knobs. The display stacks every frame of the table in 3D with the
  current frame highlighted (2D shows only that frame); while notes play it
  follows the frame actually being played. Drag it sideways or up to scrub the
  WT position; the pencil opens the table in the WT editor. Between the
  oscillators: what A does to B (sync, FM, AM), the sub / noise oscillator and
  the level meters.
- WT: frame list, draw grid with pencil / line / smooth tools, morph stack,
  add / duplicate / delete / move / normalise, import (WAV, `.wt`) and export
  (WAV, `.wt`) through `zenity`. User tables go to `$XDG_DATA_HOME/double-slit/tables`.
- FILTER: type list, CUTOFF and RESONANCE fields and the knobs around the
  response graph. Drag the curve sideways for cutoff (log axis) and up/down for
  resonance; the output and pre-filter spectra run behind it, and a second curve
  shows the cutoff the newest voice actually gets.
- FX: distortion, chorus, delay and reverb as modules with knobs; the delay's
  SYNC / DIVISION / PING PONG sit in a column, and the one of TIME and DIVISION
  that does not apply is dimmed. The output spectrum below.
- Bottom half: AMP ENV / MOD ENV and LFO 1 / LFO 2 tabs pick the envelope and
  LFO shown; the round handle on each tab is that modulator's drag source.
  Envelope graph: drag the handles - attack time, decay time and sustain level,
  release time; a dot follows the newest voice. The LFO has its trigger mode and
  shape as lists beside the graph and HZ / BPM below it (the unused one of RATE
  and DIVISION is dimmed).
- Modulation: drag a source chip or handle (MATRIX palette, the envelope and LFO
  tabs, the SOURCES strip) onto any modulatable control; an amount scale opens
  with the pointer on +50 %, and a click on it adds the row (a click outside
  cancels). Osc pitch takes modulation on OCT, SEMI and FINE. Right-click a
  control to see, adjust or remove its rows or add a source. Modulated controls
  show their range in the modulation colour, brighter while the source moves.
- MATRIX: source, target, amount, curve, mute and delete per row; drag the
  handle on the left to reorder. All 16 slots are shown; clicking an empty one
  picks a source, then a target, and adds the row at +50 %. Unresolved rows from
  patches are shown, kept and re-saved.
- Header: preset browser, value readout, CPU, voice LEDs, theme chips
  (Tokyo Night default, Catppuccin, Dracula, Gruvbox; saved with the project).
- Presets: the list shows the factory presets and the user presets in
  `$XDG_DATA_HOME/double-slit/presets`. EXPORT saves the current sound as a
  self-contained patch file (the JSON format above, wavetables embedded);
  exported into that folder it becomes a user preset. IMPORT takes one or more
  patch files and merges them into the folder: new names are added, a patch
  that is already there is skipped, and a different patch under a name in use
  gets a number, so nothing is overwritten.

## Architecture

```
src/dsp/      engine: DSP + state, no nih-plug dependency
  synth       parameter array, voice pool (16 + 4 fading), oversampling, global modulation, FX, GUI feedback
  voice       2 wavetable oscillators + sub/noise, filter, 2 envelopes, 2 LFOs, per-sample matrix
  oscillator  unison (1-16, AVX2 path), hard sync, FM/AM, noise
  wavetable   frames, FFT-built mip chain, RMS-matched factory tables, WAV/.wt import + export
  filters     SVF multimode, ladder, comb, formant; fx: distortion, chorus, delay, reverb
  mod_matrix  16 rows, stable string IDs, lock-free snapshot for the audio thread
  shared      SynthShared: matrix, tables, LFO shapes, GUI feedback atomics
  state       versioned JSON patches; presets: 48 factory patches
  library     user presets: patch files in one folder, merging import
src/params.rs host parameters generated from dsp/params.rs
src/lib.rs    plugin: events, block loop, background table loading, state
src/editor/   egui GUI: widgets, graphs, tabs, themes
```

## Measured performance

`cargo run --release --example bench`, fraction of one core, measured on a
Ryzen 5 PRO 3500U next to the C++ build compiled on the same machine:

| Case                                          | C++     | Rust    | Rust x86-64-v3 |
| --------------------------------------------- | ------- | ------- | -------------- |
| **16 voices, 2 × 8 unison, 4× OS, 48 k/128**  | 65.7 %  | 70.9 %  | 56.0 %         |
| 16 voices, 2 × 8 unison, 2× OS, 48 k/128      | 31.8 %  | 39.1 %  | 33.0 %         |
| 16 voices, 2 × 1 unison, 1× OS, 48 k/128      | 14.1 %  | 20.9 %  | 22.6 %         |
| 16 voices, 2 × 16 unison, 4× OS, 48 k/128     | 83.8 %  | 82.2 %  | 78.5 %         |
| 16 voices, 2 × 8 unison, 4× OS, 192 k/128     | 239.8 % | 267.8 % | 227.3 %        |
| 16 voices, 2 × 8 unison, 4× OS, 48 k/1 sample | 96.0 %  | 107.4 % | 93.1 %         |

The SPEC 9 row (bold) needs < 25 % of one core on a modern desktop CPU; the C++
README measured ~24 % on a Ryzen 7 7800X3D. This laptop CPU misses it for both
builds.

## Deviations from SPEC.md

- **Framework:** nih-plug instead of JUCE + clap-juce-extensions, so the plugin
  also exports VST3 (SPEC 1 lists VST3 as a non-goal). The VST3 bindings are
  GPL, hence the licence.
- **GUI:** laid out after Serum 2 (knobs with value and modulation arcs, value
  fields, cell lists) rather than the faders of the other Fonsie Audio synths;
  four selectable themes instead of the fixed Tokyo Night palette.
- **Presets** are a browser in the header: nih-plug has no host program list.
- **Voice info:** no CLAP `voice_info` extension (nih-plug only offers it
  together with polyphonic modulation).
- **Patch format** is unchanged JSON, so opus-serum patches load; saved patches
  carry `"product": "double-slit"`.
- **Plugin ID** changed with the product name, so projects using the C++
  opus-serum plugin do not load Double Slit in its place.

## Licence

GPL-3.0-or-later, forced by nih-plug's VST3 export. Copyright (C) 2026 Beat
Weber. Full text in [LICENSE](LICENSE).
