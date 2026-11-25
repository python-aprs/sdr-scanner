//! Band profiles: the channel plan a scanner instance monitors.
//!
//! Nothing about the band is hard-coded (see PLAN.md 3.7). A TOML file holds
//! one or more `[profiles.<name>]` tables. A profile can `inherit` another and
//! override any field; list entries (`channels`, `repeaters`, `lockouts`) are
//! appended to the parent's.
//!
//! [`Profile::channel_plan`] turns a resolved profile into the list of
//! [`Slot`]s the detector watches: explicitly listed channels first, then
//! repeater outputs and inputs, then a fallback grid for anything unlisted.

use crate::tone::Tone;
use crate::units::Hz;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Demodulation mode of a channel slot (PLAN.md 3.8). Only `Nbfm` is
/// implemented today; the others parse so channel lists can be written ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Nbfm,
    Afsk1200,
    Dmr,
    /// Yaesu System Fusion (C4FM)
    Ysf,
    Dstar,
    P25,
    Nxdn,
    Usb,
    Lsb,
    Cw,
}

/// Whether two frequencies (Hz) name the same channel. Channels are
/// configured and overridden in whole Hz; within 1 Hz is a match.
pub fn same_freq(a: f64, b: f64) -> bool {
    (a - b).abs() < 1.0
}

impl Mode {
    /// The name used in config files, the index and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Nbfm => "nbfm",
            Mode::Afsk1200 => "afsk1200",
            Mode::Dmr => "dmr",
            Mode::Ysf => "ysf",
            Mode::Dstar => "dstar",
            Mode::P25 => "p25",
            Mode::Nxdn => "nxdn",
            Mode::Usb => "usb",
            Mode::Lsb => "lsb",
            Mode::Cw => "cw",
        }
    }

    /// Time-division modes, where a single radio transmits in bursts.
    pub fn is_tdma(self) -> bool {
        matches!(self, Mode::Dmr)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: Hz,
    pub stop: Hz,
}

impl Span {
    pub fn width(&self) -> f64 {
        self.stop.0 - self.start.0
    }
    pub fn center(&self) -> f64 {
        (self.start.0 + self.stop.0) / 2.0
    }
    pub fn contains(&self, f: f64) -> bool {
        f >= self.start.0 && f <= self.stop.0
    }
}

/// An explicitly listed channel; unset fields fall back to the profile.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEntry {
    pub freq: Hz,
    pub name: Option<String>,
    pub bandwidth: Option<Hz>,
    pub deviation: Option<Hz>,
    pub mode: Option<Mode>,
    #[serde(default)]
    pub priority: bool,
    pub record: Option<bool>,
    /// CTCSS tone in Hz the channel is known to use, informational for now
    /// (compare with the tone tagged on each recording).
    pub tone: Option<f64>,
}

/// A repeater pair: creates an output slot and, if `monitor_input`, an input
/// slot at `output + offset`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeaterEntry {
    pub name: String,
    pub output: Hz,
    pub offset: Hz,
    /// CTCSS tone in Hz, informational (a recording keeps the tone it
    /// carried); see `sites` for outputs shared by several repeaters.
    pub tone: Option<f64>,
    /// DCS code (octal, `"023"`), informational like `tone`.
    pub dcs: Option<String>,
    /// For an output shared by several repeaters (frequency reuse, or a
    /// linked system whose sites use different tones): each repeater with
    /// its tone. A recording on the slot whose tone matches one site's is
    /// named after that site; with no tone, or one no site lists, it keeps
    /// the entry's `name`.
    #[serde(default)]
    pub sites: Vec<RepeaterSite>,
    #[serde(default = "default_true")]
    pub monitor_input: bool,
    pub bandwidth: Option<Hz>,
    pub deviation: Option<Hz>,
    pub mode: Option<Mode>,
    #[serde(default)]
    pub priority: bool,
    /// Defaults to true. Importers set false for digital-only repeaters
    /// until their decoder exists.
    pub record: Option<bool>,
    /// Informational fields carried through from coordinator lists.
    pub call: Option<String>,
    pub location: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub modes: Vec<String>,
}

