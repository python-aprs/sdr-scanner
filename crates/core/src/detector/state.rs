//! Per-slot transmission state and the statistics a close reports.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Idle,
    /// Above the open threshold since this block.
    Attack {
        since: u64,
    },
    /// Open; transmission started at this block (pre-roll included).
    Active {
        start: u64,
    },
    /// Below the close threshold since `since`.
    Hang {
        start: u64,
        since: u64,
    },
}

impl SlotState {
    /// Not open: idle or in its attack.
    pub fn is_waiting(self) -> bool {
        matches!(self, SlotState::Idle | SlotState::Attack { .. })
    }

    /// Open: active or in its hang time.
    pub fn is_open(self) -> bool {
        matches!(self, SlotState::Active { .. } | SlotState::Hang { .. })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A slot opened. `start_block` includes pre-roll; the consumer should
    /// demodulate blocks `start_block..=block` now.
    Open {
        slot: usize,
        start_block: u64,
        block: u64,
    },
    /// A slot closed. `end_block` is the last block the signal was present.
    Close {
        slot: usize,
        start_block: u64,
        end_block: u64,
        peak_snr_db: f32,
        mean_snr_db: f32,
    },
}

/// A slot's SNR this block, and its peak and mean over the open
/// transmission.
#[derive(Debug, Clone, Default)]
pub(super) struct SlotStats {
    pub snr_db: f32,
    pub peak: f32,
    pub sum: f64,
    pub count: u64,
}

impl SlotStats {
    /// Start a transmission at this block's SNR.
    pub fn open(&mut self) {
        *self = SlotStats {
            snr_db: self.snr_db,
            peak: self.snr_db,
            sum: 0.0,
            count: 0,
        };
    }

    /// Count this block towards the transmission's peak and mean.
    pub fn add(&mut self) {
        self.peak = self.peak.max(self.snr_db);
        self.sum += self.snr_db as f64;
        self.count += 1;
    }

    /// The close event for a transmission on `slot`.
    pub fn close(&self, slot: usize, start_block: u64, end_block: u64) -> Event {
        Event::Close {
            slot,
            start_block,
            end_block,
            peak_snr_db: self.peak,
            mean_snr_db: if self.count > 0 {
                (self.sum / self.count as f64) as f32
            } else {
                self.snr_db
            },
        }
    }
}
