//! Receive-only KISS over TCP, so APRS software (Xastir, YAAC, the
//! python-aprs tools) can use the scanner as a TNC.

use anyhow::{Context, Result};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Every connected client gets every decoded frame; anything clients send
/// is ignored. Frames go through a channel so a slow client never holds up
/// the DSP thread.
pub struct Kiss(Sender<Vec<u8>>);

impl Kiss {
    pub fn start(addr: SocketAddr) -> Result<Kiss> {
        let listener =
            TcpListener::bind(addr).with_context(|| format!("KISS: listening on {addr}"))?;
        let clients: Arc<Mutex<Vec<TcpStream>>> = Default::default();
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
        Ok(Kiss(tx))
    }

    /// Queue one frame (raw AX.25, no FCS) for all clients.
    pub fn send(&self, raw: &[u8]) {
        let _ = self.0.send(raw.to_vec());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn kiss_escapes_frame_bytes() {
        assert_eq!(
            super::kiss_frame(&[0x01, 0xc0, 0xdb, 0x02]),
            vec![0xc0, 0x00, 0x01, 0xdb, 0xdc, 0xdb, 0xdd, 0x02, 0xc0]
        );
    }
}
