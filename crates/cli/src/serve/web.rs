//! The link between the DSP loop and the web interface: the handles the web
//! server reads (status, spectrum, live audio), and starting the server.

use super::ServeOptions;
use super::channels::Channels;
use anyhow::{Context, Result};
use scanner_core::scanner::{LiveAudio, Scanner};
use scanner_sources::SourceInfo;
use scanner_web::{SpectrumHandle, StatusHandle};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

pub struct Web {
    /// Whether the web server is running.
    pub on: bool,
    status: StatusHandle,
    spectrum: SpectrumHandle,
    /// The spectrum's span (offsets from the centre, Hz).
    spec: (f64, f64),
    audio: Arc<scanner_web::audio::AudioHub>,
    live_tx: Option<Sender<Vec<LiveAudio>>>,
    pub egress: Option<Arc<scanner_web::egress::Egress>>,
    /// Commands from the web server.
    pub control: Receiver<scanner_web::Control>,
}

impl Web {
    /// Start the web server if `o.http` is set (the handles exist either
    /// way).
    pub fn start(
        o: &ServeOptions,
        info: &SourceInfo,
        channels: &Channels,
        storage: Option<Arc<scanner_store::retention::DiskUsage>>,
    ) -> Result<Web> {
        let (control_tx, control) = std::sync::mpsc::channel();
        // The spectrum shows the profile's span, inside what the radio
        // delivers cleanly.
        let half = info.sample_rate * 0.45;
        let mut web = Web {
            on: o.http.is_some(),
            status: Default::default(),
            spectrum: Default::default(),
            spec: (
                (o.span.0 - info.center_freq).max(-half),
                (o.span.1 - info.center_freq).min(half),
            ),
            audio: scanner_web::audio::AudioHub::new(),
            live_tx: None,
            egress: None,
            control,
        };
        let Some(addr) = o.http else {
            return Ok(web);
        };
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
            web.status.clone(),
            o.out.to_owned(),
            o.public,
            Some(Arc::new(auth)),
        );
        state.secure_cookies = !o.insecure_cookies;
        state.spectrum = web.spectrum.clone();
        state.channels = channels.list.clone();
        state.settings = Some(channels.settings.clone());
        state.control = Some(control_tx);
        web.live_tx = Some(
            web.audio
                .spawn_encoder()
                .context("starting the live audio thread")?,
        );
        state.audio = Some(web.audio.clone());
        state.storage = storage;
        let eg = Arc::new(scanner_web::egress::Egress::new(
            o.anon_egress_bytes,
            Some(channels.settings.clone()),
        ));
        state.egress = Some(eg.clone());
        web.egress = Some(eg);
        scanner_web::spawn(addr, state)
            .with_context(|| format!("starting the web API on {addr}"))?;
        eprintln!("web interface on http://{addr}/");
        Ok(web)
    }

    /// Whether anyone is listening live (the scanner skips live audio
    /// otherwise).
    pub fn live_wanted(&self) -> bool {
        self.live_tx.is_some() && self.audio.wanted()
    }

    /// Pass the scanner's live audio to the encoder.
    pub fn send_live(&self, scanner: &mut Scanner) {
        if let Some(tx) = &self.live_tx {
            let mut live = Vec::new();
            scanner.take_live(&mut live);
            if !live.is_empty() {
                let _ = tx.send(live);
            }
        }
    }

    pub fn publish_status(&self, status: scanner_web::LiveStatus) {
        if let Ok(mut g) = self.status.write() {
            *g = status;
        }
    }

    /// Publish a spectrum frame (when the web server is on).
    pub fn publish_spectrum(&self, scanner: &Scanner, center_freq: f64) {
        let (lo, hi) = self.spec;
        if !self.on || hi <= lo {
            return;
        }
        let db = scanner
            .detector()
            .spectrum_db(lo, hi, scanner_web::live::SPECTRUM_BINS);
        if let Ok(mut g) = self.spectrum.write() {
            g.seq += 1;
            g.start_hz = center_freq + lo;
            g.stop_hz = center_freq + hi;
            g.db = db;
        }
    }
}
