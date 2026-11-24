//! Where scanner outputs go: the console, KISS clients, the web UI's packet
//! feed, and the recorder thread.

use crate::kiss::Kiss;
use anyhow::{Context, Result};
use scanner_core::demod::CallInfo;
use scanner_core::scanner::Output;
use scanner_core::units::Hz;
use std::sync::mpsc::SyncSender;

pub struct Outputs {
    recorder: SyncSender<Vec<Output>>,
    kiss: Option<Kiss>,
    /// Print a line per transmission opened and packet decoded.
    log: bool,
    /// Decoded packets for the Live page.
    packets: Option<scanner_web::PacketFeed>,
}

impl Outputs {
    pub fn new(recorder: SyncSender<Vec<Output>>, kiss: Option<Kiss>, log: bool) -> Self {
        Outputs {
            recorder,
            kiss,
            log,
            packets: None,
        }
    }

    pub fn with_packets(mut self, feed: scanner_web::PacketFeed) -> Self {
        self.packets = Some(feed);
        self
    }

    /// Hand a batch on, leaving `out` empty. The recorder takes it last, by
    /// value, so audio is never copied.
    pub fn send(&self, out: &mut Vec<Output>) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        if self.log || self.kiss.is_some() || self.packets.is_some() {
            for o in out.iter() {
                match o {
                    Output::Open(s) if self.log => eprintln!(
                        "  open  {} {}",
                        Hz(s.slot.freq),
                        s.slot.name.as_deref().unwrap_or("")
                    ),
                    Output::Close(e) if self.log => {
                        if let Some(CallInfo::Ysf(d)) = &e.digital {
                            eprintln!(
                                "  ysf   {} -> {}{}{} {:.1} s",
                                d.src.as_deref().unwrap_or("?"),
                                d.dst.as_deref().unwrap_or("?"),
                                d.dg_id.map(|g| format!(" DG-ID {g}")).unwrap_or_default(),
                                d.mode().map(|m| format!(" {m}")).unwrap_or_default(),
                                e.signal_secs
                            );
                        }
                        if let Some(CallInfo::Dmr(d)) = &e.digital {
                            let who = match d.lc {
                                Some(l) => format!(
                                    "{} {} -> {}",
                                    d.call_type().unwrap_or("call"),
                                    l.src,
                                    l.dst
                                ),
                                None => "call (ids not heard)".into(),
                            };
                            eprintln!(
                                "  dmr   {}{}{}{} {:.1} s",
                                d.timeslot.map(|t| format!("TS{t} ")).unwrap_or_default(),
                                who,
                                d.alias
                                    .as_deref()
                                    .map(|a| format!(" \"{a}\""))
                                    .unwrap_or_default(),
                                if d.encrypted { " encrypted" } else { "" },
                                e.signal_secs
                            );
                        }
                    }
                    Output::Packet(p) => {
                        if self.log {
                            eprintln!("  packet {} {}", Hz(p.slot.freq), p.frame.tnc2());
                        }
                        if let Some(k) = &self.kiss {
                            k.send(&p.frame.raw);
                        }
                        // Only live viewers get these, so skip the work when
                        // there are none.
                        if let Some(f) = &self.packets
                            && f.receiver_count() > 0
                        {
                            let _ = f.send(std::sync::Arc::new(
                                scanner_store::index::PacketRow::from_decoded(p),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.recorder
            .send(std::mem::take(out))
            .ok()
            .context("recorder thread stopped")
    }
}
