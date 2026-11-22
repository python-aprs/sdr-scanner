//! The offline/online scanning pipeline: forward FFT → detector → per-slot
//! NBFM chains → transmission outputs (PLAN.md 3.2–3.4).
//!
//! [`Scanner::push`] is fed IQ samples in any chunk size and appends
//! [`Output`] records (transmission open, audio, close) for a writer to turn
//! into files. Channel chains run in parallel on the rayon pool.

use crate::channelizer::{Analyzer, BankParams, Block, Extractor, FilterBank};
use crate::clock::SampleClock;
use crate::config::{Mode, Slot, SlotOrigin};
use crate::detector::{Detector, DetectorParams, Event, SlotState};
use crate::dsp::{Deemphasis, FmDiscriminator, HighPass, Resampler};
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
    /// tail) are dropped.
    pub min_len: f64,
    /// Audio kept after the signal ends (squelch tail).
    pub tail: f64,
    /// Audio level for full deviation (1.0 = full scale).
    pub audio_gain: f32,
    /// High-pass corner for removing CTCSS tones; 0 disables.
    pub highpass_hz: f64,
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
            highpass_hz: 300.0,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Open(TxStart),
    Audio { id: u64, samples: Vec<f32> },
    Close(TxEnd),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScannerStats {
    pub blocks: u64,
    pub samples: u64,
    pub opened: u64,
    pub recorded: u64,
    pub discarded_short: u64,
    pub not_recorded: u64,
    /// Grid detections that retuned onto a signal already being recorded.
    pub duplicates: u64,
    pub max_concurrent: usize,
}

struct Chain {
    id: u64,
    slot: usize,
    /// The slot as recorded (frequency retuned for grid slots).
    tuned: Slot,
    extractor: Extractor,
    disc: FmDiscriminator,
    deemph: Option<Deemphasis>,
    resampler: Resampler,
    hp: Option<HighPass>,
    gain: f32,
    /// Audio samples still to drop for the resampler's group delay.
    skip: usize,
    /// Audio not yet emitted.
    pending: Vec<f32>,
    /// Audio samples emitted so far.
    emitted: usize,
    confirmed: bool,
    /// Stream sample (input rate) of audio sample 0, may be negative at
    /// stream start.
    audio_t0: i64,
    iq: Vec<Complex32>,
    demod: Vec<f32>,
}

impl Chain {
    fn process(&mut self, block: &Block) {
        self.iq.clear();
        self.extractor.process(block, &mut self.iq);
        self.demod.clear();
        self.disc.process(&self.iq, &mut self.demod);
        if let Some(d) = &mut self.deemph {
            d.process(&mut self.demod);
        }
        let start = self.pending.len();
        self.resampler.process(&self.demod, &mut self.pending);
        if self.skip > 0 {
            let k = self.skip.min(self.pending.len() - start);
            self.pending.drain(start..start + k);
            self.skip -= k;
        }
        let new = &mut self.pending[start..];
        if let Some(h) = &mut self.hp {
            h.process(new);
        }
        for v in new {
            *v = (*v * self.gain).clamp(-1.0, 1.0);
        }
    }

    /// Emit everything except the last `holdback` samples.
    fn drain_ready(&mut self, holdback: usize, out: &mut Vec<Output>) {
        if self.pending.len() > holdback {
            let n = self.pending.len() - holdback;
            let samples: Vec<f32> = self.pending.drain(..n).collect();
            self.emitted += samples.len();
            out.push(Output::Audio {
                id: self.id,
                samples,
            });
        }
    }
}

pub struct Scanner {
    cfg: ScannerConfig,
    bank: FilterBank,
    analyzer: Analyzer,
    detector: Detector,
    clock: SampleClock,
    /// Recent blocks for pre-roll, oldest first.
    history: VecDeque<Arc<Block>>,
    history_len: usize,
    spare: Vec<Vec<Complex32>>,
    chains: Vec<Chain>,
    next_id: u64,
    events: Vec<Event>,
    stats: ScannerStats,
    last_block: Option<u64>,
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
        self.history.push_back(block.clone());
        while self.history.len() > self.history_len {
            let old = self.history.pop_front().unwrap();
            if let Ok(b) = Arc::try_unwrap(old) {
                self.spare.push(b.bins);
            }
        }
        self.last_block = Some(block.index);

