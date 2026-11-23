//! Streaming Ogg Opus writer (RFC 7845) for the web copy of each recording.
//! Like [`crate::wav::WavWriter`] it writes `<name>.part` and renames on
//! [`OggOpusWriter::finish`].
//!
//! Mono, 40 ms frames at the recording's rate (8 kHz by default), 16 kb/s:
//! about an eighth of the 8 kHz 16-bit WAV, which matters on a metered
//! uplink.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const BITRATE: i32 = 16_000;
const FRAME_MS: usize = 40;
/// Packets per Ogg page (about a second), so the page overhead stays small.
const PACKETS_PER_PAGE: usize = 25;

fn crc_table() -> &'static [u32; 256] {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 {
                    (r << 1) ^ 0x04c1_1db7
                } else {
                    r << 1
                };
            }
            *e = r;
        }
        t
    })
}

/// The Ogg page checksum (CRC-32, polynomial 0x04c11db7, not reflected).
pub fn ogg_crc(data: &[u8]) -> u32 {
    let t = crc_table();
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ t[((crc >> 24) as u8 ^ b) as usize]
    })
}

pub struct OggOpusWriter {
    file: BufWriter<File>,
    part: PathBuf,
    path: PathBuf,
    enc: opus::Encoder,
    /// Samples per frame at the input rate.
    frame: usize,
    /// 48 kHz granule units per input sample.
    scale: u64,
    pre_skip: u64,
    serial: u32,
    seq: u32,
    /// Input samples not yet encoded.
    buf: Vec<f32>,
    /// Input samples written so far.
    samples: u64,
    /// Packets encoded but not yet on a page.
    packets: Vec<Vec<u8>>,
    /// Input samples covered by the packets on finished pages.
    paged: u64,
    bytes: u64,
    /// Set by `finish`: keep the padding frames for the last page, whose
    /// granule must be the stream's highest.
    finishing: bool,
}

