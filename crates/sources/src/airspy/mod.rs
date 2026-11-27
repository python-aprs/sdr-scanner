//! Airspy backend (R2 and Mini) via libairspy.
//!
//! libairspy is open source, so unlike the SDRplay backend there is no C
//! shim: [`ffi`] declares the handful of functions we call and links the
//! library when the `airspy` feature is on. Everything that handles samples
//! once libairspy hands them over lives here and is built and tested without
//! the library:
//!
//! - [`AirspyConfig`] and [`AirspyGain`]: radio settings and their checks.
//! - [`StreamState`]: what the stream callback does. libairspy converts the
//!   R2's real 20 MS/s ADC stream to float IQ at 10 MS/s on its own thread
//!   and calls back with ~65k samples at a time; we gather them into full
//!   ring blocks. libairspy counts buffers it had to drop when its consumer
//!   fell behind and reports them per transfer, so those spans are
//!   zero-filled and every timestamp after them stays right.
//! - [`AirspyEvents`]: counters anyone can read while streaming.
//!
//! The Airspy has a real-sampling front end, so there is no DC spike at the
//! tuned frequency (unlike the RSP1A's zero-IF one).

#[cfg(feature = "airspy")]
mod ffi;
#[cfg(feature = "airspy")]
pub use ffi::{Airspy, list_devices};

use crate::RingProducer;
use num_complex::Complex32;
use std::sync::atomic::{AtomicU64, Ordering};

/// How the tuner's three gain stages are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirspyGain {
    /// libairspy's linearity preset, 0 to 21 (more is more gain). Favours
    /// mixer and IF gain over LNA gain, which holds up better next to
    /// strong signals.
    Linearity(u8),
    /// libairspy's sensitivity preset, 0 to 21: more LNA gain for the same
    /// step, for weak signals on a quiet antenna.
    Sensitivity(u8),
    /// Each stage by hand: LNA 0 to 14, mixer 0 to 15, VGA (IF) 0 to 15.
    Manual { lna: u8, mixer: u8, vga: u8 },
    /// LNA and mixer AGC on, VGA fixed (0 to 15). The detector tracks the
    /// noise floor, but AGC steps make it jump, so this is not the default.
    Agc { vga: u8 },
}

impl Default for AirspyGain {
    fn default() -> Self {
        // Tested on the Mac's 2 m antenna (Nov 2026).
        AirspyGain::Linearity(14)
    }
}

impl std::fmt::Display for AirspyGain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AirspyGain::Linearity(g) => write!(f, "linearity:{g}"),
            AirspyGain::Sensitivity(g) => write!(f, "sensitivity:{g}"),
            AirspyGain::Manual { lna, mixer, vga } => write!(f, "manual:{lna},{mixer},{vga}"),
            AirspyGain::Agc { vga } => write!(f, "agc:{vga}"),
        }
    }
}

impl std::str::FromStr for AirspyGain {
    type Err = String;
    /// `linearity:N`, `sensitivity:N`, `manual:LNA,MIXER,VGA` or `agc:VGA`.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "airspy gain {s:?}: expected linearity:N or sensitivity:N (0-21), \
                 manual:LNA,MIXER,VGA, or agc:VGA"
            )
        };
        let (kind, rest) = s.split_once(':').ok_or_else(bad)?;
        let nums: Vec<u8> = rest
            .split(',')
            .map(|v| v.trim().parse::<u8>().map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        let g = match (kind, nums.as_slice()) {
            ("linearity", [g]) => AirspyGain::Linearity(*g),
            ("sensitivity", [g]) => AirspyGain::Sensitivity(*g),
            ("manual", [lna, mixer, vga]) => AirspyGain::Manual {
                lna: *lna,
                mixer: *mixer,
                vga: *vga,
            },
            ("agc", [vga]) => AirspyGain::Agc { vga: *vga },
            _ => return Err(bad()),
        };
        g.validate()?;
        Ok(g)
    }
}

impl AirspyGain {
    pub fn validate(&self) -> Result<(), String> {
        let check = |name: &str, v: u8, max: u8| {
            if v > max {
                Err(format!("airspy {name} gain {v} is outside 0 to {max}"))
            } else {
                Ok(())
            }
        };
        match *self {
            AirspyGain::Linearity(g) => check("linearity", g, 21),
            AirspyGain::Sensitivity(g) => check("sensitivity", g, 21),
            AirspyGain::Manual { lna, mixer, vga } => {
                check("LNA", lna, 14)?;
                check("mixer", mixer, 15)?;
                check("VGA", vga, 15)
            }
            AirspyGain::Agc { vga } => check("VGA", vga, 15),
        }
    }
}

