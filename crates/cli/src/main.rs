//! `scanner`: the sdr-scanner command line.
//!
//! Phase 0 subcommands are diagnostics used by tests and CI:
//! - `scanner info <recording>`: SigMF metadata and signal level
//! - `scanner peaks <recording>`: averaged spectrum peaks, in RF frequency
//! - `scanner config check <file> [--profile NAME]`: validate band profiles
//! - `scanner config retag <file> <recordings>`: rename recordings to the
//!   config's channel names (on shared repeater outputs, by their tone)
//!
//! Phase 1:
//! - `scanner run <recording> --config FILE`: scan a recording and write one
//!   WAV per transmission plus the SQLite index
//! - `scanner dtmf <wav or recordings folder>...`: decode DTMF digits, and
//!   tag a folder's index rows with them
//! - `scanner ambe <file.amb>...`: turn a digital call's raw voice frames into
//!   WAV (needs the `mbelib` build)

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use num_complex::Complex32;
use scanner_core::config::{ConfigFile, SlotOrigin};
use scanner_core::psd::{Welch, find_peaks, noise_floor_db};
use scanner_core::units::Hz;
use scanner_sources::{FileSource, Source};
use std::path::{Path, PathBuf};

mod admin;
mod kiss;
mod outputs;
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
    /// List the SDRplay and Airspy radios this build can see.
    Devices,
    /// Manage admin accounts, API tokens and sessions for the web API.
    Admin(admin::AdminArgs),
    /// Run continuously from a source, recording until Ctrl-C.
    Serve(ServeArgs),
    /// Sweep the SDRplay radio's gain over the profile's band and suggest
    /// gain and detection thresholds for `serve`.
    Calibrate(CalibrateArgs),
    /// Decode DTMF digits in WAV files. A recordings folder (one with an
    /// `index.sqlite`) has every complete recording scanned and its index
    /// rows tagged, for recordings made before DTMF tagging.
    Dtmf(DtmfArgs),
    /// Decode DMR, System Fusion and D-STAR calls' raw voice frames (`.amb`, kept beside each call's
    /// recording) to WAV next to them. Needs a build with `--features
    /// mbelib`; for calls recorded by a build without it.
    Ambe {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Replace WAV files that already exist.
        #[arg(long)]
        force: bool,
    },
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
struct DtmfArgs {
    /// WAV files, or recordings folders.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Also print each digit's start time and length.
    #[arg(long, short)]
    verbose: bool,
    /// For a folder: rescan rows that already have digits too.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct CalibrateArgs {
    /// `sdrplay`, `sdrplay:<serial>`, `airspy` or `airspy:<hex serial>`.
    #[arg(long, default_value = "sdrplay")]
    source: serve::SourceSpec,
    #[command(flatten)]
    tuning: Tuning,
    /// Radio settings; --lna and --gr (SDRplay) or the --airspy-gain
    /// preset number (Airspy) are ignored, as they are swept. An Airspy
    /// sweeps the kind of preset --airspy-gain names (linearity unless
    /// told otherwise).
    #[command(flatten)]
    radio: RadioArgs,
    /// Airspy presets to try.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "0,2,4,6,8,10,12,14,16,18,20,21"
    )]
    airspy_presets: Vec<u8>,
    /// Seconds measured at each step (after 0.3 s to settle).
    #[arg(long, default_value_t = 2.0)]
    dwell: f64,
    /// SDRplay LNA states to try.
    #[arg(long, value_delimiter = ',', default_value = "0,1,2,3,4,5,6,7,8,9")]
    lna_states: Vec<u8>,
    /// SDRplay IF gain reductions (dB) to try at each LNA state.
    #[arg(long, value_delimiter = ',', default_value = "59,50,40,30,20")]
    gr_steps: Vec<u8>,
    /// Print JSON (every step and the suggestion) instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// `sdrplay` (the first SDRplay radio), `sdrplay:<serial>`, `airspy`
    /// (the first Airspy), `airspy:<hex serial>`, or `replay:<recording>` / `loop:<recording>` (a recording played at its
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
    /// Monthly data budget (GB) for anonymous visitors: past 80 % their
    /// spectrum slows to one frame a second, and once it is spent their
    /// spectrum, live audio and recording audio stop until the month turns
    /// (UTC). Admin traffic is counted but never held back. 0: no budget.
    #[arg(long, default_value_t = 3.0)]
    anon_egress_gb: f64,
    /// Serve decoded AX.25 packets as KISS over TCP on this address (e.g.
    /// 127.0.0.1:8001), so APRS software can use the scanner as a
    /// receive-only TNC. Off by default.
    #[arg(long)]
    kiss: Option<std::net::SocketAddr>,
    /// Accounts database (default: auth.sqlite in --out).
    #[arg(long)]
    auth_db: Option<PathBuf>,
    /// Leave `Secure` off the session cookie, for plain HTTP on a trusted
    /// LAN. (Browsers treat localhost as secure, so local use doesn't need it.)
    #[arg(long)]
    insecure_cookies: bool,
}

