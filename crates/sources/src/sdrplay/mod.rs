//! SDRplay backend (RSP1A and friends) via the SDRplay API 3.x.
//!
//! The vendor API is closed source and cannot be redistributed, so the part
//! that talks to it is a small C file (`crates/sources/sdrplay/shim.c`)
//! compiled against the installed `sdrplay_api.h` when the `sdrplay` feature
//! is on. The library itself is opened at run time. Everything that handles
//! samples once the API hands them over lives here in Rust and is built and
//! tested without the API:
//!
//! - [`SdrplayConfig`]: radio settings and their checks.
//! - [`StreamState`]: what the stream callback does. It interleaves the API's
//!   separate I and Q arrays into full ring blocks (callbacks deliver about a
//!   thousand samples each, far smaller than a ring block), and turns gaps in
//!   the API's sample counter into zero-filled spans so timestamps never slip.
//! - [`DeviceEvents`]: counters the callbacks update and anyone can read
//!   (overloads, gain changes, unplugging).

#[cfg(feature = "sdrplay")]
mod ffi;
#[cfg(feature = "sdrplay")]
pub use ffi::{Sdrplay, list_devices};

use crate::RingProducer;
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Radio settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SdrplayConfig {
    /// Device serial number; `None` opens the first device found.
    pub serial: Option<String>,
    /// Complex samples per second (2 to 10.66 MS/s; zero-IF, no decimation).
    pub sample_rate: f64,
    pub center_hz: f64,
    /// Analogue IF filter bandwidth in kHz (rounded down to one the API has:
    /// 200, 300, 600, 1536, 5000, 6000, 7000 or 8000).
    pub bandwidth_khz: u32,
    /// LNA state: 0 is the most gain. The valid range depends on the model
    /// and band (0 to 9 for an RSP1A on 2 m).
    pub lna_state: u8,
    /// IF gain reduction in dB, 20 to 59 (more is less gain).
    pub gr_db: u8,
    /// AGC set point in dBFS, or `None` for fixed gain. Fixed gain is the
    /// default: the detector tracks the noise floor, but AGC steps make it
    /// jump.
    pub agc_setpoint_dbfs: Option<i32>,
    /// Frequency correction in parts per million.
    pub ppm: f64,
    pub bias_t: bool,
    /// The RSP1A's broadcast FM and DAB band-stop filters.
    pub rf_notch: bool,
    pub dab_notch: bool,
}

impl Default for SdrplayConfig {
    fn default() -> Self {
        Self {
            serial: None,
            sample_rate: 8e6,
            center_hz: 146.0125e6,
            bandwidth_khz: 8000,
            // A usable night-time setting on the Mac's antenna (Nov 2026
            // captures). `scanner calibrate` will suggest one per site later.
            lna_state: 3,
            gr_db: 40,
            agc_setpoint_dbfs: None,
            ppm: 0.0,
            bias_t: false,
            rf_notch: false,
            dab_notch: false,
        }
    }
}

impl SdrplayConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(2e6..=10.66e6).contains(&self.sample_rate) {
            return Err(format!(
                "sample rate {} S/s is outside 2 to 10.66 MS/s",
                self.sample_rate
            ));
        }
        if !(1e3..=2e9).contains(&self.center_hz) {
            return Err(format!(
                "center frequency {} Hz is outside 1 kHz to 2 GHz",
                self.center_hz
            ));
        }
        if !(20..=59).contains(&self.gr_db) {
            return Err(format!(
                "IF gain reduction {} dB is outside 20 to 59",
                self.gr_db
            ));
        }
        if self.lna_state > 27 {
            return Err(format!("LNA state {} is out of range", self.lna_state));
        }
        if let Some(sp) = self.agc_setpoint_dbfs
            && !(-72..=0).contains(&sp)
        {
            return Err(format!("AGC set point {sp} dBFS is outside -72 to 0"));
        }
        if self.serial.as_ref().is_some_and(|s| s.len() >= 64) {
            return Err("serial number is too long".into());
        }
        Ok(())
    }

    /// IF filter bandwidth the API will actually use.
    pub fn effective_bandwidth_khz(&self) -> u32 {
        [8000, 7000, 6000, 5000, 1536, 600, 300, 200]
            .into_iter()
            .find(|&b| self.bandwidth_khz >= b)
            .unwrap_or(200)
    }
}

