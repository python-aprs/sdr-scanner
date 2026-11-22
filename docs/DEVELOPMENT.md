# Development

The design and roadmap live in [PLAN.md](PLAN.md). This page is how to build
and test what exists so far (Phase 0, Phase 1 offline core).

## Layout

```
crates/
  core/      band-profile config, sample-counter clock, PSD + peak finding,
             DSP (dsp.rs), fast-convolution channelizer (channelizer.rs),
             detector + slot state machine (detector.rs), pipeline (scanner.rs)
  sources/   Source trait, SigMF FileSource; Airspy/SDRplay stubs (Phase 2)
  store/     WAV writer, SQLite transmission index, recorder thread
  cli/       the `scanner` binary
  web/       placeholder for the web UI and API (Phase 3)
tools/synth/ Python synthetic IQ generator + reference DSP
config/      example band profiles
```

## Rust

Requires stable Rust 1.85+ (edition 2024).

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo build --release
```

`scanner` subcommands available now:

```sh
scanner info  <recording>            # SigMF metadata, level, peak
scanner peaks <recording> [--json]   # averaged spectrum peaks, RF frequencies
scanner config check config/example-2m.toml [--profile 2m-25k] [--slots]
scanner run <recording> --config config/rainier.toml --profile rainier-2m \
    --out out/ [--open-db 6 --close-db 3 --hang 1.5 --min-len 0.3] [--json]
```

`scanner serve --source loop:<recording> --config ... --profile ...` is the
long-running mode: a radio thread pushes samples into a lock-free ring
(`scanner_sources::ring`, drops counted and zero-filled so timestamps never
slip), the DSP thread scans, and the recorder thread writes. A recording
replayed at its real rate stands in for a radio until the Phase 2 backends
land; Ctrl-C closes open transmissions and exits. It prints a status line
(DSP load, ring fill, drops, active channels) every `--status-every` seconds.

`scanner run` writes `out/YYYY/MM/DD/146.5200_20261122T065403.123Z.wav` per
transmission (via `.wav.part` and a rename) and one row per transmission in
`out/index.sqlite`. `--json` prints a machine-readable summary that
`synth score` reads.

How the pipeline fits together (PLAN.md 3.2–3.4):

1. `channelizer::Analyzer`: one forward FFT per block (2^17 points at 8–10
   MS/s, 76 Hz bins, 25 % overlap, ~9.8 ms of new samples per block).
2. `detector::Detector`: slot SNR = integrated power in the slot window over
   the per-segment noise floor; `idle → attack → active → hang` per slot.
   Fallback grid windows are one grid spacing wide so they tile the band;
   explicit slots only open for a signal centred on them.
3. On open, `scanner::Scanner` builds a chain for the slot (grid slots retune
   to the measured centre, rounded to 2.5 kHz) and replays the pre-roll blocks
   it kept. Each chain: `channelizer::Extractor` (bin slice × filter, small
   IFFT, ~31–39 kS/s) → FM discriminator → 530 µs de-emphasis → rational
   resampler to 8 kHz → 300 Hz high-pass (CTCSS) → audio.
4. `scanner_store::Recorder` (its own thread) writes WAVs and index rows.

Stage benchmark (ns per input sample and share of a core at 10 MS/s):

```sh
cargo bench -p scanner-core --bench pipeline
```

A recording path can be given as `name`, `name.sigmf-meta` or `name.sigmf-data`.

Hardware backends are behind cargo features so nothing needs a vendor SDK
until Phase 2: `--features airspy` (libairspy) and `--features sdrplay`
(SDRplay API 3.x, not redistributable, so CI never enables it for real).

## Synthetic captures (`tools/synth`)

Requires Python 3.10+ and [uv](https://docs.astral.sh/uv/).

```sh
cd tools/synth
uv run synth describe scenarios/ci_smoke.yaml
uv run synth generate scenarios/ci_smoke.yaml -o ../../out/ci_smoke
cargo run --release -p scanner-cli -- peaks ../../out/ci_smoke
```

`generate` writes:

- `<out>.sigmf-data` / `<out>.sigmf-meta`: the IQ recording (ci16 by default,
  quantised to `adc_bits`), with one SigMF annotation per transmission
- `<out>.truth.json`: ground truth (frequency, start/end time and sample,
  SNR, deviation, CTCSS, audio kind) for detection metrics
- `<out>.truth/<id>.wav`: the modulating audio of each transmission at 8 kHz,
  for golden-audio comparisons

Scenario format is documented at the top of `src/synth/scenario.py`.
Bundled scenarios:

| Scenario | Rate | Purpose |
|---|---|---|
| `ci_smoke.yaml` | 2.5 MS/s, 2 s | CI: overlap, narrow channel, off-frequency TX |
| `impairments.yaml` | 2.5 MS/s, 3 s | DC offset, IQ imbalance image, spur, drift |
| `full_band_busy.yaml` | 10 MS/s, 20 s | Full 2 m band benchmark (~800 MB; use `--duration`) |

`synth.analysis` holds the numpy/scipy reference demodulator and comparison
metrics (alignment, SNR against reference, tone SINAD).

Score a scanner run against ground truth (recall, start/end error, audio SNR
against the truth audio, false positives):

```sh
scanner run ../../out/ci_smoke --config ../../config/example-2m.toml \
    --profile 2m-25k --out ../../out/run --json > ../../out/run.json
uv run synth score ../../out/ci_smoke.truth.json ../../out/run.json
```

Tests:

```sh
cd tools/synth
uv run --extra test pytest                       # generator tests
SCANNER_BIN=../../target/release/scanner \
  uv run --extra test pytest                     # plus Python -> Rust end-to-end
```

## Recording real captures

Until `scanner record` exists (Phase 2), capture SigMF-compatible raw files
with vendor tools and write a `.sigmf-meta` by hand: datatype `ci16_le`,
sample rate, center frequency and UTC start time. At 10 MS/s int16 a capture
is ~40 MB/s, so keep clips to 30 s – 2 min.
