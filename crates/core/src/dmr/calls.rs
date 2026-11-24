//! Turns received bursts into voice calls, one stream per timeslot: who is
//! talking (link control from the voice header, the embedded signalling for
//! late entry, the terminator), talker alias, privacy, and the voice frames.

use super::burst::{self, AmbeFrame, DataType, Source};
use super::fec;
use super::lc::{self, Lc, TalkerAlias};
use super::rx::RxBurst;

/// What is known about a call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallInfo {
    /// Timeslot 1 or 2; `None` for a radio heard directly (simplex), whose
    /// bursts don't say.
    pub timeslot: Option<u8>,
    pub color_code: Option<u8>,
    /// Group or private call, source and destination ids.
    pub lc: Option<Lc>,
    /// The call is encrypted (privacy bit, PI header or EMB).
    pub encrypted: bool,
    pub alias: Option<String>,
    /// Heard from a repeater (true) or a radio (false).
    pub repeater: bool,
}

impl CallInfo {
    pub fn call_type(&self) -> Option<&'static str> {
        self.lc.map(|l| match l.flco {
            lc::FLCO_GROUP => "group",
            lc::FLCO_PRIVATE => "private",
            _ => "other",
        })
    }

    pub fn emergency(&self) -> bool {
        self.lc.is_some_and(|l| l.emergency())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallEvent {
    /// `at`: 48 kHz sample of the call's first voice burst.
    Start {
        key: u8,
        at: u64,
        info: CallInfo,
    },
    Voice {
        key: u8,
        frames: [AmbeFrame; 3],
    },
    /// `at`: 48 kHz sample just past the call's last burst.
    End {
        key: u8,
        at: u64,
        info: CallInfo,
    },
}

#[derive(Debug)]
struct Slot {
    /// Position in the voice superframe of the last voice burst (0 = A).
    voice_seq: Option<u8>,
    /// Superframes in a row whose burst A had no sync.
    missed_a: u8,
    call: Option<Call>,
    /// Link control from a voice header waiting for its voice.
    header: Option<(Lc, u64)>,
    pi_header: bool,
    embedded: [u8; 128],
    embedded_have: u8,
    alias: TalkerAlias,
    timeslot: Option<u8>,
    color_code: Option<u8>,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            voice_seq: None,
            missed_a: 0,
            call: None,
            header: None,
            pi_header: false,
            embedded: [0; 128],
            embedded_have: 0,
            alias: TalkerAlias::default(),
            timeslot: None,
            color_code: None,
        }
    }
}

#[derive(Debug)]
struct Call {
    info: CallInfo,
    /// End of the last burst.
    last: u64,
}

pub struct Calls {
    slots: [Slot; 2],
    lock: u32,
    /// Votes for which frame parity is timeslot 1 on a repeater.
    phase: i32,
    pub bursts: u64,
    pub voice_bursts: u64,
}

const BURST_SAMPLES: u64 = (burst::BURST_SYMBOLS * super::rx::SPS) as u64;
/// A header older than this (two frames) belongs to no call.
const HEADER_FRESH: u64 = 4 * (burst::FRAME_SYMBOLS * super::rx::SPS) as u64;

impl Default for Calls {
    fn default() -> Self {
        Self::new()
    }
}

impl Calls {
    pub fn new() -> Self {
        Calls {
            slots: Default::default(),
            lock: 0,
            phase: 0,
            bursts: 0,
            voice_bursts: 0,
        }
    }

    pub fn push(&mut self, b: &RxBurst, out: &mut Vec<CallEvent>) {
        self.bursts += 1;
        if b.lock != self.lock {
            // A new lock: the frame grid moved, so slots start over.
            self.end_all(out);
            self.lock = b.lock;
            self.phase = 0;
            for s in &mut self.slots {
                *s = Slot::default();
            }
        }
        let parity = (b.frame & 1) as u8;
        let key = match b.source {
            Source::Base => {
                // The CACH says which timeslot follows; vote so one bad CACH
                // can't swap the slots.
                let want = if burst::cach_timeslot(&b.cach) ^ parity == 0 {
                    1
                } else {
                    -1
                };
                self.phase = (self.phase + want).clamp(-8, 8);
                parity ^ (self.phase < 0) as u8
            }
            _ => parity,
        };
        let s = &mut self.slots[key as usize];
        s.timeslot = match (b.source, b.sync) {
            (Source::Base, _) => Some(key + 1),
            (_, Some(sy)) => match sy.source {
                Source::Direct(t) => Some(t),
                _ => None,
            },
            _ => s.timeslot,
        };
        let repeater = b.source == Source::Base;
        let end = b.start + BURST_SAMPLES;
        match b.sync {
            Some(sy) if sy.voice => {
                s.voice_seq = Some(0);
                s.missed_a = 0;
                self.voice(key, b, repeater, out);
            }
            Some(_) => self.data(key, b, out),
            None => match s.voice_seq {
                Some(q) if q < 5 => {
                    s.voice_seq = Some(q + 1);
                    if let Some(e) = burst::emb(&b.bits) {
                        s.color_code = Some(e.cc);
                        if e.pi
                            && let Some(c) = &mut s.call
                        {
                            c.info.encrypted = true;
                        }
                        Self::embedded(s, e.lcss, &burst::embedded_bits(&b.bits));
                    }
                    self.voice(key, b, repeater, out);
                }
                Some(_) if s.call.is_some() && s.missed_a < 1 => {
                    // Burst A whose sync didn't make it: carry on by position.
                    s.voice_seq = Some(0);
                    s.missed_a += 1;
                    self.voice(key, b, repeater, out);
                }
                Some(_) => {
                    s.voice_seq = None;
                    Self::end(key, s, end, out);
                }
                None => {}
            },
        }
    }

