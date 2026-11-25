//! `scanner serve`: the long-running scanner (PLAN.md 3.4).
//!
//! A radio feeds a lock-free ring: an SDRplay device's stream callback, or a
//! SigMF recording replayed at its real rate (optionally looped), which
//! exercises exactly the same path (see `scanner_sources::radio`). The DSP
//! thread reads the ring, runs the scanner and hands outputs on (console,
//! KISS, recorder thread). Ctrl-C stops cleanly: open transmissions are
//! closed and written.
//!
//! - `channels`: config flags plus lockout/priority overrides, spurs.
//! - `web`: the web server and the handles it reads.
//! - `telemetry`: the status line and status snapshot.

mod channels;
mod telemetry;
mod web;

use crate::kiss::Kiss;
use crate::outputs::Outputs;
use anyhow::{Context, Result, bail};
use channels::Channels;
use num_complex::Complex32;
use scanner_core::scanner::{Output, Scanner, ScannerConfig};
use scanner_core::units::Hz;
use scanner_sources::Source;
use scanner_sources::radio::{BLOCK, Radio, Replay, SdrplayRadio, Session, reopen};
use scanner_store::Recorder;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

fn note(m: &str) {
    eprintln!("{m}");
}

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

impl SourceSpec {
    /// The radio this names, with `sdrplay` settings for an SDRplay one.
    fn radio(&self, sdrplay: &scanner_sources::sdrplay::SdrplayConfig) -> Box<dyn Radio> {
        match self {
            SourceSpec::Replay { path, looped } => Box::new(Replay {
                path: path.clone(),
                looped: *looped,
            }),
            SourceSpec::Sdrplay { serial } => Box::new(SdrplayRadio {
                cfg: scanner_sources::sdrplay::SdrplayConfig {
                    serial: serial.clone(),
                    ..sdrplay.clone()
                },
            }),
        }
    }

    fn describe(&self) -> String {
        match self {
            SourceSpec::Replay { path, .. } => format!("opening {}", path.display()),
            SourceSpec::Sdrplay { .. } => "opening the SDRplay radio".into(),
        }
    }
}

