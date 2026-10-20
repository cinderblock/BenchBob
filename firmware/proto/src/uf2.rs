//! UF2 blocks (https://github.com/microsoft/uf2), as the RP2350 bootrom accepts them.
//!
//! Each 512-byte block carries 256 bytes of payload for one flash address. The config
//! UF2 uses the RP2350 `absolute` family, which the bootrom writes at the given address
//! regardless of partitions; the firmware UF2 uses `rp2350-arm-s`.

pub const BLOCK_SIZE: usize = 512;
pub const PAYLOAD: usize = 256;

pub const MAGIC_START0: u32 = 0x0A32_4655;
pub const MAGIC_START1: u32 = 0x9E5D_5157;
pub const MAGIC_END: u32 = 0x0AB1_6F30;
pub const FLAG_FAMILY_ID: u32 = 0x0000_2000;

pub const FAMILY_RP2040: u32 = 0xE48B_FF56;
pub const FAMILY_ABSOLUTE: u32 = 0xE48B_FF57;
pub const FAMILY_DATA: u32 = 0xE48B_FF58;
pub const FAMILY_RP2350_ARM_S: u32 = 0xE48B_FF59;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    pub flags: u32,
    pub addr: u32,
    pub block_no: u32,
    pub num_blocks: u32,
    pub family: u32,
    pub data: &'a [u8],
}

fn put(out: &mut [u8], at: usize, v: u32) {
    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn get(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Encode one block. `data` is at most 256 bytes.
pub fn encode_block(addr: u32, data: &[u8], block_no: u32, num_blocks: u32, family: u32, out: &mut [u8; BLOCK_SIZE]) {
    assert!(data.len() <= PAYLOAD);
    out.fill(0);
    put(out, 0, MAGIC_START0);
    put(out, 4, MAGIC_START1);
    put(out, 8, FLAG_FAMILY_ID);
    put(out, 12, addr);
    put(out, 16, data.len() as u32);
    put(out, 20, block_no);
    put(out, 24, num_blocks);
    put(out, 28, family);
    out[32..32 + data.len()].copy_from_slice(data);
    put(out, 508, MAGIC_END);
}

/// Parse one block; None if the magics or sizes are wrong.
pub fn parse_block(b: &[u8]) -> Option<Block<'_>> {
    if b.len() != BLOCK_SIZE || get(b, 0) != MAGIC_START0 || get(b, 4) != MAGIC_START1 || get(b, 508) != MAGIC_END {
        return None;
    }
    let size = get(b, 16) as usize;
    if size > 476 {
        return None;
    }
    Some(Block { flags: get(b, 8), addr: get(b, 12), block_no: get(b, 20), num_blocks: get(b, 24), family: get(b, 28), data: &b[32..32 + size] })
}

/// Encode a contiguous image starting at `base` (256-byte aligned) into `out`, which must
/// hold `blocks_for(image.len())` blocks. Returns the number of bytes written.
pub fn encode_image(base: u32, image: &[u8], family: u32, out: &mut [u8]) -> usize {
    let n = blocks_for(image.len());
    for (i, chunk) in image.chunks(PAYLOAD).enumerate() {
        let blk: &mut [u8; BLOCK_SIZE] = (&mut out[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE]).try_into().unwrap();
        encode_block(base + (i * PAYLOAD) as u32, chunk, i as u32, n as u32, family, blk);
    }
    n * BLOCK_SIZE
}

pub const fn blocks_for(len: usize) -> usize {
    len.div_ceil(PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Config, REGION_SIZE, SLOT_SIZE};
    extern crate std;
    use std::vec;

    #[test]
    fn block_round_trip() {
        let mut out = [0u8; BLOCK_SIZE];
        encode_block(0x1000_0100, &[1, 2, 3], 4, 9, FAMILY_RP2350_ARM_S, &mut out);
        let b = parse_block(&out).unwrap();
        assert_eq!((b.addr, b.block_no, b.num_blocks, b.family, b.data), (0x1000_0100, 4, 9, FAMILY_RP2350_ARM_S, &[1u8, 2, 3][..]));
        assert_eq!(b.flags, FLAG_FAMILY_ID);
        out[508] ^= 1;
        assert!(parse_block(&out).is_none());
    }

    /// The config UF2 as the tools produce it, read back the way the firmware sees flash.
    #[test]
    fn config_uf2_round_trip() {
        let base = 0x103F_E000;
        let c = Config::new("Bench Lab", "hunter22", "pico-a").unwrap();
        let mut img = [0u8; REGION_SIZE];
        config::region_image(&c, &mut img);
        let mut uf2 = vec![0u8; blocks_for(REGION_SIZE) * BLOCK_SIZE];
        assert_eq!(encode_image(base, &img, FAMILY_ABSOLUTE, &mut uf2), 32 * BLOCK_SIZE);

        // "Flash" it.
        let mut flash = vec![0u8; REGION_SIZE];
        for blk in uf2.chunks(BLOCK_SIZE) {
            let b = parse_block(blk).unwrap();
            assert_eq!(b.family, FAMILY_ABSOLUTE);
            assert_eq!(b.num_blocks, 32);
            let off = (b.addr - base) as usize;
            flash[off..off + b.data.len()].copy_from_slice(b.data);
        }
        let l = config::load(&flash[..SLOT_SIZE], &flash[SLOT_SIZE..]).unwrap();
        assert_eq!(l.config, c);
    }
}
