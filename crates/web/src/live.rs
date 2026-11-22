//! Live data for the web UI: the spectrum the DSP thread publishes, the
//! channel list, and the `/api/v1/live` WebSocket that streams both with the
//! status snapshot.
//!
//! Wire format (server to client):
//!
//! - Text `{"type":"hello", ...}` once, then `{"type":"status", ...}` (the
//!   [`LiveStatus`](crate::LiveStatus) fields) about once a second.
//! - Binary spectrum frames, little-endian: `u8` kind (1), three reserved
//!   bytes, `f64` start Hz, `f64` stop Hz, `f32` dB of value 0, `f32` dB per
//!   step of 255, then one `u8` per bin from start to stop.
//!
//! Client to server: text `{"type":"config","rate":2,"paused":false}`. The
//! spectrum rate is 1 to 5 frames a second (default 2); a paused client (a
//! hidden tab) gets status only, so it costs almost nothing.

use crate::{ApiError, AppState, Caller};
use axum::extract::State;
use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The latest display spectrum.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spectrum {
    /// Increases with each new frame.
    pub seq: u64,
    pub start_hz: f64,
    pub stop_hz: f64,
    /// dB per display bin, start to stop.
    pub db: Vec<f32>,
}

pub type SpectrumHandle = Arc<RwLock<Spectrum>>;

/// One watched frequency, as the channel list shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelInfo {
    pub freq_hz: f64,
    pub name: Option<String>,
    /// "channel", "repeater_output", "repeater_input" or "grid".
    pub kind: &'static str,
    pub mode: String,
    pub bandwidth_hz: f64,
    pub priority: bool,
    /// Shown on the spectrum but never recorded.
    pub locked_out: bool,
    /// For a repeater, the frequency of its other half (input or output)
    /// when that is watched too.
    pub pair_hz: Option<f64>,
}

pub type ChannelsHandle = Arc<RwLock<Vec<ChannelInfo>>>;

/// Display bins per spectrum frame.
pub const SPECTRUM_BINS: usize = 1024;
const HEADER_LEN: usize = 28;
const MAX_RATE: u32 = 5;
const DEFAULT_RATE: u32 = 2;
/// How often a connection that isn't public re-checks its login.
const RECHECK: Duration = Duration::from_secs(30);

/// Encode a spectrum frame (see the module docs).
pub fn encode_spectrum(s: &Spectrum) -> Vec<u8> {
    let (lo, hi) =
        s.db.iter()
            .filter(|v| v.is_finite())
            .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (0.0, 0.0) };
    // At least 20 dB of range so a flat band doesn't amplify noise.
    let span = (hi - lo).max(20.0);
    let step = span / 255.0;
    let mut out = Vec::with_capacity(HEADER_LEN + s.db.len());
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&s.start_hz.to_le_bytes());
    out.extend_from_slice(&s.stop_hz.to_le_bytes());
    out.extend_from_slice(&lo.to_le_bytes());
    out.extend_from_slice(&step.to_le_bytes());
    out.extend(s.db.iter().map(|&v| {
        if v.is_finite() {
            ((v - lo) / step).round().clamp(0.0, 255.0) as u8
        } else {
            0
        }
    }));
    out
}

/// Refuse a WebSocket opened by a page on another site. Browsers send
/// cookies with cross-site WebSocket handshakes unless SameSite stops them,
/// so this is a second lock on the same door.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Not a browser (scripts, tests): nothing to check.
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .trim_end_matches('/');
    !host.is_empty() && authority.eq_ignore_ascii_case(host)
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    kind: String,
    rate: Option<u32>,
    paused: Option<bool>,
}

pub(crate) async fn live_ws(
    State(s): State<AppState>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let caller = s.caller(&headers).await?;
    caller.require(s.public.live)?;
    if !same_origin(&headers) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "cross-site WebSocket refused".into(),
        ));
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(e) => return Ok(e.into_response()),
    };
    Ok(ws
        .max_message_size(4096)
        .on_upgrade(move |sock| async move {
            if let Err(e) = run(sock, s, headers, caller).await {
                // A closed tab is the usual reason; nothing to report.
                let _ = e;
            }
        }))
}

