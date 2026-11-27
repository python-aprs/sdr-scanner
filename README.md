# sdr-scanner

Scan and simultaneously decode multiple narrowband FM streams from a wide chunk of spectrum. It writes one WAV per transmission and serves a web UI for live spectrum, listening and recordings.

## Build

Needs stable Rust 1.85+ and CMake (libopus is built from source).

```sh
cargo build --release -p scanner-cli
```

The binary is `target/release/scanner`. To use an SDRplay radio, install the SDRplay API 3.x from sdrplay.com and build with `--features sdrplay`.

Every NBFM channel, listed or on the fallback grid, also listens for DMR, System Fusion (C4FM), D-STAR and AFSK1200 packet. When one of them shows up (two DMR or C4FM syncs, a D-STAR header, a packet that passes its FCS), the channel switches to that decoder: calls are recorded under their own mode and packets are logged, and the FM recording of the digital noise is thrown away. An FM transmission that runs on into C4FM within the hang time (a Fusion repeater) keeps its FM part. `--no-auto-detect` turns this off, so only a channel's `mode` decides.

DMR channels (`mode = "dmr"`) are decoded natively: calls are recorded per timeslot with their talkgroup, radio ids and talker alias, and the raw AMBE+2 voice frames are kept beside each recording (`.amb`). Turning those frames into audio needs an AMBE+2 decoder, which is not in the default build. Install [mbelib-neo](https://github.com/arancormonk/mbelib-neo) (or classic mbelib) and build with `--features mbelib`; set `MBELIB_DIR` to its install prefix if it isn't under `/usr/local`, `/opt/homebrew` or `/usr`. `scanner ambe <file.amb>...` converts kept frames to WAV later. mbelib is GPL and its patent status is unclear, so a binary built with it is yours to use, not to redistribute.

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

To pick gain and thresholds for your antenna, stop `serve` and run `scanner calibrate --config config/rainier.toml`. It sweeps the radio's gain for about two minutes and prints the flags to use.

Useful `serve` flags:

| Flag | Default | |
|---|---|---|
| `--http` | `127.0.0.1:8080` | `0.0.0.0:8080` to listen on the LAN, `off` for no web UI |
| `--public-recordings`, `--public-audio` | off | let visitors who aren't logged in browse and play |
| `--disk-budget-gb` | 8 | oldest recordings are deleted past 95 % |
| `--anon-egress-gb` | 3 | monthly data allowance for visitors |
| `--noise-squelch` | 8 | dB of quieting needed to play audio; 0 for power squelch only |
| `--spur-secs` | 300 | a carrier steady this long is dropped and locked out; 0 to turn off |
| `--background-secs` | 300 | comb guard: channels learn steady interference over this long and must rise above it to open; 0 to turn off |
| `--kiss` | off | e.g. `127.0.0.1:8001`: decoded APRS/packet frames as KISS over TCP |
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
