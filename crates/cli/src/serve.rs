//! `scanner serve`: the long-running scanner (PLAN.md 3.4).
//!
//! A radio feeds a lock-free ring: an SDRplay device's stream callback, or a
//! SigMF recording replayed at its real rate (optionally looped), which
//! exercises exactly the same path. The DSP thread reads the
//! ring, runs the scanner and hands outputs to the recorder thread. Ctrl-C
//! stops cleanly: open transmissions are closed and written.

use anyhow::{Context, Result, bail};
use num_complex::Complex32;
use scanner_core::clock::now_ns;
use scanner_core::config::same_freq;
use scanner_core::scanner::{Output, Scanner, ScannerConfig};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source, SourceInfo, ring};
use scanner_store::Recorder;
use scanner_store::settings::{Override, Settings};
use scanner_web::live::SPUR_DETECTOR;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Where samples come from.
#[derive(Debug, Clone)]
pub enum SourceSpec {
    /// Replay a recording at its sample rate, like a live radio.
    Replay { path: PathBuf, looped: bool },
    /// An SDRplay radio (RSP1A), optionally by serial number.
    Sdrplay { serial: Option<String> },
}

impl std::str::FromStr for SourceSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if let Some(p) = s.strip_prefix("replay:") {
            return Ok(SourceSpec::Replay {
                path: p.into(),
                looped: false,
            });
        }
        if let Some(p) = s.strip_prefix("loop:") {
            return Ok(SourceSpec::Replay {
                path: p.into(),
                looped: true,
            });
        }
        if s == "sdrplay" {
            return Ok(SourceSpec::Sdrplay { serial: None });
        }
        if let Some(serial) = s.strip_prefix("sdrplay:") {
            return Ok(SourceSpec::Sdrplay {
                serial: Some(serial.into()).filter(|s: &String| !s.is_empty()),
            });
        }
        match s {
            "airspy" => Err("airspy: the Airspy backend is not written yet".into()),
            _ => Err(format!(
                "unknown source {s:?} (try sdrplay, sdrplay:<serial>, replay:<recording> or loop:<recording>)"
            )),
        }
    }
}

/// Samples per ring block (~6.5 ms at 10 MS/s), and blocks in the ring
/// (~0.85 s of buffering at 10 MS/s).
const BLOCK: usize = 1 << 16;
const BLOCKS: usize = 128;

fn replay_thread(
    path: PathBuf,
    looped: bool,
    mut producer: scanner_sources::RingProducer,
    stop: Arc<AtomicBool>,
    halt: Arc<AtomicBool>,
) -> Result<()> {
    let mut buf = vec![Complex32::default(); 1 << 14];
    let mut sent = 0u64;
    let t0 = Instant::now();
    let mut rate = 0.0;
    loop {
        let mut src = FileSource::open(&path)?;
        rate = if rate == 0.0 {
            src.info().sample_rate
        } else {
            rate
        };
        loop {
            if stop.load(Ordering::Relaxed) || halt.load(Ordering::Relaxed) {
                producer.close();
                return Ok(());
            }
            let k = src.read(&mut buf)?;
            if k == 0 {
                break;
            }
            // Pace like a radio: deliver no faster than the sample rate.
            let due = Duration::from_secs_f64(sent as f64 / rate);
            if let Some(wait) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(wait);
            }
            producer.push(&buf[..k]);
            sent += k as u64;
        }
        if !looped {
            producer.close();
            return Ok(());
        }
    }
}

pub struct ServeOptions<'a> {
    pub source: SourceSpec,
    /// Radio settings for `SourceSpec::Sdrplay` (the serial comes from the
    /// source spec).
    #[cfg_attr(not(feature = "sdrplay"), allow(dead_code))]
    pub sdrplay: scanner_sources::sdrplay::SdrplayConfig,
    pub profile_name: String,
    /// Serve the HTTP API here.
    pub http: Option<std::net::SocketAddr>,
    pub public: scanner_web::Public,
    /// Accounts database (admins, sessions, API tokens).
    pub auth_db: PathBuf,
    /// Leave `Secure` off the session cookie (plain HTTP on a LAN).
    pub insecure_cookies: bool,
    pub scanner: ScannerConfig,
    pub plan: &'a [scanner_core::config::Slot],
    /// The profile's span (Hz), which the web spectrum shows.
    pub span: (f64, f64),
    pub out: &'a Path,
    /// Also write Opus copies for the web UI.
    pub opus: bool,
    pub retention: scanner_store::retention::Policy,
    /// Monthly budget for anonymous web traffic; 0 for none.
    pub anon_egress_bytes: u64,
    pub status_every: Duration,
    /// Serve decoded AX.25 frames as KISS over TCP on this address.
    pub kiss: Option<std::net::SocketAddr>,
}