pub struct ServeOptions<'a> {
    pub source: SourceSpec,
    /// Radio settings for `SourceSpec::Sdrplay` (the serial comes from the
    /// source spec).
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
    /// Tag recordings with the DTMF digits heard in them.
    pub dtmf: bool,
    pub retention: scanner_store::retention::Policy,
    /// Monthly budget for anonymous web traffic; 0 for none.
    pub anon_egress_bytes: u64,
    pub status_every: Duration,
    /// Serve decoded AX.25 frames as KISS over TCP on this address.
    pub kiss: Option<std::net::SocketAddr>,
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

    let radio = o.source.radio(&o.sdrplay);
    let mut session = radio
        .open(&stop, &note)
        .with_context(|| o.source.describe())?;
    let mut scanner = new_scanner(&o, &session);
    let (info, p) = (&session.info, *scanner.params());
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
        .with_dtmf(o.dtmf)
        .with_retention(o.retention)?;
    let disk_usage = recorder.disk_usage();
    let (tx, writer) = recorder.spawn(256);
    let kiss = o.kiss.map(Kiss::start).transpose()?;
    // Lockout and priority edits made in the web UI, on top of the config.
    let mut channels = Channels::open(o.out, &mut scanner)?;
    let web = web::Web::start(&o, &session.info, &channels, disk_usage)?;
    let outputs = Outputs::new(tx, kiss, true).with_packets(web.packets.clone());

    let mut tel = telemetry::Telemetry::new();
    let mut next_publish = Instant::now();
    let mut next_spectrum = Instant::now();
    let mut next_status = tel.started() + o.status_every;
    let mut buf = vec![Complex32::default(); BLOCK];
    let mut out: Vec<Output> = Vec::new();
    loop {
        let read = session.source.read(&mut buf);
        let read_at = SystemTime::now();
        // A radio that stalls or whose stream ends (USB failure, unplugged)
        // is reopened rather than ending the program. A replay ends for
        // good when its file does.
        let lost = radio.reopens()
            && !stop.load(Ordering::Relaxed)
            && match &read {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => e.is_stall(),
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
            outputs.send(&mut out)?;
            tel.session_ending(&scanner, &session);
            session.stop(&note);
            let Some(new) = reopen(radio.as_ref(), &stop, &note) else {
                // Ctrl-C while waiting for the radio.
                eprintln!("stopped while waiting for the radio");
                return Ok(());
            };
            session = new;
            tel.session_started();
            let old = std::mem::replace(&mut scanner, new_scanner(&o, &session));
            scanner.set_next_id(old.next_id());
            // Keep the learned comb and background, or comb lines that open
            // before they are learned again record until the comb stops.
            scanner.carry_learning(&old);
            drop(old);
            channels.apply(&mut scanner, &mut Vec::new());
            eprintln!("radio reopened: {}", session.info.description);
            continue;
        }
        let k = read?;
        if k == 0 {
            break;
        }
        while let Ok(cmd) = web.control.try_recv() {
            match cmd {
                scanner_web::Control::Overrides { list, done } => {
                    channels.replace(list, &mut scanner, &mut out);
                    let _ = done.send(());
                }
            }
        }
        let t = Instant::now();
        // Timestamps come from the sample count; keep that count within
        // reach of the wall clock in case the radio's sample accounting
        // slips (the SDRplay API's counter has reported minutes of gaps
        // that never happened).
        scanner.discipline_clock(k, read_at, max_lag(&session));
        scanner.set_live(web.live_wanted());
        scanner.push(&buf[..k], &mut out);
        tel.busy += t.elapsed();
        let spurs = scanner.take_spurs();
        if !spurs.is_empty() {
            channels.add_spurs(&spurs, o.scanner.spur_secs, &mut scanner, &mut out);
        }
        web.send_live(&mut scanner);
        outputs.send(&mut out)?;
        let now = Instant::now();
        if now >= next_publish {
            web.publish_status(tel.live_status(&scanner, &session, &o.profile_name));
            next_publish += Duration::from_millis(500);
        }
        if web.on && now >= next_spectrum {
            web.publish_spectrum(&scanner, session.info.center_freq);
            next_spectrum = Instant::now() + Duration::from_millis(200);
        }
        if now >= next_status {
            eprintln!("{}", tel.status_line(&scanner, &session));
            next_status += o.status_every;
        }
    }
    scanner.finish(&mut out);
    let _ = outputs.send(&mut out);
    drop(outputs);
    let dropped = tel.dropped(&session);
    let health = session.health.clone();
    let radio_result = session.join();
    let rec = writer.join().expect("recorder thread panicked")?;
    if let Some(e) = &web.egress {
        e.flush();
    }
    eprintln!(
        "stopped: {} transmissions, {:.1} s of audio, {} samples dropped{}",
        rec.files,
        rec.audio_secs,
        dropped,
        tel.reopened_note()
    );
    if let Some(note) = health.as_ref().and_then(|e| e.summary()) {
        eprintln!("radio: {note}");
    }
    Ok(radio_result?)
}

/// The oldest a sample can be when the DSP thread reads it: a full ring of
/// buffering plus slack for the driver and USB.
fn max_lag(session: &Session) -> Duration {
    let ring = session.source.stats_ring().capacity as f64 * BLOCK as f64;
    Duration::from_secs_f64(ring / session.info.sample_rate + 2.0)
}

/// A scanner for a radio session (a new one after each reopen).
fn new_scanner(o: &ServeOptions, session: &Session) -> Scanner {
    let info = &session.info;
    Scanner::new(
        o.scanner.clone(),
        info.sample_rate,
        info.center_freq,
        info.start_time.unwrap_or_else(SystemTime::now),
        o.plan,
    )
}
