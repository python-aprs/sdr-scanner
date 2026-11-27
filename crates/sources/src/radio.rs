//! Radios: sources that run on their own thread and can be lost and opened
//! again.
//!
//! A [`Radio`] knows how to open a stream; each open gives a [`Session`]: the
//! [`RingSource`] the DSP thread reads, the thread feeding it, and the
//! device's [`Health`] counters. When a live radio stalls or goes away, the
//! caller stops the session and calls [`reopen`]. Replaying a recording at
//! its real rate ([`Replay`]) goes through exactly the same path.

use crate::{FileSource, RingProducer, RingSource, Source, SourceError, SourceInfo, ring};
use num_complex::Complex32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// Samples per ring block (~6.5 ms at 10 MS/s), and blocks in the ring
/// (~0.85 s of buffering at 10 MS/s).
pub const BLOCK: usize = 1 << 16;
pub const BLOCKS: usize = 128;

/// What a device reports about itself while streaming.
pub trait Health: Send + Sync {
    /// Samples the device has delivered this session.
    fn samples_received(&self) -> u64;
    /// One line for logs, or `None` if nothing unusual happened.
    fn summary(&self) -> Option<String>;
}

/// Something that can open a stream of samples.
pub trait Radio: Send + Sync {
    /// Open a session. `stop` (Ctrl-C) gives up on a slow open and later
    /// stops the session's thread; `note` takes progress messages for the
    /// user.
    fn open(&self, stop: &Arc<AtomicBool>, note: &dyn Fn(&str)) -> Result<Session, SourceError>;

    /// Whether a stream that ends or stalls means the radio was lost (and
    /// should be reopened), rather than that it is done.
    fn reopens(&self) -> bool;
}

/// One open stream.
pub struct Session {
    pub info: SourceInfo,
    pub source: RingSource,
    pub health: Option<Arc<dyn Health>>,
    thread: JoinHandle<Result<(), SourceError>>,
    /// Stops this session's thread (unlike `stop`, which stops them all).
    halt: Arc<AtomicBool>,
}

impl Session {
    /// Stop the thread and wait for it (up to 10 s: the vendor API can hang
    /// tearing down a stream that died; then the thread is left behind).
    pub fn stop(self, note: &dyn Fn(&str)) {
        self.halt.store(true, Ordering::Relaxed);
        drop(self.source);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = self.thread;
        std::thread::spawn(move || {
            let _ = tx.send(thread.join());
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => note(&format!("radio: {e}")),
            Ok(Err(_)) => note("radio thread panicked"),
            Err(_) => {
                note("radio: the old stream did not stop within 10 s; opening a new one anyway")
            }
        }
    }

    /// Wait for the thread to end (after `stop` was set) and return how it
    /// ended, keeping the source's final counters readable until then.
    pub fn join(self) -> Result<(), SourceError> {
        self.thread.join().expect("radio thread panicked")
    }
}

/// Open the radio again, retrying with a growing delay (1 s, doubling to
/// 30 s) until it works or `stop` is set (then `None`).
pub fn reopen(radio: &dyn Radio, stop: &Arc<AtomicBool>, note: &dyn Fn(&str)) -> Option<Session> {
    let mut wait = Duration::from_secs(1);
    loop {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        match radio.open(stop, note) {
            Ok(s) => return Some(s),
            Err(e) => {
                wait = (wait * 2).min(Duration::from_secs(30));
                note(&format!(
                    "reopening the radio failed: {e}; trying again in {} s",
                    wait.as_secs()
                ));
            }
        }
    }
}

/// A recording played at its sample rate, like a live radio, optionally
/// looped.
pub struct Replay {
    pub path: PathBuf,
    pub looped: bool,
}

impl Radio for Replay {
    fn open(&self, stop: &Arc<AtomicBool>, _note: &dyn Fn(&str)) -> Result<Session, SourceError> {
        let f = FileSource::open(&self.path)?;
        let fi = f.info().clone();
        let info = SourceInfo {
            // A live stream starts now, whatever the recording says.
            start_time: Some(SystemTime::now()),
            total_samples: None,
            description: format!(
                "{} ({})",
                fi.description,
                if self.looped { "looped" } else { "replay" }
            ),
            ..fi
        };
        let (producer, source) = ring(BLOCKS, BLOCK, info.clone());
        let halt = Arc::new(AtomicBool::new(false));
        let (path, looped) = (self.path.clone(), self.looped);
        let (stop, h) = (stop.clone(), halt.clone());
        let thread = std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || replay(path, looped, producer, stop, h))?;
        Ok(Session {
            info,
            source,
            health: None,
            thread,
            halt,
        })
    }

    fn reopens(&self) -> bool {
        false
    }
}