/// One of the repeaters sharing an output (see [`RepeaterEntry::sites`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeaterSite {
    /// Defaults to `call location`.
    pub name: Option<String>,
    pub call: Option<String>,
    pub location: Option<String>,
    /// CTCSS tone in Hz.
    pub tone: Option<f64>,
    /// DCS code (octal, `"023"`); either polarity matches.
    pub dcs: Option<String>,
}

impl RepeaterSite {
    /// The name recordings matched to this site get.
    pub fn label(&self) -> Option<String> {
        if let Some(n) = &self.name {
            return Some(n.clone());
        }
        let parts: Vec<&str> = [&self.call, &self.location]
            .into_iter()
            .flatten()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// The tone this site uses, if it lists one.
    fn tone(&self) -> Result<Option<Tone>, String> {
        match (self.tone, &self.dcs) {
            (Some(_), Some(_)) => Err("set `tone` or `dcs`, not both".into()),
            (Some(f), None) if (60.0..=260.0).contains(&f) => Ok(Some(Tone::Ctcss(f as f32))),
            (Some(f), None) => Err(format!("tone {f} Hz is outside 60–260 Hz")),
            (None, Some(d)) => parse_dcs(d).map(Some),
            (None, None) => Ok(None),
        }
    }
}

/// A DCS code as config files write it: octal, with an optional leading
/// `D` and trailing `N`/`I` (ignored: either polarity matches).
fn parse_dcs(text: &str) -> Result<Tone, String> {
    let t = text.trim().trim_start_matches(['D', 'd']);
    let t = t.trim_end_matches(['N', 'n', 'I', 'i']);
    match u16::from_str_radix(t, 8) {
        Ok(code) if code < 0o1000 && !t.is_empty() => Ok(Tone::Dcs {
            code,
            inverted: false,
        }),
        _ => Err(format!("DCS code {text:?} is not three octal digits")),
    }
}

/// A frequency or range whose slots stay on the spectrum but are not recorded.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockoutEntry {
    pub start: Hz,
    /// If absent, the lockout covers the single channel at `start`.
    pub stop: Option<Hz>,
    pub reason: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A profile exactly as written in the file, before inheritance.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProfile {
    pub inherit: Option<String>,
    pub span: Option<Span>,
    pub spacing: Option<Hz>,
    pub offset: Option<Hz>,
    pub bandwidth: Option<Hz>,
    pub deviation: Option<Hz>,
    pub audio_rate: Option<u32>,
    pub deemphasis: Option<bool>,
    pub mode: Option<Mode>,
    /// Add fallback grid slots between listed channels (default true).
    pub grid: Option<bool>,
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,
    #[serde(default)]
    pub repeaters: Vec<RepeaterEntry>,
    #[serde(default)]
    pub lockouts: Vec<LockoutEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub profiles: BTreeMap<String, RawProfile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("no profile named {0:?}")]
    UnknownProfile(String),
    #[error("profile {0:?} inherits from itself (cycle through {1:?})")]
    InheritCycle(String, String),
    #[error("profile {profile:?}: missing required field `{field}`")]
    Missing {
        profile: String,
        field: &'static str,
    },
    #[error("profile {profile:?}: {msg}")]
    Invalid { profile: String, msg: String },
}

