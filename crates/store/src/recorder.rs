//! Turns scanner [`Output`] records into WAV files and index rows. Runs on
//! its own thread so disk latency never stalls the DSP threads.

use crate::index::{Index, IndexError, NewRow};
use crate::ogg::OggOpusWriter;
use crate::retention::{DiskUsage, Policy, Retention};
use crate::wav::WavWriter;
use scanner_core::clock::{compact_utc, unix_ns};
use scanner_core::dtmf::DtmfDecoder;
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
    /// Recordings tagged with DTMF digits.
    pub dtmf: u64,
    /// Digital voice calls recorded.
    pub calls: u64,
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
    /// Listens to the recorded audio for DTMF digits.
    dtmf: Option<DtmfDecoder>,
    /// A digital call's raw voice frames, once it has some.
    amb: Option<AmbWriter>,
}

/// Raw AMBE voice frames as dsd writes them (`.amb`): a 4-byte header, then
/// per 20 ms frame an error count and the 49 parameter bits in 7 bytes. dsd
/// and mbelib-based tools can play them; `scanner ambe` turns them into WAV
/// with a vocoder build. Written to `.part` and renamed when complete.
struct AmbWriter {
    rel: String,
    part: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    bytes: u64,
}

pub const AMB_MAGIC: &[u8; 4] = b".amb";

impl AmbWriter {
    fn create(root: &Path, rel: String) -> std::io::Result<Self> {
        use std::io::Write;
        let mut part = root.join(&rel).into_os_string();
        part.push(".part");
        let part = PathBuf::from(part);
        let mut file = std::io::BufWriter::new(std::fs::File::create(&part)?);
        file.write_all(AMB_MAGIC)?;
        Ok(AmbWriter {
            rel,
            part,
            file,
            bytes: AMB_MAGIC.len() as u64,
        })
    }

    fn write(&mut self, frames: &[scanner_core::dmr::AmbeFrame]) -> std::io::Result<()> {
        use std::io::Write;
        for f in frames {
            self.file.write_all(&f.amb_bytes())?;
            self.bytes += 8;
        }
        Ok(())
    }

    fn finish(self, root: &Path) -> std::io::Result<u64> {
        let f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        std::fs::rename(&self.part, root.join(&self.rel))?;
        Ok(self.bytes)
    }

    fn abort(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.part);
    }
}