async fn run(
    mut sock: WebSocket,
    s: AppState,
    headers: HeaderMap,
    caller: Caller,
) -> Result<(), axum::Error> {
    let hello = serde_json::json!({
        "type": "hello",
        "role": caller.role,
        "bins": SPECTRUM_BINS,
        "max_rate": MAX_RATE,
        "rate": DEFAULT_RATE,
    });
    sock.send(Message::Text(hello.to_string().into())).await?;
    let mut rate = DEFAULT_RATE;
    let mut paused = false;
    let mut last_seq = u64::MAX;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = tokio::time::Instant::now();
    let mut next_spectrum = start;
    let mut next_status = start;
    let mut next_check = start + RECHECK;
    loop {
        tokio::select! {
            msg = sock.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => {
                        if let Ok(m) = serde_json::from_str::<ClientMsg>(&t)
                            && m.kind == "config"
                        {
                            if let Some(r) = m.rate {
                                rate = r.clamp(1, MAX_RATE);
                            }
                            if let Some(p) = m.paused {
                                paused = p;
                            }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Binary(_) => {
                        let _ = sock.send(Message::Close(None)).await;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                if !s.public.live && now >= next_check {
                    next_check = now + RECHECK;
                    let ok = match s.caller(&headers).await {
                        Ok(c) => c.require(false).is_ok(),
                        Err(_) => false,
                    };
                    if !ok {
                        let _ = sock.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
                if now >= next_status {
                    next_status = now + Duration::from_secs(1);
                    let snap = s.status.read().map(|g| g.clone()).unwrap_or_default();
                    let mut v = serde_json::to_value(&snap).unwrap_or_default();
                    v["type"] = "status".into();
                    sock.send(Message::Text(v.to_string().into())).await?;
                }
                if !paused && now >= next_spectrum {
                    next_spectrum = now + Duration::from_millis(1000 / rate as u64);
                    let frame = s
                        .spectrum
                        .read()
                        .ok()
                        .filter(|g| g.seq != last_seq && !g.db.is_empty())
                        .map(|g| {
                            last_seq = g.seq;
                            encode_spectrum(&g)
                        });
                    if let Some(f) = frame {
                        sock.send(Message::Binary(f.into())).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectrum_frames_round_trip_within_a_step() {
        let s = Spectrum {
            seq: 1,
            start_hz: 144e6,
            stop_hz: 148e6,
            db: vec![-100.0, -60.0, -99.5, f32::NAN, -80.0],
        };
        let f = encode_spectrum(&s);
        assert_eq!(f.len(), HEADER_LEN + 5);
        assert_eq!(f[0], 1);
        let start = f64::from_le_bytes(f[4..12].try_into().unwrap());
        let stop = f64::from_le_bytes(f[12..20].try_into().unwrap());
        let lo = f32::from_le_bytes(f[20..24].try_into().unwrap());
        let step = f32::from_le_bytes(f[24..28].try_into().unwrap());
        assert_eq!((start, stop), (144e6, 148e6));
        assert_eq!(lo, -100.0);
        assert!((step - 40.0 / 255.0).abs() < 1e-6);
        for (i, want) in [(0, -100.0), (1, -60.0), (2, -99.5), (4, -80.0)] {
            let got = lo + f[HEADER_LEN + i] as f32 * step;
            assert!(
                (got - want).abs() <= step / 2.0 + 1e-4,
                "{i}: {got} vs {want}"
            );
        }
        assert_eq!(f[HEADER_LEN + 3], 0, "NaN encodes as the floor");
    }

    #[test]
    fn websocket_origin_must_match_host() {
        let h = |origin: Option<&str>, host: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, host.parse().unwrap());
            if let Some(o) = origin {
                m.insert(header::ORIGIN, o.parse().unwrap());
            }
            same_origin(&m)
        };
        assert!(h(None, "scanner.local:8080"));
        assert!(h(Some("http://scanner.local:8080"), "scanner.local:8080"));
        assert!(h(Some("https://Scanner.example"), "scanner.example"));
        assert!(!h(Some("https://evil.example"), "scanner.example"));
        assert!(!h(Some("https://scanner.example.evil"), "scanner.example"));
        assert!(!h(Some("null"), "scanner.example"));
    }
}
