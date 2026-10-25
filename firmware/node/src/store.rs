//! The config record (see `benchbob_proto::config`) in a reserved flash region, through
//! the blocking `embedded-storage` NOR flash traits both HALs implement. Besides the
//! Wi-Fi settings it carries the LED strings' settings: [`ConfigStore::restore_leds`]
//! re-creates them at boot, [`persist_leds`] writes them back when they change.

use crate::http::Shared;
use benchbob_proto::api::leds::MAX_STRINGS;
use benchbob_proto::api::{BoardInfo, Leds, Node};
use benchbob_proto::config::{self, Config, Loaded, SLOT_SIZE};
use benchbob_proto::json::{Buf, Json};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use embedded_storage::nor_flash::NorFlash;

/// Writes and read-back checks go in pieces this size (a multiple of every write size).
const PIECE: usize = 256;
/// LED settings are saved once they've stopped changing for this long, so an API client
/// changing patterns in a loop can't wear the flash out.
const SETTLE: Duration = Duration::from_secs(5);
/// One string's settings JSON, at most.
const STRING_JSON: usize = 640;

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
        // Newest sequence number first, decoding one slot at a time (a Config with its LED
        // strings is almost a slot's size: keep one on the stack, not three).
        let mut head = [[0u8; config::HEADER_LEN]; 2];
        for (k, h) in head.iter_mut().enumerate() {
            if flash.read(offset + (k * SLOT_SIZE) as u32, h).is_err() {
                h.fill(0);
            }
        }
        let seq = |h: &[u8; config::HEADER_LEN]| u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
        let order = if seq(&head[1]) > seq(&head[0]) && seq(&head[1]) != u32::MAX { [1, 0] } else { [0, 1] };
        let mut buf = [0u8; SLOT_SIZE];
        let mut current = None;
        for k in order {
            if flash.read(offset + (k * SLOT_SIZE) as u32, &mut buf).is_ok()
                && let Some((seq, config)) = Config::decode(&buf)
            {
                current = Some(Loaded { slot: k, seq, config });
                break;
            }
        }
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
        let mut rec = [0xFFu8; SLOT_SIZE];
        let n = c.encode(seq, &mut rec).div_ceil(PIECE) * PIECE;
        let at = self.offset + (slot * SLOT_SIZE) as u32;
        self.flash.erase(at, at + SLOT_SIZE as u32).map_err(|_| FlashError)?;
        self.flash.write(at, &rec[..n]).map_err(|_| FlashError)?;
        // Read it back before trusting it.
        let mut check = [0u8; PIECE];
        for k in (0..n).step_by(PIECE) {
            self.flash.read(at + k as u32, &mut check).map_err(|_| FlashError)?;
            if check != rec[k..k + PIECE] {
                warn!("config: verify failed after write");
                return Err(FlashError);
            }
        }
        info!("config: saved to slot {} seq {}", slot as u32, seq);
        self.current = Some(Loaded { slot, seq, config: *c });
        Ok(())
    }

    /// Re-create the stored LED strings (at boot, before anything else touches them).
    pub fn restore_leds(&self, leds: &mut Leds, info: &BoardInfo) {
        let Some(c) = self.config() else { return };
        let mut buf = [0u8; STRING_JSON * 2];
        c.led_strings(&mut buf, |json| match leds.restore_json(info, json, 0) {
            Ok(id) => info!("config: LED string {} restored", id),
            Err(e) => warn!("config: an LED string wasn't restored: {}", e.message()),
        });
    }
}

/// Save the LED strings' settings into the config record whenever they change (once
/// they've settled for [`SETTLE`]). Run as a task beside the HTTP servers.
pub async fn persist_leds<M: RawMutex, N: Node, SM: RawMutex, F: NorFlash>(
    shared: &Mutex<M, Shared<N>>,
    store: &Mutex<SM, ConfigStore<F>>,
) -> ! {
    let mut saved = shared.lock().await.api.leds.persist_gen();
    let mut pending: Option<(u32, Instant)> = None;
    let mut bufs = [[0u8; STRING_JSON]; MAX_STRINGS];
    loop {
        Timer::after(Duration::from_secs(1)).await;
        let now_gen = shared.lock().await.api.leds.persist_gen();
        if now_gen == saved {
            pending = None;
            continue;
        }
        match pending {
            Some((g, since)) if g == now_gen && since.elapsed() >= SETTLE => {}
            Some((g, _)) if g == now_gen => continue,
            _ => {
                pending = Some((now_gen, Instant::now()));
                continue;
            }
        }
        // Settled: write them out.
        let mut lens = [0usize; MAX_STRINGS];
        let mut count = 0;
        {
            let g = shared.lock().await;
            let leds = &g.api.leds;
            let ids: [Option<u16>; MAX_STRINGS] = {
                let mut a = [None; MAX_STRINGS];
                for (k, id) in leds.ids().enumerate().take(MAX_STRINGS) {
                    a[k] = Some(id);
                }
                a
            };
            for id in ids.into_iter().flatten() {
                let mut b = Buf::new(&mut bufs[count]);
                if leds.persist_json(id as i64, &mut Json::new(&mut b)).is_ok() && !b.overflowed() {
                    lens[count] = b.len();
                    count += 1;
                }
            }
        }
        let mut st = store.lock().await;
        let mut c = st.config().copied().unwrap_or_default();
        if !c.set_led_strings(bufs[..count].iter().zip(lens).map(|(b, n)| &b[..n])) {
            warn!("config: not every LED string fits in the config record");
        }
        match st.save(&c) {
            Ok(()) => saved = now_gen,
            Err(_) => warn!("config: LED strings not saved (flash error); retrying"),
        }
        pending = None;
    }
}
