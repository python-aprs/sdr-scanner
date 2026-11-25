//! DMR channels: discriminator output into the DMR receiver, calls out per
//! timeslot (see [`crate::dmr`]).

use super::{Call, CallInfo, Demod};
use crate::config::Mode;
use crate::dmr::calls::{CallEvent, Calls};
use crate::dmr::rx::{self, Receiver, RxBurst};
use crate::dmr::vocoder::{self, FRAME_SAMPLES, Leveled, Vocoder};
use num_complex::Complex32;

/// Makes calls, never channel audio: one channel can carry two calls at
/// once (a repeater's two timeslots), so each is its own recording.
pub struct DmrVoice {
    rx: Receiver,
    calls: Calls,
    rate: f64,
    bursts: Vec<RxBurst>,
    events: Vec<CallEvent>,
    out: Vec<Call>,
    vocoders: [Option<Box<dyn Vocoder>>; 2],
    /// Bursts received with a sync pattern.
    syncs: u32,
}

impl DmrVoice {
    pub fn new(rate: f64) -> Self {
        DmrVoice {
            rx: Receiver::new(rate),
            calls: Calls::new(),
            rate,
            bursts: Vec::new(),
            events: Vec::new(),
            out: Vec::new(),
            vocoders: [None, None],
            syncs: 0,
        }
    }

    /// Channel IQ sample of a 48 kHz receiver position.
    fn iq_sample(&self, at: u64) -> u64 {
        ((at as f64 - self.rx.delay()).max(0.0) * self.rate / rx::RATE) as u64
    }

    fn convert(&mut self) {
        for e in std::mem::take(&mut self.events) {
            match e {
                CallEvent::Start { key, at, info } => {
                    self.vocoders[key as usize] =
                        vocoder::new().map(|v| Box::new(Leveled::new(v)) as Box<dyn Vocoder>);
                    self.out.push(Call::Start {
                        key,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Dmr(info),
                    });
                }
                CallEvent::Voice { key, frames } => {
                    let mut audio = Vec::with_capacity(3 * FRAME_SAMPLES);
                    match &mut self.vocoders[key as usize] {
                        Some(v) => frames.iter().for_each(|f| v.decode(f, &mut audio)),
                        None => audio.resize(3 * FRAME_SAMPLES, 0.0),
                    }
                    self.out.push(Call::Voice {
                        key,
                        frames: frames.to_vec(),
                        audio,
                    });
                }
                CallEvent::End { key, at, info } => {
                    self.vocoders[key as usize] = None;
                    self.out.push(Call::End {
                        key,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Dmr(info),
                    });
                }
            }
        }
    }
}

impl Demod for DmrVoice {
    fn process(&mut self, iq: &[Complex32], _open: bool, _audio: &mut Vec<f32>) {
        self.rx.push(iq, &mut self.bursts);
        for b in self.bursts.drain(..) {
            self.syncs += b.sync.is_some() as u32;
            self.calls.push(&b, &mut self.events);
        }
        self.convert();
    }

    fn flush(&mut self, _audio: &mut Vec<f32>) {
        self.calls.end_all(&mut self.events);
        self.convert();
    }

    fn records_audio(&self) -> bool {
        false
    }

    /// Two syncs: one can be chance, the second lands a frame later.
    fn detected(&self) -> Option<Mode> {
        (self.syncs >= 2).then_some(Mode::Dmr)
    }

    fn take_calls(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.out)
    }
}
