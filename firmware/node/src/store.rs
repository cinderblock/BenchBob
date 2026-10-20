//! The config record (see `benchbob_proto::config`) in a reserved flash region, through
//! the blocking `embedded-storage` NOR flash traits both HALs implement.

use benchbob_proto::config::{self, Config, Loaded, SLOT_SIZE};
use embedded_storage::nor_flash::NorFlash;

/// Bytes read and written per slot: a record is at most 16 + 3×2 + 32 + 64 + 63 = 181.
const RECORD_SPAN: usize = 256;

pub struct ConfigStore<F> {
    flash: F,
    /// Offset of the region from the start of flash (not a bus address).
    offset: u32,
    current: Option<Loaded>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashError;

impl<F: NorFlash> ConfigStore<F> {
    /// Open the region at `offset` (two 4 KiB slots) and load the newest valid record.
    pub fn new(mut flash: F, offset: u32) -> Self {
        let mut a = [0u8; RECORD_SPAN];
        let mut b = [0u8; RECORD_SPAN];
        let ok = flash.read(offset, &mut a).is_ok() && flash.read(offset + SLOT_SIZE as u32, &mut b).is_ok();
        let current = if ok { config::load(&a, &b) } else { None };
        match &current {
            Some(l) => info!("config: slot {} seq {}", l.slot as u32, l.seq),
            None => info!("config: none stored"),
        }
        ConfigStore { flash, offset, current }
    }

    pub fn config(&self) -> Option<&Config> {
        self.current.as_ref().map(|l| &l.config)
    }

    /// Write `c` to the slot not holding the current record. The old record stays valid
    /// until the new one is completely written.
    pub fn save(&mut self, c: &Config) -> Result<(), FlashError> {
        let (slot, seq) = config::next_write(self.current.as_ref());
        let mut rec = [0xFFu8; RECORD_SPAN];
        c.encode(seq, &mut rec);
        let at = self.offset + (slot * SLOT_SIZE) as u32;
        self.flash.erase(at, at + SLOT_SIZE as u32).map_err(|_| FlashError)?;
        self.flash.write(at, &rec).map_err(|_| FlashError)?;
        // Read it back before trusting it.
        let mut check = [0u8; RECORD_SPAN];
        self.flash.read(at, &mut check).map_err(|_| FlashError)?;
        match Config::decode(&check) {
            Some((s, got)) if s == seq && got == *c => {
                info!("config: saved to slot {} seq {}", slot as u32, seq);
                self.current = Some(Loaded { slot, seq, config: *c });
                Ok(())
            }
            _ => {
                warn!("config: verify failed after write");
                Err(FlashError)
            }
        }
    }
}
