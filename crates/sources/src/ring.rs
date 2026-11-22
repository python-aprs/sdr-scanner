//! Lock-free single-producer/single-consumer ring of sample blocks, the
//! hand-off between a radio driver's callback thread and the DSP thread
//! (PLAN.md 3.4).
//!
//! The producer side ([`RingProducer::push`]) never blocks and never
//! allocates: if the ring is full the block is dropped and counted, and the
//! stream position still advances, so timestamps stay right. The consumer
//! side ([`RingSource`]) is a [`Source`]: it zero-fills dropped spans so the
//! sample counter (and therefore every timestamp downstream) stays aligned
//! with the radio.

use crate::{Source, SourceError, SourceInfo, SourceStats};
use num_complex::Complex32;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;

struct Slot {
    /// Stream index of `data[0]`.
    first: u64,
    len: usize,
    data: Box<[Complex32]>,
}

struct Shared {
    slots: Box<[UnsafeCell<Slot>]>,
    /// Blocks written (producer-owned).
    head: AtomicU64,
    /// Blocks read (consumer-owned).
    tail: AtomicU64,
    dropped_samples: AtomicU64,
    dropped_blocks: AtomicU64,
    closed: AtomicBool,
    /// Stream length at close (so a dropped tail is zero-filled too).
    end: AtomicU64,
    waiter: Mutex<Option<Thread>>,
}

// SAFETY: a slot is written only by the producer while it is outside
// [tail, head) and read only by the consumer while it is inside; head/tail
// use release/acquire ordering to hand ownership across.
unsafe impl Sync for Shared {}
unsafe impl Send for Shared {}

/// Counters both sides can read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingStats {
    pub blocks_written: u64,
    pub blocks_read: u64,
    pub dropped_blocks: u64,
    pub dropped_samples: u64,
    pub capacity: usize,
}

/// Create a ring of `blocks` slots of `block_len` samples each.
pub fn ring(blocks: usize, block_len: usize, info: SourceInfo) -> (RingProducer, RingSource) {
    assert!(blocks >= 2 && block_len > 0);
    let slots = (0..blocks)
        .map(|_| {
            UnsafeCell::new(Slot {
                first: 0,
                len: 0,
                data: vec![Complex32::default(); block_len].into_boxed_slice(),
            })
        })
        .collect();
    let shared = Arc::new(Shared {
        slots,
        head: AtomicU64::new(0),
        tail: AtomicU64::new(0),
        dropped_samples: AtomicU64::new(0),
        dropped_blocks: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        end: AtomicU64::new(0),
        waiter: Mutex::new(None),
    });
    (
        RingProducer {
            shared: shared.clone(),
            block_len,
            position: 0,
        },
        RingSource {
            shared,
            info,
            stats: SourceStats::default(),
            timeout: Duration::from_secs(5),
        },
    )
}

fn stats_of(s: &Shared) -> RingStats {
    RingStats {
        blocks_written: s.head.load(Ordering::Acquire),
        blocks_read: s.tail.load(Ordering::Acquire),
        dropped_blocks: s.dropped_blocks.load(Ordering::Relaxed),
        dropped_samples: s.dropped_samples.load(Ordering::Relaxed),
        capacity: s.slots.len(),
    }
}

/// The driver-callback side.
pub struct RingProducer {
    shared: Arc<Shared>,
    block_len: usize,
    /// Stream index of the next sample the radio delivers.
    position: u64,
}

impl RingProducer {
    /// Copy samples into the ring, splitting into blocks. Never blocks;
    /// returns the number of samples dropped because the ring was full.
    pub fn push(&mut self, samples: &[Complex32]) -> usize {
        self.push_with(samples.len(), |dst, offset| {
            dst.copy_from_slice(&samples[offset..offset + dst.len()])
        })
    }