/// Radio settings for `serve --source sdrplay|airspy`.
#[derive(clap::Args)]
struct RadioArgs {
    /// Tuner frequency in MHz (default: the middle of the profile's span plus
    /// 12.5 kHz, so the DC residual sits between 25 kHz channels).
    #[arg(long)]
    center_mhz: Option<f64>,
    /// Sample rate in S/s (default: 8 MS/s for SDRplay, which takes 2 to
    /// 10.66 MS/s; 10 MS/s for an Airspy R2, which takes 10 or 2.5 MS/s; a
    /// Mini takes 6 or 3 MS/s).
    #[arg(long)]
    rate: Option<f64>,
    /// Airspy gain: `linearity:N` or `sensitivity:N` (presets, 0 to 21,
    /// more is more gain), `manual:LNA,MIXER,VGA` (0-14, 0-15, 0-15), or
    /// `agc:VGA`.
    #[arg(long, default_value_t = scanner_sources::airspy::AirspyGain::default())]
    airspy_gain: scanner_sources::airspy::AirspyGain,
    /// Pack the Airspy's 12-bit samples on USB (less bandwidth, more CPU).
    #[arg(long)]
    packing: bool,
    /// SDRplay LNA state: 0 is the most gain (0 to 9 for an RSP1A on 2 m).
    #[arg(long, default_value_t = 3)]
    lna: u8,
    /// SDRplay IF gain reduction in dB, 20 to 59 (more is less gain).
    #[arg(long, default_value_t = 40)]
    gr: u8,
    /// IF filter bandwidth in kHz (8000, 7000, 6000, 5000, 1536, ...).
    #[arg(long, default_value_t = 8000)]
    if_bw_khz: u32,
    /// Turn on the SDRplay radio's AGC with this set point in dBFS (e.g. -30).
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
    /// SDRplay: use USB bulk transfers instead of isochronous.
    #[arg(long)]
    usb_bulk: bool,
}

impl RadioArgs {
    fn center_hz(&self, profile: &scanner_core::config::Profile) -> f64 {
        self.center_mhz
            .map(|m| m * 1e6)
            .unwrap_or(profile.span.center() + 12.5e3)
    }

    fn airspy(
        &self,
        profile: &scanner_core::config::Profile,
    ) -> scanner_sources::airspy::AirspyConfig {
        scanner_sources::airspy::AirspyConfig {
            serial: None,
            sample_rate: self.rate.unwrap_or(10e6),
            center_hz: self.center_hz(profile),
            gain: self.airspy_gain,
            ppm: self.ppm,
            bias_t: self.bias_t,
            packing: self.packing,
        }
    }

