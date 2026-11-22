//! Replay IQ recordings (SigMF) as a [`Source`].

use crate::sigmf::{self, Meta, SampleFormat};
use crate::{Source, SourceError, SourceInfo, SourceStats};
use num_complex::Complex32;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Reads a SigMF recording. With `realtime`, `read` sleeps so samples are
/// delivered no faster than the recorded sample rate (for live-like replay);
/// otherwise it runs as fast as possible (for tests and benchmarks).
pub struct FileSource {
    info: SourceInfo,
    meta: Meta,
    format: SampleFormat,
    reader: BufReader<File>,
    raw: Vec<u8>,
    stats: SourceStats,
    realtime: Option<Instant>,
    data_path: PathBuf,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self, SourceError> {
        let (meta_path, data_path) = sigmf::paths_for(path);
        let meta_text = std::fs::read_to_string(&meta_path)?;
        let meta: Meta = serde_json::from_str(&meta_text)
            .map_err(|e| SourceError::Meta(format!("{}: {e}", meta_path.display())))?;
        let format = meta
            .format()
            .ok_or_else(|| SourceError::UnsupportedFormat(meta.global.datatype.clone()))?;
        let sample_rate = meta
            .global
            .sample_rate
            .ok_or_else(|| SourceError::Meta("missing core:sample_rate".into()))?;
        let capture = meta.captures.first();
        let center_freq = capture.and_then(|c| c.frequency).unwrap_or(0.0);
        let start_time = capture
            .and_then(|c| c.datetime.as_deref())
            .and_then(sigmf::parse_datetime)
            .map(|s| UNIX_EPOCH + Duration::from_secs_f64(s));
        let file = File::open(&data_path)?;
        let len = file.metadata()?.len();
        let info = SourceInfo {
            sample_rate,
            center_freq,
            start_time,
            total_samples: Some(len / format.bytes_per_sample() as u64),
            description: format!("file {}", data_path.display()),
        };
        Ok(Self {
            info,
            meta,
            format,
            reader: BufReader::with_capacity(1 << 20, file),
            raw: Vec::new(),
            stats: SourceStats::default(),
            realtime: None,
            data_path,
        })
    }

    /// Pace delivery at the recorded sample rate.
    pub fn realtime(mut self, on: bool) -> Self {
        self.realtime = on.then(Instant::now);
        self
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn format(&self) -> SampleFormat {
        self.format
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    fn pace(&self) {
        if let Some(t0) = self.realtime {
            let due =
                Duration::from_secs_f64(self.stats.samples_read as f64 / self.info.sample_rate);
            if let Some(wait) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
}

/// Fill as much of `buf` as possible; returns bytes read (short only at EOF).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Convert raw little-endian bytes to normalised complex samples (|x| <= ~1).
pub fn convert(format: SampleFormat, raw: &[u8], out: &mut [Complex32]) {
    match format {
        SampleFormat::Ci16Le => {
            const S: f32 = 1.0 / 32768.0;
            for (o, c) in out.iter_mut().zip(raw.chunks_exact(4)) {
                let i = i16::from_le_bytes([c[0], c[1]]) as f32;
                let q = i16::from_le_bytes([c[2], c[3]]) as f32;
                *o = Complex32::new(i * S, q * S);
            }
        }
        SampleFormat::Cf32Le => {
            for (o, c) in out.iter_mut().zip(raw.chunks_exact(8)) {
                let i = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let q = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                *o = Complex32::new(i, q);
            }
        }
        SampleFormat::Cu8 => {
            for (o, c) in out.iter_mut().zip(raw.chunks_exact(2)) {
                *o = Complex32::new((c[0] as f32 - 127.5) / 128.0, (c[1] as f32 - 127.5) / 128.0);
            }
        }
        SampleFormat::Ci8 => {
            for (o, c) in out.iter_mut().zip(raw.chunks_exact(2)) {
                *o = Complex32::new(c[0] as i8 as f32 / 128.0, c[1] as i8 as f32 / 128.0);
            }
        }
    }
}

impl Source for FileSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, out: &mut [Complex32]) -> Result<usize, SourceError> {
        let bps = self.format.bytes_per_sample();
        self.raw.resize(out.len() * bps, 0);
        let got = read_full(&mut self.reader, &mut self.raw)?;
        let n = got / bps;
        convert(self.format, &self.raw[..n * bps], &mut out[..n]);
        self.stats.samples_read += n as u64;
        self.pace();
        Ok(n)
    }

    fn stats(&self) -> SourceStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_recording(dir: &Path, datatype: &str, data: &[u8]) -> PathBuf {
        let base = dir.join("rec");
        std::fs::write(
            base.with_extension("sigmf-meta"),
            format!(
                r#"{{"global":{{"core:datatype":"{datatype}","core:sample_rate":1000,"core:version":"1.2.0"}},
                   "captures":[{{"core:sample_start":0,"core:frequency":146520000,"core:datetime":"2026-11-22T06:54:03Z"}}],
                   "annotations":[]}}"#
            ),
        )
        .unwrap();
        File::create(base.with_extension("sigmf-data"))
            .unwrap()
            .write_all(data)
            .unwrap();
        base
    }

    #[test]
    fn reads_ci16_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Vec::new();
        for k in 0..10i16 {
            data.extend_from_slice(&(k * 1000).to_le_bytes());
            data.extend_from_slice(&(-k * 1000).to_le_bytes());
        }
        let base = write_recording(dir.path(), "ci16_le", &data);
        let mut src = FileSource::open(&base).unwrap();
        assert_eq!(src.info().total_samples, Some(10));
        assert_eq!(src.info().center_freq, 146.52e6);
        assert_eq!(
            src.info().start_time,
            Some(UNIX_EPOCH + Duration::from_secs(1_795_330_443))
        );
        let mut buf = vec![Complex32::default(); 4];
        let mut all = Vec::new();
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        assert_eq!(all.len(), 10);
        assert_eq!(src.stats().samples_read, 10);
        assert!((all[3].re - 3000.0 / 32768.0).abs() < 1e-7);
        assert!((all[3].im + 3000.0 / 32768.0).abs() < 1e-7);
    }

    #[test]
    fn reads_cf32() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Vec::new();
        for v in [0.5f32, -0.25, 0.125, 1.0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let base = write_recording(dir.path(), "cf32_le", &data);
        let mut src = FileSource::open(&base.with_extension("sigmf-data")).unwrap();
        let mut buf = vec![Complex32::default(); 8];
        assert_eq!(src.read(&mut buf).unwrap(), 2);
        assert_eq!(buf[0], Complex32::new(0.5, -0.25));
        assert_eq!(buf[1], Complex32::new(0.125, 1.0));
        assert_eq!(src.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn rejects_unknown_format() {
        let dir = tempfile::tempdir().unwrap();
        let base = write_recording(dir.path(), "rf64_le", &[]);
        assert!(matches!(
            FileSource::open(&base),
            Err(SourceError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn realtime_pacing_is_not_faster_than_sample_rate() {
        let dir = tempfile::tempdir().unwrap();
        // 1000 S/s, 100 samples => >= ~100 ms when paced
        let base = write_recording(dir.path(), "ci8", &[0u8; 200]);
        let mut src = FileSource::open(&base).unwrap().realtime(true);
        let t = Instant::now();
        let mut buf = vec![Complex32::default(); 25];
        while src.read(&mut buf).unwrap() > 0 {}
        assert!(
            t.elapsed() >= Duration::from_millis(95),
            "{:?}",
            t.elapsed()
        );
    }
}