    /// Like [`push`](Self::push), but `fill(dst, offset)` writes samples
    /// `offset..offset + dst.len()` directly into the slot, so a driver can
    /// convert its native format (e.g. interleaved i16) without a temporary.
    pub fn push_with(
        &mut self,
        count: usize,
        mut fill: impl FnMut(&mut [Complex32], usize),
    ) -> usize {
        let s = &*self.shared;
        let cap = s.slots.len() as u64;
        let mut dropped = 0;
        let mut offset = 0;
        while offset < count {
            let n = (count - offset).min(self.block_len);
            let head = s.head.load(Ordering::Relaxed);
            let tail = s.tail.load(Ordering::Acquire);
            if head - tail >= cap {
                dropped += n;
                s.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                s.dropped_samples.fetch_add(n as u64, Ordering::Relaxed);
            } else {
                // SAFETY: slot `head % cap` is outside [tail, head), so the
                // consumer is not reading it.
                let slot = unsafe { &mut *s.slots[(head % cap) as usize].get() };
                slot.first = self.position;
                slot.len = n;
                fill(&mut slot.data[..n], offset);
                s.head.store(head + 1, Ordering::Release);
            }
            self.position += n as u64;
            offset += n;
        }
        if let Some(t) = s.waiter.lock().unwrap().as_ref() {
            t.unpark();
        }
        dropped
    }

    /// Record samples the driver itself reports as lost (e.g. a USB
    /// overflow): the stream position advances so the gap is zero-filled.
    pub fn skip(&mut self, samples: u64) {
        self.position += samples;
        self.shared
            .dropped_samples
            .fetch_add(samples, Ordering::Relaxed);
    }

    pub fn stats(&self) -> RingStats {
        stats_of(&self.shared)
    }

    /// End of stream (radio stopped): the consumer drains, then reads 0.
    pub fn close(&self) {
        self.shared.end.store(self.position, Ordering::Relaxed);
        self.shared.closed.store(true, Ordering::Release);
        if let Some(t) = self.shared.waiter.lock().unwrap().as_ref() {
            t.unpark();
        }
    }
}

impl Drop for RingProducer {
    fn drop(&mut self) {
        self.close();
    }
}

/// The DSP side: a [`Source`] over the ring.
pub struct RingSource {
    shared: Arc<Shared>,
    info: SourceInfo,
    stats: SourceStats,
    timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
#[error("no samples from the radio for {0:?}")]
pub struct Stalled(pub Duration);

impl RingSource {
    pub fn stats_ring(&self) -> RingStats {
        stats_of(&self.shared)
    }

