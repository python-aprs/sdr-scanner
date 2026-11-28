//! NBFM slots that also listen for digital voice and packet: one
//! transmission's IQ goes to FM voice and to the DMR, System Fusion,
//! D-STAR and AFSK1200 receivers at once.
//!
//! Until a decoder finds its mode, the FM recording goes on as usual. Once
//! one does, the FM audio is muted from there on, the other decoders stop,
//! and the found decoder's calls and frames come out as they would on a
//! slot configured for that mode. The scanner then asks
//! [`Demod::records_audio`] again: if the FM recording holds little more
//! than the digital signal's own lead-in (under [`KEEP_FM_SECS`] of
//! unmuted audio before the detection), it is thrown away; if it holds a
//! real FM transmission (a mixed-mode repeater going from FM to C4FM
//! within the hang time), it is kept, up to the detection.
//!
//! A packet's frame only passes its FCS once the whole burst is in, and a
//! weak or collided one never does, so packet bursts are muted sooner: from
//! their preamble (a run of HDLC flags at the start of every burst) until
//! the carrier drops. Live listeners hear only the first few tens of ms,
//! and a burst that never decodes is not recorded as FM.

use super::{AfskPacket, Call, Demod, DmrVoice, DstarVoice, NbfmVoice, Tags, YsfVoice};
use crate::afsk::Decoded;
use crate::config::{Mode, Slot};
use crate::scanner::ScannerConfig;
use num_complex::Complex32;

/// Unmuted FM audio before a detection that makes the FM recording worth
/// keeping. A digital signal opens the FM squelch for its first few
/// hundred ms (the receivers need a couple of syncs), so this is well
/// above that.
pub const KEEP_FM_SECS: f64 = 1.5;

/// FM audio muted back from a detection: the digital signal's lead-in the
/// decoders needed to find it (two YSF frames are 200 ms, a D-STAR header
/// 137 ms, plus the FM squelch gate's block).
const LEAD_IN_SECS: f64 = 0.4;

/// FM audio muted back from a packet preamble: the flags heard before it
/// was recognized.
const PREAMBLE_LEAD_IN_SECS: f64 = 0.1;

/// Squelch closed this long ends a packet burst's mute. Longer than a
/// block or two of fading; packets sent back to back each have their own
/// preamble.
const BURST_END_SECS: f64 = 0.1;

/// Longest a preamble mutes FM without another one: a full-length AX.25
/// frame at 1200 baud is about 2.5 s. Should voice ever pass for a
/// preamble, this is all of it that is lost.
const BURST_MAX_SECS: f64 = 3.0;

pub struct AutoDetect {
    fm: NbfmVoice,
    /// Candidate decoders, until one finds its mode.
    digital: Vec<Box<dyn Demod>>,
    /// The decoder that found its mode (index into `digital`).
    found: Option<usize>,
    /// FM audio played before the detection.
    fm_secs: f64,
    frames: Vec<Decoded>,
    audio_rate: f64,
    /// Preambles counted so far, and whether a packet burst has FM muted.
    preambles: u64,
    burst: bool,
    /// Channel samples with the squelch closed in a row, and how many end
    /// a burst.
    closed: usize,
    burst_end: usize,
    /// Channel samples since the last preamble, and the most a burst lasts.
    burst_len: usize,
    burst_max: usize,
}

impl AutoDetect {
    pub fn new(slot: &Slot, rate: f64, cfg: &ScannerConfig) -> Self {
        AutoDetect {
            fm: NbfmVoice::new(slot, rate, cfg),
            digital: vec![
                Box::new(DmrVoice::new(rate)),
                Box::new(YsfVoice::new(rate)),
                Box::new(DstarVoice::new(rate)),
                Box::new(AfskPacket::new(slot, rate)),
            ],
            found: None,
            fm_secs: 0.0,
            frames: Vec::new(),
            audio_rate: cfg.audio_rate as f64,
            preambles: 0,
            burst: false,
            closed: 0,
            burst_end: (BURST_END_SECS * rate) as usize,
            burst_len: 0,
            burst_max: (BURST_MAX_SECS * rate) as usize,
        }
    }

