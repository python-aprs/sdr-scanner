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

/// Read a 16-bit PCM WAV (mono, or the first channel of several), as
/// written by [`WavWriter`] or most other tools. Returns the sample rate
/// and the samples in -1..1.
pub fn read(path: &Path) -> io::Result<(u32, Vec<f32>)> {
    parse(&fs::read(path)?)
}

fn parse(data: &[u8]) -> io::Result<(u32, Vec<f32>)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    let mut fmt = None;
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32_at(pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if len >= 16 && body + 16 <= data.len() => {
                fmt = Some((
                    u16_at(body),
                    u16_at(body + 2),
                    u32_at(body + 4),
                    u16_at(body + 14),
                ));
            }
            b"data" => {
                let (tag, channels, rate, bits) = fmt.ok_or_else(|| bad("no fmt chunk"))?;
                // 1 = PCM; 0xFFFE = extensible, which we take as PCM too.
                if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels == 0 {
                    return Err(bad("only 16-bit PCM WAV is supported"));
                }
                // A header left at 0 (a `.part`) means "to the end".
                let end = if len == 0 {
                    data.len()
                } else {
                    (body + len).min(data.len())
                };
                let step = 2 * channels as usize;
                let samples = data[body..end]
                    .chunks_exact(step)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                    .collect();
                return Ok((rate, samples));
            }
            _ => {}
        }
        pos = body + len + (len & 1);
    }
    Err(bad("no data chunk"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_what_it_writes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.wav");
        let mut w = WavWriter::create(&p, 8000).unwrap();
        w.write(&[0.0, 0.5, -0.25]).unwrap();
        w.finish(8000).unwrap();
        let (rate, s) = read(&p).unwrap();
        assert_eq!(rate, 8000);
        assert_eq!(s.len(), 3);
        assert!((s[1] - 0.5).abs() < 1e-4 && (s[2] + 0.25).abs() < 1e-4);
        assert!(parse(b"RIFF....WAVEjunk").is_err());
    }

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