/// A device the API can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub model: String,
    pub hw_ver: i32,
}

/// What the callbacks report, readable from any thread.
#[derive(Debug, Default)]
pub struct DeviceEvents {
    /// Overload warnings from the API (each also acknowledged to it).
    pub overloads: AtomicU64,
    /// An overload is in progress.
    pub overloaded: AtomicBool,
    pub gain_changes: AtomicU64,
    /// IF and LNA gain reduction from the last gain-change event, in dB.
    pub gr_db: AtomicU32,
    pub lna_gr_db: AtomicU32,
    /// The device was unplugged or failed: the stream will stall.
    pub removed: AtomicBool,
    pub failed: AtomicBool,
    /// Stream resets reported by the API (after a settings change).
    pub resets: AtomicU64,
    /// Samples the API's counter says were lost, zero-filled downstream.
    pub gap_samples: AtomicU64,
    /// Counter jumps too large or backwards to be a gap; tracking restarted.
    pub counter_jumps: AtomicU64,
    /// Stream callbacks, samples they carried, and how many showed a gap.
    pub callbacks: AtomicU64,
    pub samples_received: AtomicU64,
    pub gap_events: AtomicU64,
}

impl DeviceEvents {
    pub fn on_event(&self, kind: i32, a: i32, b: i32) {
        match kind {
            1 => {
                self.gain_changes.fetch_add(1, Ordering::Relaxed);
                self.gr_db.store(a.max(0) as u32, Ordering::Relaxed);
                self.lna_gr_db.store(b.max(0) as u32, Ordering::Relaxed);
            }
            2 => {
                self.overloads.fetch_add(1, Ordering::Relaxed);
                self.overloaded.store(true, Ordering::Relaxed);
            }
            3 => self.overloaded.store(false, Ordering::Relaxed),
            4 => self.removed.store(true, Ordering::Relaxed),
            5 => self.failed.store(true, Ordering::Relaxed),
            _ => {}
        }
    }

    /// One line for logs, or `None` if nothing unusual happened.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        let ov = self.overloads.load(Ordering::Relaxed);
        if ov > 0 {
            parts.push(format!(
                "{ov} overloads{}",
                if self.overloaded.load(Ordering::Relaxed) {
                    " (now overloaded)"
                } else {
                    ""
                }
            ));
        }
        let gaps = self.gap_samples.load(Ordering::Relaxed);
        if gaps > 0 {
            parts.push(format!(
                "{gaps} samples lost in the API ({} gaps in {} callbacks)",
                self.gap_events.load(Ordering::Relaxed),
                self.callbacks.load(Ordering::Relaxed)
            ));
        }
        let jumps = self.counter_jumps.load(Ordering::Relaxed);
        if jumps > 0 {
            parts.push(format!("{jumps} counter jumps"));
        }
        if self.removed.load(Ordering::Relaxed) {
            parts.push("device removed".into());
        }
        if self.failed.load(Ordering::Relaxed) {
            parts.push("device failure".into());
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Turns the API's 32-bit sample counter into lost-sample counts.
#[derive(Debug, Clone)]
pub struct GapTracker {
    next: Option<u32>,
    /// Larger forward jumps are treated as a counter restart, not a loss.
    max_gap: u32,
}

/// What one callback's counter says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    Contiguous,
    /// This many samples were lost before this callback.
    Gap(u32),
    /// The counter restarted or jumped; tracking starts again here.
    Restart,
}

impl GapTracker {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            next: None,
            max_gap: sample_rate.clamp(1.0, u32::MAX as f64 / 4.0) as u32,
        }
    }

    pub fn observe(&mut self, first: u32, n: u32, reset: bool) -> Continuity {
        let expected = self.next.replace(first.wrapping_add(n));
        match expected {
            None => Continuity::Contiguous,
            Some(_) if reset => Continuity::Restart,
            Some(e) => match first.wrapping_sub(e) {
                0 => Continuity::Contiguous,
                g if g <= self.max_gap => Continuity::Gap(g),
                _ => Continuity::Restart,
            },
        }
    }
}