type Started = (
    SourceInfo,
    scanner_sources::RingSource,
    std::thread::JoinHandle<Result<()>>,
    Option<Arc<scanner_sources::sdrplay::DeviceEvents>>,
);

/// Start the radio (or replay) thread feeding a ring. The thread stops when
/// `stop` (Ctrl-C) or `halt` (this session is being replaced) is set.
fn start_source(o: &ServeOptions, stop: Arc<AtomicBool>, halt: Arc<AtomicBool>) -> Result<Started> {
    match &o.source {
        SourceSpec::Replay { path, looped } => {
            let f =
                FileSource::open(path).with_context(|| format!("opening {}", path.display()))?;
            let fi = f.info().clone();
            let info = SourceInfo {
                // A live stream starts now, whatever the recording says.
                start_time: Some(SystemTime::now()),
                total_samples: None,
                description: format!(
                    "{} ({})",
                    fi.description,
                    if *looped { "looped" } else { "replay" }
                ),
                ..fi
            };
            let (producer, source) = ring::ring(BLOCKS, BLOCK, info.clone());
            let (path, looped) = (path.clone(), *looped);
            let radio = std::thread::Builder::new()
                .name("radio".into())
                .spawn(move || replay_thread(path, looped, producer, stop, halt))?;
            Ok((info, source, radio, None))
        }
        SourceSpec::Sdrplay { serial } => start_sdrplay(o, serial.clone(), stop, halt),
    }
}

