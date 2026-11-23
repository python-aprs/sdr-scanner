//! Turns scanner [`Output`] records into WAV files and index rows. Runs on
//! its own thread so disk latency never stalls the DSP threads.

use crate::index::{Index, IndexError, NewRow};
use crate::ogg::OggOpusWriter;
use crate::retention::{DiskUsage, Policy, Retention};
use crate::wav::WavWriter;
use scanner_core::clock::{compact_utc, unix_ns};
use scanner_core::scanner::{Output, TxStart};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

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
    /// Opus copies written, and their bytes.
    pub opus_files: u64,
    pub opus_bytes: u64,
    /// Opus copies abandoned after an error (the WAV is still kept).
    pub opus_errors: u64,
    /// AX.25 packets stored.
    pub packets: u64,
    /// Recordings thrown away part-way (a carrier found to be a spur).
    pub discarded: u64,
    /// Index row ids completed by this recorder, in close order.
    pub rows: Vec<i64>,
}

struct Open {
    row: i64,
    start: TxStart,
    rel: String,
    wav: WavWriter,
    /// The web copy; dropped (leaving a stray `.part`) if it fails.
    opus: Option<(String, OggOpusWriter)>,
}

pub struct Recorder {
    root: PathBuf,
    index: Index,
    open: HashMap<u64, Open>,
    stats: RecorderStats,
    opus: bool,
    retention: Option<Retention>,
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

/// Sample rates the Opus encoder accepts.
pub const OPUS_RATES: [u32; 5] = [8000, 12000, 16000, 24000, 48000];

/// The Opus copy sits next to the WAV with the same name.
pub fn opus_path(wav_rel: &str) -> String {
    match wav_rel.strip_suffix(".wav") {
        Some(stem) => format!("{stem}.opus"),
        None => format!("{wav_rel}.opus"),
    }
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
            opus: true,
            retention: None,
        })
    }

    /// Keep the recordings within `policy`, deleting the oldest. Runs a
    /// pass now, then after each recording closes.
    pub fn with_retention(mut self, policy: Policy) -> Result<Self, RecorderError> {
        let mut r = Retention::new(policy, &self.index)?;
        let n = r.run(&self.root, &self.index, true)?;
        if n > 0 {
            eprintln!("retention: deleted the {n} oldest recordings");
        }
        self.retention = Some(r);
        Ok(self)
    }

    /// Disk usage for the API, when retention is on.
    pub fn disk_usage(&self) -> Option<std::sync::Arc<DiskUsage>> {
        self.retention.as_ref().map(Retention::usage)
    }

    /// Also write an Opus copy of each recording for the web UI (on by
    /// default). Rates Opus can't take (anything but 8, 12, 16, 24 or
    /// 48 kHz) get no copy.
    pub fn with_opus(mut self, on: bool) -> Self {
        self.opus = on;
        self
    }

    fn opus_failed(&mut self, rel: &str, e: std::io::Error) {
        eprintln!("recorder: dropping the Opus copy of {rel}: {e}");
        self.stats.opus_errors += 1;
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
                let opus = if self.opus && OPUS_RATES.contains(&start.audio_rate) {
                    let orel = opus_path(&rel);
                    match OggOpusWriter::create(&self.root.join(&orel), start.audio_rate) {
                        Ok(w) => Some((orel, w)),
                        Err(e) => {
                            self.opus_failed(&orel, e);
                            None
                        }
                    }
                } else {
                    None
                };
                let row = self.index.insert(&NewRow {
                    freq_hz: start.slot.freq,
                    channel: start.slot.name.as_deref(),
                    origin: start.slot.origin.as_str(),
                    mode: start.slot.mode.as_str(),
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
                        opus,
                    },
                );
            }
            Output::Audio { id, samples } => {
                if let Some(o) = self.open.get_mut(&id) {
                    o.wav.write(&samples)?;
                    let failed = match &mut o.opus {
                        Some((orel, w)) => w.write(&samples).err().map(|e| (orel.clone(), e)),
                        None => None,
                    };
                    if let Some((orel, e)) = failed {
                        o.opus = None;
                        self.opus_failed(&orel, e);
                    }
                }
            }
            Output::Packet(p) => {
                self.index.insert_packet(&crate::index::PacketRow {
                    id: 0,
                    time_ns: unix_ns(p.time),
                    freq_hz: p.slot.freq,
                    channel: p.slot.name.clone(),
                    source: p.frame.source.clone(),
                    dest: p.frame.dest.clone(),
                    path: p.frame.path.join(","),
                    text: p.frame.tnc2(),
                    raw: p.frame.raw,
                })?;
                self.stats.packets += 1;
            }
            Output::Discard { id } => {
                if let Some(o) = self.open.remove(&id) {
                    if let Err(e) = o.wav.abort() {
                        eprintln!("discarding {}: {e}", o.rel);
                    }
                    if let Some((orel, w)) = o.opus
                        && let Err(e) = w.abort()
                    {
                        eprintln!("discarding {orel}: {e}");
                    }
                    self.index.delete(o.row)?;
                    self.stats.discarded += 1;
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
                    if let Some(t) = &end.tone {
                        self.index.set_tone(o.row, &t.label())?;
                    }
                    let mut opus_bytes = 0;
                    if let Some((orel, w)) = o.opus {
                        match w.finish() {
                            Ok((_, n)) => {
                                self.index.set_opus(o.row, &orel, n)?;
                                self.stats.opus_files += 1;
                                self.stats.opus_bytes += n;
                                opus_bytes = n;
                            }
                            Err(e) => self.opus_failed(&orel, e),
                        }
                    }
                    if let Some(r) = &mut self.retention {
                        r.added(bytes + opus_bytes);
                        // A failed pass must not stop the recording.
                        if let Err(e) = r.run(&self.root, &self.index, false) {
                            eprintln!("retention: {e}");
                        }
                    }
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
    use std::time::{Duration, UNIX_EPOCH};

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
                pair: None,
            },
            start_sample: 0,
            start_time: UNIX_EPOCH + Duration::from_millis(1_795_330_443_123),
            audio_rate: 8000,
        };
        assert_eq!(
            relative_path(&start),
            "2026/11/22/146.5200_20261122T065403.123Z.wav"
        );
        r.handle(Output::Open(start.clone())).unwrap();
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
            tone: Some(scanner_core::tone::Tone::Ctcss(100.0)),
        }))
        .unwrap();
        let rows = r.index().recent(5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tone.as_deref(), Some("100.0"));
        assert_eq!(rows[0].origin, "listed");
        assert_eq!(rows[0].audio_secs, Some(1.0));
        assert!(dir.path().join(&rows[0].wav_path).exists());
        assert_eq!(
            rows[0].opus_path.as_deref(),
            Some("2026/11/22/146.5200_20261122T065403.123Z.opus")
        );
        let opus = std::fs::read(dir.path().join(rows[0].opus_path.as_ref().unwrap())).unwrap();
        assert_eq!(rows[0].opus_bytes, Some(opus.len() as i64));
        assert_eq!(&opus[..4], b"OggS");
        assert_eq!(r.stats().files, 1);
        assert_eq!((r.stats().opus_files, r.stats().opus_errors), (1, 0));

        // A discarded recording leaves no files and no row.
        let mut start = start;
        start.id = 8;
        start.start_time += Duration::from_secs(60);
        r.handle(Output::Open(start)).unwrap();
        r.handle(Output::Audio {
            id: 8,
            samples: vec![0.1; 8000],
        })
        .unwrap();
        r.handle(Output::Discard { id: 8 }).unwrap();
        assert_eq!(r.index().recent(5).unwrap().len(), 1);
        assert_eq!(r.stats().discarded, 1);
        let day = dir.path().join("2026/11/22");
        let names: Vec<_> = std::fs::read_dir(&day)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