impl ConfigFile {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Resolve `name`, applying its `inherit` chain.
    pub fn profile(&self, name: &str) -> Result<Profile, ConfigError> {
        let mut chain = Vec::new();
        let mut cur = name.to_string();
        loop {
            if chain.contains(&cur) {
                return Err(ConfigError::InheritCycle(name.to_string(), cur));
            }
            let raw = self
                .profiles
                .get(&cur)
                .ok_or_else(|| ConfigError::UnknownProfile(cur.clone()))?;
            chain.push(cur.clone());
            match &raw.inherit {
                Some(parent) => cur = parent.clone(),
                None => break,
            }
        }
        // Merge from the root ancestor down to `name`.
        let mut merged = RawProfile::default();
        for n in chain.iter().rev() {
            let r = &self.profiles[n];
            macro_rules! take {
                ($($f:ident),*) => { $( if r.$f.is_some() { merged.$f = r.$f.clone(); } )* };
            }
            take!(
                span, spacing, offset, bandwidth, deviation, audio_rate, deemphasis, mode, grid
            );
            merged.channels.extend(r.channels.iter().cloned());
            merged.repeaters.extend(r.repeaters.iter().cloned());
            merged.lockouts.extend(r.lockouts.iter().cloned());
        }
        Profile::from_raw(name, merged)
    }
}

/// A fully resolved band profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub span: Span,
    pub spacing: Option<Hz>,
    pub offset: Hz,
    pub bandwidth: Hz,
    pub deviation: Hz,
    pub audio_rate: u32,
    pub deemphasis: bool,
    pub mode: Mode,
    pub grid: bool,
    pub channels: Vec<ChannelEntry>,
    pub repeaters: Vec<RepeaterEntry>,
    pub lockouts: Vec<LockoutEntry>,
}

impl Profile {
    fn from_raw(name: &str, r: RawProfile) -> Result<Self, ConfigError> {
        let missing = |field| ConfigError::Missing {
            profile: name.to_string(),
            field,
        };
        let invalid = |msg: String| ConfigError::Invalid {
            profile: name.to_string(),
            msg,
        };
        let span = r.span.ok_or_else(|| missing("span"))?;
        let bandwidth = r.bandwidth.ok_or_else(|| missing("bandwidth"))?;
        let deviation = r.deviation.ok_or_else(|| missing("deviation"))?;
        let grid = r.grid.unwrap_or(true);
        if span.stop.0 <= span.start.0 {
            return Err(invalid(format!(
                "span stop {} must be above start {}",
                span.stop, span.start
            )));
        }
        if bandwidth.0 <= 0.0 {
            return Err(invalid("bandwidth must be positive".into()));
        }
        if grid {
            match r.spacing {
                None => return Err(missing("spacing")),
                Some(s) if s.0 <= 0.0 => return Err(invalid("spacing must be positive".into())),
                _ => {}
            }
        }
        for rep in &r.repeaters {
            let bad = |msg: String| invalid(format!("repeater {:?}: {msg}", rep.name));
            if let Some(d) = &rep.dcs {
                parse_dcs(d).map_err(bad)?;
            }
            if rep.sites.len() == 1 {
                return Err(bad(
                    "`sites` lists one site; put its tone on the entry".into()
                ));
            }
            if !rep.sites.is_empty() && (rep.tone.is_some() || rep.dcs.is_some()) {
                return Err(bad("with `sites`, give each site its tone instead".into()));
            }
            for site in &rep.sites {
                if site.label().is_none() {
                    return Err(bad("a site needs a `name`, `call` or `location`".into()));
                }
                site.tone().map_err(bad)?;
            }
        }
        Ok(Profile {
            name: name.to_string(),
            span,
            spacing: r.spacing,
            offset: r.offset.unwrap_or_default(),
            bandwidth,
            deviation,
            audio_rate: r.audio_rate.unwrap_or(8000),
            deemphasis: r.deemphasis.unwrap_or(true),
            mode: r.mode.unwrap_or_default(),
            grid,
            channels: r.channels,
            repeaters: r.repeaters,
            lockouts: r.lockouts,
        })
    }

