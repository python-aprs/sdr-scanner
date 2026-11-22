# Development

The design and roadmap live in [PLAN.md](PLAN.md). This page is how to build
and test what exists so far (Phase 0).

## Layout

```
crates/
  core/      band-profile config, sample-counter clock, PSD + peak finding
  sources/   Source trait, SigMF FileSource; Airspy/SDRplay stubs (Phase 2)
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
metrics (alignment, SNR against reference, tone SINAD) that Phase 1 tests
will compare the Rust pipeline against.

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