/// Radio settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AirspyConfig {
    /// Device serial number; `None` opens the first device found.
    pub serial: Option<u64>,
    /// Complex samples per second. Must be one the device lists: 10 or 2.5
    /// MS/s for an R2, 6 or 3 MS/s for a Mini.
    pub sample_rate: f64,
    pub center_hz: f64,
    pub gain: AirspyGain,
    /// Frequency correction in parts per million (applied to the tuning
    /// frequency; libairspy has no correction of its own).
    pub ppm: f64,
    pub bias_t: bool,
    /// Pack 12-bit samples on the USB link (less USB bandwidth, a little more
    /// CPU). Off by default.
    pub packing: bool,
}

impl Default for AirspyConfig {
    fn default() -> Self {
        Self {
            serial: None,
            sample_rate: 10e6,
            center_hz: 146.0125e6,
            gain: AirspyGain::default(),
            ppm: 0.0,
            bias_t: false,
            packing: false,
        }
    }
}

impl AirspyConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1e6..=20e6).contains(&self.sample_rate) {
            return Err(format!(
                "sample rate {} S/s is outside what any Airspy offers (2.5 to 10 MS/s)",
                self.sample_rate
            ));
        }
        // The R820T tuner's range.
        if !(24e6..=1.8e9).contains(&self.center_hz) {
            return Err(format!(
                "center frequency {} Hz is outside 24 MHz to 1.8 GHz",
                self.center_hz
            ));
        }
        if !(-200.0..=200.0).contains(&self.ppm) {
            return Err(format!(
                "frequency correction {} ppm is too large",
                self.ppm
            ));
        }
        self.gain.validate()
    }

    /// The frequency to ask the tuner for so that it lands on `center_hz`
    /// given the crystal error (positive `ppm`: the radio's clock runs fast).
    pub fn tuner_hz(&self) -> u32 {
        (self.center_hz / (1.0 + self.ppm * 1e-6)).round() as u32
    }
}

/// Parse a serial number as `airspy_info` prints it (hex, optional `0x`).
pub fn parse_serial(s: &str) -> Result<u64, String> {
    let h = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(h, 16)
        .map_err(|_| format!("airspy serial {s:?} is not a hex number (e.g. 35AC63DC2D57964F)"))
}

/// A device libairspy can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: u64,
    /// Firmware version string, if the device was opened to read it.
    pub firmware: Option<String>,
    /// Sample rates it offers (complex S/s), if opened.
    pub sample_rates: Vec<u32>,
}

impl DeviceInfo {
    /// R2 or Mini, going by the sample rates (both share a USB ID).
    pub fn model(&self) -> &'static str {
        if self.sample_rates.contains(&10_000_000) {
            "R2"
        } else if self.sample_rates.contains(&6_000_000) {
            "Mini"
        } else {
            "?"
        }
    }
}

/// What the stream reports, readable from any thread.
#[derive(Debug, Default)]
pub struct AirspyEvents {
    pub callbacks: AtomicU64,
    pub samples_received: AtomicU64,
    /// Samples libairspy dropped (its consumer thread fell behind the USB
    /// stream); zero-filled in the delivered stream.
    pub dropped_samples: AtomicU64,
    pub drop_events: AtomicU64,
}

impl AirspyEvents {
    /// One line for logs, or `None` if nothing unusual happened.
    pub fn summary(&self) -> Option<String> {
        let d = self.dropped_samples.load(Ordering::Relaxed);
        (d > 0).then(|| {
            format!(
                "{d} samples dropped by libairspy ({} times in {} transfers)",
                self.drop_events.load(Ordering::Relaxed),
                self.callbacks.load(Ordering::Relaxed)
            )
        })
    }
}

/// Everything the stream callback touches. Owned by libairspy's consumer
/// thread while streaming.
pub struct StreamState {
    producer: RingProducer,
    pending: Vec<Complex32>,
    block_len: usize,
    events: std::sync::Arc<AirspyEvents>,
}

impl StreamState {
    pub fn new(
        producer: RingProducer,
        block_len: usize,
        events: std::sync::Arc<AirspyEvents>,
    ) -> Self {
        Self {
            producer,
            pending: Vec::with_capacity(block_len),
            block_len,
            events,
        }
    }

