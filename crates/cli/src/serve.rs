//! `scanner serve`: the long-running scanner (PLAN.md 3.4).
//!
//! A radio thread feeds a lock-free ring (a hardware driver callback in
//! Phase 2; for now a SigMF recording replayed at its real rate, optionally
//! looped, which exercises exactly the same path). The DSP thread reads the
//! ring, runs the scanner and hands outputs to the recorder thread. Ctrl-C
//! stops cleanly: open transmissions are closed and written.

use anyhow::{Context, Result, bail};
use num_complex::Complex32;
use scanner_core::scanner::{Output, Scanner, ScannerConfig};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source, SourceInfo, ring};
use scanner_store::Recorder;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Where samples come from.
#[derive(Debug, Clone)]
pub enum SourceSpec {
    /// Replay a recording at its sample rate, like a live radio.
    Replay { path: PathBuf, looped: bool },
}

impl std::str::FromStr for SourceSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if let Some(p) = s.strip_prefix("replay:") {
            return Ok(SourceSpec::Replay {
                path: p.into(),
                looped: false,
            });
        }
        if let Some(p) = s.strip_prefix("loop:") {
            return Ok(SourceSpec::Replay {
                path: p.into(),
                looped: true,
            });
        }
        match s {
            "sdrplay" | "airspy" => Err(format!("{s}: hardware backends arrive in Phase 2")),
            _ => Err(format!(
                "unknown source {s:?} (try replay:<recording> or loop:<recording>)"
            )),
        }
    }
}

/// Samples per ring block (~6.5 ms at 10 MS/s), and blocks in the ring
/// (~0.85 s of buffering at 10 MS/s).
const BLOCK: usize = 1 << 16;
const BLOCKS: usize = 128;

fn replay_thread(
    path: PathBuf,
    looped: bool,
    mut producer: scanner_sources::RingProducer,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut buf = vec![Complex32::default(); 1 << 14];
    let mut sent = 0u64;
    let t0 = Instant::now();
    let mut rate = 0.0;
    loop {
        let mut src = FileSource::open(&path)?;
        rate = if rate == 0.0 {
            src.info().sample_rate
        } else {
            rate
        };
        loop {
            if stop.load(Ordering::Relaxed) {
                producer.close();
                return Ok(());
            }
            let k = src.read(&mut buf)?;
            if k == 0 {
                break;
            }
            // Pace like a radio: deliver no faster than the sample rate.
            let due = Duration::from_secs_f64(sent as f64 / rate);
            if let Some(wait) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(wait);
            }
            producer.push(&buf[..k]);
            sent += k as u64;
        }
        if !looped {
            producer.close();
            return Ok(());
        }
    }
}

pub struct ServeOptions<'a> {
    pub source: SourceSpec,
    pub scanner: ScannerConfig,
    pub plan: &'a [scanner_core::config::Slot],
    pub out: &'a Path,
    pub status_every: Duration,
}

pub fn serve(o: ServeOptions) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            if stop.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            eprintln!("stopping (Ctrl-C again to quit immediately)");
        })
        .context("installing Ctrl-C handler")?;
    }

    let (info, radio) = match &o.source {
        SourceSpec::Replay { path, looped } => {
            let f =
                FileSource::open(path).with_context(|| format!("opening {}", path.display()))?;
            let fi = f.info().clone();
            let info = SourceInfo {
                // A live stream starts now, whatever the recording says.
                start_time: Some(SystemTime::now()),
                total_samples: None,
                description: format!(
                    "{} ({})",
                    fi.description,
                    if *looped { "looped" } else { "replay" }
                ),
                ..fi
            };
            (info, (path.clone(), *looped))
        }
    };
    let (producer, mut source) = ring::ring(BLOCKS, BLOCK, info.clone());
    let radio_stop = stop.clone();
    let radio = std::thread::Builder::new()
        .name("radio".into())
        .spawn(move || replay_thread(radio.0, radio.1, producer, radio_stop))?;

    let t0 = info.start_time.unwrap_or_else(SystemTime::now);
    let mut scanner = Scanner::new(o.scanner, info.sample_rate, info.center_freq, t0, o.plan);
    let p = *scanner.params();
    eprintln!(
        "{}: {} S/s at {}, FFT {} ({:.1} Hz bins), watching {} slots, recording to {}",
        info.description,
        info.sample_rate,
        Hz(info.center_freq),
        p.n,
        p.bin_hz(),
        scanner.detector().slots().len(),
        o.out.display()
    );
    if scanner.detector().slots().is_empty() {
        bail!("no slots of the profile fall inside the capture");
    }

    let recorder = Recorder::new(o.out)?;
    let (tx, writer) = recorder.spawn(256);
    let mut buf = vec![Complex32::default(); BLOCK];
    let mut out: Vec<Output> = Vec::new();
    let started = Instant::now();
    let mut next_status = started + o.status_every;
    let mut busy = Duration::ZERO;
    loop {
        let k = source.read(&mut buf)?;
        if k == 0 {
            break;
        }
        let t = Instant::now();
        scanner.push(&buf[..k], &mut out);
        busy += t.elapsed();
        if !out.is_empty() {
            for o in &out {
                if let Output::Open(s) = o {
                    eprintln!(
                        "  open  {} {}",
                        Hz(s.slot.freq),
                        s.slot.name.as_deref().unwrap_or("")
                    );
                }
            }
            tx.send(std::mem::take(&mut out))
                .ok()
                .context("recorder thread stopped")?;
        }
        if Instant::now() >= next_status {
            let wall = started.elapsed().as_secs_f64();
            let st = scanner.stats();
            let rs = source.stats_ring();
            let active: Vec<String> = scanner
                .active()
                .iter()
                .map(|(f, snr, secs)| format!("{:.4} ({snr:.0} dB, {secs:.0} s)", f / 1e6))
                .collect();
            eprintln!(
                "status: {:.0} s, DSP {:.0}% of a core, ring {}/{} blocks, dropped {} samples, recorded {}, active [{}]",
                wall,
                busy.as_secs_f64() / wall * 100.0,
                rs.blocks_written - rs.blocks_read,
                rs.capacity,
                rs.dropped_samples,
                st.recorded,
                active.join(", ")
            );
            next_status += o.status_every;
        }
    }
    scanner.finish(&mut out);
    tx.send(out).ok();
    drop(tx);
    radio.join().expect("radio thread panicked")?;
    let rec = writer.join().expect("recorder thread panicked")?;
    eprintln!(
        "stopped: {} transmissions, {:.1} s of audio, {} samples dropped",
        rec.files,
        rec.audio_secs,
        source.stats().samples_dropped
    );
    Ok(())
}
