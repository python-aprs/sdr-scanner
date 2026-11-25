//! Turns received headers and frames into calls: the header's callsigns
//! (or, for a late entry, the copy the slow data carries), the slow data
//! message, and the voice frames.

use super::frame::{self, SUPERFRAME, SlowData};
use super::header::Header;
use super::rx::{FRAME_SAMPLES, RxEvent};
use crate::dmr::burst::AmbeFrame;

/// What is known about a call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallInfo {
    /// From the header; `None` until a late entry's slow data repeats it.
    pub header: Option<Header>,
    /// The 20-character message the radio sends in its slow data.
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallEvent {
    /// `at`: 48 kHz sample of the call's header or first frame.
    Start {
        at: u64,
        info: CallInfo,
    },
    Voice {
        frame: AmbeFrame,
    },
    /// `at`: 48 kHz sample just past the call's last frame.
    End {
        at: u64,
        info: CallInfo,
    },
}

struct Call {
    info: CallInfo,
    last: u64,
    lock: u32,
    slow: SlowData,
}

#[derive(Default)]
pub struct Calls {
    call: Option<Call>,
    pub frames: u64,
}

impl Calls {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, e: &RxEvent, out: &mut Vec<CallEvent>) {
        match e {
            RxEvent::Header { at, lock, bytes } => {
                self.end_all(out);
                self.start(*at, *lock, Some(Header::from_bytes(bytes)), out);
            }
            RxEvent::Frame {
                at,
                lock,
                index,
                bits,
            } => {
                self.frames += 1;
                if self.call.as_ref().is_some_and(|c| c.lock != *lock) {
                    self.end_all(out);
                }
                if self.call.is_none() {
                    self.start(*at, *lock, None, out);
                }
                let c = self.call.as_mut().unwrap();
                c.last = at + FRAME_SAMPLES;
                if index % SUPERFRAME as u64 == 0 {
                    c.slow.sync();
                } else {
                    c.slow.push(frame::slow_data(bits));
                    if c.info.message.is_none() {
                        c.info.message = c.slow.message();
                    }
                    if c.info.header.is_none() {
                        c.info.header = c.slow.header();
                    }
                }
                out.push(CallEvent::Voice {
                    frame: frame::voice(bits),
                });
            }
            RxEvent::End { lock, .. } => {
                if self.call.as_ref().is_some_and(|c| c.lock == *lock) {
                    self.end_all(out);
                }
            }
        }
    }

    fn start(&mut self, at: u64, lock: u32, header: Option<Header>, out: &mut Vec<CallEvent>) {
        let info = CallInfo {
            header,
            message: None,
        };
        out.push(CallEvent::Start {
            at,
            info: info.clone(),
        });
        self.call = Some(Call {
            info,
            last: at,
            lock,
            slow: SlowData::default(),
        });
    }

    pub fn end_all(&mut self, out: &mut Vec<CallEvent>) {
        if let Some(c) = self.call.take() {
            out.push(CallEvent::End {
                at: c.last,
                info: c.info,
            });
        }
    }
}