    fn voice(&mut self, key: u8, b: &RxBurst, repeater: bool, out: &mut Vec<CallEvent>) {
        self.voice_bursts += 1;
        let s = &mut self.slots[key as usize];
        if s.call.is_none() {
            let header = s
                .header
                .take()
                .filter(|(_, at)| b.start.saturating_sub(*at) <= HEADER_FRESH)
                .map(|(l, _)| l);
            let info = CallInfo {
                timeslot: s.timeslot,
                color_code: s.color_code,
                encrypted: s.pi_header || header.is_some_and(|l| l.encrypted()),
                lc: header,
                alias: None,
                repeater,
            };
            s.pi_header = false;
            s.alias = TalkerAlias::default();
            out.push(CallEvent::Start {
                key,
                at: b.start,
                info: info.clone(),
            });
            s.call = Some(Call {
                info,
                last: b.start,
            });
        }
        let c = s.call.as_mut().unwrap();
        c.last = b.start + BURST_SAMPLES;
        c.info.timeslot = c.info.timeslot.or(s.timeslot);
        c.info.color_code = c.info.color_code.or(s.color_code);
        c.info.alias = s.alias.text().or(c.info.alias.take());
        out.push(CallEvent::Voice {
            key,
            frames: burst::ambe_frames(&b.bits),
        });
    }

    /// Collect embedded signalling: LCSS 1 starts it, 3 continues and 2
    /// ends it (bursts B to E).
    fn embedded(s: &mut Slot, lcss: u8, bits: &[u8; 32]) {
        match lcss {
            1 => {
                s.embedded[..32].copy_from_slice(bits);
                s.embedded_have = 1;
            }
            3 if (1..=2).contains(&s.embedded_have) => {
                let i = s.embedded_have as usize;
                s.embedded[i * 32..i * 32 + 32].copy_from_slice(bits);
                s.embedded_have += 1;
            }
            2 if s.embedded_have == 3 => {
                s.embedded[96..].copy_from_slice(bits);
                s.embedded_have = 0;
                if let Some(raw) = fec::embedded_decode(&s.embedded) {
                    let l = Lc::from_bytes(&raw);
                    if l.is_call() {
                        if let Some(c) = &mut s.call {
                            if c.info.lc.is_none() {
                                c.info.lc = Some(l);
                            }
                            c.info.encrypted |= l.encrypted();
                        }
                    } else if (lc::FLCO_ALIAS_HEADER..=lc::FLCO_ALIAS_BLOCK3).contains(&l.flco) {
                        s.alias.add(&raw);
                    }
                }
            }
            _ => s.embedded_have = 0,
        }
    }

    fn data(&mut self, key: u8, b: &RxBurst, out: &mut Vec<CallEvent>) {
        let s = &mut self.slots[key as usize];
        let Some((cc, dt)) = burst::slot_type(&b.bits) else {
            return;
        };
        s.color_code = Some(cc);
        let end = b.start + BURST_SAMPLES;
        let (bits, _) = fec::bptc_decode(&burst::info_bits(&b.bits));
        match dt {
            DataType::VoiceLcHeader => {
                if let Some(l) = lc::full_lc(&bits, lc::MASK_VOICE_HEADER) {
                    if s.call.as_ref().is_some_and(|c| c.info.lc != Some(l)) {
                        Self::end(key, s, end, out);
                    }
                    if s.call.is_none() {
                        s.header = Some((l, b.start));
                    }
                }
            }
            DataType::PiHeader => {
                s.pi_header = true;
                if let Some(c) = &mut s.call {
                    c.info.encrypted = true;
                }
            }
            DataType::TerminatorWithLc => {
                if let Some(c) = &mut s.call
                    && let Some(l) = lc::full_lc(&bits, lc::MASK_TERMINATOR)
                {
                    c.info.lc = Some(l);
                    c.info.encrypted |= l.encrypted();
                    c.last = end;
                }
                s.voice_seq = None;
                Self::end(key, s, end, out);
            }
            _ => {
                // Idle, control or data on the slot: any voice call is over.
                s.voice_seq = None;
                Self::end(key, s, end, out);
            }
        }
    }

    fn end(key: u8, s: &mut Slot, _at: u64, out: &mut Vec<CallEvent>) {
        if let Some(mut c) = s.call.take() {
            c.info.alias = s.alias.text().or(c.info.alias);
            out.push(CallEvent::End {
                key,
                at: c.last,
                info: c.info,
            });
        }
        s.header = None;
        s.pi_header = false;
        s.embedded_have = 0;
    }

    /// End every call in progress (signal gone).
    pub fn end_all(&mut self, out: &mut Vec<CallEvent>) {
        for (k, s) in self.slots.iter_mut().enumerate() {
            s.voice_seq = None;
            Self::end(k as u8, s, 0, out);
        }
    }

    pub fn active(&self) -> impl Iterator<Item = (u8, &CallInfo)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(k, s)| s.call.as_ref().map(|c| (k as u8, &c.info)))
    }
}
