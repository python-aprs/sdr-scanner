# sdr-scanner: design and test plan

Status: approved 2026-11-22; Phase 0 done, Phase 1 (offline core) in progress. A working config example is in `config/example-2m.toml`.

Goal: a purpose-built replacement for ham2mon (originally to bring
scanner.kf7hvm.com back online). Watch the whole 2 m band (144–148 MHz), find
every active narrowband FM channel in the wideband IQ stream, demodulate all of them at once, and write one WAV file per
transmission named by center frequency and timestamp. Run on Linux and macOS,
x86_64 and arm64. Hardware for now: SDRplay RSP1A (local Mac) and Airspy R2
(remote Raspberry Pi).

---

## 1. What ham2mon does, and why it is slow

Read from `masenf/ham2mon@py3` (`apps/receiver.py`, `scanner.py`, `estimate.py`).

**Pipeline.** One `osmosdr.source` feeds two branches:

1. *Spectrum branch:* `stream_to_vector → keep_one_in_n → FFT (Blackman-Harris)
   → |x|² → integrate 100 → probe`. The Python scanner polls the probe at up to
   10 Hz.
2. *Demod branch:* a fixed number (`num_demod`) of `TunerDemodNBFM` blocks, each
   connected to the full-rate source: `freq_xlating_fir (÷5) → fir (÷5) → fir
   (÷fs/1MHz) → pwr_squelch → quadrature_demod → audio fir (÷5) → pfb resampler →
   8 kHz → blocking squelch → wavfile_sink` (8-bit by default).

`scan_cycle()` thresholds the spectrum at an absolute dB value, merges adjacent
bins into channels with a power-weighted centroid, rounds to 5 kHz, applies
priority and lockout lists, and retunes idle demodulators. A retune closes the
WAV and opens `wav/<MHz:.3f>_<unix time>.wav`, deleting it if it stayed empty.

**Where the CPU goes.** Each demodulator runs its first FIR at the full input
rate, all the time, even when parked at 0 Hz. The first-stage filter
(`firdes_low_pass(1, 1, 0.09, 0.01)`) is roughly 240 taps, and it becomes a
complex-tap filter after frequency translation. At 2.5 MS/s that is about
0.5 G real multiply-adds per second **per demodulator**, so 4–8 demods is 2–4
GMAC/s before anything is detected. A Pi cannot keep up. The cost scales with
`num_demod × sample_rate`, not with the number of active signals.

**Behaviour problems worth fixing, not copying:**

- Fixed demod count: a busy band drops channels; a quiet band still pays full cost.
- Absolute squelch and detection thresholds: change the gain and they break.
- No hang time: a channel that dips below threshold for one 100 ms scan cycle is
  dropped and retuned, which splits one transmission into several files and
  clips the start of the next.
- 5 kHz rounding of a centroid estimate: drifts between adjacent channels on
  wide or off-frequency signals.
- 8-bit WAV and per-file `time.time()` second resolution.
- Detection sees only `1/keep_one_in_n` of the samples and lags up to ~100 ms+.

---

## 2. Signal and hardware facts that shape the design

| | RSP1A (Mac) | Airspy R2 (Pi) |
|---|---|---|
| API | SDRplay API 3.x, closed binary + `sdrplay_apiService` daemon; Linux x86_64/arm64, macOS (Intel + Apple Silicon) | `libairspy`, open source (Apache/BSD), builds everywhere |
| Delivery | Callback with separate `short *xi, *xq` arrays | Callback with `airspy_transfer` (float32 or int16 IQ, or raw real) |
| Mode for 144–148 MHz (4 MHz) | Zero-IF, 6 MS/s with the 5 MHz IF filter (or 8 MS/s with the 8 MHz filter for more edge margin). DC spike at the LO, so park the LO between channels. | 10 MS/s IQ (or 20 MS/s raw real samples). Real-sampling architecture, so no DC spike. IQ conversion is done in software by libairspy unless we take raw samples. |
| ADC | 14-bit (12-bit at higher rates) | 12-bit |

Notes:

