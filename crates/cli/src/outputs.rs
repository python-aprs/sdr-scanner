//! Where scanner outputs go: the console, KISS clients, and the recorder
//! thread.

use crate::kiss::Kiss;
use anyhow::{Context, Result};
use scanner_core::scanner::Output;
use scanner_core::units::Hz;
use std::sync::mpsc::SyncSender;

pub struct Outputs {
    recorder: SyncSender<Vec<Output>>,
    kiss: Option<Kiss>,
    /// Print a line per transmission opened and packet decoded.
    log: bool,
}

impl Outputs {
    pub fn new(recorder: SyncSender<Vec<Output>>, kiss: Option<Kiss>, log: bool) -> Self {
        Outputs {
            recorder,
            kiss,
            log,
        }
    }

    /// Hand a batch on, leaving `out` empty. The recorder takes it last, by
    /// value, so audio is never copied.
    pub fn send(&self, out: &mut Vec<Output>) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        if self.log || self.kiss.is_some() {
            for o in out.iter() {
                match o {
                    Output::Open(s) if self.log => eprintln!(
                        "  open  {} {}",
                        Hz(s.slot.freq),
                        s.slot.name.as_deref().unwrap_or("")
                    ),
                    Output::Packet(p) => {
                        if self.log {
                            eprintln!("  packet {} {}", Hz(p.slot.freq), p.frame.tnc2());
                        }
                        if let Some(k) = &self.kiss {
                            k.send(&p.frame.raw);
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