    /// How long `read` waits for data before failing (the radio stalled).
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    fn position(&self) -> u64 {
        self.stats.samples_read
    }
}

impl Source for RingSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, out: &mut [Complex32]) -> Result<usize, SourceError> {
        let s = &*self.shared;
        {
            let mut w = s.waiter.lock().unwrap();
            if w.is_none() {
                *w = Some(std::thread::current());
            }
        }
        let cap = s.slots.len() as u64;
        let started = std::time::Instant::now();
        loop {
            let tail = s.tail.load(Ordering::Relaxed);
            let head = s.head.load(Ordering::Acquire);
            if tail == head {
                if s.closed.load(Ordering::Acquire) && s.head.load(Ordering::Acquire) == tail {
                    let end = s.end.load(Ordering::Relaxed);
                    let pos = self.position();
                    if end > pos {
                        let n = ((end - pos) as usize).min(out.len());
                        out[..n].fill(Complex32::default());
                        self.stats.samples_read += n as u64;
                        self.stats.samples_dropped += n as u64;
                        return Ok(n);
                    }
                    return Ok(0);
                }
                if started.elapsed() > self.timeout {
                    return Err(SourceError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        Stalled(self.timeout),
                    )));
                }
                std::thread::park_timeout(Duration::from_millis(20));
                continue;
            }
            // SAFETY: slot `tail % cap` is inside [tail, head): the producer
            // will not touch it until we advance tail.
            let slot = unsafe { &*s.slots[(tail % cap) as usize].get() };
            let pos = self.position();
            // Zero-fill a gap left by dropped blocks before this one.
            if slot.first > pos {
                let n = ((slot.first - pos) as usize).min(out.len());
                out[..n].fill(Complex32::default());
                self.stats.samples_read += n as u64;
                self.stats.samples_dropped += n as u64;
                return Ok(n);
            }
            let off = (pos - slot.first) as usize;
            let n = (slot.len - off).min(out.len());
            out[..n].copy_from_slice(&slot.data[off..off + n]);
            self.stats.samples_read += n as u64;
            if off + n == slot.len {
                s.tail.store(tail + 1, Ordering::Release);
            }
            return Ok(n);
        }
    }

    fn stats(&self) -> SourceStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> SourceInfo {
        SourceInfo {
            sample_rate: 1e6,
            center_freq: 146e6,
            start_time: None,
            total_samples: None,
            description: "test".into(),
        }
    }

    fn ramp(start: u64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| Complex32::new((start + i as u64) as f32, 0.0))
            .collect()
    }

    fn drain(src: &mut RingSource) -> Vec<Complex32> {
        let mut all = Vec::new();
        let mut buf = vec![Complex32::default(); 777];
        loop {
            let k = src.read(&mut buf).unwrap();
            if k == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..k]);
        }
    }

    #[test]
    fn passes_samples_in_order_across_block_splits() {
        let (mut p, mut c) = ring(8, 1000, info());
        p.push(&ramp(0, 2500));
        p.push(&ramp(2500, 1500));
        p.close();
        let got = drain(&mut c);
        assert_eq!(got, ramp(0, 4000));
        assert_eq!(c.stats().samples_dropped, 0);
    }

    #[test]
    fn full_ring_drops_and_zero_fills_keeping_positions() {
        let (mut p, mut c) = ring(2, 100, info());
        assert_eq!(p.push(&ramp(0, 200)), 0);
        // Ring full: next block dropped.
        assert_eq!(p.push(&ramp(200, 100)), 100);
        let mut buf = vec![Complex32::default(); 200];
        assert_eq!(c.read(&mut buf).unwrap(), 100);
        assert_eq!(c.read(&mut buf).unwrap(), 100);
        p.push(&ramp(300, 100));
        p.close();
        let rest = drain(&mut c);
        assert_eq!(rest.len(), 200);
        assert!(rest[..100].iter().all(|v| *v == Complex32::default()));
        assert_eq!(rest[100..], ramp(300, 100)[..]);
        assert_eq!(c.stats().samples_dropped, 100);
        assert_eq!(p.stats().dropped_blocks, 1);
    }

    #[test]
    fn driver_reported_loss_is_zero_filled() {
        let (mut p, mut c) = ring(4, 100, info());
        p.push(&ramp(0, 50));
        p.skip(25);
        p.push(&ramp(75, 50));
        p.close();
        let got = drain(&mut c);
        assert_eq!(got.len(), 125);
        assert_eq!(got[49].re, 49.0);
        assert_eq!(got[50], Complex32::default());
        assert_eq!(got[75].re, 75.0);
    }

    #[test]
    fn threaded_producer_consumer() {
        let (mut p, mut c) = ring(16, 4096, info());
        let total = 2_000_000usize;
        let h = std::thread::spawn(move || {
            let mut pos = 0u64;
            while (pos as usize) < total {
                let n = 3000.min(total - pos as usize);
                p.push(&ramp(pos, n));
                pos += n as u64;
                if pos % 30_000 == 0 {
                    std::thread::yield_now();
                }
            }
        });
        let got = drain(&mut c);
        h.join().unwrap();
        assert_eq!(got.len(), total);
        // Positions must match even if some blocks were dropped and filled.
        for (i, v) in got.iter().enumerate() {
            assert!(
                v.re == 0.0 || v.re == i as f32 || (i as f32 - v.re).abs() < 1.0,
                "{i}"
            );
        }
    }

    #[test]
    fn stall_times_out() {
        let (_p, c) = ring(4, 100, info());
        let mut c = c.with_timeout(Duration::from_millis(50));
        let mut buf = vec![Complex32::default(); 10];
        assert!(c.read(&mut buf).is_err());
    }
}