    fn sdrplay(
        &self,
        profile: &scanner_core::config::Profile,
    ) -> scanner_sources::sdrplay::SdrplayConfig {
        scanner_sources::sdrplay::SdrplayConfig {
            serial: None,
            sample_rate: self.rate.unwrap_or(8e6),
            center_hz: self.center_hz(profile),
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
    /// Noise squelch: play audio only while the noise above the voice band
    /// is this many dB quieter than no signal at all (a signal quiets by
    /// about its SNR plus 4 dB). Keeps wideband noise and hash that lift a
    /// channel's power out of recordings. 0 turns it off.
    #[arg(long, default_value_t = 8.0)]
    noise_squelch: f32,
    /// Spur detection: a transmission that stays on this many seconds with
    /// a level that hardly moves (a carrier, plain or with a steady tone)
    /// is a spur. Its recording is dropped and the frequency locked out
    /// until reviewed on the Channels page. 0 turns it off.
    #[arg(long, default_value_t = 300.0)]
    spur_secs: f64,
    /// Comb guard: each channel learns the steady level it sits at over
    /// this many seconds (interference such as the comb of lines from a
    /// switching power supply), and must rise above that, not just the
    /// noise, to open. 0 turns it off.
    #[arg(long, default_value_t = 300.0)]
    background_secs: f64,
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
    /// Only decode what each channel's `mode` says. By default NBFM
    /// channels (listed or grid) also listen for DMR, System Fusion, D-STAR
    /// and AFSK1200 packet, and switch to whichever they find.
    #[arg(long)]
    no_auto_detect: bool,
    /// Skip the Opus copy of each recording (the web UI then plays the WAV).
    #[arg(long)]
    no_opus: bool,
    /// Don't listen for DTMF digits in recordings.
    #[arg(long)]
    no_dtmf: bool,
    /// Disk budget for the recordings in GB: past 95 % of it the oldest are
    /// deleted until under 85 % (their index rows stay, marked expired).
    /// 0 means no budget.
    #[arg(long, default_value_t = 8.0)]
    disk_budget_gb: f64,
    /// Also delete the oldest recordings to keep this much free (GB) on
    /// the filesystem; 0 turns it off.
    #[arg(long, default_value_t = 1.0)]
    min_free_gb: f64,
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
        use scanner_core::config::Mode;
        let ambe = plan
            .slots
            .iter()
            .any(|s| matches!(s.mode, Mode::Dmr | Mode::Ysf | Mode::Dstar) && s.record);
        if ambe && !scanner_core::dmr::vocoder::available() {
            eprintln!(
                "note: no AMBE vocoder in this build: DMR, System Fusion and D-STAR calls are \
                 recorded with their ids and raw voice frames (.amb) but silent audio. Build \
                 with --features mbelib for audio, or decode later with `scanner ambe`."
            );
        }
        Ok((profile, plan))
    }

    fn retention(&self) -> scanner_store::retention::Policy {
        let gb = |v: f64| (v.max(0.0) * scanner_store::retention::GB as f64) as u64;
        scanner_store::retention::Policy {
            budget_bytes: gb(self.disk_budget_gb),
            min_free_bytes: gb(self.min_free_gb),
            ..Default::default()
        }
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
                background_secs: self.background_secs,
                ..Default::default()
            },
            audio_rate: profile.audio_rate,
            deemphasis: profile.deemphasis,
            min_len: self.min_len,
            noise_squelch_db: self.noise_squelch,
            spur_secs: self.spur_secs,
            parallel: !self.serial,
            auto_detect: !self.no_auto_detect,
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
    /// Rename recordings to the config's current channel names: on outputs
    /// several repeaters share (`sites`), after the repeater whose tone each
    /// one carried. For recordings made before tone matching, or after a
    /// name, a site or its tone changed.
    Retag {
        file: PathBuf,
        /// Recordings folder (the one with `index.sqlite`).
        recordings: PathBuf,
        /// Only use this profile's repeaters (default: all).
        #[arg(long)]
        profile: Option<String>,
        /// Print what would change without changing it.
        #[arg(long, short = 'n')]
        dry_run: bool,
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
        Cmd::Dtmf(args) => dtmf(args),
        Cmd::Ambe { files, force } => ambe(&files, force),
        Cmd::Devices => devices(),
        Cmd::Admin(a) => admin::run(a),
        Cmd::Calibrate(a) => calibrate(a),
        Cmd::Serve(a) => {
            let (profile, plan) = a.tuning.load()?;
            let sdrplay = a.radio.sdrplay(&profile);
            let airspy = a.radio.airspy(&profile);
            let tuned = match a.source {
                serve::SourceSpec::Sdrplay { .. } => {
                    Some((sdrplay.validate(), sdrplay.sample_rate, sdrplay.center_hz))
                }
                serve::SourceSpec::Airspy { .. } => {
                    Some((airspy.validate(), airspy.sample_rate, airspy.center_hz))
                }
                serve::SourceSpec::Replay { .. } => None,
            };
            if let Some((valid, rate, center)) = tuned {
                valid.map_err(|e| anyhow::anyhow!("radio settings: {e}"))?;
                let half = rate / 2.0;
                if profile.span.start.0 < center - half || profile.span.stop.0 > center + half {
                    eprintln!(
                        "warning: {rate} S/s at {} covers only part of the profile's span",
                        Hz(center)
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
                airspy,
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
                opus: !a.tuning.no_opus,
                dtmf: !a.tuning.no_dtmf,
                retention: a.tuning.retention(),
                anon_egress_bytes: (a.anon_egress_gb.max(0.0) * 1e9) as u64,
                status_every: std::time::Duration::from_secs_f64(a.status_every.max(0.5)),
                kiss: a.kiss,
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
        Cmd::Config {
            cmd:
                ConfigCmd::Retag {
                    file,
                    recordings,
                    profile,
                    dry_run,
                },
        } => config_retag(&file, &recordings, profile, dry_run),
    }
}

fn devices() -> Result<()> {
    if !cfg!(any(feature = "sdrplay", feature = "airspy")) {
        bail!(
            "this scanner was built without radio support; rebuild with `--features sdrplay` \
             and/or `--features airspy`"
        );
    }
    #[cfg(feature = "sdrplay")]
    {
        match scanner_sources::sdrplay::list_devices() {
            Ok(devs) if devs.is_empty() => println!("no SDRplay radios found"),
            Ok(devs) => {
                for d in devs {
                    println!("sdrplay:{}  {}", d.serial, d.model);
                }
            }
            Err(e) => println!("SDRplay: {e}"),
        }
    }
    #[cfg(feature = "airspy")]
    {
        match scanner_sources::airspy::list_devices() {
            Ok(devs) if devs.is_empty() => println!("no Airspy radios found"),
            Ok(devs) => {
                for d in devs {
                    let rates: Vec<String> = d
                        .sample_rates
                        .iter()
                        .map(|r| format!("{}", *r as f64 / 1e6))
                        .collect();
                    println!(
                        "airspy:{:016X}  Airspy {}  {} MS/s  {}",
                        d.serial,
                        d.model(),
                        rates.join(", "),
                        d.firmware
                            .as_deref()
                            .unwrap_or("(busy: in use by another program?)")
                    );
                }
            }
            Err(e) => println!("Airspy: {e}"),
        }
    }
    Ok(())
}

/// Print the sweep and what it suggests.
///
/// `airspy` names the preset kind swept (`linearity` or `sensitivity`), whose
/// number each step carries in `lna`; `None` is an SDRplay LNA/GR sweep.
#[cfg_attr(not(any(feature = "sdrplay", feature = "airspy")), allow(dead_code))]
fn report_calibration(
    steps: &[scanner_core::calibrate::Step],
    json: bool,
    airspy: Option<&str>,
) -> Option<scanner_core::calibrate::Suggestion> {
    let s = scanner_core::calibrate::suggest(steps);
    if json {
        println!("{}", serde_json::json!({ "steps": steps, "suggestion": s }));
        return s;
    }
    let setting = |lna: u8, gr: u8| match airspy {
        Some(kind) => format!("{kind}:{lna}"),
        None => format!("LNA {lna} GR {gr}"),
    };
    println!(
        "{:>13} {:>6} {:>7} {:>7} {:>8} {:>7} {:>5} {:>6}",
        "setting", "gain", "rms", "peak", "clip ppm", "floor", "peaks", "idle99.9"
    );
    for x in steps {
        println!(
            "{:>13} {:>6.0} {:>7.1} {:>7.1} {:>8.1} {:>7.1} {:>5} {:>6.1}{}",
            setting(x.lna, x.gr),
            x.gain_db,
            x.rms_dbfs,
            x.peak_dbfs,
            x.clip_ppm,
            x.floor_db,
            x.peaks,
            x.idle_snr_p999_db,
            if x.overload { "  overload" } else { "" }
        );
    }
    match &s {
        Some(g) => {
            println!("\n{}: {}", setting(g.lna, g.gr), g.reason);
            let flags = match airspy {
                Some(kind) => format!("--airspy-gain {kind}:{}", g.lna),
                None => format!("--lna {} --gr {}", g.lna, g.gr),
            };
            println!(
                "\nsuggested: {flags} --open-db {} --close-db {}",
                g.open_db, g.close_db
            );
        }
        None => println!("\nno step was clean enough to suggest (all clipped or overloaded?)"),
    }
    s
}

fn calibrate(a: CalibrateArgs) -> Result<()> {
    match a.source {
        serve::SourceSpec::Sdrplay { .. } => calibrate_sdrplay(a),
        serve::SourceSpec::Airspy { .. } => calibrate_airspy(a),
        serve::SourceSpec::Replay { .. } => {
            bail!("calibrate needs a radio: --source sdrplay or --source airspy")
        }
    }
}

/// Read `secs` of samples from a radio, handing each chunk to `f`.
#[cfg(any(feature = "sdrplay", feature = "airspy"))]
fn read_for(
    source: &mut scanner_sources::RingSource,
    rate: f64,
    secs: f64,
    buf: &mut [Complex32],
    f: &mut dyn FnMut(&[Complex32]),
) -> Result<()> {
    let chunk = buf.len();
    let mut left = (secs * rate) as usize;
    while left > 0 {
        let k = source.read(&mut buf[..left.min(chunk)])?;
        if k == 0 {
            bail!("the radio stopped streaming");
        }
        f(&buf[..k]);
        left -= k;
    }
    Ok(())
}

#[cfg(feature = "sdrplay")]
fn calibrate_sdrplay(a: CalibrateArgs) -> Result<()> {
    use scanner_core::calibrate::{StepMeter, rsp1a_gain_db};
    use scanner_sources::sdrplay::Sdrplay;
    use std::sync::atomic::Ordering;
    let serve::SourceSpec::Sdrplay { serial } = &a.source else {
        bail!("calibrate needs a radio: --source sdrplay");
    };
    let (profile, plan) = a.tuning.load()?;
    let detector = a.tuning.scanner_config(&profile).detector;
    let mut steps_todo = Vec::new();
    for &lna in &a.lna_states {
        for &gr in &a.gr_steps {
            steps_todo.push((lna, gr));
        }
    }
    // Least gain first, so the radio never starts out overloaded.
    steps_todo.sort_by(|x, y| rsp1a_gain_db(x.0, x.1).total_cmp(&rsp1a_gain_db(y.0, y.1)));
    let Some(&(lna0, gr0)) = steps_todo.first() else {
        bail!("nothing to sweep");
    };
    let cfg = scanner_sources::sdrplay::SdrplayConfig {
        serial: serial.clone(),
        lna_state: lna0,
        gr_db: gr0,
        agc_setpoint_dbfs: None,
        ..a.radio.sdrplay(&profile)
    };
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("radio settings: {e}"))?;
    let (mut device, mut source) = Sdrplay::start(&cfg, 64, 1 << 16)?;
    let rate = cfg.sample_rate;
    let events = device.events().clone();
    eprintln!(
        "{}: sweeping {} gain steps, {:.1} s each",
        source.info().description,
        steps_todo.len(),
        a.dwell
    );
    let mut buf = vec![Complex32::default(); 1 << 16];
    let mut steps = Vec::new();
    for (i, &(lna, gr)) in steps_todo.iter().enumerate() {
        if i > 0 {
            device.set_gain(lna, gr)?;
        }
        read_for(&mut source, rate, 0.3, &mut buf, &mut |_| {})?;
        let overloads = events.overloads.load(Ordering::Relaxed);
        let mut meter = StepMeter::new(rate, cfg.center_hz, &plan.slots, &detector);
        read_for(&mut source, rate, a.dwell, &mut buf, &mut |x| meter.push(x))?;
        let overload = events.overloads.load(Ordering::Relaxed) > overloads
            || events.overloaded.load(Ordering::Relaxed);
        // The radio reports its actual LNA reduction; trust it over the
        // table when it has said something.
        let lna_db = events.lna_gr_db.load(Ordering::Relaxed);
        let gain_db = if events.gain_changes.load(Ordering::Relaxed) > 0 && lna_db > 0 {
            -(lna_db as f32 + gr as f32 - 20.0)
        } else {
            rsp1a_gain_db(lna, gr)
        };
        let step = meter.finish(lna, gr, gain_db, overload);
        eprintln!(
            "  LNA {lna} GR {gr}: floor {:.1} dB, rms {:.1} dBFS{}",
            step.floor_db,
            step.rms_dbfs,
            if step.clip_ppm > 0.0 || overload {
                ", clipping"
            } else {
                ""
            }
        );
        steps.push(step);
    }
    drop(device);
    report_calibration(&steps, a.json, None);
    Ok(())
}

#[cfg(not(feature = "sdrplay"))]
fn calibrate_sdrplay(_: CalibrateArgs) -> Result<()> {
    bail!("this scanner was built without SDRplay support; rebuild with `--features sdrplay`")
}

/// Sweep the Airspy's linearity (or sensitivity) presets. Each step is
/// recorded with the preset in `lna` and 0 in `gr`; gain is the R820T
/// estimate relative to the most the preset kind gives.
#[cfg(feature = "airspy")]
fn calibrate_airspy(a: CalibrateArgs) -> Result<()> {
    use scanner_core::calibrate::StepMeter;
    use scanner_sources::airspy::{Airspy, AirspyGain};
    use std::sync::atomic::Ordering;
    let serve::SourceSpec::Airspy { serial } = a.source else {
        bail!("calibrate needs a radio: --source airspy");
    };
    let (kind, mk): (&str, fn(u8) -> AirspyGain) = match a.radio.airspy_gain {
        AirspyGain::Linearity(_) => ("linearity", AirspyGain::Linearity),
        AirspyGain::Sensitivity(_) => ("sensitivity", AirspyGain::Sensitivity),
        g => bail!("calibrate sweeps linearity or sensitivity presets, not {g}"),
    };
    let (profile, plan) = a.tuning.load()?;
    let detector = a.tuning.scanner_config(&profile).detector;
    let mut presets = a.airspy_presets.clone();
    presets.retain(|&p| p <= 21);
    // Least gain first, so the radio never starts out overloaded.
    presets.sort_unstable();
    presets.dedup();
    let Some(&p0) = presets.first() else {
        bail!("nothing to sweep");
    };
    let top = mk(21).approx_gain_db().unwrap_or(0.0);
    let cfg = scanner_sources::airspy::AirspyConfig {
        serial,
        gain: mk(p0),
        ..a.radio.airspy(&profile)
    };
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("radio settings: {e}"))?;
    let (mut device, mut source) = Airspy::start(&cfg, 64, 1 << 16)?;
    let rate = cfg.sample_rate;
    let events = device.events().clone();
    eprintln!(
        "{}: sweeping {} {kind} presets, {:.1} s each",
        source.info().description,
        presets.len(),
        a.dwell
    );
    let mut buf = vec![Complex32::default(); 1 << 16];
    let mut steps = Vec::new();
    for (i, &p) in presets.iter().enumerate() {
        if i > 0 {
            device.set_gain(mk(p))?;
        }
        read_for(&mut source, rate, 0.3, &mut buf, &mut |_| {})?;
        let dropped = events.dropped_samples.load(Ordering::Relaxed);
        let mut meter = StepMeter::new(rate, cfg.center_hz, &plan.slots, &detector);
        read_for(&mut source, rate, a.dwell, &mut buf, &mut |x| meter.push(x))?;
        let gain_db = mk(p).approx_gain_db().unwrap_or(0.0) - top;
        let step = meter.finish(p, 0, gain_db, false);
        let lost = events.dropped_samples.load(Ordering::Relaxed) - dropped;
        eprintln!(
            "  {kind}:{p}: floor {:.1} dB, rms {:.1} dBFS{}{}",
            step.floor_db,
            step.rms_dbfs,
            if step.clip_ppm > 0.0 {
                ", clipping"
            } else {
                ""
            },
            if lost > 0 {
                format!(", {lost} samples dropped")
            } else {
                String::new()
            }
        );
        steps.push(step);
    }
    drop(device);
    report_calibration(&steps, a.json, Some(kind));
    Ok(())
}

#[cfg(not(feature = "airspy"))]
fn calibrate_airspy(_: CalibrateArgs) -> Result<()> {
    bail!("this scanner was built without Airspy support; rebuild with `--features airspy`")
}

fn open(path: &Path) -> Result<FileSource> {
    FileSource::open(path).with_context(|| format!("opening {}", path.display()))
}

fn dtmf(a: DtmfArgs) -> Result<()> {
    use scanner_core::dtmf::DtmfDecoder;
    let decode = |path: &Path| -> Result<DtmfDecoder> {
        let (rate, samples) = scanner_store::wav::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if rate < 4000 {
            bail!("{}: {rate} Hz is too low for DTMF", path.display());
        }
        let mut d = DtmfDecoder::new(rate as f64);
        d.push(&samples);
        Ok(d)
    };
    let show = |name: &str, d: &DtmfDecoder| {
        println!("{name}\t{}", d.text());
        if a.verbose {
            for g in d.digits() {
                println!(
                    "  {}  {:8.3} s  {:4.0} ms",
                    g.key,
                    g.start as f64 / d.rate(),
                    (g.end - g.start) as f64 / d.rate() * 1e3
                );
            }
        }
    };
    for path in &a.paths {
        if !path.is_dir() {
            let d = decode(path)?;
            if !d.digits().is_empty() || a.paths.len() == 1 {
                show(&path.display().to_string(), &d);
            }
            continue;
        }
        let index = scanner_store::Index::open(&path.join("index.sqlite"))
            .with_context(|| format!("{} is not a recordings folder", path.display()))?;
        let (mut scanned, mut tagged, mut after) = (0u64, 0u64, 0i64);
        loop {
            let rows = index.complete_after(after, 500)?;
            let Some(last) = rows.last() else { break };
            after = last.id;
            for r in rows {
                if r.dtmf.is_some() && !a.all {
                    continue;
                }
                let d = match decode(&path.join(&r.wav_path)) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("skipping row {}: {e:#}", r.id);
                        continue;
                    }
                };
                scanned += 1;
                let text = (!d.digits().is_empty()).then(|| d.text());
                if text.is_some() {
                    tagged += 1;
                    show(&r.wav_path, &d);
                }
                if text != r.dtmf {
                    index.set_dtmf(r.id, text.as_deref())?;
                }
            }
        }
        eprintln!(
            "{}: scanned {scanned} recordings, {tagged} with DTMF",
            path.display()
        );
    }
    Ok(())
}

fn ambe(files: &[PathBuf], force: bool) -> Result<()> {
    use scanner_core::dmr::{AmbeFrame, vocoder};
    if !vocoder::available() {
        bail!("this build has no vocoder: rebuild with --features mbelib");
    }
    for path in files {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let Some(codec) = scanner_store::recorder::amb_codec(&data) else {
            bail!("{} is not an .amb file", path.display());
        };
        let body = &data[4..];
        let out = path.with_extension("wav");
        if out.exists() && !force {
            eprintln!("{} exists; skipping (--force to replace)", out.display());
            continue;
        }
        let mut v = vocoder::Leveled::new(vocoder::new_for(codec).unwrap());
        let mut audio = Vec::new();
        for f in body.chunks_exact(8) {
            let f = AmbeFrame::from_amb_bytes(f.try_into().unwrap());
            vocoder::Vocoder::decode(&mut v, &f, &mut audio);
        }
        let mut w = scanner_store::wav::WavWriter::create(&out, vocoder::AUDIO_RATE)?;
        w.write(&audio)?;
        w.finish(vocoder::AUDIO_RATE)?;
        println!(
            "{}\t{:.1} s",
            out.display(),
            audio.len() as f64 / vocoder::AUDIO_RATE as f64
        );
    }
    Ok(())
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
                for site in &s.sites {
                    let tone = site.tone.map_or("no tone".into(), |t| t.label());
                    println!("{:>16}  {tone:>9}  {}", "", site.name);
                }
            }
        }
    }
    if warned {
        eprintln!("config has warnings");
    }
    Ok(())
}

