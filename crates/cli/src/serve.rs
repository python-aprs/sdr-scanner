//! `scanner serve`: the long-running scanner (PLAN.md 3.4).
//!
//! A radio feeds a lock-free ring: an SDRplay device's stream callback, or a
//! SigMF recording replayed at its real rate (optionally looped), which
//! exercises exactly the same path. The DSP thread reads the
//! ring, runs the scanner and hands outputs to the recorder thread. Ctrl-C
//! stops cleanly: open transmissions are closed and written.

use anyhow::{Context, Result, bail};
use num_complex::Complex32;
use scanner_core::scanner::{Output, Scanner, ScannerConfig};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source, SourceInfo, ring};
use scanner_store::Recorder;
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
            if stop.load(Ordering::Relaxed) {
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
    pub out: &'a Path,
    pub status_every: Duration,
}

type Started = (
    SourceInfo,
    scanner_sources::RingSource,
    std::thread::JoinHandle<Result<()>>,
    Option<Arc<scanner_sources::sdrplay::DeviceEvents>>,
);

/// Start the radio (or replay) thread feeding a ring.
fn start_source(o: &ServeOptions, stop: Arc<AtomicBool>) -> Result<Started> {
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
                .spawn(move || replay_thread(path, looped, producer, stop))?;
            Ok((info, source, radio, None))
        }
        SourceSpec::Sdrplay { serial } => start_sdrplay(o, serial.clone(), stop),
    }
}

#[cfg(feature = "sdrplay")]
fn start_sdrplay(
    o: &ServeOptions,
    serial: Option<String>,
    stop: Arc<AtomicBool>,
) -> Result<Started> {
    use scanner_sources::sdrplay::Sdrplay;
    let cfg = scanner_sources::sdrplay::SdrplayConfig {
        serial,
        ..o.sdrplay.clone()
    };
    // Callbacks are ~1000 samples; the ring is fed in full blocks.
    let (device, source) = Sdrplay::start(&cfg, BLOCKS, BLOCK)?;
    let info = source.info().clone();
    let events = device.events().clone();
    let radio = std::thread::Builder::new()
        .name("radio".into())
        .spawn(move || {
            // The API streams on its own threads; this one owns the device
            // and stops it on Ctrl-C or when the radio goes away.
            let ev = device.events().clone();
            while !stop.load(Ordering::Relaxed) {
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

#[cfg(not(feature = "sdrplay"))]
fn start_sdrplay(_: &ServeOptions, _: Option<String>, _: Arc<AtomicBool>) -> Result<Started> {
    bail!(
        "this scanner was built without SDRplay support; rebuild with `--features sdrplay` (needs the SDRplay API 3.x installed)"
    )
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

    let (info, mut source, radio, device_events) = start_source(&o, stop.clone())?;
    let t0 = info.start_time.unwrap_or_else(SystemTime::now);
    let mut scanner = Scanner::new(o.scanner, info.sample_rate, info.center_freq, t0, o.plan);
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

    let recorder = Recorder::new(o.out)?;
    let (tx, writer) = recorder.spawn(256);

    let status: scanner_web::StatusHandle = Default::default();
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
        scanner_web::spawn(addr, state)
            .with_context(|| format!("starting the web API on {addr}"))?;
        eprintln!("web API on http://{addr}/api/v1/status");
    }
    let mut next_publish = Instant::now();
    let mut buf = vec![Complex32::default(); BLOCK];
    let mut out: Vec<Output> = Vec::new();
    let started = Instant::now();
    let mut next_status = started + o.status_every;
    let mut busy = Duration::ZERO;
    loop {
        let k = source.read(&mut buf)?;
        if k == 0 {
            break;
        }
        let t = Instant::now();
        scanner.push(&buf[..k], &mut out);
        busy += t.elapsed();
        if !out.is_empty() {
            for o in &out {
                if let Output::Open(s) = o {
                    eprintln!(
                        "  open  {} {}",
                        Hz(s.slot.freq),
                        s.slot.name.as_deref().unwrap_or("")
                    );
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
                samples_dropped: source.stats().samples_dropped,
                recorded: scanner.stats().recorded,
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
                        got / wall / 1e6,
                        e.summary().map(|s| format!(", {s}")).unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            eprintln!(
                "status: {:.0} s, DSP {:.0}% of a core, ring {}/{} blocks, dropped {} samples, recorded {}, active [{}]{}{}",
                wall,
                busy.as_secs_f64() / wall * 100.0,
                rs.blocks_written - rs.blocks_read,
                rs.capacity,
                rs.dropped_samples,
                st.recorded,
                active.join(", "),
                match (
                    scanner.detector().burst_blocks(),
                    scanner.detector().step_blocks()
                ) {
                    (0, 0) => String::new(),
                    (b, s) => format!(", guard held opens in {b} burst and {s} noise-step blocks"),
                },
                radio_note
            );
            next_status += o.status_every;
        }
    }
    scanner.finish(&mut out);
    tx.send(out).ok();
    drop(tx);
    let radio_result = radio.join().expect("radio thread panicked");
    let rec = writer.join().expect("recorder thread panicked")?;
    eprintln!(
        "stopped: {} transmissions, {:.1} s of audio, {} samples dropped",
        rec.files,
        rec.audio_secs,
        source.stats().samples_dropped
    );
    if let Some(note) = device_events.as_ref().and_then(|e| e.summary()) {
        eprintln!("radio: {note}");
    }
    radio_result
}
