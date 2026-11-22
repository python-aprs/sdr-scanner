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
  store/     WAV and Ogg Opus writers, SQLite databases, recorder thread
  cli/       the `scanner` binary
  web/       HTTP API (axum): status, recordings search, audio; role guards
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
slip), the DSP thread scans, and the recorder thread writes. The source is a
radio (`--source sdrplay`, below) or a recording replayed at its real rate
(`replay:` once, `loop:` forever). Ctrl-C closes open transmissions and exits. It prints a status line
(DSP load, ring fill, drops, active channels) every `--status-every` seconds.

`serve` also starts the HTTP API (`--http 127.0.0.1:8080` by default, `off`
to disable), routes under `/api/v1` (see `crates/web/src/lib.rs`):
`status` (live snapshot: active channels, noise floor, DSP load, drops),
`recordings` (search by `freq_min`/`freq_max` Hz, `since`/`until` Unix s,
`min_secs`; page with `before_id`), `recordings/{id}` and
`recordings/{id}/audio` (the Opus copy if there is one, else the WAV;
`?format=wav` or `?format=opus` to choose; HTTP range requests). Anonymous visitors see
live status only unless `--public-recordings` / `--public-audio`.

### Admin accounts

Admin access is a password login (browser session) or an API token (scripts).
Accounts live in `auth.sqlite` (default: in `--out`; `--auth-db` to move it),
created owner-readable only. There is no default account:

```sh
scanner admin create masen                 # prompts for a password (10+ characters)
scanner admin token create backup --expires-days 90   # prints scn_... once
scanner admin list | token list | token revoke 3 | passwd masen | delete masen
scanner admin logout                       # end every browser session
scanner admin audit                        # logins, failures, changes
```

(`--db` points the commands at another `auth.sqlite`; they are safe to run
while `serve` is up.)

- `POST /api/v1/auth/login` `{"username","password"}` sets an `HttpOnly`,
  `SameSite=Strict`, `Secure` session cookie and returns a `csrf_token`.
  Requests that change something and are authenticated by the cookie must
  send it back in `X-CSRF-Token`. Bearer tokens don't need it.
- `GET /api/v1/auth/session` says who you are (and returns the CSRF token);
  `POST /api/v1/auth/logout`; `POST /api/v1/auth/password`
  `{"current","new"}` ends every other session of the account.
- `GET/POST /api/v1/auth/tokens`, `DELETE /api/v1/auth/tokens/{id}`.
- Passwords are argon2id hashes; session cookies and tokens are stored only
  as SHA-256 hashes. Sessions end after 24 h unused or 7 days in total.
  After 5 failed logins from one address or for one username, each further
  failure doubles a lockout (up to 15 minutes; `429` with `Retry-After`).
- Browsers send `Secure` cookies only over HTTPS or to localhost. Put the
  scanner behind a TLS reverse proxy when it is reachable from outside;
  `--insecure-cookies` is for plain HTTP on a trusted LAN.

```sh
scanner serve --source loop:out/ci_smoke --config config/example-2m.toml \
    --profile 2m-25k --out out/live
curl localhost:8080/api/v1/status
curl -H "Authorization: Bearer scn_..." localhost:8080/api/v1/recordings
```

### Web interface

`scanner serve` also serves a browser UI at `http://127.0.0.1:8080/`
(`--http` sets the address). It is plain HTML, CSS and JavaScript in
`crates/web/ui/`, compiled into the binary, with no build step and nothing
loaded from other sites. Edit the files and rebuild to change it.

- **Live**: status, a spectrum and waterfall of the profile's span with
  active transmissions marked, what is on the air now, and the latest
  recordings. Click the spectrum to see that channel's recordings.
- **Recordings**: search by channel or frequency, start time and length,
  then play in the browser (the player can step through the list, and
  plays the small Opus copy) or download the WAV.
- **Channels**: every watched slot with its name, type, mode and lockout or
  priority flag; repeaters link their input and output.
- **Admin**: log in and out, change your password, and create or revoke API
  tokens. It also shows what visitors can see.

Logged in as admin, the Channels page can lock out a channel, unlock it, or
make it a priority channel, and the Live page has a Lock out button on each
transmission on the air. Changes take effect at once: a recording on a
channel that gets locked out ends there. They are saved in
`<out>/settings.sqlite` and override the config file until you press Reset
(`DELETE /api/v1/channels/{freq_hz}/override`). A lockout on a frequency
that isn't a watched channel, such as an off-grid signal or a birdie that a
grid slot retunes onto, covers ±5 kHz. Every change goes in the audit log
(`scanner admin audit`). Over the API:

```sh
curl -X PATCH -H "Authorization: Bearer scn_..." -H "Content-Type: application/json" \
    -d '{"locked_out": true}' localhost:8080/api/v1/channels/145212500
```

The UI uses only `/api/v1`. Live data comes over one WebSocket,
`/api/v1/live`: about one status message a second, plus spectrum frames of
1,024 bins (one byte each) at 1, 2 or 5 frames a second. The spectrum stops
while the Live page isn't showing or the tab is hidden, so an idle tab costs
about 1 kB/s. The wire format is documented in `crates/web/src/live.rs`. The
socket checks the same login as the API, and it refuses pages from other
sites (`Origin` must match `Host`). A reverse proxy must therefore pass the
original `Host` header and forward WebSocket upgrades.

