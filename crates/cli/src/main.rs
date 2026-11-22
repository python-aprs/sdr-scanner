//! `scanner`: the sdr-scanner command line.
//!
//! Phase 0 subcommands are diagnostics used by tests and CI:
//! - `scanner info <recording>`: SigMF metadata and signal level
//! - `scanner peaks <recording>`: averaged spectrum peaks, in RF frequency
//! - `scanner config check <file> [--profile NAME]`: validate band profiles
//!
//! Phase 1:
//! - `scanner run <recording> --config FILE`: scan a recording and write one
//!   WAV per transmission plus the SQLite index

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use num_complex::Complex32;
use scanner_core::config::{ConfigFile, SlotOrigin};
use scanner_core::psd::{Welch, find_peaks, noise_floor_db};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source};
use std::path::{Path, PathBuf};

mod admin;
mod serve;

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
    /// Scan a recording: detect transmissions and write one WAV per
    /// transmission, plus `index.sqlite`, under `--out`.
    Run(RunArgs),
    /// List SDRplay radios the API can see.
    Devices,
    /// Manage admin accounts, API tokens and sessions for the web API.
    Admin(admin::AdminArgs),
    /// Run continuously from a source, recording until Ctrl-C.
    Serve(ServeArgs),
    /// Work with band-profile config files.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    recording: PathBuf,
    #[command(flatten)]
    tuning: Tuning,
    /// Pace reading at the recording's sample rate (like a live radio).
    #[arg(long)]
    realtime: bool,
    /// Print a JSON summary instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// `sdrplay` (the first SDRplay radio), `sdrplay:<serial>`, or
    /// `replay:<recording>` / `loop:<recording>` (a recording played at its
    /// real rate through the live path).
    #[arg(long)]
    source: serve::SourceSpec,
    #[command(flatten)]
    tuning: Tuning,
    #[command(flatten)]
    radio: RadioArgs,
    /// Seconds between status lines.
    #[arg(long, default_value_t = 10.0)]
    status_every: f64,
    /// Serve the HTTP API on this address ("off" to disable). Admin routes
    /// need a login or an API token (see `scanner admin`).
    #[arg(long, default_value = "127.0.0.1:8080")]
    http: String,
    /// Let anonymous visitors list recordings.
    #[arg(long)]
    public_recordings: bool,
    /// Let anonymous visitors download audio.
    #[arg(long)]
    public_audio: bool,
    /// Hide live status from anonymous visitors.
    #[arg(long)]
    private_live: bool,
    /// Accounts database (default: auth.sqlite in --out).
    #[arg(long)]
    auth_db: Option<PathBuf>,
    /// Leave `Secure` off the session cookie, for plain HTTP on a trusted
    /// LAN. (Browsers treat localhost as secure, so local use doesn't need it.)
    #[arg(long)]
    insecure_cookies: bool,
}

/// Radio settings for `serve --source sdrplay`.
#[derive(clap::Args)]
struct RadioArgs {
    /// Tuner frequency in MHz (default: the middle of the profile's span plus
    /// 12.5 kHz, so the DC residual sits between 25 kHz channels).
    #[arg(long)]
    center_mhz: Option<f64>,
    /// Sample rate in S/s (2 to 10.66 MS/s).
    #[arg(long, default_value_t = 8e6)]
    rate: f64,
    /// LNA state: 0 is the most gain (0 to 9 for an RSP1A on 2 m).
    #[arg(long, default_value_t = 3)]
    lna: u8,
    /// IF gain reduction in dB, 20 to 59 (more is less gain).
    #[arg(long, default_value_t = 40)]
    gr: u8,
    /// IF filter bandwidth in kHz (8000, 7000, 6000, 5000, 1536, ...).
    #[arg(long, default_value_t = 8000)]
    if_bw_khz: u32,
    /// Turn on the radio's AGC with this set point in dBFS (e.g. -30).
    /// Off by default: fixed gain keeps the noise floor steady.
    #[arg(long, allow_hyphen_values = true)]
    agc: Option<i32>,
    /// Frequency correction in parts per million.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    ppm: f64,
    /// Power an LNA or preamp over the coax.
    #[arg(long)]
    bias_t: bool,
    /// Switch in the RSP1A's broadcast FM band-stop filter.
    #[arg(long)]
    fm_notch: bool,
    /// Switch in the RSP1A's DAB band-stop filter.
    #[arg(long)]
    dab_notch: bool,
    /// Use USB bulk transfers instead of isochronous.
    #[arg(long)]
    usb_bulk: bool,
}

impl RadioArgs {
    fn sdrplay(
        &self,
        profile: &scanner_core::config::Profile,
    ) -> scanner_sources::sdrplay::SdrplayConfig {
        scanner_sources::sdrplay::SdrplayConfig {
            serial: None,
            sample_rate: self.rate,
            center_hz: self
                .center_mhz
                .map(|m| m * 1e6)
                .unwrap_or(profile.span.center() + 12.5e3),
            bandwidth_khz: self.if_bw_khz,
            lna_state: self.lna,
            gr_db: self.gr,
            agc_setpoint_dbfs: self.agc,
            ppm: self.ppm,
            bias_t: self.bias_t,
            rf_notch: self.fm_notch,
            dab_notch: self.dab_notch,
            usb_bulk: self.usb_bulk,
        }
    }
}