- The span is the full 2 m band, 4 MHz. Both radios cover it with margin: the
  RSP1A at 6–8 MS/s (its ADC drops to ~12 bits at these rates, which is fine
  for NBFM), the Airspy R2 at 10 MS/s. Tune the LO to ~146.0 MHz, nudged off any
  listed channel (this matters for the RSP1A's DC spike). Measure sensitivity
  at 144.0 and 148.0 on real captures to confirm the edges are usable.
- On the Airspy at 10 MS/s, libairspy's real-to-IQ conversion is a real CPU
  cost on the Pi. Taking raw real samples and using a real-input FFT gives the
  analytic spectrum directly and skips that step. Because the full band needs
  10 MS/s, this moves up from "later" to a Phase 2 benchmark item: build the
  libairspy IQ path first, measure it, and switch if it eats too much CPU.
- 10 MS/s int16 IQ is 40 MB/s (320 Mbit/s). Streaming raw IQ off the remote site
  is out of the question, so **the full scanner runs on the Pi** and only audio and
  metadata leave the site.
- Channel plan is configuration, not code. The main target is ham 2 m / 70 cm
  NBFM, where channels are traditionally 25 kHz wide (±5 kHz deviation, ~16 kHz
  occupied by Carson's rule) and denser areas are moving to 12.5 kHz (±2.5 kHz
  deviation, ~11 kHz occupied). Every value that depends on this is derived from
  a **band profile** (see 3.7): channel spacing, channel filter bandwidth,
  expected deviation (for audio gain and squelch), raster offset, and the
  per-channel output rate. Mixed plans are allowed: a 25 kHz raster with some
  frequencies overridden to 12.5 kHz.

---

## 3. Proposed architecture

### 3.1 Language: Rust core, Python for tooling

**Recommendation: Rust** for the scanner binary, Python + numpy for test tooling
(signal generation, analysis, plots, comparison with ham2mon).

Why Rust over the alternatives:

- **vs C++ (+ liquid-dsp / FFTW):** similar speed, but Rust gives data-race
  checking on the multithreaded pipeline, one build tool, and painless
  cross-compiling to `aarch64-unknown-linux-gnu` (`cross` or `cargo zigbuild`)
  for a single binary on the Pi. `rustfft` has NEON (aarch64) and AVX (x86)
  kernels; FFTW via the `fftw` crate is an option if it benchmarks better.
- **vs Python + numpy:** the heavy work (big FFTs) would be in numpy and fast
  enough, but the per-channel, per-block glue is Python-speed, the GIL forces
  multiprocessing with shared-memory IPC, and deployment on the Pi means a
  numpy/scipy stack. Python is ideal as the *reference implementation* for tests.
- C FFI is easy: `bindgen` over `sdrplay_api.h` and `airspy.h`, each behind a
  cargo feature so the core builds and tests without either SDK installed.
- Web/control: `axum` + `tokio` in the same binary, isolated from the DSP
  threads.

### 3.2 Channelizer: shared FFT, per-channel fast convolution

Options considered:

| Approach | Cost model | Verdict |
|---|---|---|
| Per-channel DDC (ham2mon) | `channels × fs × taps` | Too slow; cost paid even when idle |
| Per-channel NCO + CIC/half-band cascade | `channels × fs × ~10` | Better, still scales with channels at full rate |
| Polyphase filter bank (PFB) | `fs × (taps/branch + FFT)`, independent of channels | Fast, but channel centers fixed to bin centers; off-raster signals need a second fine-tune stage |
| **Fast-convolution filter bank (overlap-save FFT)** | One big FFT for everyone + a small IFFT per *active* channel | **Recommended** |

How the fast-convolution filter bank works:

1. Take overlapping blocks of N input samples (e.g. N = 2¹⁷ at 10 MS/s, so bins
   are ~76 Hz) and run one forward FFT per block, with 25–50 % overlap
   (overlap-save).
2. **Detection gets this FFT for free:** |X|² averaged over a few blocks is the
   PSD the detector uses. No separate spectrum branch, and it sees every sample.
3. For each *active* channel at frequency f: take the bins covering that
   channel's bandwidth around f (any center, quantized to 76 Hz), multiply by
   the channel filter's frequency response, run a small IFFT, and discard the
   overlap. The bandwidth comes from the band profile, so the slice and IFFT size
   do too. For a 25 kHz channel: ~328 bins, 512-point IFFT, ~39 kS/s complex
   baseband. For 12.5 kHz: ~164 bins, 256-point IFFT, ~20 kS/s. Filters are
   designed once per distinct bandwidth at startup and shared by all channels.
4. FM discriminate, de-emphasize, filter and resample to the audio rate, squelch,
   write.

Rough budget at 10 MS/s (full 2 m band): the forward FFT is ~100–120 flops per
input sample, about 1–1.2 GFLOP/s total, regardless of channel count. Each
active channel is a few MFLOP/s. The target Pi is a **Pi 5 (8 GB, 64-bit OS)**:
4× Cortex-A76 at 2.4 GHz with NEON, roughly 2–3× a Pi 4 per core. One A76 core
should manage a few GFLOP/s of FFT work, so the shared stage should take about
a third to a half of one core, leaving ~3 cores for libairspy's IQ conversion,
channels, encoding and the web server. A 2¹⁷-point complex FFT (1 MB of
complex floats) is bigger than the A76's L2 cache, so the benchmark should
compare one big FFT against a two-stage split (e.g. a cheap half-band split
into two 5 MS/s halves, each with its own 2¹⁶ FFT, which also spreads the work
over two cores).
These are estimates; Phase 1 benchmarks confirm them.

Fallback if fast convolution has problems (latency, block artifacts): a 2×
oversampled PFB with M = 128 bins plus a small per-channel NCO and decimating FIR.

### 3.3 Detection, channel management and squelch

- **Noise floor:** a rolling low percentile (e.g. 20th) of the PSD over the band,
  computed per segment so filter rolloff at band edges does not bias it. All
  thresholds are dB above this floor, so gain changes do not break them.
- **Raster:** detections snap to the band profile's channel plan: first an
  explicit list of known frequencies (band-plan simplex channels, coordinated
  repeater outputs and inputs, each with its own width), then a fallback grid
  (25 / 12.5 kHz, or a fine 5 kHz grid) to catch anything unlisted. Each slot
  owns a window of its channel bandwidth; the integrated power in that window
  is the detection statistic. This is the "quantized" channel model: we decide
  *which slots are active*, never chase a raw centroid. Unlisted detections are
  tagged as such, so the UI can suggest adding them to the list.
- **State machine per slot:** `idle → candidate` (above open threshold for
  ≥ attack time, e.g. 50–100 ms) `→ active` (open a file, include a pre-roll of
  ~200 ms buffered audio so the start is not clipped) `→ hang` (below close
  threshold, hysteresis of a few dB) `→ closed` after hang time (e.g. 1.5 s,
  configurable; it covers repeater squelch tails and short pauses) `→ idle`.
  Transmissions shorter than a minimum length are discarded.
- **Pre-roll:** keep a short ring of recent FFT blocks so a channel's first audio
  can be reconstructed from blocks captured before detection fired.
- **Squelch inside an active channel:** power squelch, plus an optional *noise
  squelch* (energy in the 4–7 kHz band of the discriminator output, the way
  real radios do it). Noise squelch rejects carriers without modulation and
  wideband noise better than power alone.
- **Lists:** lockout list, priority list (always demodulate if active), and
  automatic birdie detection: a slot that stays "active" for N minutes with no
  audio-band energy variance is flagged as a spur and locked out until reviewed.
- **Optional tags:** CTCSS/DCS detection with Goertzel filters, written to
  metadata so the web UI can show which repeater tone was used.
- **Concurrency cap:** a max-active limit with priority ordering, so a band-wide
  noise burst cannot spawn a hundred files.

### 3.4 Threads and buffering

One process, a small fixed set of threads, all buffers preallocated:

```
 SDR driver callback ──► [SPSC ring of fixed blocks, int16 IQ]
                                   │  (never blocks; counts overruns)
                                   ▼
                       FFT thread: convert, window-free overlap-save FFT
                                   │ spectrum frames ──► Detector / Channel manager
                                   │ FFT blocks (ref-counted, pooled)
                                   ▼
               Channel worker pool (N-2 cores): per-channel slice → IFFT
                    → discriminator → audio filter → squelch
                                   │ audio chunks
                                   ▼
                    Writer thread: WAV files + metadata index
                                   │
         tokio runtime: HTTP API, WebSocket (status, spectrum), file serving
```

- **Driver callback** only copies into a lock-free single-producer ring of fixed
  blocks (e.g. 64 K samples) and returns. If the ring is full the block is
  dropped and counted; the driver is never blocked.
- **Timestamps come from the sample counter**, not wall clock: `t = t0 +
  n / fs`, with t0 taken at stream start and periodically re-anchored. Dropped
  blocks advance the counter, so file times stay correct and drops are visible.
- **FFT blocks are shared, not copied:** each forward-FFT output goes into a
  pooled, reference-counted buffer; channel workers read their bin slice from
  it. When the last reader is done, the buffer returns to the pool.
- **Per-channel work fans out** on a small worker pool (rayon or a hand-rolled
  pool) once per FFT block. Channels are independent, so this is
  embarrassingly parallel. On the 4-core Pi 5: 1 core for driver + FFT, the rest for
  channels, writer and web.
- **Backpressure:** if channel work falls behind, whole FFT blocks are dropped for
  the lowest-priority channels first, logged and exported as a metric.
- Threads over processes: shared memory without IPC, and Rust makes it safe. The
  web UI and API run on the tokio runtime in the same binary (see 3.5a), fed
  from the DSP threads through bounded channels so a slow client never stalls
  the pipeline.

### 3.5 Output and the web interface

- **Files:** `out/YYYY/MM/DD/146.5200_20261122T065403.123Z.wav`, mono 16-bit PCM
  at 8 kHz (or 16 kHz, configurable). Written to `*.wav.part` and renamed on
  close, so anything that watches the folder only ever sees complete files.
  A ham2mon-compatible name (`146.520_1795330443.wav`) can be a config option.
- **Web copy:** when a WAV closes, the writer also encodes an Opus file
  (`.opus`, ~16 kb/s mono). The web UI plays the Opus copy; the WAV is the
  archive and is downloadable by admin (or anonymous if enabled). Opus is about
  8× smaller than 8 kHz/16-bit WAV, which matters for the uplink cap (see 3.5b).
- **Metadata:** a SQLite index (one row per transmission: frequency, start, end,
  duration, peak/mean SNR, CTCSS, channel name, repeater link and whether it was
  an input or output, file paths) plus an optional JSON sidecar per file. The
  scanner's own API queries it.
- **Retention by disk budget:** a configurable size budget for the recordings
  folder, default **8 GB**. A housekeeping task deletes the oldest recordings
  (WAV + Opus + index row marked expired) when usage crosses a high-water mark
  (e.g. 95 % of budget) until it is back under a low-water mark (e.g. 85 %). It
  also never lets free space on the filesystem fall below a floor. Priority
  channels or starred recordings can be exempt. For scale: 8 kHz/16-bit WAV is
  ~58 MB per hour of audio, so 8 GB holds ~120 hours of WAV plus Opus copies.
  An option to keep only Opus after N days stretches that ~8×.
- **Scope:** the web interface and API ship inside the scanner binary. Any
  separately hosted public site is out of scope; it can consume the same API.

### 3.5a Web interface and API

The scanner serves its own UI and API (`axum`, one port, static UI assets
embedded in the binary). It must be safe to expose publicly, so auth is built
in from the start.

**API surface (versioned under `/api/v1`):**

| Area | Endpoints (sketch) | Role |
|---|---|---|
| Live status | active channels, noise floor, source info; WebSocket feed of spectrum/waterfall (~5–10 Hz) and channel open/close events | anonymous (on by default) |
| Recordings | list/search by frequency, time range, duration, CTCSS; fetch metadata; stream/download audio (HTTP range requests) | admin by default; anonymous once the operator enables it |
| Live audio | per-channel Opus over WebSocket; listener picks one or more channels or "follow all activity" | admin by default; anonymous if enabled |
| Control | gain, thresholds, hang/attack, band profile selection, lockout and priority edits, start/stop source | admin |
| Housekeeping | delete recordings, retention settings, drop counters and diagnostics, restart | admin |
| Auth | login, logout, current session, password change | — |

**Auth and roles:**

- Two roles for now: `anonymous` (no login) and `admin`. Roles are an enum
  checked by a per-route guard, so adding e.g. `operator` (can lock out
  channels but not change gain) later is a small change.
- Each anonymous permission is a config switch. Fresh-install defaults:
  `public.live = true`, `public.recordings = false`, `public.audio = false`.
  The operator opts in to sharing recordings, so the same build covers a
  private single-operator install and a public one. The switches are also
  editable by an admin in the UI.
- Admin accounts live in a config/SQLite table with argon2id password hashes.
  First run: a `scanner admin create` CLI command, or a one-time setup token
  printed to the log. No default password.
- Browser sessions: login sets an `HttpOnly`, `Secure`, `SameSite=Strict`
  session cookie backed by a server-side session table (revocable).
  State-changing requests require the cookie plus a CSRF token header.
- Scripts and automation: admin-created API tokens (bearer, stored hashed,
  named, revocable, optional expiry).
- WebSocket upgrades check the same session or token; control messages over
  the socket are re-checked per message against the role.
- Hardening for public exposure: login rate limiting and lockout backoff,
  per-IP rate limits on audio downloads, request size limits, an audit log of
  admin actions. TLS is expected from a reverse proxy (Caddy/nginx), with an
  option for built-in TLS (rustls) for simple installs; `Secure` cookies are
  enforced unless explicitly running on localhost.
- Later, if wanted: OIDC/OAuth login as an alternative to local passwords.

**Testing the web layer:** API integration tests run the server against a
FileSource replay and assert role enforcement on every route (a table of
route × role → expected status, so a new route without a guard fails CI),
CSRF and session expiry behaviour, and WebSocket auth.

### 3.5b Live listening and the uplink budget

The remote site has ~20 Mb/s but roughly a **5 GB/month** data cap. That is
~170 MB/day for everything: listeners, the UI, updates and SSH. Every byte
served from the Pi counts, so the web layer is designed around it.

**Live audio in the browser:**
- Each active channel's audio is already at 8 kHz; an Opus encoder per channel
  (only while at least one listener is subscribed) produces 20 ms frames at
  ~12–16 kb/s, sent over the WebSocket with the channel id and sample-counter
  timestamp.
- The browser decodes with WebCodecs `AudioDecoder` (Opus) where available,
  with a small WASM Opus decoder as a fallback, and plays through an
  AudioWorklet with a short jitter buffer. Target latency under ~1 s from air to
  speaker.
- Listening modes: a single channel, a chosen set mixed together, or "follow
  activity" (like a scanner: play whichever channel opens, priority first).
- Squelched channels send nothing, so silence costs no bandwidth.

**Bandwidth budget for everything served:**

| Stream | Rough cost per viewer | Notes |
|---|---|---|
| Live audio | ~2 kB/s (~7 MB per hour listened) | only while a channel is active |
| Recording playback | ~2 kB/s of audio (Opus) | WAV would be 16 kB/s |
| Spectrum/waterfall | 0.5–5 kB/s | u8 dB values, a few hundred bins, 1–5 Hz, sent only while the tab is visible |
| Status/events | negligible | |

- The server counts bytes sent per role and per calendar month, shown on the
  admin page.
- A configurable **monthly egress budget** (default e.g. 3 GB, leaving headroom
  under the 5 GB cap) applies to anonymous traffic: as it is approached,
  anonymous spectrum drops to a lower rate, then anonymous live audio and
  downloads pause with a clear message. Admin traffic is counted but not
  blocked.
- Per-connection limits: max concurrent anonymous listeners, idle-tab timeout,
  and spectrum defaults chosen for low bandwidth (clients can ask for more if
  the operator allows it).
- If public use grows, the answer is a relay in the cloud (one stream up from
  the Pi, fan out from there). Out of scope for now, but the API should not
  prevent it.

### 3.6 Source backends

A `Source` trait (`start`, `stop`, `set_freq`, `set_gain`, `sample_rate`,
delivers blocks into the ring) with these implementations:

1. `FileSource`: SigMF / raw `cf32`, `ci16`, `cu8` files, paced in real time or
   as fast as possible. Used by nearly all tests.
2. `AirspySource` (libairspy, feature `airspy`).
3. `SdrplaySource` (SDRplay API 3.x, feature `sdrplay`).
4. Optional `SoapySource` (feature `soapy`) for other hardware later.

Direct backends rather than SoapySDR for the two target radios: fewer layers,
direct access to RSP1A low-IF mode and Airspy raw-real mode, and int16 straight
into our ring. SoapySDR stays an optional backend. The SDRplay API cannot be
redistributed, so CI builds without the `sdrplay` feature (or against a stub
header) and the real backend is tested on the Mac.

### 3.7 Band profiles (configuration)

Nothing about the band is hard-coded. A config file (TOML) holds one or more
band profiles, and each source runs one profile:

```toml
[profiles.2m-25k]
span          = { start = "144.000 MHz", stop = "148.000 MHz" }
spacing       = "25 kHz"       # raster step
offset        = "0 kHz"        # raster anchor offset
bandwidth     = "16 kHz"       # channel filter passband
deviation     = "5 kHz"        # sets audio gain and noise-squelch scaling
audio_rate    = 8000
deemphasis    = true

[profiles.2m-12k5]
inherit       = "2m-25k"
spacing       = "12.5 kHz"
bandwidth     = "11 kHz"
deviation     = "2.5 kHz"

[profiles.70cm-25k]               # bands do not share channel lists
span          = { start = "440.000 MHz", stop = "450.000 MHz" }
spacing       = "25 kHz"
bandwidth     = "16 kHz"
deviation     = "5 kHz"

# Per-frequency overrides on top of the grid
[[profiles.2m-25k.channels]]
freq      = "146.5200 MHz"
bandwidth = "11 kHz"
deviation = "2.5 kHz"
priority  = true

# A repeater entry creates two slots: the output and the input (output + offset)
[[profiles.2m-25k.repeaters]]
name          = "Example repeater"
output        = "146.8400 MHz"
offset        = "-600 kHz"
tone          = 100.0
monitor_input = true

# Stays on the spectrum, never recorded
[[profiles.2m-25k.lockouts]]
start  = "144.000 MHz"
stop   = "144.300 MHz"
reason = "CW/SSB/weak signal"
```

**Channel list for the first deployment (Rainier, OR):** the site hears
coordinated repeaters on both sides of the Columbia, so the list combines:

- Simplex channels from the WWARA 2 m band plan.
- WWARA (Washington) and ORRC (Oregon) coordinated repeaters in range, each with
  its ±600 kHz input, so the scanner records both the repeater output and the
  station keying it. Input and output recordings are linked in the index, and
  the UI can show them side by side (useful for hearing stations that the
  repeater does not pass, or that are timing out).
- Import from CHIRP-style CSV (as exported by RepeaterBook and most radio
  programming tools), which already has frequency, duplex, offset and tone
  columns. Admins can edit the list in the UI.

Span: the profile covers the full 144–148 MHz band, so every repeater pair is
inside it, both −600 kHz pairs and the +600 kHz pairs at 147.00 and up
(inputs 147.60–147.99).

Default handling for segments where an NBFM voice recording is not useful
(each can be changed):

- 144.00–144.30 MHz: CW, SSB and weak-signal work. FM demodulation turns these
  into noise, so they are not recorded until the SSB/CW modes exist (Phase 7),
  but stay on the spectrum.
- 144.39 MHz (APRS) and 144.92 MHz (packet): set to `mode = "afsk1200"`. Until
  the packet decoder exists (Phase 5) they are `record = false`; after that
  they produce decoded AX.25 frames instead of audio files.
- Beacons and other permanent carriers that birdie detection finds.

- Derived at startup from the profile: required sample rate and tuner center
  (with margin for filter rolloff and keeping the LO off a channel), FFT size
  N so the bin width stays fine compared with the narrowest channel, the IFFT
  size and output rate per bandwidth, and detection window sizes.
- A wider span such as 70 cm needs a higher sample rate (both radios can do
  ~10 MS/s, which costs about 4× the shared FFT work), or several narrower
  sub-spans. The profile states what it wants; the source reports what the
  hardware can deliver and startup fails clearly if the span does not fit.
- Tests run every synthetic scenario under both the 25 kHz and 12.5 kHz presets,
  including a mixed plan where a 12.5 kHz signal sits between two 25 kHz slots.

### 3.8 Demodulation modes (built for more than NBFM)

NBFM voice ships first, but the core should not assume it, so packet decoding
and SSB/CW (and HF) can be added later without reworking the pipeline.

- **Mode per channel slot.** Every slot has a `mode` from the band profile or
  channel list: `nbfm` now; `afsk1200` (AX.25 packet), `dmr` and other
  digital voice modes, `usb`, `lsb`, `cw` later. The channelizer does not care: it hands each active slot complex
  baseband at a rate derived from that slot's bandwidth.
- **A `Demod` trait** per mode: takes channel baseband blocks, returns typed
  outputs. NBFM returns audio; AFSK returns audio *and* decoded frames; SSB/CW
  return audio (and later decoded CW text).
- **Detection is per mode too.** NBFM uses carrier power in the slot. Packet
  uses power plus "did a frame decode". SSB has no carrier and needs voice
  activity on band energy; CW needs narrow-band tone detection. The detector
  takes a strategy from the mode instead of a single threshold rule.
- **Outputs are typed.** The index has a `mode` column and a generic "event"
  table, so a transmission can be an audio file, a list of packets, or both.
  The API and WebSocket carry the same event types.

**AX.25 / APRS (Phase 5).** 144.39 (APRS) and 144.92 (packet) are 1200-baud
Bell 202 AFSK over NBFM.

- Decoder in Rust: discriminator audio (no de-emphasis) into several parallel
  demodulators with different filter and slicer settings, as Direwolf does,
  then NRZI decoding, HDLC deframing, bit unstuffing and CRC check, with
  frames deduplicated across demodulators. Optional single-bit error fixing.
- Outputs: decoded frames (source, destination, digipeater path, payload),
  parsed APRS fields (position, symbol, comment, telemetry) stored in the
  index, shown in the UI as a live packet list and a station map, and sent on
  the WebSocket. Optional KISS-over-TCP output so other apps (Xastir, YAAC,
  APRSIS32) can use the scanner as a receive-only TNC.
- Optional receive-only iGate to APRS-IS later (needs a callsign and passcode;
  the uplink cost is small, a few hundred bytes per packet).
- 9600-baud G3RUH packet would need a wider channel and a different decoder;
  noted as a possible later mode.
- Testing: the synthetic generator gains AFSK packet signals with known
  contents. The standard WA8LMF TNC test recordings measure decode counts, and
  Direwolf decoding the same audio is the reference: our count should be within
  a few percent of Direwolf's.

**Digital voice: DMR first (Phase 6).** There is more digital voice than SSB/CW
traffic locally, so this comes first. DMR is 4-level FSK at 4800 symbols/s in
a 12.5 kHz channel, with two TDMA timeslots.

- Signal chain: 12.5 kHz channel → the existing FM discriminator → 4FSK symbol
  timing recovery and slicing → sync-pattern search (voice/data sync, base
  station vs mobile) → timeslot split → CACH/slot type decoding and FEC (BPTC,
  Golay, Hamming, Reed-Solomon on the link control) → voice frames + metadata.
- Detection: sync patterns are the reliable activity signal for DMR. Power alone
  cannot tell DMR from FM; the sync search can, so a slot can also be
  auto-classified (FM vs DMR) when the channel list does not say.
- Outputs: one audio stream and recording per timeslot, plus metadata in the
  index: color code, talkgroup or private call, source and destination radio
  IDs, and talker alias where sent. Recordings can be browsed by talkgroup, and
  radio IDs can be shown with callsigns from an optional imported ID list
  (e.g. a RadioID.net export).
- Voice codec: DMR uses AMBE+2, which is proprietary (DVSI). Behind a `Vocoder`
  trait with two implementations:
  1. `mbelib` (open-source reverse-engineered decoder, used by DSD/DSD-FME).
     Its patent status is unclear, so it is an optional plugin built
     separately, not part of the default binary.
  2. A DVSI hardware vocoder (AMBE-3000 USB stick such as a ThumbDV), which is
     licensed and sounds best but only handles a few streams at once, so
     streams queue for it.
- Encrypted traffic cannot be decoded; the scanner logs its metadata only.
- After DMR, the same pieces (symbol recovery, framing, the vocoder trait) cover
  Yaesu System Fusion (C4FM, AMBE+2), D-STAR (GMSK, AMBE) and P25 phase 1
  (C4FM, IMBE), in whatever order local traffic suggests.
- CPU: symbol recovery and FEC are light; the software vocoder costs a few
  percent of a core per active stream, so many simultaneous digital calls on
  the Pi 5 need a benchmark.
- Testing: synthetic DMR is harder to generate than FM, so tests lean on
  recorded captures, with DSD-FME decoding the same IQ as the reference
  (matching frame counts, talkgroups and IDs). Unit tests cover the FEC codes
  and framing against published test vectors.

**SSB, CW and HF (Phase 7).**

- SSB: narrow channel (~2.7–3 kHz), shift and take one sideband (complex
  baseband makes this simple: frequency-shift so the suppressed carrier is at
  0 Hz and keep the real part of the filtered upper or lower half), plus AGC
  since there is no FM limiting. The listener can fine-tune ±100 Hz because
  there is no carrier to lock to.
- CW: narrow filter (~200–500 Hz), BFO offset to make a tone, envelope
  detection; a Morse decoder can come after.
- Detection for SSB is harder than for FM (no constant carrier, speech is
  bursty), so it gets its own detector and hang time, tuned on recordings.
- HF: the RSP1A covers HF directly. The Airspy R2 needs an upconverter
  (SpyVerter); the source then applies a fixed frequency offset. HF band
  profiles (e.g. 40 m at 7.000–7.300 MHz) are much narrower spans at much
  lower sample rates, so CPU is not a concern there. The bigger work is AGC
  and detection, and wider dynamic range from strong broadcast signals.
- The 144.00–144.30 segment becomes recordable on 2 m once these modes exist.

---

## 4. Testing strategy

Most testing runs on files, with no hardware.

### 4.1 Synthetic IQ generator (Python, `tools/synth`)

Scenario files (YAML) describe a capture; the generator writes a SigMF
recording (`.sigmf-data` + `.sigmf-meta`) and a ground-truth JSON.

- Signals: unmodulated carriers, FM with 1 kHz tone, FM with speech clips (public
  domain recordings), deviation ±2.5/±5 kHz, CTCSS tones, pre-emphasis.
- Timing: start/stop times, overlapping transmissions on different channels,
  back-to-back keyups on the same channel, short blips, long ragchews.
- Impairments: AWGN at set SNR, frequency offset and slow drift, DC spike, IQ
  imbalance, fixed spurs, a strong adjacent signal, band-edge placement,
  quantization to 12/14-bit int16 like the real radios.
- Ground truth: expected transmissions (raster frequency, start, end) and the
  original modulating audio for each.

Same generator, two uses: deterministic fixtures for CI (small, seconds long),
and large stress scenarios for benchmarks (all ~100 raster slots active).

### 4.2 Recorded IQ corpus

- A `scanner record` subcommand (or `airspy_rx` / SoapySDR `rx_sdr` before ours
  exists) captures SigMF from both radios with gain and frequency metadata.
- 10 MS/s int16 is ~144 GB/hour, so capture short clips (30 s – 2 min) at busy
  times, plus a few longer captures of a sub-band decimated on the fly:
  a net, repeater traffic, a quiet period, a strong-signal overload case.
- Store outside git (object storage or git-lfs) with a manifest of hashes;
  CI pulls a small subset.
- Hand-label a few clips (transmission start/end per frequency) to use as ground
  truth for detection metrics.

### 4.3 Golden comparisons

Sample-exact audio comparisons break across x86/ARM SIMD paths and any DSP
tweak, so comparisons use tolerances and metrics:

- **Synthetic audio fidelity:** align output with the source audio by
  cross-correlation, then measure SNR / correlation. Tone signals: SINAD at
  1 kHz, and the input SNR that gives 12 dB SINAD (the standard NBFM
  sensitivity figure). Each becomes a pass/fail threshold.
- **Detection metrics:** precision/recall of transmissions vs ground truth,
  frequency error (must land on the right raster slot), start latency, clipped
  start (ms of audio lost), split rate (one transmission written as several
  files), false opens per hour on noise-only input.
- **Regression goldens:** our own outputs for fixed inputs are stored with their
  metrics; a change fails if output SNR against the golden drops below e.g.
  40 dB or detection metrics regress. Goldens are refreshed deliberately with a
  script.
- **ham2mon baseline:** run ham2mon on the same recorded files (osmosdr accepts
  `file=…,rate=…,freq=…` as a source) and compare: we should find the same
  transmissions or more, split fewer, clip less, at much lower CPU.

### 4.4 Unit and property tests (Rust)

- Channel filter frequency response (passband ripple, stopband rejection).
- Fast-convolution channelizer: a tone at f appears at baseband offset f − fc
  with correct amplitude, no leakage above the stopband spec, and continuity
  across block boundaries (no clicks; check phase continuity of a pure tone).
- Discriminator: known deviation gives the expected audio amplitude.
- State machine: table-driven tests for attack, hang, min length, pre-roll.
- Timestamping: sample counter math and drop accounting.
- Ring buffer and pool: loom or stress tests for the concurrency primitives.

### 4.5 Benchmarks

- `criterion` microbenchmarks: forward FFT stage per block, per-channel cost,
  discriminator, writer.
- End-to-end: replay a file as fast as possible and report the realtime factor
  and per-thread CPU at 0, 5, 10, 20 and 100 active channels.
- Run on CI (GitHub Actions has `ubuntu-24.04-arm` and `macos-14` arm64 runners
  plus x86 Linux) and by hand on the Pi, with results logged so regressions are
  visible.
- Starting targets (to confirm): on the Pi 5 at 10 MS/s (full band), under 50 %
  total CPU (two of four cores) with 10 active channels including libairspy
  conversion, ≥ 2× realtime on file replay, zero drops in a 24 h soak. Compare
  libairspy IQ against the raw-real path, and one big FFT against the split.
- Watch thermals: run the Pi 5 benchmarks with its actual cooling and log
  CPU clock, since throttling would skew results and could cause drops.

### 4.6 Hardware and soak tests

- Smoke test per radio: a handheld on low power into a dummy load (or a signal
  generator) sending a 1 kHz tone on a simplex frequency; check frequency,
  timing and SINAD in the output.
- 24-hour soak on each radio: memory flat, no file-handle growth, drop counter
  zero, file count plausible.

### 4.7 CI matrix

Linux x86_64, Linux arm64, macOS arm64 (macOS x86_64 as a build check). Build
with `airspy` on Linux, without `sdrplay` in CI; run unit tests, synthetic
fixtures, a small recorded subset, and benchmarks with a soft regression gate.

---

## 5. Phased roadmap

**Phase 0: groundwork (small)**
- Cargo workspace skeleton (`core`, `sources`, `cli`, `web`), CI matrix.
- Python synthetic generator + SigMF writer + ground truth.
- Capture a first recorded corpus from both radios (the RSP1A first, since the
  remote site is offline).
- Build the starting channel list: WWARA 2 m simplex channels plus WWARA/ORRC
  repeaters in range, as CHIRP CSV.
- Run ham2mon on the recordings for baseline outputs and CPU numbers.

**Phase 1: offline core (largest)**
- FileSource → fast-convolution channelizer → detector + state machine →
  discriminator/audio chain → WAV writer + SQLite index.
- Unit tests, synthetic fixtures, golden and detection metrics, benchmarks.
- Exit: beats ham2mon on the recorded corpus, meets CPU estimate on the Pi
  using file replay.

**Phase 2: live radios**
- Airspy backend on the Pi, SDRplay backend on the Mac, ring buffer, drop
  accounting, sample-counter timestamps, gain/frequency config.
- Hardware smoke tests; 24 h soak on each.
- Airspy: benchmark libairspy's IQ path against raw real samples with a
  real-input FFT at the full 10 MS/s; adopt raw-real if IQ conversion takes
  more than about a core.
- Calibrate defaults from scratch (no old ham2mon settings exist): a
  `scanner calibrate` command sweeps gain, records noise floor and the onset of
  overload/intermod at each step, and suggests gain and threshold values.
- Exit: unattended 24 h on the Pi with zero drops.

**Phase 3: service and web**
- REST + WebSocket API, auth (anonymous/admin, sessions, API tokens), web UI
  with recordings browser (Opus playback, repeater input/output linked),
  live spectrum/waterfall, channel list and lockout/priority editing.
- Live listening in the browser (Opus over WebSocket, follow-activity mode).
- Egress accounting and the monthly budget; disk-budget retention.
- systemd unit (Pi) and launchd plist (Mac), log rotation.
- Exit: the Pi install is reachable through a reverse proxy, anonymous users see
  live status, recordings are hidden until an admin enables them, admin can
  change settings, route × role tests pass for both default and enabled configs.

**Phase 4: hardening and optimization**
- Noise squelch; CTCSS/DCS tags;
  automatic birdie lockout; packaging (.deb for the Pi, Homebrew formula or
  prebuilt binary for macOS); more roles or OIDC login if needed; optional
  cloud relay for public listeners.

**Phase 5: AX.25 packet and APRS**
- `afsk1200` mode on 144.39 and 144.92: multi-demodulator decoder, HDLC/AX.25
  parsing, APRS field parsing, packet list and station map in the UI, KISS-TCP
  output.
- Exit: decode count within a few percent of Direwolf on the WA8LMF test
  recordings and on our own captures.

**Phase 6: digital voice (DMR first)**
- `dmr` mode: 4FSK symbol recovery, sync detection and FM/DMR auto-classify,
  timeslot split, FEC and link control, per-timeslot recordings with talkgroup
  and radio ID metadata, `Vocoder` trait with an optional mbelib plugin and
  DVSI hardware support.
- Then System Fusion, D-STAR and P25 based on local traffic.
- Exit: on recorded local DMR traffic, frame counts, talkgroups and IDs match
  DSD-FME, and the audio is intelligible.
- Status (2026-11-24): the `dmr` mode is in (`crates/core/src/dmr`). It
  decodes sync, CACH, slot type, BPTC/RS full LC, embedded LC and talker
  alias, records one call per timeslot with its ids, and keeps the raw AMBE+2
  frames (`.amb`). Audio needs the optional `mbelib` feature (mbelib-neo
  preferred, classic mbelib also works). The encoders match MMDVMHost bit for
  bit, and tests run synthetic simplex and repeater traffic through the whole
  receiver. Not done yet: validation on a real capture, FM/DMR
  auto-classification of grid slots, a RadioID.net callsign list, and DVSI
  hardware.

**Phase 7: SSB, CW and HF**
- `usb`/`lsb`/`cw` modes, AGC, SSB/CW detectors, HF band profiles, upconverter
  offset for the Airspy, then optional Morse decoding.
- Exit: clean recordings of SSB and CW in 144.0–144.3 and on one HF band with
  the RSP1A.

---

## 6. Decisions so far

- Channel spacing and bandwidth are configurable per band profile (2 m / 70 cm,
  25 and 12.5 kHz presets).
- The web UI and API are built into the scanner; a separately hosted public
  site is out of scope.
- Auth with roles from the start: `anonymous` and `admin` for now.
- Anonymous users get live status by default; recordings must be enabled by
  the operator.
- Remote site hardware: Raspberry Pi 5, 8 GB, 64-bit OS (build target
  `aarch64-unknown-linux-gnu`).
- Remote site uplink: ~20 Mb/s with a ~5 GB/month cap. The web layer serves
  Opus, tracks egress and enforces a monthly budget for anonymous traffic.
- One WAV file per transmission (with hang time).
- Channel list: WWARA 2 m simplex plus WWARA/ORRC coordinated repeaters in range,
  monitoring repeater inputs (±600 kHz) as well as outputs.
- Live listening in the browser is in scope (phase 3).
- Retention: disk budget, default 8 GB, oldest recordings deleted first.
- One radio per scanner instance.
- Language: Rust.
- Span: the full 2 m band, 144–148 MHz (RSP1A at 6–8 MS/s, Airspy at
  10 MS/s).
- Satellite subband (145.8–146.0): no special handling for now.
- No old ham2mon settings are available; defaults come from calibration on
  the new captures.
- Data cap: treat the 5 GB/month as combined upload and download. Default
  anonymous egress budget is 3 GB/month.
- Packet: decode AX.25 on 144.39 (APRS) and 144.92 (Phase 5, after the core is
  working).
- Digital voice next (Phase 6): DMR first, then System Fusion, D-STAR and P25
  as local traffic suggests. The AMBE vocoder is an optional plugin or a DVSI
  hardware stick, never in the default binary.
- SSB and CW after that, which also opens up HF (Phase 7).
- The core is mode-agnostic from the start so these do not need a rewrite.

## 7. Open questions for Masen

1. **Which digital voice modes are busiest locally,** and on which bands? DMR
   is planned first; if System Fusion or D-STAR is more common on 2 m around
   Rainier, it could go first instead, since the shared pieces are the same.
