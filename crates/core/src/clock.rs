//! Timestamps derived from the sample counter (PLAN.md 3.4).
//!
//! Wall-clock time is read once when the stream starts; after that, the time
//! of sample `n` is `t0 + n / fs`. Dropped blocks still advance the counter,
//! so file timestamps stay correct across overruns and drops stay visible.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleClock {
    /// Wall-clock time of sample 0, in nanoseconds since the Unix epoch.
    t0_ns: i128,
    sample_rate: f64,
}

impl SampleClock {
    pub fn new(t0: SystemTime, sample_rate: f64) -> Self {
        assert!(sample_rate > 0.0, "sample rate must be positive");
        let t0_ns = match t0.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        Self { t0_ns, sample_rate }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Nanoseconds since the epoch at sample index `n`.
    pub fn unix_nanos_at(&self, n: u64) -> i128 {
        // Split to keep precision at high sample counts:
        // whole seconds exactly, then the fractional remainder.
        let fs = self.sample_rate;
        let whole = (n as f64 / fs).floor();
        let rem_samples = n as f64 - whole * fs;
        self.t0_ns + (whole as i128) * 1_000_000_000 + (rem_samples / fs * 1e9).round() as i128
    }

    pub fn time_at(&self, n: u64) -> SystemTime {
        let ns = self.unix_nanos_at(n);
        if ns >= 0 {
            UNIX_EPOCH + Duration::from_nanos(ns as u64)
        } else {
            UNIX_EPOCH - Duration::from_nanos((-ns) as u64)
        }
    }

    /// Seconds from sample 0 to sample `n`.
    pub fn elapsed(&self, n: u64) -> f64 {
        n as f64 / self.sample_rate
    }

    /// Re-anchor so that sample `n` maps to `t`, e.g. after measuring drift
    /// between the radio's clock and the host clock.
    pub fn reanchor(&mut self, n: u64, t: SystemTime) {
        let target = SampleClock::new(t, self.sample_rate).t0_ns;
        let since_zero = self.unix_nanos_at(n) - self.t0_ns;
        self.t0_ns = target - since_zero;
    }
}

/// Nanoseconds since the Unix epoch (0 for times before it).
pub fn unix_ns(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// [`unix_ns`] of now.
pub fn now_ns() -> i64 {
    unix_ns(SystemTime::now())
}

/// Format a timestamp as compact ISO 8601 UTC with milliseconds,
/// e.g. `20261122T065403.123Z`, as used in recording file names.
pub fn compact_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    let (y, mo, da) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}{mo:02}{da:02}T{:02}{:02}{:02}.{ms:03}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// The UTC calendar month holding `t` as (year, month), and the Unix
/// seconds at which that month starts and the next one starts.
pub fn utc_month(t: SystemTime) -> ((i64, u32), i64, i64) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, _) = civil_from_days(secs.div_euclid(86_400));
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (
        (y, m),
        days_from_civil(y, m, 1) * 86_400,
        days_from_civil(ny, nm, 1) * 86_400,
    )
}

/// (year, month, day) to days since 1970-01-01. Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn maps_samples_to_time() {
        let c = SampleClock::new(at(1_795_330_443), 10e6);
        assert_eq!(c.unix_nanos_at(0), 1_795_330_443_000_000_000);
        assert_eq!(c.unix_nanos_at(10_000_000), 1_795_330_444_000_000_000);
        assert_eq!(c.unix_nanos_at(5), 1_795_330_443_000_000_500);
    }

    #[test]
    fn precise_after_a_year_of_samples() {
        let fs = 10e6;
        let c = SampleClock::new(at(0), fs);
        let year = 365 * 86_400u64;
        let n = year * fs as u64 + 1; // one sample past a year
        assert_eq!(c.unix_nanos_at(n), year as i128 * 1_000_000_000 + 100);
    }

    #[test]
    fn non_integer_rate() {
        let c = SampleClock::new(at(0), 2.048e6);
        // 2048 samples = 1 ms exactly
        assert_eq!(c.unix_nanos_at(2048), 1_000_000);
    }

    #[test]
    fn reanchor_moves_mapping() {
        let mut c = SampleClock::new(at(100), 1000.0);
        c.reanchor(1000, at(102)); // radio clock says 101, host says 102
        assert_eq!(c.unix_nanos_at(1000), 102_000_000_000);
        assert_eq!(c.unix_nanos_at(0), 101_000_000_000);
    }

    #[test]
    fn finds_the_utc_month() {
        // 2026-11-22T06:54:03Z
        let ((y, m), start, next) = utc_month(at(1_795_330_443));
        assert_eq!((y, m), (2026, 11));
        assert_eq!(compact_utc(at(start as u64)), "20261101T000000.000Z");
        assert_eq!(compact_utc(at(next as u64)), "20261201T000000.000Z");
        let ((y, m), _, next) = utc_month(at(1_796_083_200 + 5)); // 2026-12-01
        assert_eq!((y, m), (2026, 12));
        assert_eq!(compact_utc(at(next as u64)), "20270101T000000.000Z");
        for z in [-800_000, -1, 0, 59, 60, 11_016, 20_514, 2_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn formats_compact_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_795_330_443_123);
        assert_eq!(compact_utc(t), "20261122T065403.123Z");
        assert_eq!(compact_utc(UNIX_EPOCH), "19700101T000000.000Z");
        // leap day
        assert_eq!(compact_utc(at(951_782_400)), "20000229T000000.000Z");
    }
}
