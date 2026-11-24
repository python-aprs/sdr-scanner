//! The offline/online scanning pipeline: forward FFT → detector → per-slot
//! NBFM chains → transmission outputs (PLAN.md 3.2–3.4).
//!
//! [`Scanner::push`] is fed IQ samples in any chunk size and appends
//! [`Output`] records (transmission open, audio, close) for a writer to turn
//! into files. Channel chains run in parallel on the rayon pool.

mod channel;

use crate::afsk::Frame;
use crate::channelizer::{Analyzer, BankParams, Block, FilterBank};
use crate::clock::SampleClock;
use crate::config::{Slot, SlotOrigin};
use crate::demod;
use crate::detector::{Detector, DetectorParams, Event, SlotState};
use crate::dmr::{AmbeFrame, CallInfo};
use crate::tone::Tone;
use channel::{CallRec, Channel};
use num_complex::Complex32;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct ScannerConfig {
    pub detector: DetectorParams,
    pub audio_rate: u32,
    pub deemphasis: bool,
    /// De-emphasis time constant (s). 530 µs is a 300 Hz corner, matching the
    /// 6 dB/octave pre-emphasis of NBFM transmitters.
    pub deemphasis_tau: f64,
    /// Transmissions shorter than this (signal time, without pre-roll or
    /// tail), or with less unmuted audio than this, are dropped.
    pub min_len: f64,
    /// Audio kept after the signal ends (squelch tail).
    pub tail: f64,
    /// Audio level for full deviation (1.0 = full scale) when `agc` is off.
    pub audio_gain: f32,
    /// Level recorded audio automatically (see [`Agc`]) instead of using
    /// the fixed `audio_gain`.
    pub agc: bool,
    /// High-pass corner for removing CTCSS tones; 0 disables.
    pub highpass_hz: f64,
    /// Noise squelch (see [`NoiseSquelch`]): audio plays only while the
    /// discriminator's noise above the voice band is at least this many dB
    /// below pure noise, on top of the power squelch. 0 disables.
    pub noise_squelch_db: f32,
    /// Tag transmissions with their CTCSS tone or DCS code.
    pub tones: bool,
    /// Spur detection: a transmission that has stayed on this long (s)
    /// with its audio level steady to within `spur_spread_db` (standard
    /// deviation of its level in 100 ms frames) is a carrier, not a voice: its
    /// recording is thrown away and the frequency locked out. 0 disables.
    pub spur_secs: f64,
    pub spur_spread_db: f32,
    /// Lowest channel IQ rate.
    pub min_channel_rate: f64,
    /// Process channels in parallel.
    pub parallel: bool,
    /// Fallback grid slots retune to the signal's measured centre, rounded
    /// to this step (Hz). 0 keeps the grid frequency.
    pub grid_tune_step: f64,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        ScannerConfig {
            detector: DetectorParams::default(),
            audio_rate: 8000,
            deemphasis: true,
            deemphasis_tau: 530e-6,
            min_len: 0.3,
            tail: 0.2,
            audio_gain: 0.5,
            agc: true,
            highpass_hz: 300.0,
            // An FM signal quiets by about its SNR plus 4 dB; pure noise
            // reads 2-5 dB. 8 dB opens from about 4 dB SNR.
            noise_squelch_db: 8.0,
            tones: true,
            spur_secs: 300.0,
            spur_spread_db: 2.0,
            min_channel_rate: 25_000.0,
            parallel: true,
            grid_tune_step: 2500.0,
        }
    }
}

