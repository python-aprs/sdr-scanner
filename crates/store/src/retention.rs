//! Retention by disk budget (PLAN.md 3.5): when the recordings pass a
//! high-water mark of their budget, or free space on the filesystem falls
//! below a floor, delete the oldest recordings (WAV, Opus copy, and any
//! `.part` a crash left) until usage is back under the low-water mark. The
//! index rows stay, marked `expired`.
//!
//! Usage comes from the index (the bytes of every complete row), so a pass
//! never walks the folder. Files put there by hand don't count.

use crate::index::{Index, IndexError, Row};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const GB: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Bytes the recordings may use; 0 means no budget (the free-space
    /// floor still applies).
    pub budget_bytes: u64,
    /// Start deleting above this share of the budget...
    pub high: f64,
    /// ...and stop once under this share.
    pub low: f64,
    /// Keep at least this much free on the filesystem; 0 turns it off.
    pub min_free_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            budget_bytes: 8 * GB,
            high: 0.95,
            low: 0.85,
            min_free_bytes: GB,
        }
    }
}

/// Shared counters for the API's storage view.
#[derive(Debug, Default)]
pub struct DiskUsage {
    used: AtomicU64,
    budget: AtomicU64,
    min_free: AtomicU64,
    /// `u64::MAX` when unknown.
    free: AtomicU64,
    expired: AtomicU64,
    expired_bytes: AtomicU64,
    /// Start of the newest recording deleted so far (0: none yet).
    expired_until_ns: AtomicI64,
    /// Bytes of recordings deleted by hand that retention hasn't taken off
    /// its count yet.
    released: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageStatus {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub min_free_bytes: u64,
    pub free_bytes: Option<u64>,
    /// Recordings deleted since the scanner started, and their bytes.
    pub expired: u64,
    pub expired_bytes: u64,
    pub expired_until_ns: Option<i64>,
}

impl DiskUsage {
    /// Recordings of this many (counted) bytes were deleted outside
    /// retention, e.g. from the web UI.
    pub fn released(&self, bytes: u64) {
        self.released.fetch_add(bytes, Ordering::Relaxed);
        // (A plain CAS loop: `fetch_update` is deprecated on newer Rust and
        // its replacement isn't on older ones.)
        let mut u = self.used.load(Ordering::Relaxed);
        while let Err(now) = self.used.compare_exchange_weak(
            u,
            u.saturating_sub(bytes),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            u = now;
        }
    }

    pub fn snapshot(&self) -> StorageStatus {
        let free = self.free.load(Ordering::Relaxed);
        let until = self.expired_until_ns.load(Ordering::Relaxed);
        StorageStatus {
            used_bytes: self.used.load(Ordering::Relaxed),
            budget_bytes: self.budget.load(Ordering::Relaxed),
            min_free_bytes: self.min_free.load(Ordering::Relaxed),
            free_bytes: (free != u64::MAX).then_some(free),
            expired: self.expired.load(Ordering::Relaxed),
            expired_bytes: self.expired_bytes.load(Ordering::Relaxed),
            expired_until_ns: (until != 0).then_some(until),
        }
    }
}

/// Free bytes for unprivileged writers on the filesystem holding `path`.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // the field types vary by platform
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// How often to look at free space when nothing forces it.
const FREE_EVERY: Duration = Duration::from_secs(10);
/// Rows read from the index per round of deletions.
const BATCH: usize = 200;

pub struct Retention {
    policy: Policy,
    usage: std::sync::Arc<DiskUsage>,
    used: u64,
    free: Option<u64>,
    free_at: Option<Instant>,
    /// Stand-in for statvfs in tests.
    free_fn: fn(&Path) -> Option<u64>,
}

impl Retention {
    pub fn new(policy: Policy, index: &Index) -> Result<Self, IndexError> {
        let usage = std::sync::Arc::new(DiskUsage::default());
        usage.budget.store(policy.budget_bytes, Ordering::Relaxed);
        usage
            .min_free
            .store(policy.min_free_bytes, Ordering::Relaxed);
        usage.free.store(u64::MAX, Ordering::Relaxed);
        let used = index.usage_bytes()?;
        usage.used.store(used, Ordering::Relaxed);
        Ok(Retention {
            policy,
            usage,
            used,
            free: None,
            free_at: None,
            free_fn: free_bytes,
        })
    }

    pub fn usage(&self) -> std::sync::Arc<DiskUsage> {
        self.usage.clone()
    }

