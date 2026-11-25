//! What `serve` reports about itself: the status line, the web status
//! snapshot, and counters carried across radio reopens.

use scanner_core::scanner::Scanner;
use scanner_sources::Source;
use scanner_sources::radio::Session;
use std::time::{Duration, Instant};

pub struct Telemetry {
    started: Instant,
    /// Time spent in `Scanner::push`.
    pub busy: Duration,
    /// Radio reopens, and the counts of the scanners and sessions before
    /// the current one.
    reopened: u32,
    recorded_before: u64,
    dropped_before: u64,
    /// When this radio session started (its counters start from zero).
    session_started: Instant,
}

impl Telemetry {
    pub fn new() -> Self {
        let now = Instant::now();
        Telemetry {
            started: now,
            busy: Duration::ZERO,
            reopened: 0,
            recorded_before: 0,
            dropped_before: 0,
            session_started: now,
        }
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    /// A radio session and its scanner are being replaced: keep their
    /// counts.
    pub fn session_ending(&mut self, scanner: &Scanner, session: &Session) {
        self.recorded_before += scanner.stats().recorded;
        self.dropped_before += session.source.stats().samples_dropped;
    }

    /// The radio was reopened.
    pub fn session_started(&mut self) {
        self.reopened += 1;
        self.session_started = Instant::now();
    }

    pub fn dropped(&self, session: &Session) -> u64 {
        self.dropped_before + session.source.stats().samples_dropped
    }

    pub fn recorded(&self, scanner: &Scanner) -> u64 {
        self.recorded_before + scanner.stats().recorded
    }

    /// ", radio reopened N times", or nothing.
    pub fn reopened_note(&self) -> String {
        match self.reopened {
            0 => String::new(),
            n => format!(", radio reopened {n} times"),
        }
    }

    /// The snapshot the web UI's status page shows.
    pub fn live_status(
        &self,
        scanner: &Scanner,
        session: &Session,
        profile: &str,
    ) -> scanner_web::LiveStatus {
        let wall = self.started.elapsed().as_secs_f64().max(1e-9);
        let info = &session.info;
        scanner_web::LiveStatus {
            source: info.description.clone(),
            sample_rate: info.sample_rate,
            center_freq_hz: info.center_freq,
            profile: profile.to_owned(),
            slots: scanner.detector().slots().len(),
            uptime_s: wall,
            dsp_load: self.busy.as_secs_f64() / wall,
            samples_dropped: self.dropped(session),
            recorded: self.recorded(scanner),
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
        }
    }

    /// The periodic status line.
    pub fn status_line(&self, scanner: &Scanner, session: &Session) -> String {
        let wall = self.started.elapsed().as_secs_f64();
        let rs = session.source.stats_ring();
        let active: Vec<String> = scanner
            .active()
            .iter()
            .map(|(f, snr, secs)| format!("{:.4} ({snr:.0} dB, {secs:.0} s)", f / 1e6))
            .collect();
        let radio_note = session
            .health
            .as_ref()
            .map(|e| {
                let got = e.samples_received() as f64;
                format!(
                    ", radio: received {:.3} MS/s{}",
                    got / self.session_started.elapsed().as_secs_f64().max(1e-9) / 1e6,
                    e.summary().map(|s| format!(", {s}")).unwrap_or_default()
                )
            })
            .unwrap_or_default();
        let st = scanner.stats();
        let clock_note = match st.clock_steps {
            0 => String::new(),
            n => format!(
                ", clock stepped {n} times to match the wall ({:+.1} s net)",
                st.clock_step_ns as f64 / 1e9
            ),
        };
        let d = scanner.detector();
        let guards = match (d.burst_blocks(), d.step_blocks()) {
            (0, 0) => String::new(),
            (b, s) => format!(", guard held opens in {b} burst and {s} noise-step blocks"),
        };
        let pulses = match d.comb_events() {
            0 => String::new(),
            n => format!(", {n} comb pulses"),
        };
        let raised = match (d.background_slots(), d.comb()) {
            (0, _) => String::new(),
            (n, None) => format!(", {n} channels raised over interference{pulses}"),
            (n, Some((spacing, teeth))) => format!(
                ", {n} channels raised over interference (comb: {teeth} lines every {:.0} kHz{pulses})",
                spacing / 1e3
            ),
        };
        format!(
            "status: {:.0} s, DSP {:.0}% of a core, ring {}/{} blocks, dropped {} samples, recorded {}, active [{}]{}{}{}{}{}",
            wall,
            self.busy.as_secs_f64() / wall * 100.0,
            rs.blocks_written - rs.blocks_read,
            rs.capacity,
            self.dropped_before + rs.dropped_samples,
            self.recorded(scanner),
            active.join(", "),
            guards,
            raised,
            radio_note,
            clock_note,
            self.reopened_note(),
        )
    }
}
