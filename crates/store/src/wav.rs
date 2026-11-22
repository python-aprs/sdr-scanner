//! Minimal streaming WAV writer (mono, 16-bit PCM). Writes to `<name>.part`
//! and renames on [`WavWriter::finish`], so folder watchers only ever see
//! complete files.

use std::fs::{self, File};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct WavWriter {
    file: BufWriter<File>,
    part: PathBuf,
    path: PathBuf,
    samples: u64,
}

fn header(rate: u32, data_bytes: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let mut file = BufWriter::with_capacity(64 * 1024, File::create(&part)?);
        file.write_all(&header(rate, 0))?;
        Ok(WavWriter {
            file,
            part,
            path: path.to_owned(),
            samples: 0,
        })
    }

    /// Append samples in -1..1 (clipped).
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        self.file.write_all(&buf)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Patch the header, close and rename into place. Returns the final
    /// path and the file size in bytes.
    pub fn finish(mut self, rate: u32) -> io::Result<(PathBuf, u64)> {
        let data = (self.samples * 2).min(u32::MAX as u64 - 36) as u32;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(rate, data))?;
        let f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.sync_data()?;
        drop(f);
        fs::rename(&self.part, &self.path)?;
        Ok((self.path, 44 + data as u64))
    }

    /// Abandon the file (deletes the `.part`).
    pub fn abort(self) -> io::Result<()> {
        drop(self.file);
        fs::remove_file(&self.part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_header_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/x.wav");
        let mut w = WavWriter::create(&p, 8000).unwrap();
        assert!(dir.path().join("a/b/x.wav.part").exists());
        w.write(&[0.0, 0.5, -1.0, 2.0]).unwrap();
        let (path, bytes) = w.finish(8000).unwrap();
        assert_eq!(path, p);
        assert_eq!(bytes, 52);
        let data = fs::read(&p).unwrap();
        assert_eq!(data.len(), 52);
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 44);
        assert_eq!(u32::from_le_bytes(data[24..28].try_into().unwrap()), 8000);
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 8);
        let s: Vec<i16> = data[44..]
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(s, vec![0, 16384, -32767, 32767]);
        assert!(!dir.path().join("a/b/x.wav.part").exists());
    }
}