    /// Handle one transfer: `dropped` samples were lost just before these.
    pub fn on_samples(&mut self, samples: &[Complex32], dropped: u64) {
        self.events.callbacks.fetch_add(1, Ordering::Relaxed);
        self.events
            .samples_received
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
        if dropped > 0 {
            self.events.drop_events.fetch_add(1, Ordering::Relaxed);
            self.events
                .dropped_samples
                .fetch_add(dropped, Ordering::Relaxed);
            self.flush();
            self.producer.skip(dropped);
        }
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (self.block_len - self.pending.len()).min(rest.len());
            if take == self.block_len {
                // Whole block straight from libairspy's buffer.
                self.producer.push(&rest[..take]);
            } else {
                self.pending.extend_from_slice(&rest[..take]);
                if self.pending.len() == self.block_len {
                    self.flush();
                }
            }
            rest = &rest[take..];
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
            sample_rate: 10e6,
            center_freq: 146e6,
            start_time: None,
            total_samples: None,
            description: "test".into(),
        }
    }

    fn drain(src: &mut crate::RingSource) -> Vec<Complex32> {
        let mut all = Vec::new();
        let mut buf = vec![Complex32::default(); 700];
        loop {
            let k = src.read(&mut buf).unwrap();
            if k == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..k]);
        }
    }

    #[test]
    fn gain_specs_parse_and_check_ranges() {
        assert_eq!("linearity:14".parse(), Ok(AirspyGain::Linearity(14)));
        assert_eq!("sensitivity:0".parse(), Ok(AirspyGain::Sensitivity(0)));
        assert_eq!(
            "manual:10,12, 8".parse(),
            Ok(AirspyGain::Manual {
                lna: 10,
                mixer: 12,
                vga: 8
            })
        );
        assert_eq!("agc:9".parse(), Ok(AirspyGain::Agc { vga: 9 }));
        for bad in [
            "linearity:22",
            "manual:15,1,1",
            "manual:1,2",
            "loud",
            "agc:x",
        ] {
            assert!(bad.parse::<AirspyGain>().is_err(), "{bad}");
        }
        for g in [
            AirspyGain::Linearity(3),
            AirspyGain::Manual {
                lna: 1,
                mixer: 2,
                vga: 3,
            },
            AirspyGain::Agc { vga: 4 },
        ] {
            assert_eq!(g.to_string().parse(), Ok(g));
        }
    }

    #[test]
    fn config_checks_and_ppm() {
        let c = AirspyConfig::default();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.tuner_hz(), 146_012_500);
        assert!(
            AirspyConfig {
                center_hz: 10e6,
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        let fast = AirspyConfig { ppm: 10.0, ..c };
        // A clock 10 ppm fast lands 1460 Hz high, so ask for that much less.
        assert_eq!(fast.tuner_hz(), 146_011_040);
    }

    #[test]
    fn serials_parse_as_hex() {
        assert_eq!(parse_serial("35AC63DC2D57964F"), Ok(0x35AC63DC2D57964F));
        assert_eq!(parse_serial("0x35ac63dc2d57964f"), Ok(0x35AC63DC2D57964F));
        assert!(parse_serial("nope").is_err());
    }

    #[test]
    fn model_from_rates() {
        let d = |r: Vec<u32>| DeviceInfo {
            serial: 1,
            firmware: None,
            sample_rates: r,
        };
        assert_eq!(d(vec![10_000_000, 2_500_000]).model(), "R2");
        assert_eq!(d(vec![6_000_000, 3_000_000]).model(), "Mini");
    }

    #[test]
    fn transfers_become_full_blocks() {
        let (p, mut c) = ring(16, 1000, info());
        let events = Arc::new(AirspyEvents::default());
        let mut st = StreamState::new(p, 1000, events.clone());
        let mut k = 0;
        for len in [300, 2500, 450] {
            let xs: Vec<Complex32> = (k..k + len)
                .map(|v| Complex32::new(v as f32, -(v as f32)))
                .collect();
            st.on_samples(&xs, 0);
            k += len;
        }
        // 3250 samples: three full blocks pushed, 250 still buffered.
        assert_eq!(c.stats_ring().blocks_written, 3);
        st.close();
        let got = drain(&mut c);
        assert_eq!(got.len(), 3250);
        for (i, v) in got.iter().enumerate() {
            assert_eq!(*v, Complex32::new(i as f32, -(i as f32)), "{i}");
        }
        assert_eq!(events.summary(), None);
        assert_eq!(events.samples_received.load(Ordering::Relaxed), 3250);
    }

    #[test]
    fn dropped_spans_are_zero_filled() {
        let (p, mut c) = ring(16, 1000, info());
        let events = Arc::new(AirspyEvents::default());
        let mut st = StreamState::new(p, 1000, events.clone());
        let ones = vec![Complex32::new(1.0, 0.0); 600];
        st.on_samples(&ones, 0);
        st.on_samples(&ones, 400);
        st.close();
        let got = drain(&mut c);
        assert_eq!(got.len(), 1600);
        assert!(got[..600].iter().all(|v| v.re == 1.0));
        assert!(got[600..1000].iter().all(|v| *v == Complex32::default()));
        assert!(got[1000..].iter().all(|v| v.re == 1.0));
        assert_eq!(c.stats().samples_dropped, 400);
        assert!(events.summary().unwrap().contains("400 samples dropped"));
    }
}