    /// Build the list of slots the detector watches, sorted by frequency.
    pub fn channel_plan(&self) -> ChannelPlan {
        let mut warnings = Vec::new();
        let mut explicit: Vec<Slot> = Vec::new();

        let mut push = |slot: Slot, warnings: &mut Vec<String>| {
            if !self.span.contains(slot.freq) {
                warnings.push(format!(
                    "{} at {} is outside the span {}–{}; skipped",
                    slot.label(),
                    Hz(slot.freq),
                    self.span.start,
                    self.span.stop
                ));
                return;
            }
            if let Some(existing) = explicit.iter().find(|e| same_freq(e.freq, slot.freq)) {
                warnings.push(format!(
                    "{} at {} duplicates {}; keeping the first",
                    slot.label(),
                    Hz(slot.freq),
                    existing.label()
                ));
                return;
            }
            explicit.push(slot);
        };

        for c in &self.channels {
            push(
                Slot {
                    freq: c.freq.0,
                    bandwidth: c.bandwidth.unwrap_or(self.bandwidth).0,
                    deviation: c.deviation.unwrap_or(self.deviation).0,
                    mode: c.mode.unwrap_or(self.mode),
                    priority: c.priority,
                    record: c.record.unwrap_or(true),
                    name: c.name.clone(),
                    origin: SlotOrigin::Listed,
                    pair: None,
                    sites: Vec::new(),
                },
                &mut warnings,
            );
        }
        for r in &self.repeaters {
            let base = Slot {
                freq: r.output.0,
                bandwidth: r.bandwidth.unwrap_or(self.bandwidth).0,
                deviation: r.deviation.unwrap_or(self.deviation).0,
                mode: r.mode.unwrap_or(self.mode),
                priority: r.priority,
                record: r.record.unwrap_or(true),
                name: Some(r.name.clone()),
                origin: SlotOrigin::RepeaterOutput,
                pair: r.monitor_input.then_some(r.output.0 + r.offset.0),
                sites: r
                    .sites
                    .iter()
                    .map(|s| SlotSite {
                        name: s.label().expect("validated"),
                        tone: s.tone().expect("validated"),
                    })
                    .collect(),
            };
            if r.monitor_input {
                let input = Slot {
                    freq: r.output.0 + r.offset.0,
                    origin: SlotOrigin::RepeaterInput,
                    pair: Some(r.output.0),
                    ..base.clone()
                };
                push(base, &mut warnings);
                push(input, &mut warnings);
            } else {
                push(base, &mut warnings);
            }
        }

        let mut slots = explicit.clone();
        if self.grid {
            let spacing = self.spacing.expect("validated").0;
            let off = self.offset.0;
            let first = ((self.span.start.0 - off) / spacing).ceil() as i64;
            let last = ((self.span.stop.0 - off) / spacing).floor() as i64;
            for k in first..=last {
                let f = k as f64 * spacing + off;
                let taken = explicit
                    .iter()
                    .any(|e| (e.freq - f).abs() < (spacing.max(e.bandwidth)) / 2.0);
                if !taken {
                    slots.push(Slot {
                        freq: f,
                        bandwidth: self.bandwidth.0,
                        deviation: self.deviation.0,
                        mode: self.mode,
                        priority: false,
                        record: true,
                        name: None,
                        origin: SlotOrigin::Grid,
                        pair: None,
                        sites: Vec::new(),
                    });
                }
            }
        }

        for l in &self.lockouts {
            for s in slots.iter_mut() {
                let hit = match l.stop {
                    Some(stop) => s.freq >= l.start.0 && s.freq <= stop.0,
                    None => (s.freq - l.start.0).abs() <= s.bandwidth / 2.0,
                };
                if hit {
                    s.record = false;
                }
            }
        }

        slots.sort_by(|a, b| a.freq.total_cmp(&b.freq));
        ChannelPlan { slots, warnings }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOrigin {
    Listed,
    RepeaterOutput,
    RepeaterInput,
    Grid,
}

impl SlotOrigin {
    /// The name stored in the recordings index.
    pub fn as_str(self) -> &'static str {
        match self {
            SlotOrigin::Listed => "listed",
            SlotOrigin::RepeaterOutput => "repeateroutput",
            SlotOrigin::RepeaterInput => "repeaterinput",
            SlotOrigin::Grid => "grid",
        }
    }
}

/// One frequency the detector watches.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub freq: f64,
    pub bandwidth: f64,
    pub deviation: f64,
    pub mode: Mode,
    pub priority: bool,
    /// False for locked-out slots: shown on the spectrum, never recorded.
    pub record: bool,
    pub name: Option<String>,
    pub origin: SlotOrigin,
    /// For a repeater with its input monitored too: the other half's
    /// frequency (the input for an output slot, and the reverse).
    pub pair: Option<f64>,
    /// For a repeater output shared by several repeaters (and its input):
    /// each one with its tone, to tell recordings apart by the tone heard.
    /// Empty otherwise.
    pub sites: Vec<SlotSite>,
}

