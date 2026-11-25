//! Stage timings for the scanning pipeline at 10 MS/s (PLAN.md 4.5).
//!
//!     cargo bench -p scanner-core --bench pipeline
//!
//! Reports nanoseconds per input sample and the share of one core needed at
//! 10 MS/s for: the forward FFT (analyzer), the detector, and one NBFM
//! channel chain.

use num_complex::Complex32;
use scanner_core::channelizer::{Analyzer, BankParams, Block, FilterBank};
use scanner_core::config::{Mode, Slot, SlotOrigin};
use scanner_core::detector::{Detector, DetectorParams};
use scanner_core::dsp::{Deemphasis, FmDiscriminator, HighPass, Resampler};
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let fs = 10e6;
    let p = BankParams::for_rate(fs);
    let secs = 3.0;
    let n = (fs * secs) as usize;
    let mut seed = 1u64;
    let x: Vec<Complex32> = (0..n)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            Complex32::new(
                (seed >> 40) as f32 / 1.6e7 - 0.5,
                (seed >> 16 & 0xffffff) as f32 / 1.6e7 - 0.5,
            )
        })
        .collect();

    // Forward FFT.
    let mut an = Analyzer::new(p);
    let mut blocks: Vec<Block> = Vec::new();
    let t = Instant::now();
    an.push(&x, || None, |b| blocks.push(b));
    let fft_s = t.elapsed().as_secs_f64();

    // Detector over the 2 m channel plan (161 grid slots).
    let plan: Vec<Slot> = (0..=160)
        .map(|k| Slot {
            freq: 144e6 + k as f64 * 25e3,
            bandwidth: 16e3,
            deviation: 5e3,
            mode: Mode::Nbfm,
            priority: false,
            record: true,
            name: None,
            origin: SlotOrigin::Grid,
            pair: None,
            sites: Vec::new(),
        })
        .collect();
    let mut det = Detector::new(DetectorParams::default(), p, 146e6, &plan);
    let mut ev = Vec::new();
    let t = Instant::now();
    for b in &blocks {
        det.update(b, &mut ev);
    }
    let det_s = t.elapsed().as_secs_f64();

    // One channel chain.
    let mut bank = FilterBank::new(p, 25_000.0);
    let mut ex = bank.extractor(312_500.0, 16e3);
    let rate = ex.output_rate();
    let mut disc = FmDiscriminator::new(rate, 5e3);
    let mut de = Deemphasis::new(rate, 530e-6);
    let mut rs = Resampler::new(rate, 8000.0, 3400.0, 1000.0);
    let mut hp = HighPass::new(8000.0, 300.0);
    let (mut iq, mut dm, mut au) = (Vec::new(), Vec::new(), Vec::new());
    let (mut t_ex, mut t_dm, mut t_rs) = (0.0, 0.0, 0.0);
    for b in &blocks {
        iq.clear();
        dm.clear();
        au.clear();
        let t = Instant::now();
        ex.process(b, &mut iq);
        t_ex += t.elapsed().as_secs_f64();
        let t = Instant::now();
        disc.process(&iq, &mut dm);
        de.process(&mut dm);
        t_dm += t.elapsed().as_secs_f64();
        let t = Instant::now();
        rs.process(&dm, &mut au);
        hp.process(&mut au);
        t_rs += t.elapsed().as_secs_f64();
        black_box(&au);
    }
    let ch_s = t_ex + t_dm + t_rs;

    let real = blocks.len() as f64 * p.block_secs();
    let show = |name: &str, s: f64| {
        println!(
            "{name:<28} {:>8.2} ns/sample {:>7.1} % of a core at 10 MS/s",
            s / real / fs * 1e9,
            s / real * 100.0
        );
    };
    println!(
        "FFT {} ({:.1} Hz bins), {} blocks, {:.2} s of signal",
        p.n,
        p.bin_hz(),
        blocks.len(),
        real
    );
    show("forward FFT", fft_s);
    show("detector (161 slots)", det_s);
    show("one channel: extract", t_ex);
    show("one channel: discriminator", t_dm);
    show("one channel: resample+HPF", t_rs);
    show("one channel: total", ch_s);
    println!(
        "estimated cores at 10 MS/s with 8 channels: {:.2}",
        (fft_s + det_s + 8.0 * ch_s) / real
    );
}