fn replay(
    path: PathBuf,
    looped: bool,
    mut producer: RingProducer,
    stop: Arc<AtomicBool>,
    halt: Arc<AtomicBool>,
) -> Result<(), SourceError> {
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

/// An SDRplay radio (RSP1A and friends).
pub struct SdrplayRadio {
    pub cfg: crate::sdrplay::SdrplayConfig,
}

impl Health for crate::sdrplay::DeviceEvents {
    fn samples_received(&self) -> u64 {
        self.samples_received.load(Ordering::Relaxed)
    }
    fn summary(&self) -> Option<String> {
        crate::sdrplay::DeviceEvents::summary(self)
    }
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

#[cfg(feature = "sdrplay")]
impl Radio for SdrplayRadio {
    fn open(&self, stop: &Arc<AtomicBool>, note: &dyn Fn(&str)) -> Result<Session, SourceError> {
        use crate::sdrplay::Sdrplay;
        use std::sync::mpsc::RecvTimeoutError;
        // Opening goes through blocking SDRplay API calls with no timeout:
        // they wait for as long as another program holds the API lock or the
        // service is stuck. Open on a helper thread so we can say what we
        // are waiting on and give up on Ctrl-C.
        note("opening the SDRplay radio...");
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg = self.cfg.clone();
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
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(SourceError::Device(
                        "the thread opening the radio died".into(),
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if stop.load(Ordering::Relaxed) {
                return Err(SourceError::Device(
                    "stopped while waiting for the SDRplay API to open the radio".into(),
                ));
            }
            if !hinted && opened.elapsed() >= OPEN_HINT_AFTER {
                hinted = true;
                note(&open_hint(opened.elapsed()));
            }
        };
        let info = source.info().clone();
        let events = device.events().clone();
        let halt = Arc::new(AtomicBool::new(false));
        let (stop, h) = (stop.clone(), halt.clone());
        let thread = std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || {
                // The API streams on its own threads; this one owns the
                // device and stops it on Ctrl-C or when the radio goes away.
                let ev = device.events().clone();
                while !stop.load(Ordering::Relaxed) && !h.load(Ordering::Relaxed) {
                    let removed = ev.removed.load(Ordering::Relaxed);
                    if removed || ev.failed.load(Ordering::Relaxed) {
                        drop(device);
                        return Err(SourceError::Device(format!(
                            "the radio was {}",
                            if removed {
                                "unplugged"
                            } else {
                                "reported as failed"
                            }
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                drop(device);
                Ok(())
            })?;
        Ok(Session {
            info,
            source,
            health: Some(events),
            thread,
            halt,
        })
    }

    fn reopens(&self) -> bool {
        true
    }
}

#[cfg(not(feature = "sdrplay"))]
impl Radio for SdrplayRadio {
    fn open(&self, _: &Arc<AtomicBool>, _: &dyn Fn(&str)) -> Result<Session, SourceError> {
        Err(SourceError::Device(
            "this scanner was built without SDRplay support; rebuild with `--features sdrplay` \
             (needs the SDRplay API 3.x installed)"
                .into(),
        ))
    }

    fn reopens(&self) -> bool {
        true
    }
}

/// An Airspy R2 or Mini.
pub struct AirspyRadio {
    pub cfg: crate::airspy::AirspyConfig,
}

impl Health for crate::airspy::AirspyEvents {
    fn samples_received(&self) -> u64 {
        self.samples_received.load(Ordering::Relaxed)
    }
    fn summary(&self) -> Option<String> {
        crate::airspy::AirspyEvents::summary(self)
    }
}

#[cfg(feature = "airspy")]
impl Radio for AirspyRadio {
    fn open(&self, stop: &Arc<AtomicBool>, note: &dyn Fn(&str)) -> Result<Session, SourceError> {
        use crate::airspy::Airspy;
        note("opening the Airspy...");
        let (device, source) = Airspy::start(&self.cfg, BLOCKS, BLOCK)?;
        let info = source.info().clone();
        let events = device.events().clone();
        let halt = Arc::new(AtomicBool::new(false));
        let (stop, h) = (stop.clone(), halt.clone());
        let thread = std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || {
                // libairspy streams on its own threads; this one owns the
                // device and stops it on Ctrl-C or when streaming dies
                // (unplugged, USB failure).
                while !stop.load(Ordering::Relaxed) && !h.load(Ordering::Relaxed) {
                    if !device.is_streaming() {
                        drop(device);
                        return Err(SourceError::Device(
                            "the Airspy stopped streaming (unplugged?)".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                drop(device);
                Ok(())
            })?;
        Ok(Session {
            info,
            source,
            health: Some(events),
            thread,
            halt,
        })
    }

    fn reopens(&self) -> bool {
        true
    }
}

#[cfg(not(feature = "airspy"))]
impl Radio for AirspyRadio {
    fn open(&self, _: &Arc<AtomicBool>, _: &dyn Fn(&str)) -> Result<Session, SourceError> {
        Err(SourceError::Device(
            "this scanner was built without Airspy support; rebuild with `--features airspy` \
             (needs libairspy installed)"
                .into(),
        ))
    }

    fn reopens(&self) -> bool {
        true
    }
}