/// One of the repeaters sharing a slot.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotSite {
    pub name: String,
    pub tone: Option<Tone>,
}

/// Whether a heard tone is the one a site lists: CTCSS within 1 Hz (the
/// closest standard tones are 2.5 Hz apart), DCS by code in either polarity.
fn tone_matches(heard: &Tone, listed: &Tone) -> bool {
    match (heard, listed) {
        (Tone::Ctcss(a), Tone::Ctcss(b)) => (a - b).abs() < 1.0,
        (Tone::Dcs { code: a, .. }, Tone::Dcs { code: b, .. }) => a == b,
        _ => false,
    }
}

impl Slot {
    /// The channel name for a transmission on this slot that carried
    /// `tone`. On a slot shared by several repeaters, the sites whose tone
    /// matches (usually one; several when linked sites share a tone);
    /// with no tone heard, or one no site lists, the slot's own name,
    /// which names them all.
    pub fn name_for(&self, tone: Option<&Tone>) -> Option<String> {
        let matched: Vec<&str> = match tone {
            Some(t) => self
                .sites
                .iter()
                .filter(|s| s.tone.as_ref().is_some_and(|l| tone_matches(t, l)))
                .map(|s| s.name.as_str())
                .collect(),
            None => Vec::new(),
        };
        if matched.is_empty() || matched.len() == self.sites.len() {
            self.name.clone()
        } else {
            Some(matched.join(" / "))
        }
    }

