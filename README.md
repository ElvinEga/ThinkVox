# ThickVox

> **8-Voice Vocal Thickener & ADT Stereo Processor**  
> Built with Rust, [`truce`](https://github.com/noda-audio), and [`noda-audio`](https://github.com/noda-audio).

ThickVox is a professional, zero-latency vocal doubler and stereo width processor inspired by classic hardware ADT (Automatic Double Tracking) and micro-pitch shifting units (such as the Eventide H910 / H949 / DSP4000 and AMS DMX).

Eight micro-pitch-shifted, short-delayed copies of the input are panned across four mirrored stereo pairs, summed into a dedicated wet bus, and shaped with Mid/Side filtering before blending with the untouched dry path.

---

## Features

- **8-Voice Micro-Pitch Detune Engine**:
  - 4 mirrored L/R pairs calibrated to classic doubling bands: `±3`, `±6`, `±9`, and `±12` cents.
  - Global **Detune** (`0.50×` to `2.00×`) and **Delay** (`0.70×` to `1.30×`) multipliers.
  - Individual **Pair Spread** knobs (`0%` to `100%`) for custom stereo geometries with constant-power panning.
- **Mid/Side Tone & Phase Sculpting**:
  - **Side HPF** (`20 Hz` to `300 Hz`): Highpass filter applied exclusively to the wet Side signal, keeping low-end and bass fundamentals centered and punchy.
  - **Air Shelf** (`0 dB` to `+6 dB` at 4 kHz): Adds presence and sheen to the doubled voices without introducing harsh sibilance to the lead dry vocal.
  - **Mid Only Wet**: Instantly collapses the wet doubling cluster to mono Mid for tight, focused center thickening.
  - **Stereo Width** (`0%` to `200%`): Scales the stereo difference component from pure mono up to super-wide.
- **Master Gain Staging**:
  - **Input Gain (`In`)**: `-24 dB` to `+12 dB` input trim with exponential smoothing.
  - **Voice Level**: Dedicated `-24 dB` to `+6 dB` fader for trimming the level of the wet doubled voices relative to the dry path.
  - **Output Gain (`Out`)**: `-24 dB` to `+12 dB` master output level trim.
- **Visualizer Suite**:
  - **Stereo Imager / Goniometer**: Rotated Lissajous vectorscope displaying stereo width, phase correlation, and dispersion in real time.
  - **Precision Peak Meters**: Calibrated stereo meters (`-60 dB` to `0 dB`) with peak hold and clip detection.
  - **Correlation Meter**: Real-time phase correlation bar (`-1.00` to `+1.00`) flagging mono cancellation risks.
- **Workflow & Convenience**:
  - **Preset Engine**: Embedded factory presets + in-plugin user preset creation (`💾`) with category organization, collision warnings, and Finder / Explorer folder access.
  - **A/B Comparison**: Instant switching between two parameter snapshots.
  - **True Zero Latency**: The dry vocal path is completely undelayed (0 sample latency reported to the host).
  - **Real-Time Safety**: Guaranteed 100% allocation-free audio processing (`--features rt-paranoid`).

---

## Signal Architecture

```text
               ┌────────────────────────────────────────────────────────┐
               │                        Dry Path                        │
               │                   (0 samples latency)                  │
               └──────────────────────────┬─────────────────────────────┘
                                          │
Input ──[ Input Gain ]──┬─────────────────┼──────────────────────────────┐
                        │                 │                              │
                        ▼                 │                              ▼
        ┌───────────────────────────────┐ │               ┌──────────────────────────────┐
        │       8-Voice Detuner         │ │               │       Stereo Imager          │
        │ Pair 1: ±3c  · [Delay Stage]  │ │               │        (Goniometer)          │
        │ Pair 2: ±6c  · [Delay Stage]  │ │               └──────────────▲───────────────┘
        │ Pair 3: ±9c  · [Delay Stage]  │ │                              │
        │ Pair 4: ±12c · [Delay Stage]  │ │                              │
        └───────────────┬───────────────┘ │                              │
                        │                 │                              │
                        ▼                 │                              │
        ┌───────────────────────────────┐ │                              │
        │   Mid/Side Cluster Shaping    │ │                              │
        │  • Side HPF (20 Hz - 300 Hz)  │ │                              │
        │  • Side Air Shelf (+0..+6 dB) │ │                              │
        │  • Mid Only Collapse Mode     │ │                              │
        │  • Stereo Width (0% - 200%)   │ │                              │
        │  • Voice Level Trim           │ │                              │
        └───────────────┬───────────────┘ │                              │
                        │                 │                              │
                        ▼                 ▼                              │
                     [ Mix ] ─────────► ( + ) ──► [ Output Gain ] ───────┴──► Output
```

---

## Parameter Reference

| ID | Parameter | Range | Default | Unit | Description |
|:---|:---|:---:|:---:|:---:|:---|
| `0` | **Mix** | `0% – 100%` | `35%` | `%` | Dry / Wet blend |
| `1` | **Detune Depth** | `0.50× – 2.00×` | `1.00×` | `×` | Voice detune spread multiplier |
| `2` | **Delay Spread** | `0.70× – 1.30×` | `1.00×` | `×` | Voice delay offset multiplier |
| `4` | **Stereo Width** | `0% – 200%` | `100%` | `%` | Stereo Side component width |
| `5` | **Voice Level** | `-24 dB – +6 dB` | `-8.0 dB` | `dB` | Wet doubling cluster volume trim |
| `6` | **Side HPF** | `20 Hz – 300 Hz` | `120 Hz` | `Hz` | Highpass on wet Side signal |
| `7` | **Air** | `0 dB – +6 dB` | `+1.5 dB` | `dB` | High shelf on wet Side (4 kHz) |
| `8` | **Mid Only** | `Off / On` | `Off` | — | Collapses doubled voices to Mid |
| `9` | **Bypass** | `Off / On` | `Off` | — | True bit-transparent host bypass |
| `10` | **Pair 1 Spread** | `0% – 100%` | `100%` | `%` | Pan width for ±3c voice pair |
| `11` | **Pair 2 Spread** | `0% – 100%` | `100%` | `%` | Pan width for ±6c voice pair |
| `12` | **Pair 3 Spread** | `0% – 100%` | `100%` | `%` | Pan width for ±9c voice pair |
| `13` | **Pair 4 Spread** | `0% – 100%` | `100%` | `%` | Pan width for ±12c voice pair |
| `14` | **Input Gain** | `-24 dB – +12 dB` | `0.0 dB` | `dB` | Pre-processing input trim |
| `15` | **Output Gain** | `-24 dB – +12 dB` | `0.0 dB` | `dB` | Master output volume trim |

---

## Building & Installation

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (2024 edition supported)
- [`cargo-truce`](https://crates.io/crates/cargo-truce) build helper:
  ```bash
  cargo install cargo-truce
  ```

### Run Tests
Run the comprehensive suite covering DSP response, filter transparency, pitch ratios, preset parsing, and host automation:
```bash
cargo test
```

Verify zero audio thread memory allocations:
```bash
cargo test --features rt-paranoid
```

### Install Plugins (VST3 & CLAP)
Build in release mode, sign, and install directly into your system's plug-in directory:

```bash
# Install VST3 (macOS: ~/Library/Audio/Plug-Ins/VST3/ThickVox.vst3)
cargo truce install --vst3

# Install CLAP (macOS: ~/Library/Audio/Plug-Ins/CLAP/ThickVox.clap)
cargo truce install --clap
```

### Run Standalone
Launch the standalone desktop application with audio device selection:
```bash
cargo run --bin thickvox-standalone
```

---

## User Presets
User presets are stored in standard `.preset` TOML format under:
- **macOS / Linux**: `~/.config/thickvox/presets/<Category>/<Name>.preset`
- **Windows**: `%USERPROFILE%\.config\thickvox\presets\<Category>\<Name>.preset`

Clicking the `💾` button in the top toolbar opens the save modal, and clicking **📁 Presets Folder** reveals the presets directory in your system file manager.

---

## License

MIT / Apache-2.0
