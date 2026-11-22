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
}

/// A repeater pair: creates an output slot and, if `monitor_input`, an input
/// slot at `output + offset`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeaterEntry {
    pub name: String,
    pub output: Hz,
    pub offset: Hz,
    /// CTCSS tone in Hz, informational for now.
    pub tone: Option<f64>,
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
            if let Some(existing) = explicit.iter().find(|e| (e.freq - slot.freq).abs() < 1.0) {
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
            };
            if r.monitor_input {
                let input = Slot {
                    freq: r.output.0 + r.offset.0,
                    origin: SlotOrigin::RepeaterInput,
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
}

impl Slot {
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
            .find(|s| (s.freq - f).abs() < 1.0)
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