    pub fn label(&self) -> String {
        let kind = match self.origin {
            SlotOrigin::Listed => "channel",
            SlotOrigin::RepeaterOutput => "repeater output",
            SlotOrigin::RepeaterInput => "repeater input",
            SlotOrigin::Grid => "grid slot",
        };
        match &self.name {
            Some(n) => format!("{kind} {n:?}"),
            None => kind.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelPlan {
    pub slots: Vec<Slot>,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_stored_and_parsed_forms() {
        // The index has always stored lowercase Debug names; keep them.
        use SlotOrigin::*;
        for o in [Listed, RepeaterOutput, RepeaterInput, Grid] {
            assert_eq!(o.as_str(), format!("{o:?}").to_lowercase());
        }
        use Mode::*;
        for m in [Nbfm, Afsk1200, Dmr, Ysf, Dstar, P25, Nxdn, Usb, Lsb, Cw] {
            assert_eq!(m.as_str(), format!("{m:?}").to_lowercase());
            #[derive(Deserialize)]
            struct W {
                mode: Mode,
            }
            let w: W = toml::from_str(&format!("mode = \"{}\"", m.as_str())).unwrap();
            assert_eq!(w.mode, m);
        }
    }

    const EXAMPLE: &str = r#"
[profiles.2m-25k]
span       = { start = "144.000 MHz", stop = "148.000 MHz" }
spacing    = "25 kHz"
bandwidth  = "16 kHz"
deviation  = "5 kHz"

[[profiles.2m-25k.channels]]
freq = "146.5200 MHz"
name = "National simplex"
bandwidth = "11 kHz"
deviation = "2.5 kHz"
priority = true

[[profiles.2m-25k.channels]]
freq = "144.3900 MHz"
name = "APRS"
mode = "afsk1200"
record = false

[[profiles.2m-25k.repeaters]]
name = "Example"
output = "146.8400 MHz"
offset = "-600 kHz"
tone = 100.0

[[profiles.2m-25k.lockouts]]
start = "144.000 MHz"
stop  = "144.300 MHz"
reason = "CW/SSB/weak signal"

[profiles.2m-12k5]
inherit   = "2m-25k"
spacing   = "12.5 kHz"
bandwidth = "11 kHz"
deviation = "2.5 kHz"
"#;

    fn plan(name: &str) -> ChannelPlan {
        ConfigFile::parse(EXAMPLE)
            .unwrap()
            .profile(name)
            .unwrap()
            .channel_plan()
    }

    fn slot_at(p: &ChannelPlan, f: f64) -> &Slot {
        p.slots
            .iter()
            .find(|s| same_freq(s.freq, f))
            .unwrap_or_else(|| panic!("no slot at {f}"))
    }

    #[test]
    fn resolves_defaults() {
        let p = ConfigFile::parse(EXAMPLE)
            .unwrap()
            .profile("2m-25k")
            .unwrap();
        assert_eq!(p.audio_rate, 8000);
        assert!(p.deemphasis);
        assert_eq!(p.mode, Mode::Nbfm);
        assert_eq!(p.offset, Hz(0.0));
    }

    #[test]
    fn listed_channels_override_profile() {
        let p = plan("2m-25k");
        let s = slot_at(&p, 146.52e6);
        assert_eq!(s.bandwidth, 11e3);
        assert!(s.priority);
        assert_eq!(s.origin, SlotOrigin::Listed);
        assert_eq!(slot_at(&p, 144.39e6).mode, Mode::Afsk1200);
        assert!(!slot_at(&p, 144.39e6).record);
    }

    #[test]
    fn repeater_creates_output_and_input() {
        let p = plan("2m-25k");
        assert_eq!(slot_at(&p, 146.84e6).origin, SlotOrigin::RepeaterOutput);
        let input = slot_at(&p, 146.24e6);
        assert_eq!(input.origin, SlotOrigin::RepeaterInput);
        assert_eq!(input.name.as_deref(), Some("Example"));
    }

    #[test]
    fn grid_fills_gaps_without_duplicating_listed() {
        let p = plan("2m-25k");
        // 4 MHz / 25 kHz + 1 = 161 grid positions; 4 are taken by explicit slots
        // that sit on the grid (146.52, 144.39 is off-grid, 146.84, 146.24).
        let grid = p
            .slots
            .iter()
            .filter(|s| s.origin == SlotOrigin::Grid)
            .count();
        let explicit = p.slots.len() - grid;
        assert_eq!(explicit, 4);
        // 144.39 (off-grid) is 15 kHz from 144.375 and 10 kHz from 144.400.
        // A grid slot closer than half a spacing (12.5 kHz) to a listed slot is
        // dropped, so 144.400 goes and 144.375 stays.
        assert!(p.slots.iter().any(|s| (s.freq - 144.375e6).abs() < 1.0));
        assert!(p.slots.iter().all(|s| (s.freq - 144.400e6).abs() > 1.0));
        assert_eq!(grid, 161 - 3 - 1);
        // sorted
        assert!(p.slots.windows(2).all(|w| w[0].freq < w[1].freq));
    }

    #[test]
    fn lockout_range_disables_recording() {
        let p = plan("2m-25k");
        assert!(!slot_at(&p, 144.1e6).record);
        assert!(!slot_at(&p, 144.3e6).record);
        assert!(slot_at(&p, 144.325e6).record);
    }

    #[test]
    fn inheritance_overrides_and_appends() {
        let p = plan("2m-12k5");
        // Inherited listed channel and repeater are present.
        assert_eq!(slot_at(&p, 146.84e6).origin, SlotOrigin::RepeaterOutput);
        // Grid is now 12.5 kHz.
        assert_eq!(slot_at(&p, 146.5125e6).bandwidth, 11e3);
    }

    #[test]
    fn repeater_info_fields_and_record_flag() {
        let cfg = r#"
[profiles.p]
span = { start = "144 MHz", stop = "148 MHz" }
spacing = "25 kHz"
bandwidth = "16 kHz"
deviation = "5 kHz"
[[profiles.p.repeaters]]
name = "W7XXX Somewhere"
call = "W7XXX"
location = "Somewhere, WA"
source = "WWARA"
modes = ["DMR"]
output = "146.4500 MHz"
offset = "1 MHz"
mode = "dmr"
record = false
"#;
        let p = ConfigFile::parse(cfg)
            .unwrap()
            .profile("p")
            .unwrap()
            .channel_plan();
        let out = slot_at(&p, 146.45e6);
        assert_eq!(out.mode, Mode::Dmr);
        assert!(!out.record);
        assert!(!slot_at(&p, 147.45e6).record);
    }

    #[test]
    fn out_of_span_input_is_warned_and_skipped() {
        let cfg = r#"
[profiles.narrow]
span = { start = "146.000 MHz", stop = "147.300 MHz" }
spacing = "25 kHz"
bandwidth = "16 kHz"
deviation = "5 kHz"
[[profiles.narrow.repeaters]]
name = "High"
output = "147.2000 MHz"
offset = "600 kHz"
"#;
        let p = ConfigFile::parse(cfg)
            .unwrap()
            .profile("narrow")
            .unwrap()
            .channel_plan();
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("outside the span"));
        assert!(
            p.slots
                .iter()
                .all(|s| s.origin != SlotOrigin::RepeaterInput)
        );
    }

    #[test]
    fn shipped_configs_load_without_warnings() {
        for (file, text) in [
            (
                "example-2m.toml",
                include_str!("../../../config/example-2m.toml"),
            ),
            ("rainier.toml", include_str!("../../../config/rainier.toml")),
            ("cowlitz.toml", include_str!("../../../config/cowlitz.toml")),
        ] {
            let cfg = ConfigFile::parse(text).unwrap_or_else(|e| panic!("{file}: {e}"));
            for name in cfg.profiles.keys() {
                let p = cfg
                    .profile(name)
                    .unwrap_or_else(|e| panic!("{file} {name}: {e}"));
                let plan = p.channel_plan();
                assert!(
                    plan.warnings.is_empty(),
                    "{file} {name}: {:?}",
                    plan.warnings
                );
            }
        }
    }

    #[test]
    fn channel_tone_is_carried() {
        let cfg = ConfigFile::parse(include_str!("../../../config/cowlitz.toml")).unwrap();
        let p = cfg.profile("cowlitz-vhf").unwrap();
        let c = p.channels.iter().find(|c| c.freq.0 == 155.325e6).unwrap();
        assert_eq!(c.tone, Some(107.2));
        // Wider than the RSP1A's usable 0.9 x 8 MS/s would not fit.
        assert!(p.span.width() <= 0.9 * 8e6);
    }

    const SHARED: &str = r#"
[profiles.p]
span = { start = "144 MHz", stop = "148 MHz" }
spacing = "25 kHz"
bandwidth = "16 kHz"
deviation = "5 kHz"
[[profiles.p.repeaters]]
name = "NM7R / W7LT Meglar Mtn, WA / Portland, OR"
output = "147.1800 MHz"
offset = "600 kHz"
sites = [
    { call = "NM7R", location = "Meglar Mtn, WA", tone = 82.5 },
    { call = "W7LT", location = "Portland, OR", tone = 103.5 },
    { name = "Linked A", tone = 100 },
    { name = "Linked B", tone = 100.0 },
    { name = "Digital", dcs = "023" },
    { name = "Unknown" },
]
"#;

    #[test]
    fn shared_output_is_named_by_the_tone_heard() {
        let p = ConfigFile::parse(SHARED)
            .unwrap()
            .profile("p")
            .unwrap()
            .channel_plan();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let all = Some("NM7R / W7LT Meglar Mtn, WA / Portland, OR".to_string());
        // The input slot (users keying up with the tone) resolves the same way.
        for f in [147.18e6, 147.78e6] {
            let s = slot_at(&p, f);
            assert_eq!(s.sites.len(), 6);
            let name = |t: Option<Tone>| s.name_for(t.as_ref());
            assert_eq!(
                name(Some(Tone::Ctcss(103.5))).as_deref(),
                Some("W7LT Portland, OR")
            );
            assert_eq!(
                name(Some(Tone::Ctcss(82.5))).as_deref(),
                Some("NM7R Meglar Mtn, WA")
            );
            // Linked sites sharing a tone stay together.
            assert_eq!(
                name(Some(Tone::Ctcss(100.0))).as_deref(),
                Some("Linked A / Linked B")
            );
            // Either DCS polarity.
            let d = |inverted| {
                Some(Tone::Dcs {
                    code: 0o023,
                    inverted,
                })
            };
            assert_eq!(name(d(true)).as_deref(), Some("Digital"));
            assert_eq!(name(d(false)).as_deref(), Some("Digital"));
            // No tone, or one nobody lists: still every candidate.
            assert_eq!(name(None), all);
            assert_eq!(name(Some(Tone::Ctcss(123.0))), all);
            assert_eq!(
                name(Some(Tone::Dcs {
                    code: 0o754,
                    inverted: false
                })),
                all
            );
        }
        // A slot with no sites is just its name.
        let plain = Slot {
            sites: Vec::new(),
            ..slot_at(&p, 147.18e6).clone()
        };
        assert_eq!(plain.name_for(Some(&Tone::Ctcss(103.5))), all);
    }

    #[test]
    fn bad_sites_are_rejected() {
        let swap = |from: &str, to: &str| {
            let text = SHARED.replace(from, to);
            match ConfigFile::parse(&text).unwrap().profile("p") {
                Err(ConfigError::Invalid { msg, .. }) => msg,
                other => panic!("{to}: {other:?}"),
            }
        };
        let site = r#"{ name = "Unknown" }"#;
        assert!(swap(site, r#"{ tone = 100 }"#).contains("needs a `name`"));
        assert!(swap(site, r#"{ name = "X", dcs = "089" }"#).contains("octal"));
        assert!(swap(site, r#"{ name = "X", tone = 100, dcs = "023" }"#).contains("not both"));
        assert!(swap(site, r#"{ name = "X", tone = 1000 }"#).contains("60–260"));
        assert!(
            swap("offset = \"600 kHz\"", "offset = \"600 kHz\"\ntone = 82.5").contains("each site")
        );
        let one = "sites = [\n    { name = \"Only\", tone = 100 },\n]\n";
        let start = SHARED.find("sites = [").unwrap();
        let text = format!("{}{one}", &SHARED[..start]);
        assert!(matches!(
            ConfigFile::parse(&text).unwrap().profile("p"),
            Err(ConfigError::Invalid { msg, .. }) if msg.contains("one site")
        ));
    }

    #[test]
    fn errors_are_specific() {
        let cfg = ConfigFile::parse(
            r#"
[profiles.a]
inherit = "b"
[profiles.b]
inherit = "a"
[profiles.c]
span = { start = "1 MHz", stop = "2 MHz" }
bandwidth = "10 kHz"
deviation = "5 kHz"
"#,
        )
        .unwrap();
        assert!(matches!(
            cfg.profile("a"),
            Err(ConfigError::InheritCycle(..))
        ));
        assert!(matches!(
            cfg.profile("zzz"),
            Err(ConfigError::UnknownProfile(_))
        ));
        assert!(matches!(
            cfg.profile("c"),
            Err(ConfigError::Missing {
                field: "spacing",
                ..
            })
        ));
        assert!(ConfigFile::parse("[profiles.x]\nbogus = 1\n").is_err());
    }
}