/// A transmission started (sent once it has passed the minimum length).
#[derive(Debug, Clone, PartialEq)]
pub struct TxStart {
    pub id: u64,
    pub slot: Slot,
    /// Stream sample index of the first audio sample.
    pub start_sample: u64,
    pub start_time: SystemTime,
    pub audio_rate: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TxEnd {
    pub id: u64,
    /// Stream sample index just past the end of the signal.
    pub end_sample: u64,
    /// Signal duration (s), without pre-roll and tail.
    pub signal_secs: f64,
    pub peak_snr_db: f32,
    pub mean_snr_db: f32,
    /// CTCSS tone or DCS code heard, if any.
    pub tone: Option<Tone>,
    /// What a digital voice call said about itself (ids, timeslot, ...).
    pub digital: Option<CallInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Open(TxStart),
    Audio {
        id: u64,
        samples: Vec<f32>,
    },
    Close(TxEnd),
    /// Throw the recording away (it turned out to be a spur).
    Discard {
        id: u64,
    },
    /// An AX.25 frame decoded on an `afsk1200` slot.
    Packet(Packet),
    /// A digital call's voice frames, as received (its `Audio` is what a
    /// vocoder made of them, or silence without one).
    Ambe {
        id: u64,
        frames: Vec<AmbeFrame>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub slot: Slot,
    /// When the frame ended.
    pub time: SystemTime,
    pub frame: Frame,
}

/// Audio for live listeners, sent as soon as it is demodulated (recordings
/// hold audio back so a close can trim the hang time; live audio doesn't
/// wait, and what it sends of the hang is already squelched to silence).
#[derive(Debug, Clone, PartialEq)]
pub enum LiveAudio {
    Start {
        id: u64,
        freq: f64,
        priority: bool,
        audio_rate: u32,
    },
    Audio {
        id: u64,
        samples: Vec<f32>,
    },
    End {
        id: u64,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScannerStats {
    pub blocks: u64,
    pub samples: u64,
    pub opened: u64,
    pub recorded: u64,
    pub discarded_short: u64,
    /// Transmissions dropped because their audio stayed muted: the slot
    /// read just over the close threshold, but never steadily enough to
    /// unmute (noise, not a signal).
    pub discarded_silent: u64,
    pub not_recorded: u64,
    /// Grid detections that retuned onto a signal already being recorded.
    pub duplicates: u64,
    /// Steady carriers found and locked out (see `ScannerConfig::spur_secs`).
    pub spurs: u64,
    /// AX.25 frames decoded on `afsk1200` slots.
    pub packets: u64,
    /// Digital voice calls recorded.
    pub calls: u64,
    pub max_concurrent: usize,
}

pub struct Scanner {
    cfg: ScannerConfig,
    bank: FilterBank,
    analyzer: Analyzer,
    detector: Detector,
    clock: SampleClock,
    /// Recent blocks for pre-roll, oldest first.
    /// Recent blocks with each watched slot's SNR in that block.
    history: VecDeque<(Arc<Block>, Arc<[f32]>)>,
    history_len: usize,
    spare: Vec<Vec<Complex32>>,
    chains: Vec<Channel>,
    next_id: u64,
    events: Vec<Event>,
    stats: ScannerStats,
    last_block: Option<u64>,
    /// Frequency ranges (Hz) never recorded, whichever slot hears them.
    lockouts: Vec<(f64, f64)>,
    /// Live audio for listeners, when someone is listening.
    live: Option<Vec<LiveAudio>>,
    /// Spurs found since the last `take_spurs` (frequencies, Hz).
    new_spurs: Vec<f64>,
}

impl Scanner {
    pub fn new(
        cfg: ScannerConfig,
        sample_rate: f64,
        center_freq: f64,
        t0: SystemTime,
        plan: &[Slot],
    ) -> Self {
        let params = BankParams::for_rate(sample_rate);
        Self::with_params(cfg, params, center_freq, t0, plan)
    }

    pub fn with_params(
        cfg: ScannerConfig,
        params: BankParams,
        center_freq: f64,
        t0: SystemTime,
        plan: &[Slot],
    ) -> Self {
        let detector = Detector::new(cfg.detector.clone(), params, center_freq, plan);
        let history_len = detector.preroll_blocks() as usize + 1;
        Scanner {
            bank: FilterBank::new(params, cfg.min_channel_rate),
            analyzer: Analyzer::new(params),
            clock: SampleClock::new(t0, params.sample_rate),
            detector,
            history: VecDeque::with_capacity(history_len + 1),
            history_len,
            spare: Vec::new(),
            chains: Vec::new(),
            next_id: 1,
            events: Vec::new(),
            stats: ScannerStats::default(),
            last_block: None,
            lockouts: Vec::new(),
            live: None,
            new_spurs: Vec::new(),
            cfg,
        }
    }

    pub fn params(&self) -> &BankParams {
        self.bank.params()
    }

    pub fn detector(&self) -> &Detector {
        &self.detector
    }

    pub fn stats(&self) -> ScannerStats {
        self.stats
    }

    /// Transmissions being recorded now: (frequency, current SNR dB,
    /// seconds since the transmission started).
    pub fn active(&self) -> Vec<(f64, f32, f64)> {
        let now = self.stats.samples as f64 / self.bank.params().sample_rate;
        self.chains
            .iter()
            .map(|c| {
                (
                    c.tuned.freq,
                    self.detector.snr_db(c.slot),
                    now - c.audio_t0 as f64 / self.bank.params().sample_rate,
                )
            })
            .collect()
    }

    /// The id the next transmission will get.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Number transmissions from `id` on, so a scanner that replaces
    /// another (after the radio is reopened) never reuses an id.
    pub fn set_next_id(&mut self, id: u64) {
        self.next_id = self.next_id.max(id);
    }

    /// Take over what `old` learned about the band's interference (see
    /// [`Detector::carry_learning`]), for a scanner that replaces it after
    /// the radio is reopened.
    pub fn carry_learning(&mut self, old: &Scanner) {
        self.detector.carry_learning(&old.detector);
    }

    pub fn clock(&self) -> &SampleClock {
        &self.clock
    }

    /// Feed IQ samples.
    pub fn push(&mut self, samples: &[Complex32], out: &mut Vec<Output>) {
        self.stats.samples += samples.len() as u64;
        let mut blocks = Vec::new();
        let spare = &mut self.spare;
        self.analyzer
            .push(samples, || spare.pop(), |b| blocks.push(b));
        for b in blocks {
            self.on_block(b, out);
        }
    }

    /// Turn live audio on or off. Off (the default) costs nothing; turning
    /// it on picks up transmissions already open from where they are.
    pub fn set_live(&mut self, on: bool) {
        match (on, self.live.is_some()) {
            (true, false) => {
                // Join transmissions already open at their newest audio.
                let rate = self.cfg.audio_rate;
                let mut live = Vec::new();
                for c in self.chains.iter_mut().filter(|c| c.records_audio()) {
                    live.push(LiveAudio::Start {
                        id: c.id,
                        freq: c.tuned.freq,
                        priority: c.tuned.priority,
                        audio_rate: rate,
                    });
                    c.live_sent =
                        Some(c.emitted + c.pending.len().saturating_sub(rate as usize / 5));
                }
                self.live = Some(live);
            }
            (false, true) => {
                self.live = None;
                for c in &mut self.chains {
                    c.live_sent = None;
                }
            }
            _ => {}
        }
    }

    /// Live audio since the last call (empty while live audio is off).
    pub fn take_live(&mut self, into: &mut Vec<LiveAudio>) {
        if let Some(l) = &mut self.live {
            into.append(l);
        }
    }

    fn feed_live(&mut self) {
        let Some(live) = &mut self.live else { return };
        let rate = self.cfg.audio_rate;
        for c in self.chains.iter_mut().filter(|c| c.records_audio()) {
            let sent = *c.live_sent.get_or_insert_with(|| {
                live.push(LiveAudio::Start {
                    id: c.id,
                    freq: c.tuned.freq,
                    priority: c.tuned.priority,
                    audio_rate: rate,
                });
                // Opened while live audio is on: from its first sample.
                c.emitted
            });
            let total = c.emitted + c.pending.len();
            if total > sent {
                let from = sent.saturating_sub(c.emitted);
                live.push(LiveAudio::Audio {
                    id: c.id,
                    samples: c.pending[from..].to_vec(),
                });
                c.live_sent = Some(total);
            }
        }
    }

    /// Steady carriers found since the last call (frequencies, Hz). Each
    /// is already locked out in this scanner; keep it locked out with
    /// [`Scanner::set_lockouts`] or the slot's flags if the lockouts are
    /// set again.
    pub fn take_spurs(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.new_spurs)
    }

    fn locked_out(&self, freq: f64) -> bool {
        self.lockouts
            .iter()
            .any(|&(lo, hi)| freq >= lo && freq <= hi)
    }

    /// Set the record and priority flags of the watched slot at `freq` (Hz,
    /// within 1 Hz). A recording in progress on a slot that stops being
    /// recorded is closed now. Returns false if no watched slot is there.
    pub fn set_slot_flags(
        &mut self,
        freq: f64,
        record: bool,
        priority: bool,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(i) = self.detector.slot_at(freq) else {
            return false;
        };
        self.detector.set_flags(i, record, priority);
        if !record {
            self.close_where(|c| c.slot == i, out);
        }
        true
    }

    /// Frequency ranges that are never recorded, on top of the slots' own
    /// flags (for signals a grid slot retunes onto). Recordings in progress
    /// inside a range are closed now.
    pub fn set_lockouts(&mut self, ranges: Vec<(f64, f64)>, out: &mut Vec<Output>) {
        self.lockouts = ranges;
        let locked: Vec<f64> = self
            .chains
            .iter()
            .map(|c| c.tuned.freq)
            .filter(|&f| self.locked_out(f))
            .collect();
        self.close_where(|c| locked.contains(&c.tuned.freq), out);
    }

    /// Close the transmissions of chains matching `pick` as if their signal
    /// had just stopped.
    fn close_where(&mut self, pick: impl Fn(&Channel) -> bool, out: &mut Vec<Output>) {
        let Some(last) = self.last_block else { return };
        let slots: Vec<usize> = self
            .chains
            .iter()
            .filter(|c| pick(c))
            .map(|c| c.slot)
            .collect();
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        for i in slots {
            self.detector.close(i, last, &mut events);
        }
        for e in events.drain(..) {
            self.on_event(e, out);
        }
        self.events = events;
    }

    /// End of stream: close all open transmissions.
    pub fn finish(&mut self, out: &mut Vec<Output>) {
        if let Some(last) = self.last_block {
            let mut events = std::mem::take(&mut self.events);
            events.clear();
            self.detector.finish(last, &mut events);
            for e in events.drain(..) {
                self.on_event(e, out);
            }
            self.events = events;
        }
    }

    fn on_block(&mut self, block: Block, out: &mut Vec<Output>) {
        self.stats.blocks += 1;
        let block = Arc::new(block);
        self.last_block = Some(block.index);

        let mut events = std::mem::take(&mut self.events);
        events.clear();
        self.detector.update(&block, &mut events);
        // Per slot, the SNR that gates its audio (see Detector::squelch_db).
        let snrs: Arc<[f32]> = (0..self.detector.slots().len())
            .map(|i| self.detector.squelch_db(i))
            .collect();
        self.history.push_back((block.clone(), snrs.clone()));
        while self.history.len() > self.history_len {
            let (old, _) = self.history.pop_front().unwrap();
            if let Ok(b) = Arc::try_unwrap(old) {
                self.spare.push(b.bins);
            }
        }

        // Existing chains process this block (new chains catch up from
        // history when they open, below).
        // Each chain is ~50 µs of work per block; below a dozen chains the
        // fan-out costs more than it saves.
        let close = self.cfg.detector.close_db;
        if self.cfg.parallel && self.chains.len() >= 12 {
            self.chains
                .par_iter_mut()
                .for_each(|c| c.process(&block, snrs[c.slot] >= close));
        } else {
            for c in &mut self.chains {
                c.process(&block, snrs[c.slot] >= close);
            }
        }

        for e in events.drain(..) {
            self.on_event(e, out);
        }
        self.events = events;

        self.feed_live();

        // Confirm transmissions that passed the minimum length, and stream
        // audio that can no longer be trimmed away by a close.
        let block_secs = self.bank.params().block_secs();
        let holdback = self.holdback();
        let min_blocks = (self.cfg.min_len / block_secs).ceil() as u64;
        let preroll = self.detector.signal_offset_blocks();
        for c in &mut self.chains {
            Self::emit_packets(&self.clock, &mut self.stats, self.bank.params(), c, out);
            Self::emit_calls(
                &self.clock,
                &mut self.stats,
                self.bank.params(),
                &mut self.next_id,
                &mut self.live,
                snrs[c.slot],
                c,
                out,
            );
            if !c.records_audio() {
                continue;
            }
            if !c.confirmed {
                // Only time with the signal present counts, not hang time.
                if let SlotState::Active { start } = self.detector.state(c.slot) {
                    if block.index + 1 >= start + preroll + min_blocks
                        && c.demod.played_secs() >= self.cfg.min_len
                    {
                        c.confirmed = true;
                        out.push(Output::Open(Self::tx_start(
                            &self.clock,
                            c,
                            self.cfg.audio_rate,
                        )));
                    }
                }
            }
            if c.confirmed {
                c.drain_ready(holdback, out);
            }
        }
        self.stats.max_concurrent = self.stats.max_concurrent.max(self.chains.len());
        if self.cfg.spur_secs > 0.0 {
            self.drop_spurs(out);
        }
    }

    /// Throw away recordings that turned out to be steady carriers, and
    /// lock their frequencies out.
    fn drop_spurs(&mut self, out: &mut Vec<Output>) {
        let found: Vec<usize> = self
            .chains
            .iter_mut()
            .enumerate()
            .filter_map(|(i, c)| c.demod.is_spur().then_some(i))
            .collect();
        for i in found.into_iter().rev() {
            let c = self.chains.swap_remove(i);
            if let (Some(live), Some(_)) = (&mut self.live, c.live_sent) {
                live.push(LiveAudio::End { id: c.id });
            }
            if c.confirmed {
                out.push(Output::Discard { id: c.id });
            }
            let f = c.tuned.freq;
            let half = (self.cfg.grid_tune_step / 2.0).max(500.0);
            self.lockouts.push((f - half, f + half));
            self.new_spurs.push(f);
            self.stats.spurs += 1;
        }
    }

    fn emit_packets(
        clock: &SampleClock,
        stats: &mut ScannerStats,
        p: &BankParams,
        c: &mut Channel,
        out: &mut Vec<Output>,
    ) {
        for d in c.demod.take_frames() {
            let at = c.audio_t0 + (d.at_sample as f64 * p.sample_rate / c.rate) as i64;
            stats.packets += 1;
            out.push(Output::Packet(Packet {
                slot: c.tuned.clone(),
                time: clock.time_at(at.max(0) as u64),
                frame: d.frame,
            }));
        }
    }

    /// Turn a channel's digital call events into recordings: each call is
    /// its own transmission, opened as it starts (sync has already shown
    /// it is real) and streamed as its voice arrives.
    #[allow(clippy::too_many_arguments)]
    fn emit_calls(
        clock: &SampleClock,
        stats: &mut ScannerStats,
        p: &BankParams,
        next_id: &mut u64,
        live: &mut Option<Vec<LiveAudio>>,
        snr_db: f32,
        c: &mut Channel,
        out: &mut Vec<Output>,
    ) {
        use crate::demod::Call;
        use crate::dmr::vocoder::AUDIO_RATE;
        for rec in c.calls.iter_mut().flatten() {
            rec.peak_snr_db = rec.peak_snr_db.max(snr_db);
            rec.snr_sum += snr_db as f64;
            rec.blocks += 1;
        }
        let stream = |c: &Channel, at: u64| -> u64 {
            (c.audio_t0 + (at as f64 * p.sample_rate / c.rate) as i64).max(0) as u64
        };
        for e in c.demod.take_calls() {
            match e {
                Call::Start { key, at_sample, .. } => {
                    let start = stream(c, at_sample);
                    let id = *next_id;
                    *next_id += 1;
                    out.push(Output::Open(TxStart {
                        id,
                        slot: c.tuned.clone(),
                        start_sample: start,
                        start_time: clock.time_at(start),
                        audio_rate: AUDIO_RATE,
                    }));
                    if let Some(l) = live.as_mut() {
                        l.push(LiveAudio::Start {
                            id,
                            freq: c.tuned.freq,
                            priority: c.tuned.priority,
                            audio_rate: AUDIO_RATE,
                        });
                    }
                    c.calls[key as usize & 1] = Some(CallRec {
                        id,
                        start,
                        peak_snr_db: snr_db,
                        snr_sum: snr_db as f64,
                        blocks: 1,
                        live: live.is_some(),
                    });
                }
                Call::Voice { key, frames, audio } => {
                    let Some(rec) = &c.calls[key as usize & 1] else {
                        continue;
                    };
                    if rec.live
                        && let Some(l) = live.as_mut()
                    {
                        l.push(LiveAudio::Audio {
                            id: rec.id,
                            samples: audio.clone(),
                        });
                    }
                    out.push(Output::Audio {
                        id: rec.id,
                        samples: audio,
                    });
                    out.push(Output::Ambe {
                        id: rec.id,
                        frames: frames.to_vec(),
                    });
                }
                Call::End {
                    key,
                    at_sample,
                    info,
                } => {
                    let Some(rec) = c.calls[key as usize & 1].take() else {
                        continue;
                    };
                    if rec.live
                        && let Some(l) = live.as_mut()
                    {
                        l.push(LiveAudio::End { id: rec.id });
                    }
                    let end = stream(c, at_sample).max(rec.start);
                    stats.calls += 1;
                    out.push(Output::Close(TxEnd {
                        id: rec.id,
                        end_sample: end,
                        signal_secs: (end - rec.start) as f64 / p.sample_rate,
                        peak_snr_db: rec.peak_snr_db,
                        mean_snr_db: (rec.snr_sum / rec.blocks as f64) as f32,
                        tone: None,
                        digital: Some(info),
                    }));
                }
            }
        }
    }

    /// Audio held back so that a close can trim the hang time.
    fn holdback(&self) -> usize {
        let hang = self.detector.hang_blocks() as f64 * self.bank.params().block_secs();
        ((hang + 0.1) * self.cfg.audio_rate as f64).ceil() as usize
    }

    fn tx_start(clock: &SampleClock, c: &Channel, audio_rate: u32) -> TxStart {
        let s = c.audio_t0.max(0) as u64;
        TxStart {
            id: c.id,
            slot: c.tuned.clone(),
            start_sample: s,
            start_time: clock.time_at(s),
            audio_rate,
        }
    }

    fn on_event(&mut self, e: Event, out: &mut Vec<Output>) {
        match e {
            Event::Open {
                slot,
                start_block,
                block,
            } => {
                self.stats.opened += 1;
                let ws = &self.detector.slots()[slot];
                if !ws.slot.record || !demod::supports(ws.slot.mode) {
                    self.stats.not_recorded += 1;
                    return;
                }
                let mut tuned = ws.slot.clone();
                let mut offset = ws.offset_hz;
                if ws.slot.origin == SlotOrigin::Grid && self.cfg.grid_tune_step > 0.0 {
                    let centre = self.detector.centroid_hz(slot);
                    let rf = ws.slot.freq - ws.offset_hz + centre;
                    let snapped = (rf / self.cfg.grid_tune_step).round() * self.cfg.grid_tune_step;
                    offset += snapped - ws.slot.freq;
                    tuned.freq = snapped;
                    // A grid slot pulled onto the tuner's DC residual (the
                    // notch's skirt) is not a transmission.
                    if offset.abs() < self.cfg.detector.dc_notch_hz {
                        self.stats.not_recorded += 1;
                        return;
                    }
                }
                if self.locked_out(tuned.freq) {
                    self.stats.not_recorded += 1;
                    return;
                }
                // A signal between two grid windows can open both; once
                // retuned they land on the same frequency. Keep the first.
                let dup = self.chains.iter().any(|c| {
                    (c.tuned.freq - tuned.freq).abs() < c.tuned.bandwidth.min(tuned.bandwidth) / 2.0
                });
                if dup {
                    self.stats.duplicates += 1;
                    return;
                }
                let ex = self.bank.extractor(offset, ws.slot.bandwidth);
                let Some(dm) = demod::build(&ws.slot, ex.output_rate(), &self.cfg) else {
                    // `supports` said yes above.
                    unreachable!("no demodulator for {:?}", ws.slot.mode);
                };
                let first = self
                    .history
                    .iter()
                    .map(|(b, _)| b)
                    .find(|b| b.index >= start_block)
                    .cloned();
                let audio_t0 = match &first {
                    Some(b) => b.first_sample as i64 - (self.bank.params().overlap() / 2) as i64,
                    None => 0,
                };
                let mut c = Channel::new(self.next_id, slot, tuned, ex, dm, audio_t0);
                self.next_id += 1;
                let close = self.cfg.detector.close_db;
                for (b, snrs) in self
                    .history
                    .iter()
                    .filter(|(b, _)| b.index >= start_block && b.index <= block)
                {
                    c.process(b, snrs[slot] >= close);
                }
                self.chains.push(c);
            }
            Event::Close {
                slot,
                start_block,
                end_block,
                peak_snr_db,
                mean_snr_db,
            } => {
                let Some(pos) = self.chains.iter().position(|c| c.slot == slot) else {
                    return;
                };
                let mut c = self.chains.swap_remove(pos);
                let p = *self.bank.params();
                Self::emit_packets(&self.clock, &mut self.stats, &p, &mut c, out);
                if !c.records_audio() {
                    // End digital calls still open.
                    c.flush();
                    let snr = self.detector.snr_db(c.slot);
                    Self::emit_calls(
                        &self.clock,
                        &mut self.stats,
                        &p,
                        &mut self.next_id,
                        &mut self.live,
                        snr,
                        &mut c,
                        out,
                    );
                    return;
                }
                if let (Some(live), Some(_)) = (&mut self.live, c.live_sent) {
                    live.push(LiveAudio::End { id: c.id });
                }
                let end_sample = (end_block + 1) * p.hop as u64;
                let signal_start =
                    (start_block + self.detector.signal_offset_blocks()) * p.hop as u64;
                let signal_secs = end_sample.saturating_sub(signal_start) as f64 / p.sample_rate;
                if !c.confirmed && signal_secs < self.cfg.min_len {
                    self.stats.discarded_short += 1;
                    return;
                }
                if !c.confirmed && c.demod.played_secs() < self.cfg.min_len {
                    self.stats.discarded_silent += 1;
                    return;
                }
                if !c.confirmed {
                    out.push(Output::Open(Self::tx_start(
                        &self.clock,
                        &c,
                        self.cfg.audio_rate,
                    )));
                }
                // Keep audio up to the end of the signal plus the tail.
                let keep_until = end_sample as f64 + self.cfg.tail * p.sample_rate;
                let keep_audio = ((keep_until - c.audio_t0 as f64) / p.sample_rate
                    * self.cfg.audio_rate as f64)
                    .max(0.0) as usize;
                c.flush();
                let total = c.emitted + c.pending.len();
                if keep_audio < total {
                    let cut = keep_audio.saturating_sub(c.emitted);
                    c.pending.truncate(cut);
                }
                c.drain_ready(0, out);
                self.stats.recorded += 1;
                out.push(Output::Close(TxEnd {
                    id: c.id,
                    end_sample,
                    signal_secs,
                    peak_snr_db,
                    mean_snr_db,
                    tone: c.demod.tags().tone,
                    digital: None,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::dsp::Agc;
    use std::f64::consts::PI;

    fn slot(freq: f64) -> Slot {
        Slot {
            freq,
            bandwidth: 16e3,
            deviation: 5e3,
            mode: Mode::Nbfm,
            priority: false,
            record: true,
            name: None,
            origin: SlotOrigin::Grid,
            pair: None,
        }
    }

    fn params() -> BankParams {
        BankParams {
            sample_rate: 2_000_000.0,
            n: 1 << 15,
            hop: (1 << 15) / 4 * 3,
        }
    }

    /// Clean FM tone transmissions plus a little deterministic noise.
    fn scene(secs: f64, sigs: &[(f64, f64, f64, f64)]) -> Vec<Complex32> {
        scene_dev(secs, sigs, 3000.0)
    }

    /// Like `scene`, with the tone at `dev` Hz deviation.
    fn scene_dev(secs: f64, sigs: &[(f64, f64, f64, f64)], dev: f64) -> Vec<Complex32> {
        scene_amp(secs, sigs, dev, 0.1)
    }

    /// Like `scene_dev`, with carriers of amplitude `amp`.
    fn scene_amp(secs: f64, sigs: &[(f64, f64, f64, f64)], dev: f64, amp: f32) -> Vec<Complex32> {
        let fs = params().sample_rate;
        let n = (secs * fs) as usize;
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut x: Vec<Complex32> = (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let a = ((seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 2e-3;
                let b = ((seed & 0xff_ffff) as f32 / (1u64 << 24) as f32 - 0.5) * 2e-3;
                Complex32::new(a, b)
            })
            .collect();
        for &(off, tone, on, offt) in sigs {
            let mut ph = 0.0f64;
            for (i, v) in x.iter_mut().enumerate() {
                let t = i as f64 / fs;
                if t >= on && t < offt {
                    let f = off + dev * (2.0 * PI * tone * t).sin();
                    ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
                    *v += Complex32::from_polar(amp, ph as f32);
                }
            }
        }
        x
    }

    fn collect(x: &[Complex32], plan: &[Slot], cfg: ScannerConfig) -> (Vec<Output>, ScannerStats) {
        let mut s = Scanner::with_params(cfg, params(), 0.0, SystemTime::UNIX_EPOCH, plan);
        let mut out = Vec::new();
        for c in x.chunks(50_000) {
            s.push(c, &mut out);
        }
        s.finish(&mut out);
        (out, s.stats())
    }

    fn audio_of(out: &[Output], id: u64) -> Vec<f32> {
        out.iter()
            .filter_map(|o| match o {
                Output::Audio { id: i, samples } if *i == id => Some(samples.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn records_two_overlapping_transmissions() {
        let x = scene(4.0, &[(-200e3, 1000.0, 0.5, 2.5), (300e3, 700.0, 1.0, 3.0)]);
        let plan: Vec<Slot> = (-16..=16).map(|k| slot(k as f64 * 25e3)).collect();
        let cfg = ScannerConfig {
            detector: DetectorParams {
                hang: 0.5,
                ..Default::default()
            },
            ..Default::default()
        };
        let (out, stats) = collect(&x, &plan, cfg);
        let opens: Vec<&TxStart> = out
            .iter()
            .filter_map(|o| match o {
                Output::Open(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(opens.len(), 2, "{stats:?}");
        assert_eq!(stats.max_concurrent, 2);
        for (s, tone, on, off) in [(opens[0], 1000.0, 0.5, 2.5), (opens[1], 700.0, 1.0, 3.0)] {
            let audio = audio_of(&out, s.id);
            let start = s.start_sample as f64 / params().sample_rate;
            assert!(start > on - 0.35 && start < on, "start {start}");
            let len = audio.len() as f64 / 8000.0;
            let expect = off - start + 0.2;
            assert!((len - expect).abs() < 0.06, "len {len} expect {expect}");
            // Dominant tone via zero crossings in the middle of the signal.
            let a = ((on + 0.2 - start) * 8000.0) as usize;
            let b = ((off - 0.2 - start) * 8000.0) as usize;
            let mid = &audio[a..b];
            let zc = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count() as f64;
            let f = zc / ((b - a) as f64 / 8000.0);
            assert!((f - tone).abs() < 10.0, "tone {f}");
            // Level: the AGC puts a steady tone's RMS at its target.
            let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
            assert!((rms / Agc::TARGET - 1.0).abs() < 0.12, "rms {rms}");
            let close = out
                .iter()
                .find_map(|o| match o {
                    Output::Close(e) if e.id == s.id => Some(e.clone()),
                    _ => None,
                })
                .unwrap();
            assert!((close.signal_secs - (off - on)).abs() < 0.08, "{close:?}");
        }
    }

    #[test]
    fn light_and_full_deviation_record_at_the_same_level() {
        // A handheld's voice can deviate ~300 Hz where a repeater's reaches
        // 3 kHz: 20 dB apart with a fixed gain, level with the AGC.
        let level = |dev: f64, agc: bool| {
            let x = scene_dev(3.0, &[(100e3, 1000.0, 0.5, 2.5)], dev);
            let cfg = ScannerConfig {
                agc,
                ..Default::default()
            };
            let (out, _) = collect(&x, &[slot(100e3)], cfg);
            let Some(Output::Open(s)) = out.iter().find(|o| matches!(o, Output::Open(_))) else {
                panic!("no transmission")
            };
            let audio = audio_of(&out, s.id);
            let start = s.start_sample as f64 / params().sample_rate;
            let mid = &audio[((1.0 - start) * 8000.0) as usize..((2.3 - start) * 8000.0) as usize];
            let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
            20.0 * rms.log10()
        };
        let (light, full) = (level(300.0, false), level(3000.0, false));
        assert!(
            (full - light - 20.0).abs() < 1.0,
            "fixed: {light} vs {full}"
        );
        let (light, full) = (level(300.0, true), level(3000.0, true));
        assert!((full - light).abs() < 1.0, "agc: {light} vs {full}");
        let target = 20.0 * Agc::TARGET.log10();
        assert!((light - target).abs() < 1.5, "agc level {light} dBFS");
    }

    #[test]
    fn off_grid_signals_retune_to_their_own_frequency() {
        // 5 kHz off the 25 kHz grid (like 147.520 between 147.500 and
        // 147.525), modulated by a slow tone so single blocks are lopsided.
        let sigs = [(120e3, 40.0, 0.5, 1.5), (-205e3, 55.0, 0.5, 1.5)];
        let x = scene(2.0, &sigs);
        let plan: Vec<Slot> = (-16..=16).map(|k| slot(k as f64 * 25e3)).collect();
        let (out, _) = collect(&x, &plan, ScannerConfig::default());
        let mut tuned: Vec<f64> = out
            .iter()
            .filter_map(|o| match o {
                Output::Open(s) => Some(s.slot.freq),
                _ => None,
            })
            .collect();
        tuned.sort_by(f64::total_cmp);
        assert_eq!(tuned, vec![-205e3, 120e3]);
    }

    #[test]
    fn noise_outside_the_signal_is_muted() {
        let x = scene(3.0, &[(100e3, 1000.0, 1.0, 2.0)]);
        let plan = vec![slot(100e3)];
        let (out, _) = collect(&x, &plan, ScannerConfig::default());
        let Some(Output::Open(s)) = out.iter().find(|o| matches!(o, Output::Open(_))) else {
            panic!("no transmission: {out:?}")
        };
        let audio = audio_of(&out, s.id);
        let start = s.start_sample as f64 / params().sample_rate;
        let rms = |a: &[f32]| (a.iter().map(|v| v * v).sum::<f32>() / a.len().max(1) as f32).sqrt();
        // Pre-roll before key-up, and the tail after key-down, are silent.
        let pre = &audio[..((1.0 - start - 0.03) * 8000.0) as usize];
        let post = &audio[((2.0 - start + 0.05) * 8000.0) as usize..];
        let mid = &audio[((1.2 - start) * 8000.0) as usize..((1.8 - start) * 8000.0) as usize];
        assert!(!pre.is_empty() && !post.is_empty());
        assert!(rms(pre) < 1e-3, "pre {}", rms(pre));
        assert!(rms(post) < 1e-3, "post {}", rms(post));
        assert!(rms(mid) > 0.05, "mid {}", rms(mid));
    }

    #[test]
    fn no_transients_at_key_up_or_key_down() {
        // Abrupt key-up/down at several block alignments: the loudest sample
        // must be the tone itself, not a click at the squelch edges.
        for (on, off) in [
            (0.5, 1.5),
            (0.5031, 1.5044),
            (0.5077, 1.5103),
            (0.5049, 1.5021),
        ] {
            let x = scene(2.5, &[(100e3, 400.0, on, off)]);
            let (out, _) = collect(&x, &[slot(100e3)], ScannerConfig::default());
            let Some(Output::Open(s)) = out.iter().find(|o| matches!(o, Output::Open(_))) else {
                panic!("no transmission")
            };
            let audio = audio_of(&out, s.id);
            let start = s.start_sample as f64 / params().sample_rate;
            let a = ((on + 0.3 - start) * 8000.0) as usize;
            let b = ((off - 0.3 - start) * 8000.0) as usize;
            let steady = audio[a..b].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let worst = audio.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(
                worst <= steady * 1.1,
                "on {on}: worst {worst} vs steady {steady}"
            );
        }
    }

    #[test]
    fn zero_filled_gap_mid_transmission() {
        // A 60 ms overrun gap (zeros) inside a transmission: one file, no
        // NaN, no clicks, no false opens elsewhere.
        let clean = scene(3.0, &[(100e3, 1000.0, 0.5, 2.5)]);
        let mut x = clean.clone();
        let fs = params().sample_rate;
        for v in &mut x[(1.5 * fs) as usize..(1.56 * fs) as usize] {
            *v = Complex32::default();
        }
        let plan: Vec<Slot> = (-8..=8).map(|k| slot(k as f64 * 25e3)).collect();
        let worst_jump = |x: &[Complex32]| {
            let (out, stats) = collect(x, &plan, ScannerConfig::default());
            assert_eq!(stats.recorded, 1, "{stats:?}");
            let audio: Vec<f32> = out
                .iter()
                .filter_map(|o| match o {
                    Output::Audio { samples, .. } => Some(samples.clone()),
                    _ => None,
                })
                .flatten()
                .collect();
            assert!(audio.iter().all(|v| v.is_finite()));
            audio
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0f32, f32::max)
        };
        let base = worst_jump(&clean);
        let gap = worst_jump(&x);
        // A click would jump far beyond the tone's own sample-to-sample step.
        assert!(gap < base * 1.2, "gap {gap} vs clean {base}");
    }

    #[test]
    fn short_transmissions_are_discarded() {
        let x = scene(2.0, &[(100e3, 1000.0, 0.5, 0.7)]);
        let plan = vec![slot(100e3)];
        let cfg = ScannerConfig {
            min_len: 0.5,
            ..Default::default()
        };
        let (out, stats) = collect(&x, &plan, cfg);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(stats.discarded_short, 1);
    }

    #[test]
    fn locked_out_slot_is_not_recorded() {
        let x = scene(2.0, &[(100e3, 1000.0, 0.3, 1.5)]);
        let mut s = slot(100e3);
        s.record = false;
        let (out, stats) = collect(&x, &[s], ScannerConfig::default());
        assert!(out.is_empty());
        assert_eq!(stats.not_recorded, 1);
    }

    /// Run `x` in chunks, calling `edit` once after `at` seconds.
    fn collect_with_edit(
        x: &[Complex32],
        plan: &[Slot],
        at: f64,
        edit: impl FnOnce(&mut Scanner, &mut Vec<Output>),
    ) -> (Vec<Output>, ScannerStats) {
        let mut s = Scanner::with_params(
            ScannerConfig::default(),
            params(),
            0.0,
            SystemTime::UNIX_EPOCH,
            plan,
        );
        let mut out = Vec::new();
        let split = (at * params().sample_rate) as usize;
        for c in x[..split].chunks(50_000) {
            s.push(c, &mut out);
        }
        edit(&mut s, &mut out);
        for c in x[split..].chunks(50_000) {
            s.push(c, &mut out);
        }
        s.finish(&mut out);
        (out, s.stats())
    }

    fn closes(out: &[Output]) -> Vec<&TxEnd> {
        out.iter()
            .filter_map(|o| match o {
                Output::Close(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn locking_out_mid_transmission_ends_it_and_stops_new_ones() {
        // On 0.3-3.0 s; locked out at 1.5 s, unlocked at never.
        let x = scene(4.0, &[(100e3, 1000.0, 0.3, 3.0)]);
        let (out, stats) = collect_with_edit(&x, &[slot(100e3)], 1.5, |s, out| {
            assert!(s.set_slot_flags(100e3, false, false, out));
            assert!(!s.set_slot_flags(123e3, false, false, out), "no slot there");
        });
        let ends = closes(&out);
        assert_eq!(ends.len(), 1, "{stats:?}");
        // Ended at the lockout, not at key-down (2.7 s of signal).
        assert!(ends[0].signal_secs < 1.4, "{}", ends[0].signal_secs);
        assert!(s_opens(&out) == 1);
        assert!(
            stats.not_recorded >= 1,
            "re-opened but not recorded: {stats:?}"
        );
    }

    fn s_opens(out: &[Output]) -> usize {
        out.iter().filter(|o| matches!(o, Output::Open(_))).count()
    }

    #[test]
    fn lockout_ranges_catch_retuned_grid_signals() {
        // An off-grid signal at 112.5 kHz heard by the 100 kHz grid slot,
        // which retunes onto it; a lockout at 112.5 kHz +/- 5 kHz stops it.
        let x = scene(3.0, &[(112.5e3, 1000.0, 0.3, 2.5)]);
        let plan = vec![slot(100e3), slot(125e3)];
        let (out, _) = collect(&x, &plan, ScannerConfig::default());
        assert_eq!(s_opens(&out), 1);
        let (out, stats) = collect_with_edit(&x, &plan, 0.0, |s, out| {
            s.set_lockouts(vec![(107.5e3, 117.5e3)], out);
        });
        assert_eq!(s_opens(&out), 0, "{stats:?}");
        assert!(stats.not_recorded >= 1);
        // Mid-transmission: closed at the edit.
        let (out, _) = collect_with_edit(&x, &plan, 1.2, |s, out| {
            s.set_lockouts(vec![(107.5e3, 117.5e3)], out);
        });
        let ends = closes(&out);
        assert_eq!(ends.len(), 1);
        assert!(ends[0].signal_secs < 1.1, "{}", ends[0].signal_secs);
    }

    #[test]
    fn live_audio_matches_the_recording_and_ends() {
        let x = scene(3.0, &[(100e3, 1000.0, 0.5, 2.0)]);
        let mut s = Scanner::with_params(
            ScannerConfig::default(),
            params(),
            0.0,
            SystemTime::UNIX_EPOCH,
            &[slot(100e3)],
        );
        s.set_live(true);
        let (mut out, mut live) = (Vec::new(), Vec::new());
        // When each live sample arrived vs when the recording got it.
        let (mut live_at, mut rec_at) = (Vec::new(), Vec::new());
        for (k, c) in x.chunks(20_000).enumerate() {
            s.push(c, &mut out);
            s.take_live(&mut live);
            for o in out.drain(..) {
                if let Output::Audio { samples, .. } = o {
                    rec_at.extend(std::iter::repeat_n(k, samples.len()));
                }
            }
            for l in &live {
                if let LiveAudio::Audio { samples, .. } = l {
                    live_at.extend(std::iter::repeat_n(k, samples.len()));
                }
            }
            if k == 0 {
                assert!(live.is_empty());
            }
            live.retain(|l| !matches!(l, LiveAudio::Audio { .. }));
        }
        s.finish(&mut out);
        s.take_live(&mut live);
        assert!(matches!(live[0], LiveAudio::Start { freq, .. } if freq == 100e3));
        assert!(matches!(live.last(), Some(LiveAudio::End { .. })));
        // Live audio arrives well before the recording's held-back copy.
        let i = rec_at.len() / 2;
        assert!(
            live_at[i] < rec_at[i],
            "live {} vs recorded {}",
            live_at[i],
            rec_at[i]
        );
        // Off: nothing more, and no cost.
        s.set_live(false);
        let mut more = Vec::new();
        s.push(&x[..20_000], &mut out);
        s.take_live(&mut more);
        assert!(more.is_empty());
    }

    /// Add band-limited noise (random-phase tones every 125 Hz across
    /// `centre` +/- `half` Hz, total RMS `rms`) from `on` to `off` seconds.
    fn add_noise_band(x: &mut [Complex32], centre: f64, half: f64, rms: f32, on: f64, off: f64) {
        let fs = params().sample_rate;
        let k = (half / 125.0) as i64;
        let n = (2 * k + 1) as f32;
        let mut seed = 99u32;
        let tones: Vec<(f64, f64)> = (-k..=k)
            .map(|j| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (
                    centre + j as f64 * 125.0,
                    seed as f64 / u32::MAX as f64 * 2.0 * PI,
                )
            })
            .collect();
        let a = rms / n.sqrt();
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f64 / fs;
            if t >= on && t < off {
                for &(f, p) in &tones {
                    *v += Complex32::from_polar(a, (2.0 * PI * f * t + p) as f32);
                }
            }
        }
    }

    #[test]
    fn noise_squelch_keeps_wideband_noise_out() {
        // Noise filling the slot (like a switching supply's hash) lifts its
        // power well over the open threshold, but it is not an FM signal.
        let mut x = scene(2.0, &[]);
        add_noise_band(&mut x, 100e3, 7000.0, 4e-4, 0.4, 1.6);
        let plan = vec![slot(100e3)];
        let off = ScannerConfig {
            noise_squelch_db: 0.0,
            ..Default::default()
        };
        let (out, stats) = collect(&x, &plan, off);
        assert_eq!(
            s_opens(&out),
            1,
            "power squelch alone records it: {stats:?}"
        );
        let (out, stats) = collect(&x, &plan, ScannerConfig::default());
        assert_eq!(s_opens(&out), 0, "{stats:?}");
        assert_eq!(stats.discarded_silent, 1, "{stats:?}");
        // A real transmission at the same power still records.
        let x = scene_amp(2.0, &[(100e3, 1000.0, 0.4, 1.6)], 3000.0, 4e-4);
        let (out, stats) = collect(&x, &plan, ScannerConfig::default());
        assert_eq!(s_opens(&out), 1, "{stats:?}");
    }

    #[test]
    fn transmissions_are_tagged_with_their_tone() {
        let fs = params().sample_rate;
        // Voice-band tone at 3 kHz deviation, plus a sub-audible part.
        let make = |sub: &dyn Fn(f64) -> f64| {
            let mut x = scene(3.0, &[]);
            let mut ph = 0.0f64;
            for (i, v) in x.iter_mut().enumerate() {
                let t = i as f64 / fs;
                if (0.4..2.6).contains(&t) {
                    let f = 100e3 + 3000.0 * (2.0 * PI * 1000.0 * t).sin() + sub(t);
                    ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
                    *v += Complex32::from_polar(0.1, ph as f32);
                }
            }
            let (out, _) = collect(&x, &[slot(100e3)], ScannerConfig::default());
            closes(&out).first().map(|e| e.tone)
        };
        let ctcss = make(&|t| 600.0 * (2.0 * PI * 127.3 * t).sin());
        assert_eq!(ctcss, Some(Some(Tone::Ctcss(127.3))));
        let w = crate::tone::dcs_word(0o244);
        let dcs = make(&|t| {
            let bit = (t * crate::tone::DCS_BAUD) as usize % 23;
            if (w >> bit) & 1 == 1 { 600.0 } else { -600.0 }
        });
        assert_eq!(
            dcs,
            Some(Some(Tone::Dcs {
                code: 0o244,
                inverted: false
            }))
        );
        assert_eq!(make(&|_| 0.0), Some(None));
    }

    #[test]
    fn steady_carriers_are_dropped_and_locked_out() {
        let fs = params().sample_rate;
        let cfg = ScannerConfig {
            spur_secs: 3.0,
            ..Default::default()
        };
        // A carrier with modulation `dev(t)` from 0.3 to 6 s.
        let run = |dev: &dyn Fn(f64) -> f64| {
            let mut x = scene(7.0, &[]);
            let mut ph = 0.0f64;
            for (i, v) in x.iter_mut().enumerate() {
                let t = i as f64 / fs;
                if (0.3..6.0).contains(&t) {
                    let f = 100e3 + dev(t) * (2.0 * PI * 800.0 * t).sin();
                    ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
                    *v += Complex32::from_polar(0.1, ph as f32);
                }
            }
            let mut s = Scanner::with_params(
                cfg.clone(),
                params(),
                0.0,
                SystemTime::UNIX_EPOCH,
                &[slot(100e3)],
            );
            let mut out = Vec::new();
            for c in x.chunks(50_000) {
                s.push(c, &mut out);
            }
            s.finish(&mut out);
            (out, s.stats(), s.take_spurs())
        };
        // Unmodulated, and a steady tone: spurs.
        for dev in [0.0, 2000.0] {
            let (out, stats, spurs) = run(&|_| dev);
            assert_eq!(stats.spurs, 1, "{dev}: {stats:?}");
            assert_eq!(spurs, vec![100e3]);
            assert!(closes(&out).is_empty(), "{dev}");
            let id = out
                .iter()
                .find_map(|o| match o {
                    Output::Open(s) => Some(s.id),
                    _ => None,
                })
                .unwrap();
            assert!(out.contains(&Output::Discard { id }));
        }
        // Speech-like (talking in bursts with pauses): kept.
        let (out, stats, spurs) = run(&|t| if t % 0.7 < 0.4 { 3000.0 } else { 0.0 });
        assert_eq!(stats.spurs, 0, "{stats:?}");
        assert!(spurs.is_empty());
        assert_eq!(closes(&out).len(), 1);
    }

    #[test]
    fn afsk_slots_decode_packets_instead_of_recording() {
        use crate::afsk::tests::{afsk_audio, ui_frame};
        let fs = params().sample_rate;
        let frame = ui_frame("KF7HVM", "APRS", &[("WIDE2", 1, false)], b">scanner test");
        // Audio at 31.25 kHz, held per sample at 2 MS/s.
        let audio = afsk_audio(&frame, 31250.0, 0.6, 0.6);
        let mut x = scene(1.5, &[]);
        let on = (0.3 * fs) as usize;
        let mut ph = 0.0f64;
        let len = (audio.len() as f64 / 31250.0 * fs) as usize;
        for i in 0..len + (0.1 * fs) as usize {
            let a = audio
                .get((i as f64 * 31250.0 / fs) as usize)
                .copied()
                .unwrap_or(0.0);
            let f = 100e3 + 5000.0 * a as f64;
            ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
            x[on + i] += Complex32::from_polar(0.1, ph as f32);
        }
        let mut s = slot(100e3);
        s.mode = Mode::Afsk1200;
        s.origin = SlotOrigin::Listed;
        let (out, stats) = collect(&x, &[s], ScannerConfig::default());
        let packets: Vec<&Packet> = out
            .iter()
            .filter_map(|o| match o {
                Output::Packet(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(packets.len(), 1, "{stats:?}");
        assert_eq!(
            packets[0].frame.tnc2(),
            "KF7HVM-9>APRS,WIDE2-1:>scanner test"
        );
        let t = packets[0]
            .time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let end = 0.3 + audio.len() as f64 / 31250.0;
        assert!((t - end).abs() < 0.05, "{t} vs {end}");
        assert_eq!(s_opens(&out), 0, "no audio files");
        assert_eq!(stats.packets, 1);
    }

    #[test]
    fn dmr_slots_record_each_call_with_its_ids() {
        use crate::dmr::burst::Source;
        use crate::dmr::tx::{self, TxCall};
        let fs = params().sample_rate;
        let mut call = TxCall::group(99, 3_171_501, 3);
        call.alias = Some("KF7HVM".into());
        let bursts = tx::call_bursts(&call, Source::Mobile);
        let bb = tx::modulate(&tx::mobile_symbols(&bursts), fs, 0.1);
        let mut x = scene(2.0, &[]);
        let on = (0.4 * fs) as usize;
        for (i, (v, b)) in x[on..].iter_mut().zip(&bb).enumerate() {
            let t = (on + i) as f64 / fs;
            let ph = (2.0 * PI * 100e3 * t).rem_euclid(2.0 * PI);
            *v += b * Complex32::from_polar(1.0, ph as f32);
        }
        let mut s = slot(100e3);
        s.mode = Mode::Dmr;
        s.bandwidth = 11e3;
        s.origin = SlotOrigin::Listed;
        let (out, stats) = collect(&x, &[s], ScannerConfig::default());
        assert_eq!(s_opens(&out), 1, "{stats:?}");
        let Some(Output::Open(start)) = out.iter().find(|o| matches!(o, Output::Open(_))) else {
            unreachable!()
        };
        assert_eq!(start.audio_rate, 8000);
        let t0 = start
            .start_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        // Voice starts after the two header bursts (and their 60 ms frames).
        assert!((t0 - 0.52).abs() < 0.05, "{t0}");
        let frames: usize = out
            .iter()
            .map(|o| match o {
                Output::Ambe { frames, .. } => frames.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(frames, 3 * 6 * 3);
        assert_eq!(audio_of(&out, start.id).len(), frames * 160);
        let ends = closes(&out);
        assert_eq!(ends.len(), 1);
        let info = ends[0].digital.as_ref().expect("call info");
        let lc = info.lc.unwrap();
        assert_eq!((lc.dst, lc.src), (99, 3_171_501));
        assert_eq!(info.alias.as_deref(), Some("KF7HVM"));
        assert!(
            (ends[0].signal_secs - 1.08).abs() < 0.1,
            "{}",
            ends[0].signal_secs
        );
        assert_eq!(stats.calls, 1);
    }

    #[test]
    fn serial_and_parallel_match() {
        // Enough simultaneous signals to take the parallel path (>= 12).
        let sigs: Vec<(f64, f64, f64, f64)> = (0..14)
            .map(|k| ((k as f64 - 7.0) * 50e3, 500.0 + 50.0 * k as f64, 0.3, 1.5))
            .collect();
        let x = scene(2.0, &sigs);
        let plan: Vec<Slot> = (-16..=16).map(|k| slot(k as f64 * 25e3)).collect();
        let (a, sa) = collect(&x, &plan, ScannerConfig::default());
        assert!(sa.max_concurrent >= 12, "{sa:?}");
        let (b, _) = collect(
            &x,
            &plan,
            ScannerConfig {
                parallel: false,
                ..Default::default()
            },
        );
        assert_eq!(a, b);
    }
}
