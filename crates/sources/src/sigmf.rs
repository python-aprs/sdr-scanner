//! Minimal SigMF metadata support: the parts needed to replay a recording.
//!
//! Spec: <https://sigmf.org>. We read `core:datatype`, `core:sample_rate`, the
//! first capture's `core:frequency` and `core:datetime`, and annotations.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Sample encodings we can read. SigMF names: `ci16_le`, `cf32_le`, `cu8`, `ci8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Ci16Le,
    Cf32Le,
    Cu8,
    Ci8,
}

impl SampleFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ci16_le" | "ci16" => Self::Ci16Le,
            "cf32_le" | "cf32" => Self::Cf32Le,
            "cu8" => Self::Cu8,
            "ci8" => Self::Ci8,
            _ => return None,
        })
    }

    pub fn sigmf_name(self) -> &'static str {
        match self {
            Self::Ci16Le => "ci16_le",
            Self::Cf32Le => "cf32_le",
            Self::Cu8 => "cu8",
            Self::Ci8 => "ci8",
        }
    }

    /// Bytes per complex sample.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Ci16Le => 4,
            Self::Cf32Le => 8,
            Self::Cu8 | Self::Ci8 => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub global: Global,
    #[serde(default)]
    pub captures: Vec<Capture>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Global {
    #[serde(rename = "core:datatype")]
    pub datatype: String,
    #[serde(rename = "core:sample_rate")]
    pub sample_rate: Option<f64>,
    #[serde(rename = "core:version", default)]
    pub version: Option<String>,
    #[serde(rename = "core:description", default)]
    pub description: Option<String>,
    #[serde(rename = "core:recorder", default)]
    pub recorder: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    #[serde(rename = "core:sample_start", default)]
    pub sample_start: u64,
    #[serde(rename = "core:frequency")]
    pub frequency: Option<f64>,
    #[serde(rename = "core:datetime")]
    pub datetime: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    #[serde(rename = "core:sample_start")]
    pub sample_start: u64,
    #[serde(rename = "core:sample_count")]
    pub sample_count: Option<u64>,
    #[serde(rename = "core:freq_lower_edge")]
    pub freq_lower_edge: Option<f64>,
    #[serde(rename = "core:freq_upper_edge")]
    pub freq_upper_edge: Option<f64>,
    #[serde(rename = "core:label")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Meta {
    pub fn format(&self) -> Option<SampleFormat> {
        SampleFormat::parse(&self.global.datatype)
    }
}

/// Given any of `x`, `x.sigmf-meta` or `x.sigmf-data`, return (meta, data) paths.
pub fn paths_for(p: &Path) -> (PathBuf, PathBuf) {
    let s = p.to_string_lossy();
    let base = s
        .strip_suffix(".sigmf-meta")
        .or_else(|| s.strip_suffix(".sigmf-data"))
        .or_else(|| s.strip_suffix(".sigmf"))
        .unwrap_or(&s)
        .to_string();
    (
        PathBuf::from(format!("{base}.sigmf-meta")),
        PathBuf::from(format!("{base}.sigmf-data")),
    )
}

/// Parse an RFC 3339 UTC timestamp like `2026-11-22T06:54:03.123Z` into
/// seconds since the epoch. Only `Z` / `+00:00` offsets are accepted.
pub fn parse_datetime(s: &str) -> Option<f64> {
    let s = s.trim();
    let (main, tz_ok) = if let Some(m) = s.strip_suffix('Z') {
        (m, true)
    } else if let Some(m) = s.strip_suffix("+00:00") {
        (m, true)
    } else {
        (s, false)
    };
    if !tz_ok {
        return None;
    }
    let (date, time) = main.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.splitn(3, ':');
    let (h, mi) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let sec: f64 = t.next()?.parse().ok()?;
    let days = days_from_civil(y, mo as u32, da as u32);
    Some((days * 86_400 + h * 3600 + mi * 60) as f64 + sec)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_meta() {
        let m: Meta = serde_json::from_str(
            r#"{"global":{"core:datatype":"ci16_le","core:sample_rate":2500000,"core:version":"1.2.0","x:custom":1},
                "captures":[{"core:sample_start":0,"core:frequency":146000000,"core:datetime":"2026-11-22T06:54:03Z"}],
                "annotations":[{"core:sample_start":10,"core:sample_count":5,"core:label":"tx0"}]}"#,
        )
        .unwrap();
        assert_eq!(m.format(), Some(SampleFormat::Ci16Le));
        assert_eq!(m.global.sample_rate, Some(2.5e6));
        assert_eq!(m.captures[0].frequency, Some(146e6));
        assert_eq!(m.annotations[0].label.as_deref(), Some("tx0"));
        assert!(m.global.extra.contains_key("x:custom"));
    }

    #[test]
    fn path_variants() {
        let want = (
            PathBuf::from("a/b.sigmf-meta"),
            PathBuf::from("a/b.sigmf-data"),
        );
        assert_eq!(paths_for(Path::new("a/b")), want);
        assert_eq!(paths_for(Path::new("a/b.sigmf-meta")), want);
        assert_eq!(paths_for(Path::new("a/b.sigmf-data")), want);
    }

    #[test]
    fn datetime() {
        assert_eq!(parse_datetime("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(
            parse_datetime("2026-11-22T06:54:03.5Z"),
            Some(1_795_330_443.5)
        );
        assert_eq!(
            parse_datetime("2026-11-22T06:54:03+00:00"),
            Some(1_795_330_443.0)
        );
        assert_eq!(parse_datetime("2026-11-22T06:54:03-08:00"), None);
    }
}