#[cfg(feature = "sdrplay")]
fn start_sdrplay(
    o: &ServeOptions,
    serial: Option<String>,
    stop: Arc<AtomicBool>,
    halt: Arc<AtomicBool>,
) -> Result<Started> {
    use scanner_sources::sdrplay::Sdrplay;
    let cfg = scanner_sources::sdrplay::SdrplayConfig {
        serial,
        ..o.sdrplay.clone()
    };
    // Opening goes through blocking SDRplay API calls with no timeout: they
    // wait for as long as another program holds the API lock or the service
    // is stuck. Open on a helper thread so we can say what we are waiting on
    // and give up on Ctrl-C.
    eprintln!("opening the SDRplay radio...");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("radio-open".into())
        .spawn(move || {
            // Callbacks are ~1000 samples; the ring is fed in full blocks.
            let _ = tx.send(Sdrplay::start(&cfg, BLOCKS, BLOCK));
        })?;
    let opened = Instant::now();
    let mut hinted = false;
    let (device, source) = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => break r?,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the thread opening the radio died")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if stop.load(Ordering::Relaxed) || halt.load(Ordering::Relaxed) {
            bail!("stopped while waiting for the SDRplay API to open the radio");
        }
        if !hinted && opened.elapsed() >= OPEN_HINT_AFTER {
            hinted = true;
            eprintln!("{}", open_hint(opened.elapsed()));
        }
    };
    let info = source.info().clone();
    let events = device.events().clone();
    let radio = std::thread::Builder::new()
        .name("radio".into())
        .spawn(move || {
            // The API streams on its own threads; this one owns the device
            // and stops it on Ctrl-C or when the radio goes away.
            let ev = device.events().clone();
            while !stop.load(Ordering::Relaxed) && !halt.load(Ordering::Relaxed) {
                if ev.removed.load(Ordering::Relaxed) || ev.failed.load(Ordering::Relaxed) {
                    drop(device);
                    bail!(
                        "the radio was {}",
                        if ev.removed.load(Ordering::Relaxed) {
                            "unplugged"
                        } else {
                            "reported as failed"
                        }
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(device);
            Ok(())
        })?;
    Ok((info, source, radio, Some(events)))
}

/// How long opening the radio may take before we tell the user why.
#[cfg_attr(not(feature = "sdrplay"), allow(dead_code))]
const OPEN_HINT_AFTER: Duration = Duration::from_secs(10);

#[cfg_attr(not(feature = "sdrplay"), allow(dead_code))]
fn open_hint(waited: Duration) -> String {
    format!(
        "still waiting for the SDRplay API after {} s. Another program probably has the \
         radio (an earlier `scanner serve`, SDRconnect, SDRuno), or sdrplay_apiService is \
         stuck. Check with `pgrep -fl 'scanner|SDRconnect|sdrplay'`; quit the other program, \
         or restart the service or replug the radio. Ctrl-C to give up.",
        waited.as_secs()
    )
}

#[cfg(not(feature = "sdrplay"))]
fn start_sdrplay(
    _: &ServeOptions,
    _: Option<String>,
    _: Arc<AtomicBool>,
    _: Arc<AtomicBool>,
) -> Result<Started> {
    bail!(
        "this scanner was built without SDRplay support; rebuild with `--features sdrplay` (needs the SDRplay API 3.x installed)"
    )
}

/// A KISS data frame (port 0) for an AX.25 frame without its FCS.
pub fn kiss_frame(raw: &[u8]) -> Vec<u8> {
    const FEND: u8 = 0xc0;
    const FESC: u8 = 0xdb;
    let mut f = Vec::with_capacity(raw.len() + 4);
    f.extend([FEND, 0x00]);
    for &b in raw {
        match b {
            FEND => f.extend([FESC, 0xdc]),
            FESC => f.extend([FESC, 0xdd]),
            _ => f.push(b),
        }
    }
    f.push(FEND);
    f
}

/// Receive-only KISS-over-TCP server: every connected client gets every
/// decoded frame; anything clients send is ignored. Frames go through a
/// channel so a slow client never holds up the DSP thread.
fn start_kiss(addr: std::net::SocketAddr) -> Result<std::sync::mpsc::Sender<Vec<u8>>> {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind(addr).with_context(|| format!("KISS: listening on {addr}"))?;
    let clients: Arc<std::sync::Mutex<Vec<TcpStream>>> = Default::default();
    let c2 = clients.clone();
    std::thread::Builder::new()
        .name("kiss-accept".into())
        .spawn(move || {
            for s in listener.incoming().flatten() {
                let _ = s.set_nodelay(true);
                let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                if let Ok(peer) = s.peer_addr() {
                    eprintln!("KISS client connected: {peer}");
                }
                if let Ok(mut g) = c2.lock() {
                    g.push(s);
                }
            }
        })?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("kiss-send".into())
        .spawn(move || {
            for raw in rx {
                let f = kiss_frame(&raw);
                if let Ok(mut g) = clients.lock() {
                    g.retain_mut(|c| c.write_all(&f).is_ok());
                }
            }
        })?;
    eprintln!("KISS over TCP on {addr} (receive only)");
    Ok(tx)
}

/// The watched slots as the web channel list shows them.
/// Half-width of a lockout placed on a frequency that isn't a watched slot
/// (an off-grid signal a grid slot retunes onto, or a birdie).
const LOCKOUT_HALF_HZ: f64 = 5e3;

/// The config file's flags for each watched slot: (freq, record, priority).
type BaseFlags = Vec<(f64, bool, bool)>;

/// Put the config's flags back, then apply `list` on top. Recordings that
/// become locked out end now (their outputs go into `out`).
fn apply_overrides(
    scanner: &mut Scanner,
    base: &BaseFlags,
    list: &[Override],
    out: &mut Vec<Output>,
) {
    let mut ranges = Vec::new();
    for &(freq, record, priority) in base {
        let o = list.iter().find(|o| same_freq(o.freq_hz as f64, freq));
        let record = o.and_then(|o| o.locked_out).map_or(record, |l| !l);
        let priority = o.and_then(|o| o.priority).unwrap_or(priority);
        scanner.set_slot_flags(freq, record, priority, out);
    }
    for o in list {
        let freq = o.freq_hz as f64;
        let on_slot = base.iter().any(|&(f, _, _)| same_freq(f, freq));
        if !on_slot && o.locked_out == Some(true) {
            ranges.push((freq - LOCKOUT_HALF_HZ, freq + LOCKOUT_HALF_HZ));
        }
    }
    scanner.set_lockouts(ranges, out);
}

fn channel_list(
    slots: &[scanner_core::detector::WatchedSlot],
    overrides: &[Override],
) -> Vec<scanner_web::ChannelInfo> {
    use scanner_core::config::SlotOrigin;
    // Only link to a half that is actually watched (inside the capture).
    let watched = |f: f64| slots.iter().any(|w| same_freq(w.slot.freq, f));
    slots
        .iter()
        .map(|w| {
            let s = &w.slot;
            scanner_web::ChannelInfo {
                freq_hz: s.freq,
                name: s.name.clone(),
                kind: match s.origin {
                    SlotOrigin::Listed => "channel",
                    SlotOrigin::RepeaterOutput => "repeater_output",
                    SlotOrigin::RepeaterInput => "repeater_input",
                    SlotOrigin::Grid => "grid",
                },
                mode: s.mode.as_str().into(),
                bandwidth_hz: s.bandwidth,
                priority: s.priority,
                locked_out: !s.record,
                pair_hz: s.pair.filter(|&f| watched(f)),
                overridden: overrides
                    .iter()
                    .any(|o| same_freq(o.freq_hz as f64, s.freq)),
                spur: overrides
                    .iter()
                    .any(|o| same_freq(o.freq_hz as f64, s.freq) && o.updated_by == SPUR_DETECTOR),
            }
        })
        .chain(
            overrides
                .iter()
                .filter(|o| o.locked_out == Some(true) && !watched(o.freq_hz as f64))
                .map(|o| scanner_web::ChannelInfo {
                    freq_hz: o.freq_hz as f64,
                    name: None,
                    kind: "lockout",
                    mode: "nbfm".into(),
                    bandwidth_hz: 2.0 * LOCKOUT_HALF_HZ,
                    priority: false,
                    locked_out: true,
                    pair_hz: None,
                    overridden: true,
                    spur: o.updated_by == SPUR_DETECTOR,
                }),
        )
        .collect()
}

/// Whether a source error means the radio stopped delivering samples.
fn is_stall(e: &scanner_sources::SourceError) -> bool {
    matches!(e, scanner_sources::SourceError::Io(io)
        if io.get_ref().is_some_and(|r| r.is::<ring::Stalled>()))
}

/// Wait (up to 10 s) for a radio thread whose session has been halted. The
/// vendor API can hang tearing down a stream that died; then the thread is
/// left behind and the radio is opened again anyway.
fn stop_radio(radio: std::thread::JoinHandle<Result<()>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(radio.join());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => eprintln!("radio: {e:#}"),
        Ok(Err(_)) => eprintln!("radio thread panicked"),
        Err(_) => {
            eprintln!("radio: the old stream did not stop within 10 s; opening a new one anyway")
        }
    }
}

/// Open the radio again, retrying with a growing delay (1 s, doubling to
/// 30 s) until it works or Ctrl-C. Returns the new session and its halt
/// flag, or `None` on Ctrl-C.
fn reopen(o: &ServeOptions, stop: &Arc<AtomicBool>) -> Option<(Started, Arc<AtomicBool>)> {
    let mut wait = Duration::from_secs(1);
    loop {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let halt = Arc::new(AtomicBool::new(false));
        match start_source(o, stop.clone(), halt.clone()) {
            Ok(s) => return Some((s, halt)),
            Err(e) => {
                wait = (wait * 2).min(Duration::from_secs(30));
                eprintln!(
                    "reopening the radio failed: {e:#}; trying again in {} s",
                    wait.as_secs()
                );
            }
        }
    }
}

pub fn serve(o: ServeOptions) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            if stop.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            eprintln!("stopping (Ctrl-C again to quit immediately)");
        })
        .context("installing Ctrl-C handler")?;
    }

    let mut halt = Arc::new(AtomicBool::new(false));
    let (mut info, mut source, mut radio, mut device_events) =
        start_source(&o, stop.clone(), halt.clone())?;
    let t0 = info.start_time.unwrap_or_else(SystemTime::now);
    let mut scanner = Scanner::new(
        o.scanner.clone(),
        info.sample_rate,
        info.center_freq,
        t0,
        o.plan,
    );
    let p = *scanner.params();
    eprintln!(
        "{}: {} S/s at {}, FFT {} ({:.1} Hz bins), watching {} slots, recording to {}",
        info.description,
        info.sample_rate,
        Hz(info.center_freq),
        p.n,
        p.bin_hz(),
        scanner.detector().slots().len(),
        o.out.display()
    );
    if scanner.detector().slots().is_empty() {
        bail!("no slots of the profile fall inside the capture");
    }

    let recorder = Recorder::new(o.out)?
        .with_opus(o.opus)
        .with_retention(o.retention)?;
    let disk_usage = recorder.disk_usage();
    let (tx, writer) = recorder.spawn(256);
    let kiss = o.kiss.map(start_kiss).transpose()?;

    // Lockout and priority edits made in the web UI, on top of the config.
    let settings_path = o.out.join("settings.sqlite");
    let settings = Arc::new(
        Settings::open(&settings_path)
            .with_context(|| format!("opening {}", settings_path.display()))?,
    );
    let base: BaseFlags = scanner
        .detector()
        .slots()
        .iter()
        .map(|w| (w.slot.freq, w.slot.record, w.slot.priority))
        .collect();
    let mut overrides = settings.overrides()?;
    if !overrides.is_empty() {
        let mut ignored = Vec::new();
        apply_overrides(&mut scanner, &base, &overrides, &mut ignored);
        eprintln!(
            "{} channel overrides from {}",
            overrides.len(),
            settings_path.display()
        );
    }
    let (control_tx, control_rx) = std::sync::mpsc::channel::<scanner_web::Control>();
    let channels: scanner_web::ChannelsHandle = Default::default();
    let publish_channels = |scanner: &Scanner, overrides: &[Override]| {
        if let Ok(mut g) = channels.write() {
            *g = channel_list(scanner.detector().slots(), overrides);
        }
    };
    publish_channels(&scanner, &overrides);
    let status: scanner_web::StatusHandle = Default::default();
    let audio_hub = scanner_web::audio::AudioHub::new();
    let mut live_tx = None;
    let mut egress = None;
    let spectrum: scanner_web::SpectrumHandle = Default::default();
    // The spectrum shows the profile's span, inside what the radio delivers
    // cleanly.
    let half = info.sample_rate * 0.45;
    let spec_lo = (o.span.0 - info.center_freq).max(-half);
    let spec_hi = (o.span.1 - info.center_freq).min(half);
    if let Some(addr) = o.http {
        let auth = scanner_web::auth::AuthStore::open(&o.auth_db)
            .with_context(|| format!("opening {}", o.auth_db.display()))?;
        if !auth.has_admins().unwrap_or(false) {
            eprintln!(
                "note: no admin accounts yet, so admin-only routes are closed; create one with \
                 `scanner admin create <name> --db {}`",
                o.auth_db.display()
            );
        }
        let mut state = scanner_web::AppState::new(
            status.clone(),
            o.out.to_owned(),
            o.public,
            Some(Arc::new(auth)),
        );
        state.secure_cookies = !o.insecure_cookies;
        state.spectrum = spectrum.clone();
        state.channels = channels.clone();
        state.settings = Some(settings.clone());
        state.control = Some(control_tx.clone());
        live_tx = Some(
            audio_hub
                .spawn_encoder()
                .context("starting the live audio thread")?,
        );
        state.audio = Some(audio_hub.clone());
        state.storage = disk_usage.clone();
        let eg = Arc::new(scanner_web::egress::Egress::new(
            o.anon_egress_bytes,
            Some(settings.clone()),
        ));
        state.egress = Some(eg.clone());
        egress = Some(eg);
        scanner_web::spawn(addr, state)
            .with_context(|| format!("starting the web API on {addr}"))?;
        eprintln!("web interface on http://{addr}/");
    }
    let mut next_publish = Instant::now();
    let mut next_spectrum = Instant::now();
    let mut buf = vec![Complex32::default(); BLOCK];
    let mut out: Vec<Output> = Vec::new();
    let started = Instant::now();
    let mut next_status = started + o.status_every;
    let mut busy = Duration::ZERO;
    // Carried over from scanners and radio sessions before a reopen.
    let mut reopened = 0u32;
    let mut recorded_before = 0u64;
    let mut dropped_before = 0u64;
    let live_radio = matches!(o.source, SourceSpec::Sdrplay { .. });
    // When this radio session started (its counters start from zero).
    let mut session_started = started;
    loop {
        let read = source.read(&mut buf);
        // A radio that stalls or whose stream ends (USB failure, unplugged)
        // is reopened rather than ending the program. A replay ends for
        // good when its file does.
        let lost = live_radio
            && !stop.load(Ordering::Relaxed)
            && match &read {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => is_stall(e),
            };
        if lost {
            eprintln!(
                "radio lost ({}); closing open transmissions and reopening it",
                match &read {
                    Err(e) => e.to_string(),
                    _ => "the stream ended".into(),
                }
            );
            scanner.finish(&mut out);
            if !out.is_empty() {
                tx.send(std::mem::take(&mut out))
                    .ok()
                    .context("recorder thread stopped")?;
            }
            recorded_before += scanner.stats().recorded;
            dropped_before += source.stats().samples_dropped;
            halt.store(true, Ordering::Relaxed);
            drop(source);
            stop_radio(radio);
            let Some((started, new_halt)) = reopen(&o, &stop) else {
                // Ctrl-C while waiting for the radio.
                eprintln!("stopped while waiting for the radio");
                return Ok(());
            };
            reopened += 1;
            halt = new_halt;
            (info, source, radio, device_events) = started;
            let next_id = scanner.next_id();
            scanner = Scanner::new(
                o.scanner.clone(),
                info.sample_rate,
                info.center_freq,
                info.start_time.unwrap_or_else(SystemTime::now),
                o.plan,
            );
            scanner.set_next_id(next_id);
            let mut ignored = Vec::new();
            apply_overrides(&mut scanner, &base, &overrides, &mut ignored);
            session_started = Instant::now();
            eprintln!("radio reopened: {}", info.description);
            continue;
        }
        let k = read?;
        if k == 0 {
            break;
        }
        while let Ok(cmd) = control_rx.try_recv() {
            match cmd {
                scanner_web::Control::Overrides { list, done } => {
                    apply_overrides(&mut scanner, &base, &list, &mut out);
                    overrides = list;
                    publish_channels(&scanner, &overrides);
                    let _ = done.send(());
                }
            }
        }
        let t = Instant::now();
        scanner.set_live(live_tx.is_some() && audio_hub.wanted());
        scanner.push(&buf[..k], &mut out);
        busy += t.elapsed();
        let spurs = scanner.take_spurs();
        if !spurs.is_empty() {
            for &f in &spurs {
                eprintln!(
                    "  spur  {} steady for {:.0} s: recording dropped, locked out (Channels page to review)",
                    Hz(f),
                    o.scanner.spur_secs
                );
                if let Err(e) =
                    settings.set(f.round() as i64, Some(true), None, now_ns(), SPUR_DETECTOR)
                {
                    eprintln!("saving spur lockout: {e}");
                }
            }
            match settings.overrides() {
                Ok(list) => {
                    apply_overrides(&mut scanner, &base, &list, &mut out);
                    overrides = list;
                    publish_channels(&scanner, &overrides);
                }
                Err(e) => eprintln!("reading overrides: {e}"),
            }
        }
        if let Some(tx) = &live_tx {
            let mut live = Vec::new();
            scanner.take_live(&mut live);
            if !live.is_empty() {
                let _ = tx.send(live);
            }
        }
        if !out.is_empty() {
            for o in &out {
                match o {
                    Output::Open(s) => eprintln!(
                        "  open  {} {}",
                        Hz(s.slot.freq),
                        s.slot.name.as_deref().unwrap_or("")
                    ),
                    Output::Packet(p) => {
                        eprintln!("  packet {} {}", Hz(p.slot.freq), p.frame.tnc2());
                        if let Some(k) = &kiss {
                            let _ = k.send(p.frame.raw.clone());
                        }
                    }
                    _ => {}
                }
            }
            tx.send(std::mem::take(&mut out))
                .ok()
                .context("recorder thread stopped")?;
        }
        if Instant::now() >= next_publish {
            let wall = started.elapsed().as_secs_f64().max(1e-9);
            let snap = scanner_web::LiveStatus {
                source: info.description.clone(),
                sample_rate: info.sample_rate,
                center_freq_hz: info.center_freq,
                profile: o.profile_name.clone(),
                slots: scanner.detector().slots().len(),
                uptime_s: wall,
                dsp_load: busy.as_secs_f64() / wall,
                samples_dropped: dropped_before + source.stats().samples_dropped,
                recorded: recorded_before + scanner.stats().recorded,
                active: scanner
                    .active()
                    .into_iter()
                    .map(|(freq_hz, snr_db, secs)| scanner_web::ActiveChannel {
                        freq_hz,
                        snr_db,
                        secs,
                    })
                    .collect(),
                noise_floor_db: scanner.detector().floor_db(),
            };
            if let Ok(mut g) = status.write() {
                *g = snap;
            }
            next_publish += Duration::from_millis(500);
        }
        if o.http.is_some() && Instant::now() >= next_spectrum && spec_hi > spec_lo {
            let db =
                scanner
                    .detector()
                    .spectrum_db(spec_lo, spec_hi, scanner_web::live::SPECTRUM_BINS);
            if let Ok(mut g) = spectrum.write() {
                g.seq += 1;
                g.start_hz = info.center_freq + spec_lo;
                g.stop_hz = info.center_freq + spec_hi;
                g.db = db;
            }
            next_spectrum = Instant::now() + Duration::from_millis(200);
        }
        if Instant::now() >= next_status {
            let wall = started.elapsed().as_secs_f64();
            let st = scanner.stats();
            let rs = source.stats_ring();
            let active: Vec<String> = scanner
                .active()
                .iter()
                .map(|(f, snr, secs)| format!("{:.4} ({snr:.0} dB, {secs:.0} s)", f / 1e6))
                .collect();
            let radio_note = device_events
                .as_ref()
                .map(|e| {
                    let got = e.samples_received.load(Ordering::Relaxed) as f64;
                    format!(
                        ", radio: received {:.3} MS/s{}",
                        got / session_started.elapsed().as_secs_f64().max(1e-9) / 1e6,
                        e.summary().map(|s| format!(", {s}")).unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            eprintln!(
                "status: {:.0} s, DSP {:.0}% of a core, ring {}/{} blocks, dropped {} samples, recorded {}, active [{}]{}{}{}{}",
                wall,
                busy.as_secs_f64() / wall * 100.0,
                rs.blocks_written - rs.blocks_read,
                rs.capacity,
                dropped_before + rs.dropped_samples,
                recorded_before + st.recorded,
                active.join(", "),
                match (
                    scanner.detector().burst_blocks(),
                    scanner.detector().step_blocks()
                ) {
                    (0, 0) => String::new(),
                    (b, s) => format!(", guard held opens in {b} burst and {s} noise-step blocks"),
                },
                {
                    let d = scanner.detector();
                    let pulses = match d.comb_events() {
                        0 => String::new(),
                        n => format!(", {n} comb pulses"),
                    };
                    match (d.background_slots(), d.comb()) {
                        (0, _) => String::new(),
                        (n, None) => format!(", {n} channels raised over interference{pulses}"),
                        (n, Some((spacing, teeth))) => format!(
                            ", {n} channels raised over interference (comb: {teeth} lines every {:.0} kHz{pulses})",
                            spacing / 1e3
                        ),
                    }
                },
                radio_note,
                match reopened {
                    0 => String::new(),
                    n => format!(", radio reopened {n} times"),
                }
            );
            next_status += o.status_every;
        }
    }
    scanner.finish(&mut out);
    tx.send(out).ok();
    drop(tx);
    let radio_result = radio.join().expect("radio thread panicked");
    let rec = writer.join().expect("recorder thread panicked")?;
    if let Some(e) = &egress {
        e.flush();
    }
    eprintln!(
        "stopped: {} transmissions, {:.1} s of audio, {} samples dropped{}",
        rec.files,
        rec.audio_secs,
        dropped_before + source.stats().samples_dropped,
        match reopened {
            0 => String::new(),
            n => format!(", radio reopened {n} times"),
        }
    );
    if let Some(note) = device_events.as_ref().and_then(|e| e.summary()) {
        eprintln!("radio: {note}");
    }
    radio_result
}

#[cfg(test)]
mod kiss_tests {
    #[test]
    fn kiss_escapes_frame_bytes() {
        assert_eq!(
            super::kiss_frame(&[0x01, 0xc0, 0xdb, 0x02]),
            vec![0xc0, 0x00, 0x01, 0xdb, 0xdc, 0xdb, 0xdd, 0x02, 0xc0]
        );
    }
}