    /// A recording finished with this many bytes on disk.
    pub fn added(&mut self, bytes: u64) {
        self.used += bytes;
        self.free = self.free.map(|f| f.saturating_sub(bytes));
        self.usage.used.store(self.used, Ordering::Relaxed);
    }

    fn over_budget(&self, share: f64) -> bool {
        self.policy.budget_bytes > 0 && self.used as f64 > share * self.policy.budget_bytes as f64
    }

    fn short_of_space(&self, margin: u64) -> bool {
        self.policy.min_free_bytes > 0
            && self
                .free
                .is_some_and(|f| f < self.policy.min_free_bytes + margin)
    }

    /// Delete old recordings if the policy calls for it. Returns how many
    /// were deleted. `force` re-reads free space now.
    pub fn run(&mut self, root: &Path, index: &Index, force: bool) -> Result<u64, IndexError> {
        let released = self.usage.released.swap(0, Ordering::Relaxed);
        if released > 0 {
            self.used = self.used.saturating_sub(released);
            self.usage.used.store(self.used, Ordering::Relaxed);
        }
        if force || self.free_at.is_none_or(|t| t.elapsed() >= FREE_EVERY) {
            self.free = (self.free_fn)(root);
            self.free_at = Some(Instant::now());
            self.usage
                .free
                .store(self.free.unwrap_or(u64::MAX), Ordering::Relaxed);
        }
        if !self.over_budget(self.policy.high) && !self.short_of_space(0) {
            return Ok(0);
        }
        // Free space gets a tenth of the floor as hysteresis.
        let margin = self.policy.min_free_bytes / 10;
        let mut deleted = 0;
        'rounds: loop {
            let rows = index.oldest_expirable(BATCH)?;
            if rows.is_empty() {
                break;
            }
            let mut done = Vec::with_capacity(rows.len());
            for r in &rows {
                if !self.over_budget(self.policy.low) && !self.short_of_space(margin) {
                    index.expire(&done)?;
                    break 'rounds;
                }
                let (counted, on_disk) = delete_files(root, r);
                self.used = self.used.saturating_sub(counted);
                self.free = self.free.map(|f| f + on_disk);
                self.usage.expired.fetch_add(1, Ordering::Relaxed);
                self.usage
                    .expired_bytes
                    .fetch_add(on_disk, Ordering::Relaxed);
                self.usage
                    .expired_until_ns
                    .fetch_max(r.start_ns, Ordering::Relaxed);
                done.push(r.id);
                deleted += 1;
            }
            index.expire(&done)?;
        }
        self.usage.used.store(self.used, Ordering::Relaxed);
        if let Some(f) = self.free {
            self.usage.free.store(f, Ordering::Relaxed);
        }
        Ok(deleted)
    }
}

/// Why [`delete_recording`] refused.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("no such recording")]
    NotFound,
    #[error("that recording is still being written")]
    Recording,
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Delete a recording by hand: its index row, WAV, Opus copy and any
/// leftover `.part` files. Returns the bytes the index counted for it,
/// for [`DiskUsage::released`]. Starred or not makes no difference here.
pub fn delete_recording(root: &Path, index: &Index, id: i64) -> Result<u64, DeleteError> {
    let row = index.get(id)?.ok_or(DeleteError::NotFound)?;
    if row.status == "recording" {
        return Err(DeleteError::Recording);
    }
    // The row goes first: a file that can't be removed is better left
    // behind than a row pointing at audio that's gone.
    index.delete(id)?;
    Ok(delete_files(root, &row).0)
}

