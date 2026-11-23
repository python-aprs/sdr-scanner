//! Live listening: the DSP thread's live audio, encoded to Opus only while
//! someone listens, and streamed over the `/api/v1/audio` WebSocket.
//!
//! Wire format (server to client):
//!
//! - Text `{"type":"start","id":7,"freq_hz":146520000.0,"priority":false}`
//!   when a transmission this listener gets begins, and `{"type":"end","id":7}`
//!   when it ends.
//! - Binary audio frames, little-endian: `u8` kind (2), three reserved
//!   bytes, `u32` transmission id, `u32` frame sequence number, then one
//!   Opus packet of 40 ms (mono, at the transmission's audio rate, 8 kHz by
//!   default; about 12 kb/s).
//!
//! Client to server: text `{"type":"listen","mode":"follow"}`, where mode is
//! `off`, `follow` (one transmission at a time, priority channels first,
//! like a scanner), `all` (everything, mixed by the browser) or `freqs`
//! with `"freqs": [146520000, ...]` (those channels, +/- 3 kHz).
//! Squelched channels send nothing, so silence costs no bandwidth.

use crate::egress::Sock;
use crate::{ApiError, AppState, Caller};
use axum::extract::State;
use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use scanner_core::scanner::LiveAudio;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// Opus frame length.
pub const FRAME_MS: usize = 40;
/// Opus bitrate for 8 kHz narrowband voice.
pub const BITRATE: i32 = 12_000;
const HEADER_LEN: usize = 12;
const RECHECK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub enum HubMsg {
    Start { id: u64, freq: f64, priority: bool },
    Frame { id: u64, seq: u32, packet: Vec<u8> },
    End { id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Stream {
    freq: f64,
    priority: bool,
    /// Order of arrival, for "follow" mode.
    order: u64,
}

/// Fan-out point between the encoder thread and the listeners.
pub struct AudioHub {
    listeners: AtomicUsize,
    tx: broadcast::Sender<Arc<HubMsg>>,
    active: Mutex<HashMap<u64, Stream>>,
    next_order: AtomicUsize,
}

impl AudioHub {
    pub fn new() -> Arc<Self> {
        Arc::new(AudioHub {
            listeners: AtomicUsize::new(0),
            tx: broadcast::channel(512).0,
            active: Mutex::new(HashMap::new()),
            next_order: AtomicUsize::new(0),
        })
    }

    /// Whether anyone is listening (the DSP thread produces live audio only
    /// then).
    pub fn wanted(&self) -> bool {
        self.listeners.load(Ordering::Relaxed) > 0
    }

    fn publish(&self, m: HubMsg) {
        {
            let mut a = self.active.lock().unwrap_or_else(|p| p.into_inner());
            match &m {
                HubMsg::Start { id, freq, priority } => {
                    let order = self.next_order.fetch_add(1, Ordering::Relaxed) as u64;
                    a.insert(
                        *id,
                        Stream {
                            freq: *freq,
                            priority: *priority,
                            order,
                        },
                    );
                }
                HubMsg::End { id } => {
                    a.remove(id);
                }
                HubMsg::Frame { .. } => {}
            }
        }
        // No receivers is fine.
        let _ = self.tx.send(Arc::new(m));
    }

    /// Start the encoder thread. Send it the DSP thread's live audio.
    pub fn spawn_encoder(
        self: &Arc<Self>,
    ) -> std::io::Result<std::sync::mpsc::Sender<Vec<LiveAudio>>> {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<LiveAudio>>();
        let hub = self.clone();
        std::thread::Builder::new()
            .name("live-audio".into())
            .spawn(move || {
                let mut enc = Encoders::default();
                while let Ok(batch) = rx.recv() {
                    for item in batch {
                        enc.handle(item, &mut |m| hub.publish(m));
                    }
                }
            })?;
        Ok(tx)
    }
}

struct Encoding {
    enc: opus::Encoder,
    frame: usize,
    buf: Vec<f32>,
    seq: u32,
}

/// One Opus encoder per transmission.
#[derive(Default)]
pub struct Encoders {
    open: HashMap<u64, Encoding>,
}

impl Encoders {
    pub fn handle(&mut self, item: LiveAudio, out: &mut impl FnMut(HubMsg)) {
        match item {
            LiveAudio::Start {
                id,
                freq,
                priority,
                audio_rate,
            } => {
                let Ok(mut enc) =
                    opus::Encoder::new(audio_rate, opus::Channels::Mono, opus::Application::Voip)
                else {
                    // Not an Opus rate (8, 12, 16, 24 or 48 kHz): not offered live.
                    return;
                };
                let _ = enc.set_bitrate(opus::Bitrate::Bits(BITRATE));
                self.open.insert(
                    id,
                    Encoding {
                        enc,
                        frame: audio_rate as usize * FRAME_MS / 1000,
                        buf: Vec::new(),
                        seq: 0,
                    },
                );
                out(HubMsg::Start { id, freq, priority });
            }
            LiveAudio::Audio { id, samples } => {
                if let Some(e) = self.open.get_mut(&id) {
                    e.buf.extend_from_slice(&samples);
                    e.drain(id, false, out);
                }
            }
            LiveAudio::End { id } => {
                if let Some(mut e) = self.open.remove(&id) {
                    e.drain(id, true, out);
                    out(HubMsg::End { id });
                }
            }
        }
    }
}

impl Encoding {
    fn drain(&mut self, id: u64, last: bool, out: &mut impl FnMut(HubMsg)) {
        if last && !self.buf.is_empty() {
            let pad = self.frame - self.buf.len() % self.frame;
            if pad < self.frame {
                self.buf.extend(std::iter::repeat_n(0.0, pad));
            }
        }
        let mut packet = [0u8; 1500];
        let mut used = 0;
        while self.buf.len() - used >= self.frame {
            if let Ok(n) = self
                .enc
                .encode_float(&self.buf[used..used + self.frame], &mut packet)
            {
                out(HubMsg::Frame {
                    id,
                    seq: self.seq,
                    packet: packet[..n].to_vec(),
                });
            }
            self.seq = self.seq.wrapping_add(1);
            used += self.frame;
        }
        self.buf.drain(..used);
    }
}

/// Encode an audio frame message (see the module docs).
pub fn encode_frame(id: u64, seq: u32, packet: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + packet.len());
    v.extend_from_slice(&[2, 0, 0, 0]);
    v.extend_from_slice(&(id as u32).to_le_bytes());
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(packet);
    v
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Off,
    Follow,
    All,
    Freqs(Vec<f64>),
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    kind: String,
    mode: Option<String>,
    freqs: Option<Vec<f64>>,
}

/// Which transmissions one listener gets.
struct Selector {
    mode: Mode,
    /// Streams this listener has been told about (sent "start").
    sending: Vec<u64>,
}

impl Selector {
    fn wants(&self, s: &Stream) -> bool {
        match &self.mode {
            Mode::Off => false,
            Mode::All => true,
            Mode::Follow => false, // decided by `follow_pick`
            Mode::Freqs(f) => f.iter().any(|&x| (x - s.freq).abs() <= 3e3),
        }
    }
}

/// The transmission "follow" mode should play: priority channels first,
/// then the one that started first.
fn follow_pick(active: &HashMap<u64, Stream>) -> Option<u64> {
    active
        .iter()
        .min_by_key(|(_, s)| (!s.priority, s.order))
        .map(|(id, _)| *id)
}

struct ListenerGuard<'a> {
    hub: &'a AudioHub,
    counted: bool,
}