/// Options shared by `run` and `serve`.
#[derive(clap::Args)]
struct Tuning {
    /// Band-profile config file.
    #[arg(long)]
    config: PathBuf,
    /// Profile to use (default: the first profile in the file).
    #[arg(long)]
    profile: Option<String>,
    /// Output folder.
    #[arg(long, default_value = "out")]
    out: PathBuf,
    /// SNR (dB in the slot bandwidth) to open a transmission.
    #[arg(long, default_value_t = 6.0)]
    open_db: f32,
    /// SNR to keep a transmission open.
    #[arg(long, default_value_t = 3.0)]
    close_db: f32,
    /// Seconds below the close threshold before a transmission ends.
    #[arg(long, default_value_t = 1.5)]
    hang: f64,
    /// Drop transmissions shorter than this (seconds).
    #[arg(long, default_value_t = 0.3)]
    min_len: f64,
    /// Most transmissions recorded at once.
    #[arg(long, default_value_t = 32)]
    max_active: usize,
    /// Half-width (Hz) of the notch over the tuner's DC residual. Zero-IF
    /// radios (RSP1A) need a few kHz; 0 disables.
    #[arg(long, default_value_t = 6000.0)]
    dc_notch: f64,
    /// Run channels on one thread.
    #[arg(long)]
    serial: bool,
}

impl Tuning {
    fn load(
        &self,
    ) -> Result<(
        scanner_core::config::Profile,
        scanner_core::config::ChannelPlan,
    )> {
        let cfg = ConfigFile::load(&self.config)?;
        let name = match &self.profile {
            Some(n) => n.clone(),
            None => cfg
                .profiles
                .keys()
                .next()
                .cloned()
                .context("config defines no profiles")?,
        };
        let profile = cfg.profile(&name)?;
        let plan = profile.channel_plan();
        for w in &plan.warnings {
            eprintln!("warning: {w}");
        }
        Ok((profile, plan))
    }