fn config_retag(file: &Path, dir: &Path, only: Option<String>, dry_run: bool) -> Result<()> {
    use scanner_core::config::same_freq;
    use scanner_core::tone::Tone;
    let cfg = ConfigFile::load(file)?;
    let names: Vec<String> = match only {
        Some(n) => vec![n],
        None => cfg.profiles.keys().cloned().collect(),
    };
    let mut shared = Vec::new();
    for name in names {
        let plan = cfg.profile(&name)?.channel_plan();
        // Grid slots retune and carry no name: leave their rows alone.
        shared.extend(
            plan.slots
                .into_iter()
                .filter(|s| s.origin != SlotOrigin::Grid),
        );
    }
    let index = scanner_store::Index::open(&dir.join("index.sqlite"))
        .with_context(|| format!("{} is not a recordings folder", dir.display()))?;
    let (mut seen, mut renamed, mut after) = (0u64, 0u64, 0i64);
    loop {
        let rows = index.complete_after(after, 500)?;
        let Some(last) = rows.last() else { break };
        after = last.id;
        for r in rows {
            let Some(slot) = shared
                .iter()
                .find(|s| s.origin.as_str() == r.origin && same_freq(s.freq, r.freq_hz))
            else {
                continue;
            };
            seen += 1;
            let tone = r.tone.as_deref().and_then(Tone::parse);
            let name = slot.name_for(tone.as_ref());
            if name != r.channel {
                renamed += 1;
                println!(
                    "{}\t{}\t{}",
                    r.wav_path,
                    r.tone.as_deref().unwrap_or("-"),
                    name.as_deref().unwrap_or("-")
                );
                if !dry_run {
                    index.set_channel(r.id, name.as_deref())?;
                }
            }
        }
    }
    eprintln!(
        "{}: {seen} recordings on listed channels and repeaters, {renamed} {}",
        dir.display(),
        if dry_run {
            "would be renamed"
        } else {
            "renamed"
        }
    );
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

    let recorder = Recorder::new(&a.tuning.out)?
        .with_opus(!a.tuning.no_opus)
        .with_dtmf(!a.tuning.no_dtmf)
        .with_retention(a.tuning.retention())?;
    let (tx, writer) = recorder.spawn(64);
    let outputs = outputs::Outputs::new(tx, None, false);

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
    for chunk in rrx {
        scanner.push(&chunk, &mut out);
        outputs.send(&mut out)?;
    }
    scanner.finish(&mut out);
    outputs.send(&mut out)?;
    drop(outputs);
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
    let t0_ns = scanner_core::clock::unix_ns(t0);
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
                    "dtmf": r.dtmf,
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
                "discarded_silent": stats.discarded_silent,
                "discarded_digital": stats.discarded_digital,
                "calls": stats.calls,
                "spurs": stats.spurs,
                "packets": stats.packets,
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
            if let Some(d) = &r.dtmf {
                println!("{:>14} DTMF {d}", "");
            }
        }
        println!(
            "{} transmissions ({} opened, {} too short, {} not recorded, {} turned out digital, {} digital calls, {} packets), {:.1} s of audio, {:.1} MB WAV, {:.1} MB Opus",
            rec.files,
            stats.opened,
            stats.discarded_short + stats.discarded_silent,
            stats.not_recorded,
            stats.discarded_digital,
            stats.calls,
            stats.packets,
            rec.audio_secs,
            rec.bytes as f64 / 1e6,
            rec.opus_bytes as f64 / 1e6
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