    fn keeps_fm(&self) -> bool {
        self.found.is_none() || self.fm_secs >= KEEP_FM_SECS
    }
}

impl Demod for AutoDetect {
    fn process(&mut self, iq: &[Complex32], open: bool, audio: &mut Vec<f32>) {
        let was_found = self.found.is_some();
        match self.found {
            Some(i) => self.digital[i].process(iq, open, &mut Vec::new()),
            None => {
                for (i, d) in self.digital.iter_mut().enumerate() {
                    d.process(iq, open, &mut Vec::new());
                    // Packet frames are only counted as they are taken.
                    self.frames.extend(d.take_frames());
                    if self.found.is_none() && d.detected().is_some() {
                        self.found = Some(i);
                        self.fm_secs = self.fm.played_secs();
                    }
                }
                if let Some(i) = self.found {
                    // The others may have half-built calls; a channel
                    // carries one mode at a time.
                    for (j, d) in self.digital.iter_mut().enumerate() {
                        if j != i {
                            d.flush(&mut Vec::new());
                            d.take_calls();
                            d.take_frames();
                        }
                    }
                }
            }
        }
        let was_burst = self.burst;
        if self.found.is_none() {
            let heard: u64 = self.digital.iter().map(|d| d.preambles()).sum();
            if heard > self.preambles {
                self.preambles = heard;
                self.burst = true;
                self.burst_len = 0;
            }
        }
        self.burst_len += iq.len();
        if self.burst_len >= self.burst_max {
            self.burst = false;
        }
        if open {
            self.closed = 0;
        } else {
            self.closed += iq.len();
            if self.closed >= self.burst_end {
                self.burst = false;
            }
        }
        // FM keeps running muted so its audio stays sample-aligned. Muting
        // this block also mutes the squelch gate's look-ahead behind it.
        self.fm
            .process(iq, open && self.found.is_none() && !self.burst, audio);
        if !was_burst && self.burst && self.found.is_none() {
            let n = (PREAMBLE_LEAD_IN_SECS * self.audio_rate) as usize;
            let from = audio.len().saturating_sub(n);
            audio[from..].fill(0.0);
        }
        if !was_found && self.found.is_some() {
            // `audio` still holds the recording's held-back tail (the
            // scanner keeps more than the hang time): mute the lead-in.
            let n = (LEAD_IN_SECS * self.audio_rate) as usize;
            let from = audio.len().saturating_sub(n);
            audio[from..].fill(0.0);
        }
    }

    fn flush(&mut self, audio: &mut Vec<f32>) {
        self.fm.flush(audio);
        // Nothing found: calls that never got going are not calls.
        if let Some(i) = self.found {
            self.digital[i].flush(&mut Vec::new());
        }
    }

    fn records_audio(&self) -> bool {
        self.keeps_fm()
    }

    fn played_secs(&self) -> f64 {
        if self.found.is_some() {
            self.fm_secs
        } else {
            self.fm.played_secs()
        }
    }

    fn tags(&self) -> Tags {
        self.fm.tags()
    }

    fn is_spur(&mut self) -> bool {
        // A digital signal's level is steady too; only FM carriers are spurs.
        self.found.is_none() && self.fm.is_spur()
    }

    fn take_frames(&mut self) -> Vec<Decoded> {
        if let Some(i) = self.found {
            self.frames.extend(self.digital[i].take_frames());
        }
        std::mem::take(&mut self.frames)
    }

    fn take_calls(&mut self) -> Vec<Call> {
        match self.found {
            Some(i) => self.digital[i].take_calls(),
            None => Vec::new(),
        }
    }

    fn detected(&self) -> Option<Mode> {
        self.found.and_then(|i| self.digital[i].detected())
    }
}
