//! Parsing human-friendly frequency strings such as `"146.520 MHz"` or `"-600 kHz"`.
//!
//! Config files use strings with units so they read like a band plan. Bare
//! numbers (TOML integers or floats) are taken as Hz.

use serde::{Deserialize, Deserializer, de};
use std::fmt;

/// A frequency (or frequency offset/bandwidth) in Hz.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Hz(pub f64);

impl Hz {
    pub fn hz(self) -> f64 {
        self.0
    }
}

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        let a = v.abs();
        if a >= 1e9 {
            write!(f, "{:.6} GHz", v / 1e9)
        } else if a >= 1e6 {
            write!(f, "{:.4} MHz", v / 1e6)
        } else if a >= 1e3 {
            write!(f, "{:.3} kHz", v / 1e3)
        } else {
            write!(f, "{v} Hz")
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseFreqError {
    #[error("empty frequency string")]
    Empty,
    #[error("invalid number in frequency {0:?}")]
    BadNumber(String),
    #[error("unknown frequency unit {0:?} (use Hz, kHz, MHz or GHz)")]
    BadUnit(String),
}

/// Parse `"146.52 MHz"`, `"25kHz"`, `"-600 kHz"`, `"8000"` (Hz).
pub fn parse_freq(s: &str) -> Result<Hz, ParseFreqError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseFreqError::Empty);
    }
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num = num.trim().replace('_', "");
    let value: f64 = num
        .parse()
        .map_err(|_| ParseFreqError::BadNumber(s.to_string()))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "hz" => 1.0,
        "khz" => 1e3,
        "mhz" => 1e6,
        "ghz" => 1e9,
        other => return Err(ParseFreqError::BadUnit(other.to_string())),
    };
    Ok(Hz(value * mult))
}

impl<'de> Deserialize<'de> for Hz {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Hz;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a frequency like \"146.52 MHz\" or a number of Hz")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Hz, E> {
                parse_freq(v).map_err(E::custom)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Hz, E> {
                Ok(Hz(v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Hz, E> {
                Ok(Hz(v as f64))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Hz, E> {
                Ok(Hz(v))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_freq("146.52 MHz").unwrap(), Hz(146.52e6));
        assert_eq!(parse_freq("25kHz").unwrap(), Hz(25e3));
        assert_eq!(parse_freq("-600 kHz").unwrap(), Hz(-600e3));
        assert_eq!(parse_freq("8000").unwrap(), Hz(8000.0));
        assert_eq!(parse_freq("1.2 GHz").unwrap(), Hz(1.2e9));
        assert_eq!(parse_freq(" 12.5 khz ").unwrap(), Hz(12.5e3));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_freq(""), Err(ParseFreqError::Empty));
        assert!(matches!(
            parse_freq("abc"),
            Err(ParseFreqError::BadNumber(_))
        ));
        assert!(matches!(
            parse_freq("5 parsecs"),
            Err(ParseFreqError::BadUnit(_))
        ));
    }

    #[test]
    fn display_is_readable() {
        assert_eq!(Hz(146.52e6).to_string(), "146.5200 MHz");
        assert_eq!(Hz(12.5e3).to_string(), "12.500 kHz");
    }
}
