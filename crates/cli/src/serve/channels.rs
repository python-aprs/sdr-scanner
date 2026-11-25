//! Channel flags while running: the config's record and priority flags,
//! with the lockout and priority overrides (web UI edits and spurs found)
//! on top, and the channel list the web UI shows.

use anyhow::{Context, Result};
use scanner_core::config::{SlotOrigin, same_freq};
use scanner_core::detector::WatchedSlot;
use scanner_core::scanner::{Output, Scanner};
use scanner_core::units::Hz;
use scanner_store::settings::{Override, Settings};
use scanner_web::live::ChannelSite;
use scanner_web::live::SPUR_DETECTOR;
use scanner_web::{ChannelInfo, ChannelsHandle};
use std::path::Path;
use std::sync::Arc;

/// Half-width of a lockout placed on a frequency that isn't a watched slot
/// (an off-grid signal a grid slot retunes onto, or a birdie).
const LOCKOUT_HALF_HZ: f64 = 5e3;

pub struct Channels {
    /// The config file's flags for each watched slot: (freq, record,
    /// priority).
    base: Vec<(f64, bool, bool)>,
    overrides: Vec<Override>,
    pub settings: Arc<Settings>,
    /// What the web UI's channel list shows.
    pub list: ChannelsHandle,
}

impl Channels {
    /// Open `out/settings.sqlite` and apply its overrides to `scanner`.
    pub fn open(out: &Path, scanner: &mut Scanner) -> Result<Self> {
        let path = out.join("settings.sqlite");
        let settings =
            Arc::new(Settings::open(&path).with_context(|| format!("opening {}", path.display()))?);
        let c = Channels {
            base: scanner
                .detector()
                .slots()
                .iter()
                .map(|w| (w.slot.freq, w.slot.record, w.slot.priority))
                .collect(),
            overrides: settings.overrides()?,
            settings,
            list: Default::default(),
        };
        if !c.overrides.is_empty() {
            c.apply(scanner, &mut Vec::new());
            eprintln!(
                "{} channel overrides from {}",
                c.overrides.len(),
                path.display()
            );
        }
        c.publish(scanner);
        Ok(c)
    }

    /// Put the config's flags back, then the overrides on top. Recordings
    /// that become locked out end now (their outputs go into `out`).
    pub fn apply(&self, scanner: &mut Scanner, out: &mut Vec<Output>) {
        let mut ranges = Vec::new();
        for &(freq, record, priority) in &self.base {
            let o = self
                .overrides
                .iter()
                .find(|o| same_freq(o.freq_hz as f64, freq));
            let record = o.and_then(|o| o.locked_out).map_or(record, |l| !l);
            let priority = o.and_then(|o| o.priority).unwrap_or(priority);
            scanner.set_slot_flags(freq, record, priority, out);
        }
        for o in &self.overrides {
            let freq = o.freq_hz as f64;
            let on_slot = self.base.iter().any(|&(f, _, _)| same_freq(f, freq));
            if !on_slot && o.locked_out == Some(true) {
                ranges.push((freq - LOCKOUT_HALF_HZ, freq + LOCKOUT_HALF_HZ));
            }
        }
        scanner.set_lockouts(ranges, out);
    }

    /// Replace the overrides (the full set, as edited in the web UI).
    pub fn replace(&mut self, list: Vec<Override>, scanner: &mut Scanner, out: &mut Vec<Output>) {
        self.overrides = list;
        self.apply(scanner, out);
        self.publish(scanner);
    }

    /// Lock out steady carriers the scanner found (it already has), save
    /// them, and pick up the saved set.
    pub fn add_spurs(
        &mut self,
        spurs: &[f64],
        steady_secs: f64,
        scanner: &mut Scanner,
        out: &mut Vec<Output>,
    ) {
        for &f in spurs {
            eprintln!(
                "  spur  {} steady for {steady_secs:.0} s: recording dropped, locked out (Channels page to review)",
                Hz(f),
            );
            let now = scanner_core::clock::now_ns();
            if let Err(e) =
                self.settings
                    .set(f.round() as i64, Some(true), None, now, SPUR_DETECTOR)
            {
                eprintln!("saving spur lockout: {e}");
            }
        }
        match self.settings.overrides() {
            Ok(list) => self.replace(list, scanner, out),
            Err(e) => eprintln!("reading overrides: {e}"),
        }
    }

    fn publish(&self, scanner: &Scanner) {
        if let Ok(mut g) = self.list.write() {
            *g = channel_list(scanner.detector().slots(), &self.overrides);
        }
    }
}

/// The watched slots as the web channel list shows them, plus lockouts
/// that are not on a watched slot.
fn channel_list(slots: &[WatchedSlot], overrides: &[Override]) -> Vec<ChannelInfo> {
    // Only link to a half that is actually watched (inside the capture).
    let watched = |f: f64| slots.iter().any(|w| same_freq(w.slot.freq, f));
    let overridden = |f: f64| overrides.iter().find(|o| same_freq(o.freq_hz as f64, f));
    slots
        .iter()
        .map(|w| {
            let s = &w.slot;
            ChannelInfo {
                freq_hz: s.freq,
                name: s.name.clone(),
                kind: match s.origin {
                    SlotOrigin::Listed => "channel",
                    SlotOrigin::RepeaterOutput => "repeater_output",
                    SlotOrigin::RepeaterInput => "repeater_input",
                    SlotOrigin::Grid => "grid",
                },
                mode: s.mode.as_str().into(),
                bandwidth_hz: s.bandwidth,
                priority: s.priority,
                locked_out: !s.record,
                pair_hz: s.pair.filter(|&f| watched(f)),
                overridden: overridden(s.freq).is_some(),
                spur: overrides
                    .iter()
                    .any(|o| same_freq(o.freq_hz as f64, s.freq) && o.updated_by == SPUR_DETECTOR),
                sites: s
                    .sites
                    .iter()
                    .map(|site| ChannelSite {
                        name: site.name.clone(),
                        tone: site.tone.map(|t| t.label()),
                    })
                    .collect(),
            }
        })
        .chain(
            overrides
                .iter()
                .filter(|o| o.locked_out == Some(true) && !watched(o.freq_hz as f64))
                .map(|o| ChannelInfo {
                    freq_hz: o.freq_hz as f64,
                    name: None,
                    kind: "lockout",
                    mode: "nbfm".into(),
                    bandwidth_hz: 2.0 * LOCKOUT_HALF_HZ,
                    priority: false,
                    locked_out: true,
                    pair_hz: None,
                    overridden: true,
                    spur: o.updated_by == SPUR_DETECTOR,
                    sites: Vec::new(),
                }),
        )
        .collect()
}