pub struct Recorder {
    root: PathBuf,
    index: Index,
    open: HashMap<u64, Open>,
    stats: RecorderStats,
    opus: bool,
    dtmf: bool,
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

/// The raw voice frames of a digital call sit next to the WAV (`.amb`).
pub fn amb_path(wav_rel: &str) -> String {
    match wav_rel.strip_suffix(".wav") {
        Some(stem) => format!("{stem}.amb"),
        None => format!("{wav_rel}.amb"),
    }
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
            dtmf: true,
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

    /// Decode DTMF digits from each recording and store them in the index
    /// (on by default). Runs on the recorder thread, off the DSP path.
    pub fn with_dtmf(mut self, on: bool) -> Self {
        self.dtmf = on;
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
                let dtmf = (self.dtmf && start.audio_rate >= 4000)
                    .then(|| DtmfDecoder::new(start.audio_rate as f64));
                self.open.insert(
                    start.id,
                    Open {
                        row,
                        start,
                        rel,
                        wav,
                        opus,
                        dtmf,
                        amb: None,
                    },
                );
            }
            Output::Ambe { id, frames } => {
                if let Some(o) = self.open.get_mut(&id) {
                    if o.amb.is_none() {
                        o.amb = Some(AmbWriter::create(&self.root, amb_path(&o.rel))?);
                    }
                    if let Some(w) = &mut o.amb {
                        w.write(&frames)?;
                    }
                }
            }
            Output::Audio { id, samples } => {
                if let Some(o) = self.open.get_mut(&id) {
                    o.wav.write(&samples)?;
                    if let Some(d) = &mut o.dtmf {
                        d.push(&samples);
                    }
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
                self.index
                    .insert_packet(&crate::index::PacketRow::from_decoded(&p))?;
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
                    if let Some(w) = o.amb {
                        w.abort();
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
                    if let Some(d) = &o.dtmf
                        && !d.digits().is_empty()
                    {
                        self.index.set_dtmf(o.row, Some(&d.text()))?;
                        self.stats.dtmf += 1;
                    }
                    let mut amb_bytes = 0;
                    if let Some(w) = o.amb {
                        amb_bytes = w.finish(&self.root)?;
                    }
                    if let Some(d) = &end.digital {
                        let d = crate::index::Digital::from_call(d);
                        self.index.set_digital(o.row, &d, amb_bytes)?;
                        self.stats.calls += 1;
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
                        r.added(bytes + opus_bytes + amb_bytes);
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
            digital: None,
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

    #[test]
    fn keeps_dmr_voice_frames_and_ids() {
        use scanner_core::dmr::{AmbeFrame, CallInfo, Lc};
        let dir = tempfile::tempdir().unwrap();
        let mut r = Recorder::new(dir.path()).unwrap();
        r.handle(Output::Open(TxStart {
            id: 3,
            slot: Slot {
                freq: 145.79e6,
                bandwidth: 11e3,
                deviation: 0.0,
                mode: Mode::Dmr,
                priority: false,
                record: true,
                name: Some("DMR simplex".into()),
                origin: SlotOrigin::Listed,
                pair: None,
            },
            start_sample: 0,
            start_time: UNIX_EPOCH + Duration::from_millis(1_795_330_443_123),
            audio_rate: 8000,
        }))
        .unwrap();
        for n in 0..4 {
            r.handle(Output::Ambe {
                id: 3,
                frames: (0..3)
                    .map(|k| AmbeFrame {
                        bits: scanner_core::dmr::tx::test_frame(n * 3 + k),
                        errs: 0,
                    })
                    .collect(),
            })
            .unwrap();
            r.handle(Output::Audio {
                id: 3,
                samples: vec![0.1; 480],
            })
            .unwrap();
        }
        let info = CallInfo {
            timeslot: None,
            color_code: Some(1),
            lc: Some(Lc::group(99, 3_171_501)),
            encrypted: false,
            alias: Some("KF7HVM".into()),
            repeater: false,
        };
        r.handle(Output::Close(TxEnd {
            id: 3,
            end_sample: 0,
            signal_secs: 0.24,
            peak_snr_db: 30.0,
            mean_snr_db: 25.0,
            tone: None,
            digital: Some(scanner_core::demod::CallInfo::Dmr(info)),
        }))
        .unwrap();
        let rows = r.index().recent(5).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.dst_id, row.src_id), (Some(99), Some(3_171_501)));
        assert_eq!(row.call_type.as_deref(), Some("group"));
        assert_eq!(row.color_code, Some(1));
        assert_eq!(row.timeslot, None);
        assert_eq!(row.alias.as_deref(), Some("KF7HVM"));
        assert_eq!(row.encrypted, Some(false));
        let amb = std::fs::read(dir.path().join(amb_path(&row.wav_path))).unwrap();
        assert_eq!(&amb[..4], AMB_MAGIC);
        assert_eq!(amb.len(), 4 + 12 * 8);
        assert_eq!(row.amb_bytes, Some(amb.len() as i64));
        let f = AmbeFrame::from_amb_bytes(amb[4 + 8..4 + 16].try_into().unwrap());
        assert_eq!(f.bits, scanner_core::dmr::tx::test_frame(1));
        let q = crate::index::Query {
            dst_id: Some(99),
            ..Default::default()
        };
        assert_eq!(r.index().search(&q).unwrap().len(), 1);
    }

    #[test]
    fn tags_dtmf_digits() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Recorder::new(dir.path()).unwrap().with_opus(false);
        let slot = Slot {
            freq: 147.52e6,
            bandwidth: 16e3,
            deviation: 5e3,
            mode: Mode::Nbfm,
            priority: false,
            record: true,
            name: None,
            origin: SlotOrigin::Listed,
            pair: None,
        };
        for (id, audio) in [
            (
                1,
                scanner_core::dtmf::synth(
                    8000.0,
                    &[('*', 0.1, 0.1), ('7', 0.1, 0.1), ('3', 0.1, 0.3)],
                    0.1,
                ),
            ),
            (2, vec![0.05; 8000]),
        ] {
            r.handle(Output::Open(TxStart {
                id,
                slot: slot.clone(),
                start_sample: 0,
                start_time: UNIX_EPOCH + Duration::from_secs(1_795_330_443 + id),
                audio_rate: 8000,
            }))
            .unwrap();
            // In uneven batches, as the scanner sends them.
            for c in audio.chunks(700) {
                r.handle(Output::Audio {
                    id,
                    samples: c.to_vec(),
                })
                .unwrap();
            }
            r.handle(Output::Close(TxEnd {
                id,
                end_sample: 0,
                signal_secs: 1.0,
                peak_snr_db: 30.0,
                mean_snr_db: 25.0,
                tone: None,
                digital: None,
            }))
            .unwrap();
        }
        let rows = r.index().recent(5).unwrap();
        let by_id = |i: usize| rows.iter().find(|row| row.id == r.stats().rows[i]).unwrap();
        assert_eq!(by_id(0).dtmf.as_deref(), Some("*73"));
        assert_eq!(by_id(1).dtmf, None);
        assert_eq!(r.stats().dtmf, 1);
    }
}