    fn scanner_config(
        &self,
        profile: &scanner_core::config::Profile,
    ) -> scanner_core::scanner::ScannerConfig {
        use scanner_core::detector::DetectorParams;
        scanner_core::scanner::ScannerConfig {
            detector: DetectorParams {
                open_db: self.open_db,
                close_db: self.close_db,
                hang: self.hang,
                max_active: self.max_active,
                dc_notch_hz: self.dc_notch,
                grid_window_hz: profile.spacing.map(|s| s.0).unwrap_or(0.0),
                ..Default::default()
            },
            audio_rate: profile.audio_rate,
            deemphasis: profile.deemphasis,
            min_len: self.min_len,
            parallel: !self.serial,
            ..Default::default()
        }
    }
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
        Cmd::Run(args) => run(args),
        Cmd::Devices => devices(),
        Cmd::Admin(a) => admin::run(a),
        Cmd::Serve(a) => {
            let (profile, plan) = a.tuning.load()?;
            let sdrplay = a.radio.sdrplay(&profile);
            if matches!(a.source, serve::SourceSpec::Sdrplay { .. }) {
                sdrplay
                    .validate()
                    .map_err(|e| anyhow::anyhow!("radio settings: {e}"))?;
                let half = sdrplay.sample_rate / 2.0;
                if profile.span.start.0 < sdrplay.center_hz - half
                    || profile.span.stop.0 > sdrplay.center_hz + half
                {
                    eprintln!(
                        "warning: {} S/s at {} covers only part of the profile's span",
                        sdrplay.sample_rate,
                        Hz(sdrplay.center_hz)
                    );
                }
            }
            let http = match a.http.as_str() {
                "off" | "" => None,
                s => Some(
                    s.parse()
                        .with_context(|| format!("--http {s:?} is not an address"))?,
                ),
            };
            serve::serve(serve::ServeOptions {
                source: a.source,
                sdrplay,
                profile_name: profile.name.clone(),
                http,
                public: scanner_web::Public {
                    live: !a.private_live,
                    recordings: a.public_recordings,
                    audio: a.public_audio,
                },
                auth_db: a
                    .auth_db
                    .clone()
                    .unwrap_or_else(|| a.tuning.out.join("auth.sqlite")),
                insecure_cookies: a.insecure_cookies,
                scanner: a.tuning.scanner_config(&profile),
                plan: &plan.slots,
                span: (profile.span.start.0, profile.span.stop.0),
                out: &a.tuning.out,
                status_every: std::time::Duration::from_secs_f64(a.status_every.max(0.5)),
            })
        }
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

#[cfg(feature = "sdrplay")]
fn devices() -> Result<()> {
    let devs = scanner_sources::sdrplay::list_devices()?;
    if devs.is_empty() {
        println!("no SDRplay radios found");
    }
    for d in devs {
        println!("sdrplay:{}  {}", d.serial, d.model);
    }
    Ok(())
}

#[cfg(not(feature = "sdrplay"))]
fn devices() -> Result<()> {
    bail!("this scanner was built without SDRplay support; rebuild with `--features sdrplay`")
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

fn run(a: RunArgs) -> Result<()> {
    use scanner_core::scanner::{Output, Scanner};
    use scanner_store::Recorder;
    use std::time::{Instant, SystemTime};

    let (profile, plan) = a.tuning.load()?;
    let src = open(&a.recording)?.realtime(a.realtime);
    let info = src.info().clone();
    let t0 = info.start_time.unwrap_or(SystemTime::now());
    let sc = a.tuning.scanner_config(&profile);
    let mut scanner = Scanner::new(sc, info.sample_rate, info.center_freq, t0, &plan.slots);
    let p = *scanner.params();
    if !a.json {
        eprintln!(
            "{}: {} S/s at {}, FFT {} ({:.1} Hz bins, {:.1} ms blocks), watching {} of {} slots",
            info.description,
            info.sample_rate,
            Hz(info.center_freq),
            p.n,
            p.bin_hz(),
            p.block_secs() * 1e3,
            scanner.detector().slots().len(),
            plan.slots.len()
        );
    }

    let recorder = Recorder::new(&a.tuning.out)?;
    let (tx, writer) = recorder.spawn(64);

    // Reader thread: file I/O and sample conversion overlap with DSP.
    let (rtx, rrx) = std::sync::mpsc::sync_channel::<Vec<Complex32>>(4);
    let reader =
        std::thread::Builder::new()
            .name("reader".into())
            .spawn(move || -> Result<()> {
                let mut src = src;
                loop {
                    let mut buf = vec![Complex32::default(); 1 << 18];
                    let k = src.read(&mut buf)?;
                    if k == 0 {
                        return Ok(());
                    }
                    buf.truncate(k);
                    if rtx.send(buf).is_err() {
                        return Ok(());
                    }
                }
            })?;

    let started = Instant::now();
    let mut out: Vec<Output> = Vec::new();
    let send = |out: &mut Vec<Output>| -> Result<()> {
        if !out.is_empty() {
            tx.send(std::mem::take(out))
                .ok()
                .context("recorder thread stopped")?;
        }
        Ok(())
    };
    for chunk in rrx {
        scanner.push(&chunk, &mut out);
        send(&mut out)?;
    }
    scanner.finish(&mut out);
    send(&mut out)?;
    drop(tx);
    reader.join().expect("reader thread panicked")?;
    let rec = writer.join().expect("recorder thread panicked")?;
    let elapsed = started.elapsed().as_secs_f64();
    let stats = scanner.stats();
    let secs = stats.samples as f64 / info.sample_rate;

    let index = scanner_store::Index::open(&a.tuning.out.join("index.sqlite"))?;
    let mut ours = Vec::new();
    for id in &rec.rows {
        ours.extend(index.get(*id)?);
    }
    ours.sort_by_key(|r| (r.start_ns, r.id));
    let t0_ns = t0
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    if a.json {
        let txs: Vec<serde_json::Value> = ours
            .iter()
            .map(|r| {
                serde_json::json!({
                    "freq_hz": r.freq_hz,
                    "channel": r.channel,
                    "origin": r.origin,
                    "start_s": (r.start_ns - t0_ns) as f64 / 1e9,
                    "audio_s": r.audio_secs,
                    "signal_s": r.signal_secs,
                    "peak_snr_db": r.peak_snr_db,
                    "mean_snr_db": r.mean_snr_db,
                    "wav": a.tuning.out.join(&r.wav_path),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "input_s": secs,
                "elapsed_s": elapsed,
                "realtime_factor": secs / elapsed,
                "blocks": stats.blocks,
                "opened": stats.opened,
                "recorded": stats.recorded,
                "discarded_short": stats.discarded_short,
                "not_recorded": stats.not_recorded,
                "duplicates": stats.duplicates,
                "max_concurrent": stats.max_concurrent,
                "transmissions": txs,
            })
        );
    } else {
        println!(
            "{:>14} {:>9} {:>8} {:>8} {:>7}  file",
            "frequency", "start s", "signal s", "audio s", "SNR dB"
        );
        for r in &ours {
            println!(
                "{:>14} {:>9.2} {:>8.2} {:>8.2} {:>7.1}  {}",
                Hz(r.freq_hz).to_string(),
                (r.start_ns - t0_ns) as f64 / 1e9,
                r.signal_secs.unwrap_or(0.0),
                r.audio_secs.unwrap_or(0.0),
                r.peak_snr_db.unwrap_or(0.0),
                r.wav_path
            );
        }
        println!(
            "{} transmissions ({} opened, {} too short, {} not recorded), {:.1} s of audio, {:.1} MB",
            rec.files,
            stats.opened,
            stats.discarded_short,
            stats.not_recorded,
            rec.audio_secs,
            rec.bytes as f64 / 1e6
        );
        println!(
            "{:.1} s of IQ in {:.2} s ({:.1}x realtime), up to {} channels at once",
            secs,
            elapsed,
            secs / elapsed,
            stats.max_concurrent
        );
    }
    Ok(())
}