**Live listening.** The Live page's speaker controls play what is on the
air: **Follow** plays one transmission at a time, priority channels first,
like a hardware scanner. **All** mixes everything that is open. The
headphone button on a row listens to just that channel (pick several to mix
them). The audio is Opus at about 12 kb/s in 40 ms frames, sent over
`/api/v1/audio` (format in `crates/web/src/audio.rs`) and decoded by the
browser's WebCodecs. Squelched channels send nothing. The scanner encodes
only while someone is listening, and it uses a tap that doesn't wait for
the recordings' squelch-tail hold-back, so audio arrives about a quarter
second after the air. Listening needs the same permission as recording
audio (`--public-audio`, or a login). Building needs CMake, because libopus
is compiled from source (`brew install cmake` or `apt install cmake`).

Every response carries a strict Content Security Policy: same-origin
scripts, styles and connections only, no inline script, and no framing.
Static files are revalidated with ETags, so repeat visits cost a 304.

`scanner run` writes `out/YYYY/MM/DD/146.5200_20261122T065403.123Z.wav` per
transmission (via `.wav.part` and a rename) and one row per transmission in
`out/index.sqlite`. Next to each WAV goes an Ogg Opus copy (`.opus`, mono,
16 kb/s, 40 ms frames) for the web player: about an eighth of the size, and
it decodes to exactly the WAV's length. `--no-opus` skips it; audio rates
Opus can't take (anything but 8, 12, 16, 24 or 48 kHz) get no copy. `--json` prints a machine-readable summary that
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

### Database migrations

The scanner keeps three SQLite databases: the recordings index
(`<out>/index.sqlite`, `scanner_store::index::SCHEMA`), the accounts
database (`auth.sqlite`, `scanner_web::auth::SCHEMA`) and settings edited
while running (`<out>/settings.sqlite`, `scanner_store::settings::SCHEMA`). Each schema is a list
of migrations in `scanner_store::migrate`; `PRAGMA user_version` records how
many have run and `PRAGMA application_id` marks which database a file is
(`SCNI`, `SCNA` or `SCNS`), so passing one where another belongs fails cleanly.

- The writer upgrades on open: the recorder for the index, `scanner serve`
  and `scanner admin` for accounts. All pending steps run in one
  `BEGIN IMMEDIATE` transaction, so a failed step changes nothing and two
  processes can't migrate at once.
- Before upgrading a database that holds data it saves
  `<file>.v<old version>-<unix seconds>.bak` next to it, with the same
  permissions as the database.
- A database from a newer scanner is refused with a message, never
  downgraded.
- The web API opens the index with `Index::open_reader`, which requires the
  current version and doesn't migrate.

To change a schema, append a `Migration` (SQL, or a Rust function for data
changes) to its list. Never edit one that has shipped. Then update the
snapshot that `schema_snapshot` compares against
(`crates/store/schema/index.sql` or `crates/web/schema/auth.sql`) and add
an upgrade test from the previous version. Databases made before migrations
existed are adopted as version 1, so later steps must not use
`IF NOT EXISTS`.

### SDRplay (RSP1A)

The SDRplay backend is behind the `sdrplay` cargo feature, because it needs
the vendor's SDRplay API 3.x, which can't be redistributed. Install the API
from sdrplay.com (it puts `sdrplay_api.h` in `/usr/local/include` and starts
the `sdrplay_apiService` daemon), then:

```sh
cargo build --release -p scanner-cli --features sdrplay
target/release/scanner devices        # lists sdrplay:<serial>  <model>
target/release/scanner serve --source sdrplay --config config/rainier.toml \
    --out out/live [--lna 3 --gr 40 --rate 8e6 --center-mhz 146.0125]
```

Set `SDRPLAY_API_INCLUDE` if the header is somewhere else. Without the
header the feature still builds (with a warning) so CI can check it, but the
radio then fails to open with a message saying to install the API and rebuild. The library is
opened at run time (`SDRPLAY_API_LIB` overrides where), so the binary starts
and explains what is missing on a machine without the API. Only another
program holding the radio (SDRconnect, SDRuno) stops it from opening.

Radio options: `--lna` (LNA state, 0 is the most gain), `--gr` (IF gain
reduction 20–59 dB), `--rate` (2–10.66 MS/s), `--center-mhz` (default: middle
of the profile's span plus 12.5 kHz), `--if-bw-khz` (8000), `--agc <dBFS>`
(off by default, because AGC steps make the noise floor jump), `--ppm`,
`--bias-t`, `--fm-notch`, `--dab-notch`. Overload warnings, samples the API
reports lost (zero-filled, so timestamps hold) and unplugging show up in the
status line; unplugging ends the run after the open recordings are written.

How it fits: `crates/sources/sdrplay/shim.c` is the only code that sees the
vendor header; it gives Rust a small interface (`scanner_sdrplay.h`).
`scanner_sources::sdrplay::StreamState` (plain Rust, unit tested without the
API) interleaves each callback's I and Q arrays into full ring blocks and
turns jumps in the API's sample counter into zero-filled gaps.
`crates/sources/sdrplay/fake/run-e2e.sh` builds the feature against a
stand-in header and runs `serve` against a fake API library that streams a
keyed carrier, so the whole path can be exercised without the SDK or a radio.

The Airspy backend (`--features airspy`, libairspy) is next.

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