        let mut events = std::mem::take(&mut self.events);
        events.clear();
        self.detector.update(&block, &mut events);

        // Existing chains process this block (new chains catch up from
        // history when they open, below).
        // Each chain is ~50 µs of work per block; below a dozen chains the
        // fan-out costs more than it saves.
        if self.cfg.parallel && self.chains.len() >= 12 {
            self.chains.par_iter_mut().for_each(|c| c.process(&block));
        } else {
            for c in &mut self.chains {
                c.process(&block);
            }
        }

        for e in events.drain(..) {
            self.on_event(e, out);
        }
        self.events = events;

        // Confirm transmissions that passed the minimum length, and stream
        // audio that can no longer be trimmed away by a close.
        let block_secs = self.bank.params().block_secs();
        let holdback = self.holdback();
        let min_blocks = (self.cfg.min_len / block_secs).ceil() as u64;
        let preroll = self.detector.signal_offset_blocks();
        for c in &mut self.chains {
            if !c.confirmed {
                // Only time with the signal present counts, not hang time.
                if let SlotState::Active { start } = self.detector.state(c.slot) {
                    if block.index + 1 >= start + preroll + min_blocks {
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
    }

    /// Audio held back so that a close can trim the hang time.
    fn holdback(&self) -> usize {
        let hang = self.detector.hang_blocks() as f64 * self.bank.params().block_secs();
        ((hang + 0.1) * self.cfg.audio_rate as f64).ceil() as usize
    }

    fn tx_start(clock: &SampleClock, c: &Chain, audio_rate: u32) -> TxStart {
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
                if !ws.slot.record || ws.slot.mode != Mode::Nbfm {
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
                let rate = ex.output_rate();
                let audio_rate = self.cfg.audio_rate as f64;
                let cutoff = (audio_rate / 2.0 * 0.85).min(ws.slot.bandwidth / 2.0);
                let resampler = Resampler::new(rate, audio_rate, cutoff, audio_rate / 2.0 - cutoff);
                let skip = resampler.delay_out().round() as usize;
                let first = self
                    .history
                    .iter()
                    .find(|b| b.index >= start_block)
                    .cloned();
                let audio_t0 = match &first {
                    Some(b) => b.first_sample as i64 - (self.bank.params().overlap() / 2) as i64,
                    None => 0,
                };
                let mut c = Chain {
                    id: self.next_id,
                    slot,
                    tuned,
                    disc: FmDiscriminator::new(rate, ws.slot.deviation),
                    deemph: self
                        .cfg
                        .deemphasis
                        .then(|| Deemphasis::new(rate, self.cfg.deemphasis_tau)),
                    resampler,
                    hp: (self.cfg.highpass_hz > 0.0)
                        .then(|| HighPass::new(audio_rate, self.cfg.highpass_hz)),
                    gain: self.cfg.audio_gain,
                    skip,
                    pending: Vec::new(),
                    emitted: 0,
                    confirmed: false,
                    audio_t0,
                    iq: Vec::new(),
                    demod: Vec::new(),
                    extractor: ex,
                };
                self.next_id += 1;
                for b in self
                    .history
                    .iter()
                    .filter(|b| b.index >= start_block && b.index <= block)
                {
                    c.process(b);
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
                let end_sample = (end_block + 1) * p.hop as u64;
                let signal_start =
                    (start_block + self.detector.signal_offset_blocks()) * p.hop as u64;
                let signal_secs = end_sample.saturating_sub(signal_start) as f64 / p.sample_rate;
                if !c.confirmed && signal_secs < self.cfg.min_len {
                    self.stats.discarded_short += 1;
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
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    let f = off + 3000.0 * (2.0 * PI * tone * t).sin();
                    ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
                    *v += Complex32::from_polar(0.1, ph as f32);
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
            // Level: 3 kHz of 5 kHz deviation, de-emphasised, gain 0.5.
            let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
            assert!(rms > 0.05 && rms < 0.3, "rms {rms}");
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
