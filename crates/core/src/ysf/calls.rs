//! Turns received frames into calls: who is talking (the header's
//! callsigns, or for a late entry the pieces voice frames carry), the
//! DG-ID and data type, and the AMBE+2 voice frames.

use super::frame::{self, DataType, Fi, Fich};
use super::rx::{FRAME_SAMPLES, RxFrame};
use crate::dmr::burst::AmbeFrame;

/// What is known about a call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallInfo {
    /// Source and destination callsigns (the destination is usually `ALL`
    /// or blank).
    pub src: Option<String>,
    pub dst: Option<String>,
    /// Repeater callsigns: the one heard (downlink) and the one talked
    /// into (uplink), as the radio set them.
    pub downlink: Option<String>,
    pub uplink: Option<String>,
    pub dg_id: Option<u8>,
    /// What the voice frames carry, as the last frame said.
    pub data_type: Option<DataType>,
}

impl CallInfo {
    /// `dn`, `vw`, `vd1` or `data`.
    pub fn mode(&self) -> Option<&'static str> {
        self.data_type.map(|d| d.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallEvent {
    /// `at`: 48 kHz sample of the call's first frame.
    Start { at: u64, info: CallInfo },
    /// The voice frames of one 100 ms frame (none for frames without
    /// AMBE+2 voice).
    Voice { frames: Vec<AmbeFrame> },
    /// `at`: 48 kHz sample just past the call's last frame.
    End { at: u64, info: CallInfo },
}

struct Call {
    info: CallInfo,
    last: u64,
    /// Frames in a row whose FICH didn't decode.
    missed: u32,
    dt: DataType,
    /// Voice from frames without a FICH, held until a good frame shows the
    /// call went on (dropped if it ends instead).
    held: Vec<AmbeFrame>,
}

/// Frames without a FICH before a call is over (its signal faded or it
/// ended without a terminator heard).
const MISSED_END: u32 = 5;

#[derive(Default)]
pub struct Calls {
    call: Option<Call>,
    lock: u32,
    pub frames: u64,
    pub voice_frames: u64,
}

fn set(field: &mut Option<String>, v: Option<String>) {
    if v.is_some() {
        *field = v;
    }
}

impl Calls {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, f: &RxFrame, out: &mut Vec<CallEvent>) {
        self.frames += 1;
        if f.lock != self.lock {
            self.end_all(out);
            self.lock = f.lock;
        }
        let bits = &f.bits[..];
        let end = f.start + FRAME_SAMPLES;
        let Some(fich) = f.fich else {
            // Carry the voice on through a damaged frame or two.
            if let Some(c) = &mut self.call {
                c.missed += 1;
                if c.missed >= MISSED_END {
                    self.end_all(out);
                } else {
                    let frames = Self::voice_frames(bits, c.dt);
                    c.held.extend(frames);
                }
            }
            return;
        };
        match fich.fi {
            Fi::Header => {
                let mut info = Self::fich_info(&fich);
                Self::csd(bits, &mut info);
                if self
                    .call
                    .as_ref()
                    .is_some_and(|c| info.src.is_some() && c.info.src != info.src)
                {
                    self.end_all(out);
                }
                match &mut self.call {
                    Some(c) => {
                        Self::merge(&mut c.info, info);
                        c.last = end;
                        c.missed = 0;
                    }
                    None => self.start(f.start, end, fich.dt, info, out),
                }
            }
            Fi::Communications => {
                if self.call.is_none() {
                    self.start(f.start, end, fich.dt, Self::fich_info(&fich), out);
                }
                let c = self.call.as_mut().unwrap();
                c.last = end;
                c.missed = 0;
                c.dt = fich.dt;
                c.info.dg_id = Some(fich.dg_id);
                c.info.data_type = Some(fich.dt);
                Self::comm_data(bits, &fich, &mut c.info);
                let mut frames = std::mem::take(&mut c.held);
                frames.extend(Self::voice_frames(bits, fich.dt));
                self.voice_frames += frames.len() as u64;
                out.push(CallEvent::Voice { frames });
            }
            Fi::Terminator => {
                if let Some(c) = &mut self.call {
                    let mut info = CallInfo::default();
                    Self::csd(bits, &mut info);
                    Self::merge(&mut c.info, info);
                    c.last = end;
                }
                self.end_all(out);
            }
            Fi::Test => {}
        }
    }

    fn fich_info(fich: &Fich) -> CallInfo {
        CallInfo {
            dg_id: Some(fich.dg_id),
            data_type: Some(fich.dt),
            ..Default::default()
        }
    }

    fn merge(into: &mut CallInfo, from: CallInfo) {
        set(&mut into.src, from.src);
        set(&mut into.dst, from.dst);
        set(&mut into.downlink, from.downlink);
        set(&mut into.uplink, from.uplink);
        into.dg_id = from.dg_id.or(into.dg_id);
    }

    fn start(&mut self, at: u64, end: u64, dt: DataType, info: CallInfo, out: &mut Vec<CallEvent>) {
        out.push(CallEvent::Start {
            at,
            info: info.clone(),
        });
        self.call = Some(Call {
            info,
            last: end,
            missed: 0,
            dt,
            held: Vec::new(),
        });
    }

    /// A header's or terminator's two data channels: destination and
    /// source, then downlink and uplink.
    fn csd(bits: &[u8], info: &mut CallInfo) {
        let l = frame::CALLSIGN_LEN;
        if let Some(d) = frame::dch(bits, true, false) {
            info.dst = frame::callsign(&d[..l]);
            info.src = frame::callsign(&d[l..2 * l]);
        }
        if let Some(d) = frame::dch(bits, true, true) {
            info.downlink = frame::callsign(&d[..l]);
            info.uplink = frame::callsign(&d[l..2 * l]);
        }
    }

    /// The piece of the callsigns a voice frame carries, by frame number.
    fn comm_data(bits: &[u8], fich: &Fich, info: &mut CallInfo) {
        let l = frame::CALLSIGN_LEN;
        match fich.dt {
            DataType::VdMode2 => {
                let Some(d) = frame::dch(bits, false, false) else {
                    return;
                };
                let field = match fich.fn_ {
                    0 => &mut info.dst,
                    1 => &mut info.src,
                    2 => &mut info.downlink,
                    3 => &mut info.uplink,
                    _ => return,
                };
                set(field, frame::callsign(&d));
            }
            DataType::VdMode1 => {
                let Some(d) = frame::dch(bits, true, false) else {
                    return;
                };
                match fich.fn_ {
                    0 => {
                        set(&mut info.dst, frame::callsign(&d[..l]));
                        set(&mut info.src, frame::callsign(&d[l..]));
                    }
                    1 => {
                        set(&mut info.downlink, frame::callsign(&d[..l]));
                        set(&mut info.uplink, frame::callsign(&d[l..]));
                    }
                    _ => {}
                }
            }
            // Voice FR and data FR frames carry their callsigns in the
            // header; their payload is IMBE voice or data.
            _ => {}
        }
    }

    fn voice_frames(bits: &[u8], dt: DataType) -> Vec<AmbeFrame> {
        match dt {
            DataType::VdMode2 => (0..5).map(|k| frame::vch_dn(bits, k)).collect(),
            DataType::VdMode1 => (0..5).map(|k| frame::vch_vd1(bits, k)).collect(),
            _ => Vec::new(),
        }
    }

    /// End the call in progress (signal gone).
    pub fn end_all(&mut self, out: &mut Vec<CallEvent>) {
        if let Some(c) = self.call.take() {
            out.push(CallEvent::End {
                at: c.last,
                info: c.info,
            });
        }
    }

    pub fn active(&self) -> Option<&CallInfo> {
        self.call.as_ref().map(|c| &c.info)
    }
}