/// Delete a row's files. Returns the bytes the index counted for it and
/// the bytes actually freed.
fn delete_files(root: &Path, r: &Row) -> (u64, u64) {
    let counted = if r.status == "complete" {
        (r.wav_bytes.unwrap_or(0) + r.opus_bytes.unwrap_or(0) + r.amb_bytes.unwrap_or(0)).max(0)
            as u64
    } else {
        0
    };
    let mut freed = 0;
    let opus = crate::recorder::opus_path(&r.wav_path);
    let amb = crate::recorder::amb_path(&r.wav_path);
    let mut dirs = Vec::new();
    for rel in [
        Some(r.wav_path.as_str()),
        r.opus_path.as_deref(),
        Some(&opus),
        Some(&amb),
    ]
    .into_iter()
    .flatten()
    {
        // Only paths the recorder wrote: relative, inside the folder.
        let p = Path::new(rel);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        for f in [root.join(p), {
            let mut s = root.join(p).into_os_string();
            s.push(".part");
            s.into()
        }] {
            if let Ok(m) = std::fs::symlink_metadata(&f)
                && m.is_file()
                && std::fs::remove_file(&f).is_ok()
            {
                freed += m.len();
            }
        }
        if let Some(d) = p.parent() {
            dirs.push(d.to_owned());
        }
    }
    // Drop the day, month and year folders once empty (remove_dir refuses
    // a folder that still has files).
    for d in dirs {
        let mut d = d.as_path();
        while !d.as_os_str().is_empty() {
            if std::fs::remove_dir(root.join(d)).is_err() {
                break;
            }
            d = d.parent().unwrap_or(Path::new(""));
        }
    }
    (counted, freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NewRow;

    /// `n` complete recordings of `size` bytes (WAV) plus `size / 8` (Opus),
    /// an hour apart, on disk under `root`.
    fn fill(root: &Path, ix: &Index, n: usize, size: usize) -> Vec<i64> {
        (0..n)
            .map(|k| {
                let wav = format!("2026/11/{:02}/{k}.wav", 1 + k / 24);
                let opus = crate::recorder::opus_path(&wav);
                std::fs::create_dir_all(root.join(&wav).parent().unwrap()).unwrap();
                std::fs::write(root.join(&wav), vec![0u8; size]).unwrap();
                std::fs::write(root.join(&opus), vec![0u8; size / 8]).unwrap();
                let id = ix
                    .insert(&NewRow {
                        freq_hz: 146.52e6,
                        channel: None,
                        origin: "grid",
                        mode: "nbfm",
                        start_ns: k as i64 * 3_600_000_000_000,
                        audio_rate: 8000,
                        wav_path: &wav,
                    })
                    .unwrap();
                ix.complete(id, 0, 1.0, 1.0, 20.0, 10.0, &wav, size as u64)
                    .unwrap();
                ix.set_opus(id, &opus, (size / 8) as u64).unwrap();
                id
            })
            .collect()
    }

    #[test]
    fn deletes_the_oldest_down_to_the_low_water_mark() {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        // 30 recordings of 9 kB = 270 kB against a 250 kB budget.
        let ids = fill(dir.path(), &ix, 30, 8000);
        let policy = Policy {
            budget_bytes: 250_000,
            min_free_bytes: 0,
            ..Policy::default()
        };
        let mut r = Retention::new(policy, &ix).unwrap();
        assert_eq!(r.usage().snapshot().used_bytes, 270_000);
        let n = r.run(dir.path(), &ix, true).unwrap();
        // Down to at most 85 % (212.5 kB): 23 left, the oldest 7 gone.
        assert_eq!(n, 7);
        let s = r.usage().snapshot();
        assert_eq!(s.used_bytes, 207_000);
        assert_eq!(s.used_bytes, ix.usage_bytes().unwrap());
        assert_eq!((s.expired, s.expired_bytes), (7, 63_000));
        assert_eq!(s.expired_until_ns, Some(6 * 3_600_000_000_000));
        assert_eq!(ix.get(ids[6]).unwrap().unwrap().status, "expired");
        assert_eq!(ix.get(ids[7]).unwrap().unwrap().status, "complete");
        assert!(!dir.path().join("2026/11/01/6.wav").exists());
        assert!(!dir.path().join("2026/11/01/6.opus").exists());
        assert!(dir.path().join("2026/11/01/7.wav").exists());
        // Under the high-water mark nothing more goes.
        assert_eq!(r.run(dir.path(), &ix, true).unwrap(), 0);
        r.added(30_000); // 237 kB, still under 95 %
        assert_eq!(r.run(dir.path(), &ix, false).unwrap(), 0);
    }

    #[test]
    fn empty_day_folders_go_with_their_last_recording() {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        fill(dir.path(), &ix, 30, 8000); // day 01 has 24, day 02 has 6
        let mut r = Retention::new(
            Policy {
                budget_bytes: 60_000,
                min_free_bytes: 0,
                ..Policy::default()
            },
            &ix,
        )
        .unwrap();
        r.run(dir.path(), &ix, true).unwrap();
        assert!(!dir.path().join("2026/11/01").exists());
        assert!(dir.path().join("2026/11/02").exists());
        assert!(dir.path().join("index.sqlite").exists());
    }

    #[test]
    fn keeps_free_space_above_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        fill(dir.path(), &ix, 10, 8000);
        let mut r = Retention::new(
            Policy {
                budget_bytes: 0,
                min_free_bytes: 100_000,
                ..Policy::default()
            },
            &ix,
        )
        .unwrap();
        // The disk claims 75 kB free: needs 110 kB with the margin, so
        // four recordings (36 kB) go.
        r.free_fn = |_| Some(75_000);
        assert_eq!(r.run(dir.path(), &ix, true).unwrap(), 4);
        assert_eq!(r.usage().snapshot().free_bytes, Some(111_000));
    }

    #[test]
    fn crash_leftovers_are_cleaned_up_too() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.sqlite");
        let ix = Index::open(&p).unwrap();
        let id = ix
            .insert(&NewRow {
                freq_hz: 1.0,
                channel: None,
                origin: "grid",
                mode: "nbfm",
                start_ns: 0,
                audio_rate: 8000,
                wav_path: "2026/11/01/c.wav",
            })
            .unwrap();
        std::fs::create_dir_all(dir.path().join("2026/11/01")).unwrap();
        std::fs::write(dir.path().join("2026/11/01/c.wav.part"), [0u8; 500]).unwrap();
        std::fs::write(dir.path().join("2026/11/01/c.opus.part"), [0u8; 50]).unwrap();
        drop(ix);
        let ix = Index::open(&p).unwrap(); // marks the row interrupted
        fill(dir.path(), &ix, 2, 8000);
        let mut r = Retention::new(
            Policy {
                budget_bytes: 18_000,
                min_free_bytes: 0,
                ..Policy::default()
            },
            &ix,
        )
        .unwrap();
        assert_eq!(r.run(dir.path(), &ix, true).unwrap(), 2);
        assert_eq!(ix.get(id).unwrap().unwrap().status, "expired");
        assert!(!dir.path().join("2026/11/01/c.wav.part").exists());
        assert_eq!(r.usage().snapshot().expired_bytes, 550 + 9000);
    }

    #[test]
    fn starred_recordings_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let ids = fill(dir.path(), &ix, 30, 8000);
        // The two oldest are starred; the next seven go instead.
        assert!(ix.set_starred(ids[0], true).unwrap());
        assert!(ix.set_starred(ids[1], true).unwrap());
        assert!(!ix.set_starred(9999, true).unwrap());
        let mut r = Retention::new(
            Policy {
                budget_bytes: 250_000,
                min_free_bytes: 0,
                ..Policy::default()
            },
            &ix,
        )
        .unwrap();
        assert_eq!(r.run(dir.path(), &ix, true).unwrap(), 7);
        for (k, id) in ids.iter().enumerate().take(10) {
            let want = if (2..9).contains(&k) {
                "expired"
            } else {
                "complete"
            };
            assert_eq!(ix.get(*id).unwrap().unwrap().status, want, "recording {k}");
        }
        assert!(dir.path().join("2026/11/01/0.wav").exists());
        // With everything left starred, retention stops rather than loop.
        for id in &ids {
            ix.set_starred(*id, true).unwrap();
        }
        r.added(1_000_000);
        assert_eq!(r.run(dir.path(), &ix, true).unwrap(), 0);
    }

    #[test]
    fn deleting_by_hand_removes_files_and_row_and_frees_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let ix = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let ids = fill(dir.path(), &ix, 3, 8000);
        let mut r = Retention::new(
            Policy {
                budget_bytes: 1_000_000,
                min_free_bytes: 0,
                ..Policy::default()
            },
            &ix,
        )
        .unwrap();
        ix.set_starred(ids[1], true).unwrap();
        let freed = delete_recording(dir.path(), &ix, ids[1]).unwrap();
        assert_eq!(freed, 9000);
        assert!(ix.get(ids[1]).unwrap().is_none());
        assert!(!dir.path().join("2026/11/01/1.wav").exists());
        assert!(!dir.path().join("2026/11/01/1.opus").exists());
        assert!(dir.path().join("2026/11/01/2.wav").exists());
        let usage = r.usage();
        usage.released(freed);
        assert_eq!(usage.snapshot().used_bytes, 18_000);
        r.run(dir.path(), &ix, true).unwrap();
        assert_eq!(usage.snapshot().used_bytes, ix.usage_bytes().unwrap());
        assert!(matches!(
            delete_recording(dir.path(), &ix, ids[1]),
            Err(DeleteError::NotFound)
        ));
        let busy = ix
            .insert(&NewRow {
                freq_hz: 1.0,
                channel: None,
                origin: "grid",
                mode: "nbfm",
                start_ns: 0,
                audio_rate: 8000,
                wav_path: "2026/11/01/busy.wav",
            })
            .unwrap();
        assert!(matches!(
            delete_recording(dir.path(), &ix, busy),
            Err(DeleteError::Recording)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn reads_free_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).is_some_and(|f| f > 0));
        assert_eq!(free_bytes(&dir.path().join("missing")), None);
    }
}
