//! Turns scanner [`Output`] records into WAV files and index rows. Runs on
//! its own thread so disk latency never stalls the DSP threads.

use crate::index::{Index, IndexError, NewRow};
use crate::wav::WavWriter;
use scanner_core::clock::compact_utc;
use scanner_core::scanner::{Output, TxStart};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("index: {0}")]
    Index(#[from] IndexError),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecorderStats {
    pub files: u64,
    pub bytes: u64,
    pub audio_secs: f64,
    /// Index row ids completed by this recorder, in close order.
    pub rows: Vec<i64>,
}

struct Open {
    row: i64,
    start: TxStart,
    rel: String,
    wav: WavWriter,
}

pub struct Recorder {
    root: PathBuf,
    index: Index,
    open: HashMap<u64, Open>,
    stats: RecorderStats,
}

/// `146.5200_20261122T065403.123Z.wav` under `YYYY/MM/DD/`.
pub fn relative_path(start: &TxStart) -> String {
    let ts = compact_utc(start.start_time);
    format!(
        "{}/{}/{}/{:.4}_{ts}.wav",
        &ts[0..4],
        &ts[4..6],
        &ts[6..8],
        start.slot.freq / 1e6
    )
}

fn unix_ns(t: std::time::SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

impl Recorder {
    /// Recordings go under `root`; the index is `root/index.sqlite`.
    pub fn new(root: &Path) -> Result<Self, RecorderError> {
        std::fs::create_dir_all(root)?;
        Ok(Recorder {
            index: Index::open(&root.join("index.sqlite"))?,
            root: root.to_owned(),
            open: HashMap::new(),
            stats: RecorderStats::default(),
        })
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn stats(&self) -> &RecorderStats {
        &self.stats
    }

    pub fn handle(&mut self, o: Output) -> Result<(), RecorderError> {
        match o {
            Output::Open(start) => {
                let mut rel = relative_path(&start);
                // Two slots can start in the same millisecond on the same
                // frequency only if the plan has duplicates; still never clobber.
                let mut k = 1;
                while self.root.join(&rel).exists() {
                    rel = relative_path(&start).replace(".wav", &format!("-{k}.wav"));
                    k += 1;
                }
                let wav = WavWriter::create(&self.root.join(&rel), start.audio_rate)?;
                let row = self.index.insert(&NewRow {
                    freq_hz: start.slot.freq,
                    channel: start.slot.name.as_deref(),
                    origin: &format!("{:?}", start.slot.origin).to_lowercase(),
                    mode: &format!("{:?}", start.slot.mode).to_lowercase(),
                    start_ns: unix_ns(start.start_time),
                    audio_rate: start.audio_rate,
                    wav_path: &rel,
                })?;
                self.open.insert(
                    start.id,
                    Open {
                        row,
                        start,
                        rel,
                        wav,
                    },
                );
            }
            Output::Audio { id, samples } => {
                if let Some(o) = self.open.get_mut(&id) {
                    o.wav.write(&samples)?;
                }
            }
            Output::Close(end) => {
                if let Some(o) = self.open.remove(&end.id) {
                    let rate = o.start.audio_rate;
                    let audio_secs = o.wav.samples() as f64 / rate as f64;
                    let (_, bytes) = o.wav.finish(rate)?;
                    let end_ns = unix_ns(o.start.start_time) + (audio_secs * 1e9) as i64;
                    self.index.complete(
                        o.row,
                        end_ns,
                        audio_secs,
                        end.signal_secs,
                        end.peak_snr_db,
                        end.mean_snr_db,
                        &o.rel,
                        bytes,
                    )?;
                    self.stats.files += 1;
                    self.stats.bytes += bytes;
                    self.stats.audio_secs += audio_secs;
                    self.stats.rows.push(o.row);
                }
            }
        }
        Ok(())
    }

    /// Run on a thread, fed through a bounded channel of output batches.
    pub fn spawn(
        mut self,
        depth: usize,
    ) -> (
        SyncSender<Vec<Output>>,
        JoinHandle<Result<RecorderStats, RecorderError>>,
    ) {
        let (tx, rx): (SyncSender<Vec<Output>>, Receiver<Vec<Output>>) = sync_channel(depth);
        let h = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                for batch in rx {
                    for o in batch {
                        self.handle(o)?;
                    }
                }
                // Stream ended with files still open (should not happen after
                // Scanner::finish); leave them as .part and rows 'recording'.
                Ok(self.stats)
            })
            .expect("spawn recorder thread");
        (tx, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scanner_core::config::{Mode, Slot, SlotOrigin};
    use scanner_core::scanner::TxEnd;
    use std::time::Duration;

    #[test]
    fn writes_file_and_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Recorder::new(dir.path()).unwrap();
        let start = TxStart {
            id: 7,
            slot: Slot {
                freq: 146.52e6,
                bandwidth: 16e3,
                deviation: 5e3,
                mode: Mode::Nbfm,
                priority: true,
                record: true,
                name: Some("Calling".into()),
                origin: SlotOrigin::Listed,
            },
            start_sample: 0,
            start_time: UNIX_EPOCH + Duration::from_millis(1_795_330_443_123),
            audio_rate: 8000,
        };
        assert_eq!(
            relative_path(&start),
            "2026/11/22/146.5200_20261122T065403.123Z.wav"
        );
        r.handle(Output::Open(start)).unwrap();
        r.handle(Output::Audio {
            id: 7,
            samples: vec![0.1; 8000],
        })
        .unwrap();
        r.handle(Output::Close(TxEnd {
            id: 7,
            end_sample: 0,
            signal_secs: 0.8,
            peak_snr_db: 30.0,
            mean_snr_db: 25.0,
        }))
        .unwrap();
        let rows = r.index().recent(5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin, "listed");
        assert_eq!(rows[0].audio_secs, Some(1.0));
        assert!(dir.path().join(&rows[0].wav_path).exists());
        assert_eq!(r.stats().files, 1);
    }
}