impl ListenerGuard<'_> {
    fn set(&mut self, on: bool) {
        if on != self.counted {
            if on {
                self.hub.listeners.fetch_add(1, Ordering::Relaxed);
            } else {
                self.hub.listeners.fetch_sub(1, Ordering::Relaxed);
            }
            self.counted = on;
        }
    }
}

impl Drop for ListenerGuard<'_> {
    fn drop(&mut self) {
        self.set(false);
    }
}

pub(crate) async fn audio_ws(
    State(s): State<AppState>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let caller = s.caller(&headers).await?;
    caller.require(s.public.audio)?;
    if s.egress_level(caller.role) == crate::egress::Level::Spent {
        return Err(crate::budget_spent());
    }
    if !crate::live::same_origin(&headers) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "cross-site WebSocket refused".into(),
        ));
    }
    let Some(hub) = s.audio.clone() else {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "live audio is not available on this scanner".into(),
        ));
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(e) => return Ok(e.into_response()),
    };
    Ok(ws
        .max_message_size(16 * 1024)
        .on_upgrade(move |sock| async move {
            let _ = run(sock, s, hub, headers, caller).await;
        }))
}

async fn run(
    sock: WebSocket,
    s: AppState,
    hub: Arc<AudioHub>,
    headers: HeaderMap,
    caller: Caller,
) -> Result<(), axum::Error> {
    let mut sock = Sock::new(sock, s.meter(caller.role));
    let mut rx = hub.tx.subscribe();
    let mut guard = ListenerGuard {
        hub: &hub,
        counted: false,
    };
    let mut sel = Selector {
        mode: Mode::Off,
        sending: Vec::new(),
    };
    let mut follow: Option<u64> = None;
    let mut check = tokio::time::interval(RECHECK);
    check.tick().await;
    sock.send(Message::Text(
        serde_json::json!({"type": "hello", "frame_ms": FRAME_MS})
            .to_string()
            .into(),
    ))
    .await?;
    loop {
        tokio::select! {
            msg = sock.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<ClientMsg>(&t) else { continue };
                        if m.kind != "listen" {
                            continue;
                        }
                        sel.mode = match m.mode.as_deref() {
                            Some("follow") => Mode::Follow,
                            Some("all") => Mode::All,
                            Some("freqs") => Mode::Freqs(
                                m.freqs.unwrap_or_default().into_iter().take(32).collect(),
                            ),
                            _ => Mode::Off,
                        };
                        guard.set(sel.mode != Mode::Off);
                        // Re-evaluate what this listener gets right away.
                        let active = hub.active.lock().map(|a| a.clone()).unwrap_or_default();
                        let want: Vec<u64> = if sel.mode == Mode::Follow {
                            follow = follow_pick(&active);
                            follow.into_iter().collect()
                        } else {
                            follow = None;
                            active.iter().filter(|(_, st)| sel.wants(st)).map(|(id, _)| *id).collect()
                        };
                        for id in sel.sending.clone() {
                            if !want.contains(&id) {
                                send_end(&mut sock, id).await?;
                            }
                        }
                        sel.sending.retain(|id| want.contains(id));
                        for id in want {
                            if !sel.sending.contains(&id) {
                                if let Some(st) = active.get(&id) {
                                    send_start(&mut sock, id, st).await?;
                                }
                                sel.sending.push(id);
                            }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            m = rx.recv() => {
                let m = match m {
                    Ok(m) => m,
                    // Fell behind: skip what was missed.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                };
                match &*m {
                    HubMsg::Start { id, freq, priority } => {
                        let st = Stream { freq: *freq, priority: *priority, order: 0 };
                        let take = match sel.mode {
                            Mode::Follow => {
                                // Idle, or a priority channel cuts in.
                                let cut_in = *priority && follow.is_some_and(|f| {
                                    !hub.active.lock().map(|a| a.get(&f).is_some_and(|s| s.priority)).unwrap_or(false)
                                });
                                if follow.is_none() || cut_in {
                                    if let Some(old) = follow.take() {
                                        send_end(&mut sock, old).await?;
                                        sel.sending.retain(|x| *x != old);
                                    }
                                    follow = Some(*id);
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => sel.wants(&st),
                        };
                        if take {
                            send_start(&mut sock, *id, &st).await?;
                            sel.sending.push(*id);
                        }
                    }
                    HubMsg::Frame { id, seq, packet } => {
                        if sel.sending.contains(id) {
                            if sock.meter.level() == crate::egress::Level::Spent {
                                // Out of data for the month: say so and stop.
                                let v = serde_json::json!({"type": "budget", "level": "spent"});
                                let _ = sock.send(Message::Text(v.to_string().into())).await;
                                let _ = sock.send(Message::Close(None)).await;
                                return Ok(());
                            }
                            sock.send(Message::Binary(encode_frame(*id, *seq, packet).into())).await?;
                        }
                    }
                    HubMsg::End { id } => {
                        if sel.sending.contains(id) {
                            send_end(&mut sock, *id).await?;
                            sel.sending.retain(|x| x != id);
                        }
                        if follow == Some(*id) {
                            // Move on to whatever else is on the air.
                            let active = hub.active.lock().map(|a| a.clone()).unwrap_or_default();
                            follow = follow_pick(&active);
                            if let Some(next) = follow
                                && let Some(st) = active.get(&next)
                            {
                                send_start(&mut sock, next, st).await?;
                                sel.sending.push(next);
                            }
                        }
                    }
                }
            }
            _ = check.tick() => {
                if !s.public.audio {
                    let ok = match s.caller(&headers).await {
                        Ok(c) => c.require(false).is_ok(),
                        Err(_) => false,
                    };
                    if !ok {
                        let _ = sock.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn send_start(sock: &mut Sock, id: u64, st: &Stream) -> Result<(), axum::Error> {
    let v = serde_json::json!({
        "type": "start", "id": id as u32, "freq_hz": st.freq, "priority": st.priority,
    });
    sock.send(Message::Text(v.to_string().into())).await
}

async fn send_end(sock: &mut Sock, id: u64) -> Result<(), axum::Error> {
    let v = serde_json::json!({"type": "end", "id": id as u32});
    sock.send(Message::Text(v.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoders_frame_audio_and_flush_at_the_end() {
        let mut e = Encoders::default();
        let mut got = Vec::new();
        e.handle(
            LiveAudio::Start {
                id: 3,
                freq: 146.52e6,
                priority: false,
                audio_rate: 8000,
            },
            &mut |m| got.push(m),
        );
        // 0.5 s of a 1 kHz tone in uneven pieces.
        let tone: Vec<f32> = (0..4000)
            .map(|i| 0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 8000.0).sin())
            .collect();
        for piece in tone.chunks(777) {
            e.handle(
                LiveAudio::Audio {
                    id: 3,
                    samples: piece.to_vec(),
                },
                &mut |m| got.push(m),
            );
        }
        e.handle(LiveAudio::End { id: 3 }, &mut |m| got.push(m));
        assert!(matches!(got[0], HubMsg::Start { id: 3, .. }));
        assert!(matches!(got.last(), Some(HubMsg::End { id: 3 })));
        let frames: Vec<&Vec<u8>> = got
            .iter()
            .filter_map(|m| match m {
                HubMsg::Frame { packet, .. } => Some(packet),
                _ => None,
            })
            .collect();
        // 500 ms in 40 ms frames, the last one padded.
        assert_eq!(frames.len(), 13);
        let bytes: usize = frames.iter().map(|p| p.len()).sum();
        // ~12 kb/s: about 750 bytes for half a second (allow slack).
        assert!(bytes < 1200, "{bytes} bytes");
        // Decodes back to a tone.
        let mut dec = opus::Decoder::new(8000, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0f32; 320];
        let mut energy = 0.0;
        for p in &frames[2..10] {
            let n = dec.decode_float(p, &mut pcm, false).unwrap();
            assert_eq!(n, 320);
            energy += pcm.iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy / (8.0 * 320.0) > 0.05, "decoded energy {energy}");
        // Audio for an unknown or unsupported stream is ignored.
        got.clear();
        e.handle(
            LiveAudio::Start {
                id: 4,
                freq: 1.0,
                priority: false,
                audio_rate: 11025,
            },
            &mut |m| got.push(m),
        );
        e.handle(
            LiveAudio::Audio {
                id: 4,
                samples: vec![0.0; 1000],
            },
            &mut |m| got.push(m),
        );
        assert!(got.is_empty());
    }

    #[test]
    fn follow_prefers_priority_then_the_earliest() {
        let mut a = HashMap::new();
        assert_eq!(follow_pick(&a), None);
        a.insert(
            1,
            Stream {
                freq: 1.0,
                priority: false,
                order: 5,
            },
        );
        a.insert(
            2,
            Stream {
                freq: 2.0,
                priority: false,
                order: 3,
            },
        );
        assert_eq!(follow_pick(&a), Some(2));
        a.insert(
            3,
            Stream {
                freq: 3.0,
                priority: true,
                order: 9,
            },
        );
        assert_eq!(follow_pick(&a), Some(3));
    }

    #[test]
    fn frames_carry_id_and_sequence() {
        let f = encode_frame(7, 42, &[9, 8, 7]);
        assert_eq!(&f[..4], &[2, 0, 0, 0]);
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(f[8..12].try_into().unwrap()), 42);
        assert_eq!(&f[12..], &[9, 8, 7]);
    }
}