/// Everything the stream callback touches. Owned by the callback thread
/// while streaming.
pub struct StreamState {
    producer: RingProducer,
    pending: Vec<Complex32>,
    block_len: usize,
    gaps: GapTracker,
    events: std::sync::Arc<DeviceEvents>,
    /// Diagnostics (SCANNER_SDRPLAY_DEBUG=N prints the first N callbacks
    /// and the first N gaps with their counter values).
    debug_left: u32,
    debug_gaps_left: u32,
    last: (u32, u32),
    /// SCANNER_SDRPLAY_IGNORE_GAPS=1 counts counter gaps but does not
    /// zero-fill them (to tell real loss from a miscounted counter).
    fill_gaps: bool,
}

const SCALE: f32 = 1.0 / 32768.0;

fn debug_count() -> u32 {
    std::env::var("SCANNER_SDRPLAY_DEBUG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl StreamState {
    pub fn new(
        producer: RingProducer,
        block_len: usize,
        sample_rate: f64,
        events: std::sync::Arc<DeviceEvents>,
    ) -> Self {
        Self {
            producer,
            pending: Vec::with_capacity(block_len),
            block_len,
            gaps: GapTracker::new(sample_rate),
            events,
            debug_left: debug_count(),
            debug_gaps_left: debug_count(),
            last: (0, 0),
            fill_gaps: std::env::var("SCANNER_SDRPLAY_IGNORE_GAPS").map_or(true, |v| v != "1"),
        }
    }

    /// Handle one stream callback.
    pub fn on_samples(&mut self, xi: &[i16], xq: &[i16], first: u32, reset: bool) {
        let n = xi.len().min(xq.len());
        if reset {
            self.events.resets.fetch_add(1, Ordering::Relaxed);
        }
        self.events.callbacks.fetch_add(1, Ordering::Relaxed);
        self.events
            .samples_received
            .fetch_add(n as u64, Ordering::Relaxed);
        if self.debug_left > 0 {
            self.debug_left -= 1;
            eprintln!("sdrplay debug: callback first={first} n={n} reset={reset}");
        }
        let c = self.gaps.observe(first, n as u32, reset);
        if self.debug_gaps_left > 0 && !matches!(c, Continuity::Contiguous) {
            self.debug_gaps_left -= 1;
            eprintln!(
                "sdrplay debug: {c:?}: previous first={} n={}, this first={first} n={n} reset={reset}",
                self.last.0, self.last.1
            );
        }
        self.last = (first, n as u32);
        match c {
            Continuity::Contiguous => {}
            Continuity::Gap(g) => {
                self.events.gap_events.fetch_add(1, Ordering::Relaxed);
                self.events
                    .gap_samples
                    .fetch_add(g as u64, Ordering::Relaxed);
                if self.fill_gaps {
                    self.flush();
                    self.producer.skip(g as u64);
                }
            }
            Continuity::Restart => {
                if !reset {
                    self.events.counter_jumps.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let mut done = 0;
        while done < n {
            let take = (self.block_len - self.pending.len()).min(n - done);
            self.pending.extend(
                xi[done..done + take]
                    .iter()
                    .zip(&xq[done..done + take])
                    .map(|(&i, &q)| Complex32::new(i as f32 * SCALE, q as f32 * SCALE)),
            );
            done += take;
            if self.pending.len() == self.block_len {
                self.flush();
            }
        }
    }

    /// Push buffered samples to the ring now (a partial block).
    pub fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.producer.push(&self.pending);
            self.pending.clear();
        }
    }

    /// Stream over: deliver what is buffered and end the ring.
    pub fn close(&mut self) {
        self.flush();
        self.producer.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Source, SourceInfo, ring};
    use std::sync::Arc;

    fn info() -> SourceInfo {
        SourceInfo {
            sample_rate: 8e6,
            center_freq: 146e6,
            start_time: None,
            total_samples: None,
            description: "test".into(),
        }
    }

    fn drain(src: &mut crate::RingSource) -> Vec<Complex32> {
        let mut all = Vec::new();
        let mut buf = vec![Complex32::default(); 500];
        loop {
            let k = src.read(&mut buf).unwrap();
            if k == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..k]);
        }
    }

    #[test]
    fn defaults_are_valid_and_checks_catch_mistakes() {
        let c = SdrplayConfig::default();
        assert_eq!(c.validate(), Ok(()));
        assert!(
            SdrplayConfig {
                sample_rate: 12e6,
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            SdrplayConfig {
                gr_db: 10,
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            SdrplayConfig {
                agc_setpoint_dbfs: Some(5),
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        assert_eq!(c.effective_bandwidth_khz(), 8000);
        let narrow = SdrplayConfig {
            bandwidth_khz: 5500,
            ..c
        };
        assert_eq!(narrow.effective_bandwidth_khz(), 5000);
    }

    #[test]
    fn gap_tracker_classifies_counter_steps() {
        let mut g = GapTracker::new(8e6);
        assert_eq!(g.observe(100, 10, false), Continuity::Contiguous);
        assert_eq!(g.observe(110, 10, false), Continuity::Contiguous);
        assert_eq!(g.observe(125, 10, false), Continuity::Gap(5));
        // Wraparound of the 32-bit counter is contiguous.
        let mut w = GapTracker::new(8e6);
        w.observe(u32::MAX - 4, 5, false);
        assert_eq!(w.observe(0, 10, false), Continuity::Contiguous);
        // Backwards or huge jumps restart tracking.
        assert_eq!(w.observe(5, 10, false), Continuity::Restart);
        assert_eq!(w.observe(15, 10, false), Continuity::Contiguous);
        assert_eq!(w.observe(100_000_000, 10, false), Continuity::Restart);
        assert_eq!(w.observe(0, 10, true), Continuity::Restart);
        assert_eq!(w.observe(10, 10, false), Continuity::Contiguous);
    }

    #[test]
    fn callbacks_are_interleaved_into_full_blocks() {
        let (p, mut c) = ring(16, 1000, info());
        let events = Arc::new(DeviceEvents::default());
        let mut st = StreamState::new(p, 1000, 8e6, events.clone());
        let mut first = 7u32;
        for _ in 0..5 {
            let xi: Vec<i16> = (0..336).map(|k| (first + k) as i16).collect();
            let xq: Vec<i16> = xi.iter().map(|v| -v).collect();
            st.on_samples(&xi, &xq, first, false);
            first += 336;
        }
        // 1680 samples: one full block pushed, 680 still buffered.
        assert_eq!(c.stats_ring().blocks_written, 1);
        st.close();
        let got = drain(&mut c);
        assert_eq!(got.len(), 1680);
        for (k, v) in got.iter().enumerate() {
            let want = (7 + k) as f32 / 32768.0;
            assert_eq!(*v, Complex32::new(want, -want), "{k}");
        }
        assert_eq!(events.summary(), None);
    }

    #[test]
    fn counter_gaps_become_zero_filled_spans() {
        let (p, mut c) = ring(16, 1000, info());
        let events = Arc::new(DeviceEvents::default());
        let mut st = StreamState::new(p, 1000, 8e6, events.clone());
        let ones = vec![16384i16; 100];
        st.on_samples(&ones, &ones, 0, false);
        st.on_samples(&ones, &ones, 100, false);
        // 50 samples lost in the API.
        st.on_samples(&ones, &ones, 250, false);
        st.close();
        let got = drain(&mut c);
        assert_eq!(got.len(), 350);
        assert!(got[..200].iter().all(|v| v.re == 0.5));
        assert!(got[200..250].iter().all(|v| *v == Complex32::default()));
        assert!(got[250..].iter().all(|v| v.re == 0.5));
        assert_eq!(c.stats().samples_dropped, 50);
        assert_eq!(events.gap_samples.load(Ordering::Relaxed), 50);
        assert!(events.summary().unwrap().contains("50 samples lost"));
    }

    #[test]
    fn events_update_counters() {
        let e = DeviceEvents::default();
        e.on_event(1, 40, 24);
        e.on_event(2, 0, 0);
        assert!(e.overloaded.load(Ordering::Relaxed));
        e.on_event(3, 0, 0);
        assert!(!e.overloaded.load(Ordering::Relaxed));
        assert_eq!(e.gr_db.load(Ordering::Relaxed), 40);
        e.on_event(4, 0, 0);
        let s = e.summary().unwrap();
        assert!(
            s.contains("1 overloads") && s.contains("device removed"),
            "{s}"
        );
    }
}
