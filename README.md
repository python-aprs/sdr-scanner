# sdr-scanner

Scan and simultaneously decode multiple narrowband FM streams from a wide chunk of spectrum. It writes one WAV per transmission and serves a web UI for live spectrum, listening and recordings.

## Build

Needs stable Rust 1.85+ and CMake (libopus is built from source).

```sh
cargo build --release -p scanner-cli
```

The binary is `target/release/scanner`. To use an SDRplay radio, install the SDRplay API 3.x from sdrplay.com and build with `--features sdrplay`.

## Run

Make an admin account, then start the scanner:

```sh
scanner admin create masen --db out/auth.sqlite
scanner serve --source sdrplay --config config/rainier.toml --out out
```

Open http://127.0.0.1:8080. Recordings go to `out/YYYY/MM/DD/`, indexed in `out/index.sqlite`.

Without a radio, replay a SigMF recording at its real rate:

```sh
scanner serve --source loop:captures/x.sigmf-meta --config config/rainier.toml
```

Or scan a recording as fast as possible and exit:

```sh
scanner run captures/x.sigmf-meta --config config/rainier.toml --out out
```

Useful `serve` flags:

| Flag | Default | |
|---|---|---|
| `--http` | `127.0.0.1:8080` | `0.0.0.0:8080` to listen on the LAN, `off` for no web UI |
| `--public-recordings`, `--public-audio` | off | let visitors who aren't logged in browse and play |
| `--disk-budget-gb` | 8 | oldest recordings are deleted past 95 % |
| `--anon-egress-gb` | 3 | monthly data allowance for visitors |
| `--noise-squelch` | 8 | dB of quieting needed to play audio; 0 for power squelch only |
| `--spur-secs` | 300 | a carrier steady this long is dropped and locked out; 0 to turn off |
| `--lna`, `--gr` | 3, 40 | RSP1A gain; use 9 and 59 near a strong transmitter |

`scanner <command> --help` lists the rest.

## Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

End-to-end tests generate synthetic IQ in Python (needs [uv](https://docs.astral.sh/uv/)) and run the scanner on it:

```sh
cd tools/synth
SCANNER_BIN=../../target/release/scanner uv run --extra test pytest
```

[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) covers the internals, the synthetic scenarios and recording captures. [docs/PLAN.md](docs/PLAN.md) is the design and roadmap.

## Prior art

https://github.com/madengr/ham2mon - python glue, using gnuradio. Not
maintained. This is what I was using before, but it's not performant enough to
run on single board computers. (my fork https://github.com/masenf/ham2mon/tree/py3). This project has no intention of recreating the cursed curses interface this app uses... It's 2026, of course we're going to expose the controls via a web interface!