impl OggOpusWriter {
    /// `rate` must be one Opus accepts: 8, 12, 16, 24 or 48 kHz.
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        let mut enc = opus::Encoder::new(rate, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let _ = enc.set_bitrate(opus::Bitrate::Bits(BITRATE));
        let scale = 48_000 / rate as u64;
        let pre_skip = enc.get_lookahead().unwrap_or(0).max(0) as u64 * scale;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let file = BufWriter::with_capacity(16 * 1024, File::create(&part)?);
        // Any serial works for a single-stream file; vary it a little.
        let serial = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0))
            ^ 0x5343_4e4f;
        let mut w = OggOpusWriter {
            file,
            part,
            path: path.to_owned(),
            enc,
            frame: rate as usize * FRAME_MS / 1000,
            scale,
            pre_skip,
            serial,
            seq: 0,
            buf: Vec::new(),
            samples: 0,
            packets: Vec::new(),
            paged: 0,
            bytes: 0,
            finishing: false,
        };
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
        head.extend_from_slice(&rate.to_le_bytes()); // original rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // mapping family
        w.page(&[head], 0, 0x02)?;
        let vendor = b"sdr-scanner";
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.page(&[tags], 0, 0)?;
        Ok(w)
    }

    /// Write one Ogg page holding `packets` (each < 255 * 255 bytes).
    fn page(&mut self, packets: &[Vec<u8>], granule: u64, flags: u8) -> io::Result<()> {
        let mut lacing = Vec::new();
        for p in packets {
            let mut n = p.len();
            while n >= 255 {
                lacing.push(255u8);
                n -= 255;
            }
            lacing.push(n as u8);
        }
        let mut page =
            Vec::with_capacity(27 + lacing.len() + packets.iter().map(Vec::len).sum::<usize>());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.seq.to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        for p in packets {
            page.extend_from_slice(p);
        }
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.file.write_all(&page)?;
        self.seq += 1;
        self.bytes += page.len() as u64;
        Ok(())
    }

    fn encode_frames(&mut self) -> io::Result<()> {
        let mut used = 0;
        let mut out = [0u8; 1500];
        while self.buf.len() - used >= self.frame {
            let n = self
                .enc
                .encode_float(&self.buf[used..used + self.frame], &mut out)
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.packets.push(out[..n].to_vec());
            used += self.frame;
            if self.packets.len() >= PACKETS_PER_PAGE && !self.finishing {
                self.flush_page(false)?;
            }
        }
        self.buf.drain(..used);
        Ok(())
    }

    fn flush_page(&mut self, last: bool) -> io::Result<()> {
        if self.packets.is_empty() && !last {
            return Ok(());
        }
        let covered = self.paged + (self.packets.len() * self.frame) as u64;
        // A page's granule counts the 48 kHz samples decoded so far, pre-skip
        // included (RFC 7845 section 4). The last page's instead marks the
        // real end, so players trim the padding.
        let granule = if last {
            self.pre_skip + self.samples * self.scale
        } else {
            covered * self.scale
        };
        let packets = std::mem::take(&mut self.packets);
        self.page(&packets, granule, if last { 0x04 } else { 0 })?;
        self.paged = covered;
        Ok(())
    }

    /// Append samples in -1..1.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.buf.extend_from_slice(samples);
        self.samples += samples.len() as u64;
        self.encode_frames()
    }

    /// Pad with silence to cover the encoder's look-ahead, end the stream and rename into place. Returns the path
    /// and size in bytes.
    /// Abandon the file (deletes the `.part`).
    pub fn abort(self) -> io::Result<()> {
        drop(self.file);
        fs::remove_file(&self.part)
    }

    pub fn finish(mut self) -> io::Result<(PathBuf, u64)> {
        // Encode just enough frames that the decoder's output, delayed by
        // the look-ahead, reaches the last real sample.
        let lookahead = self.pre_skip / self.scale;
        let encoded = self.samples - self.buf.len() as u64;
        let total = (self.samples + lookahead).div_ceil(self.frame as u64) * self.frame as u64;
        let pad = (total - encoded) as usize - self.buf.len();
        self.buf.extend(std::iter::repeat_n(0.0, pad));
        self.finishing = true;
        self.encode_frames()?;
        self.flush_page(true)?;
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        drop(self.file);
        fs::rename(&self.part, &self.path)?;
        Ok((self.path, self.bytes))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Pages of an Ogg file: (granule, flags, packets), checking each CRC.
    pub fn read_pages(data: &[u8]) -> Vec<(u64, u8, Vec<Vec<u8>>)> {
        let mut pages = Vec::new();
        let mut i = 0;
        while i < data.len() {
            assert_eq!(&data[i..i + 4], b"OggS");
            let flags = data[i + 5];
            let granule = u64::from_le_bytes(data[i + 6..i + 14].try_into().unwrap());
            let nseg = data[i + 26] as usize;
            let lacing = &data[i + 27..i + 27 + nseg];
            let body_len: usize = lacing.iter().map(|&l| l as usize).sum();
            let end = i + 27 + nseg + body_len;
            let mut page = data[i..end].to_vec();
            let crc = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].fill(0);
            assert_eq!(ogg_crc(&page), crc, "page CRC");
            let mut packets = Vec::new();
            let mut cur = Vec::new();
            let mut off = i + 27 + nseg;
            for &l in lacing {
                cur.extend_from_slice(&data[off..off + l as usize]);
                off += l as usize;
                if l < 255 {
                    packets.push(std::mem::take(&mut cur));
                }
            }
            pages.push((granule, flags, packets));
            i = end;
        }
        pages
    }

    #[test]
    fn crc_matches_the_ogg_reference() {
        // The catalogued check value for CRC-32 with this polynomial, zero
        // init and no final xor (CRC-32/CKSUM before its xor).
        assert_eq!(ogg_crc(b""), 0);
        assert_eq!(ogg_crc(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn writes_a_valid_stream_that_decodes_to_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.opus");
        let mut w = OggOpusWriter::create(&p, 8000).unwrap();
        // 2.3 s of a 700 Hz tone in uneven pieces.
        let n = 18_400;
        let tone: Vec<f32> = (0..n)
            .map(|i| 0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 700.0 / 8000.0).sin())
            .collect();
        for piece in tone.chunks(1234) {
            w.write(piece).unwrap();
        }
        assert!(!p.exists(), "only the .part exists while writing");
        let (path, bytes) = w.finish().unwrap();
        assert_eq!(path, p);
        let data = std::fs::read(&p).unwrap();
        assert_eq!(data.len() as u64, bytes);
        // 16 kb/s for 2.3 s is ~4.6 kB; the WAV would be 36.8 kB.
        assert!(bytes < 7000, "{bytes}");
        let pages = read_pages(&data);
        assert_eq!(&pages[0].2[0][..8], b"OpusHead");
        assert_eq!(pages[0].1, 0x02, "first page is BOS");
        assert_eq!(&pages[1].2[0][..8], b"OpusTags");
        let (last_granule, last_flags, _) = pages.last().unwrap();
        assert_eq!(*last_flags, 0x04, "last page is EOS");
        let pre_skip = u16::from_le_bytes(pages[0].2[0][10..12].try_into().unwrap()) as u64;
        assert_eq!(
            *last_granule,
            pre_skip + n as u64 * 6,
            "granule marks the true end"
        );
        // Decode and compare energy with the input.
        let mut dec = opus::Decoder::new(8000, opus::Channels::Mono).unwrap();
        let mut pcm = Vec::new();
        let mut buf = vec![0f32; 960];
        let mut last_g = 0;
        for (g, flags, packets) in &pages[2..] {
            for pk in packets {
                let k = dec.decode_float(pk, &mut buf, false).unwrap();
                pcm.extend_from_slice(&buf[..k]);
            }
            assert!(*g >= last_g, "granules never go back");
            last_g = *g;
            if *flags != 0x04 {
                assert_eq!(*g, pcm.len() as u64 * 6, "granule counts decoded samples");
            }
        }
        assert!(
            pcm.len() as u64 * 6 >= last_g,
            "the stream reaches its end granule"
        );
        let skip = (pre_skip / 6) as usize;
        let out = &pcm[skip..skip + n];
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let (a, b) = (rms(&tone[800..n - 800]), rms(&out[800..n - 800]));
        assert!((a - b).abs() / a < 0.2, "rms in {a} out {b}");
    }

    #[test]
    fn non_opus_rates_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OggOpusWriter::create(&dir.path().join("x.opus"), 11025).is_err());
    }
}
