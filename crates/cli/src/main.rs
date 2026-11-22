//! `scanner`: the sdr-scanner command line.
//!
//! Phase 0 subcommands are diagnostics used by tests and CI:
//! - `scanner info <recording>`: SigMF metadata and signal level
//! - `scanner peaks <recording>`: averaged spectrum peaks, in RF frequency
//! - `scanner config check <file> [--profile NAME]`: validate band profiles

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use num_complex::Complex32;
use scanner_core::config::{ConfigFile, SlotOrigin};
use scanner_core::psd::{Welch, find_peaks, noise_floor_db};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "scanner", version, about = "Multi-channel NBFM scanner")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show a recording's metadata and overall level.
    Info { recording: PathBuf },
    /// List spectral peaks in a recording (averaged over the whole file).
    Peaks {
        recording: PathBuf,
        /// FFT size (power of two).
        #[arg(long, default_value_t = 8192)]
        fft: usize,
        /// Report peaks this many dB above the noise floor.
        #[arg(long, default_value_t = 10.0)]
        threshold: f32,
        /// Merge regions closer than this many Hz.
        #[arg(long, default_value_t = 10e3)]
        merge: f64,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Work with band-profile config files.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate a config file and summarise each profile's channel plan.
    Check {
        file: PathBuf,
        /// Only check this profile (default: all).
        #[arg(long)]
        profile: Option<String>,
        /// List every slot, not just a summary.
        #[arg(long)]
        slots: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info { recording } => info(recording),
        Cmd::Peaks {
            recording,
            fft,
            threshold,
            merge,
            json,
        } => peaks(recording, fft, threshold, merge, json),
        Cmd::Config {
            cmd:
                ConfigCmd::Check {
                    file,
                    profile,
                    slots,
                },
        } => config_check(file, profile, slots),
    }
}

fn open(path: &Path) -> Result<FileSource> {
    FileSource::open(path).with_context(|| format!("opening {}", path.display()))
}

fn info(path: PathBuf) -> Result<()> {
    let mut src = open(&path)?;
    let i = src.info().clone();
    println!("source:      {}", i.description);
    println!("format:      {}", src.format().sigmf_name());
    println!("sample rate: {} S/s", i.sample_rate);
    println!("center:      {}", Hz(i.center_freq));
    if let Some(n) = i.total_samples {
        println!("samples:     {n} ({:.3} s)", n as f64 / i.sample_rate);
    }
    if let Some(d) = &src.meta().captures.first().and_then(|c| c.datetime.clone()) {
        println!("start:       {d}");
    }
    println!("annotations: {}", src.meta().annotations.len());

    let mut buf = vec![Complex32::default(); 1 << 16];
    let (mut sum, mut peak, mut n) = (0f64, 0f32, 0u64);
    loop {
        let k = src.read(&mut buf)?;
        if k == 0 {
            break;
        }
        for s in &buf[..k] {
            let p = s.norm_sqr();
            sum += p as f64;
            peak = peak.max(s.re.abs()).max(s.im.abs());
        }
        n += k as u64;
    }
    if n > 0 {
        println!("mean power:  {:.1} dBFS", 10.0 * (sum / n as f64).log10());
        println!(
            "peak |I|,|Q|: {:.4} ({:.1} dBFS)",
            peak,
            20.0 * peak.max(1e-12).log10()
        );
    }
    Ok(())
}

fn peaks(path: PathBuf, fft: usize, threshold: f32, merge: f64, json: bool) -> Result<()> {
    if !fft.is_power_of_two() || fft < 16 {
        bail!("--fft must be a power of two >= 16");
    }
    let mut src = open(&path)?;
    let fs = src.info().sample_rate;
    let fc = src.info().center_freq;
    let mut welch = Welch::new(fft);
    let mut buf = vec![Complex32::default(); 1 << 16];
    loop {
        let k = src.read(&mut buf)?;
        if k == 0 {
            break;
        }
        welch.push(&buf[..k]);
    }
    let spec = welch.spectrum_db();
    let floor = noise_floor_db(&spec, 0.2);
    let found = find_peaks(&spec, fs, threshold, merge);
    if json {
        print!("[");
        for (k, p) in found.iter().enumerate() {
            if k > 0 {
                print!(",");
            }
            print!(
                "{{\"freq_hz\":{:.1},\"power_db\":{:.2},\"snr_db\":{:.2},\"width_hz\":{:.1}}}",
                fc + p.offset_hz,
                p.power_db,
                p.snr_db,
                p.width_hz
            );
        }
        println!("]");
    } else {
        println!(
            "{} frames, {:.1} Hz bins, noise floor {:.1} dB/bin",
            welch.frames(),
            fs / fft as f64,
            floor
        );
        println!(
            "{:>16} {:>9} {:>8} {:>10}",
            "frequency", "power dB", "SNR dB", "width"
        );
        for p in &found {
            println!(
                "{:>16} {:>9.1} {:>8.1} {:>10}",
                Hz(fc + p.offset_hz).to_string(),
                p.power_db,
                p.snr_db,
                Hz(p.width_hz).to_string()
            );
        }
    }
    Ok(())
}

fn config_check(file: PathBuf, only: Option<String>, list_slots: bool) -> Result<()> {
    let cfg = ConfigFile::load(&file)?;
    let names: Vec<String> = match only {
        Some(n) => vec![n],
        None => cfg.profiles.keys().cloned().collect(),
    };
    if names.is_empty() {
        bail!("{} defines no profiles", file.display());
    }
    let mut warned = false;
    for name in names {
        let p = cfg.profile(&name)?;
        let plan = p.channel_plan();
        let count = |o: SlotOrigin| plan.slots.iter().filter(|s| s.origin == o).count();
        println!(
            "{name}: {}–{} ({}), {} slots: {} listed, {} repeater outputs, {} inputs, {} grid; {} not recorded",
            p.span.start,
            p.span.stop,
            Hz(p.span.width()),
            plan.slots.len(),
            count(SlotOrigin::Listed),
            count(SlotOrigin::RepeaterOutput),
            count(SlotOrigin::RepeaterInput),
            count(SlotOrigin::Grid),
            plan.slots.iter().filter(|s| !s.record).count(),
        );
        for w in &plan.warnings {
            warned = true;
            println!("  warning: {w}");
        }
        if list_slots {
            for s in &plan.slots {
                println!(
                    "  {:>14}  bw {:>10}  {:?}{}{}  {}",
                    Hz(s.freq).to_string(),
                    Hz(s.bandwidth).to_string(),
                    s.mode,
                    if s.priority { " priority" } else { "" },
                    if s.record { "" } else { " no-record" },
                    s.label()
                );
            }
        }
    }
    if warned {
        eprintln!("config has warnings");
    }
    Ok(())
}
