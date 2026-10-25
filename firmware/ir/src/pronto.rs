//! Pronto hex ("CCF"), the interchange format remote databases use: words of four hex
//! digits. `0000` (learned, modulated), the carrier as a period in units of 0.241246 µs,
//! the burst-pair counts of the once and repeat sequences, then the pairs as durations in
//! carrier periods.

/// Pronto's time unit, 0.241246 µs, in picoseconds.
const UNIT_PS: u64 = 241_246;

/// Parse Pronto hex into `once` then `repeat` timings (µs) in `out`. Returns the carrier
/// (Hz) and the two lengths.
pub fn parse(s: &str, out: &mut [u32]) -> Option<(u32, usize, usize)> {
    let mut words = s.split_whitespace().map(|w| u16::from_str_radix(w, 16).ok());
    let mut next = || words.next().flatten();
    if next()? != 0 {
        return None; // only learned (raw, modulated) codes
    }
    let freq_word = next()? as u64;
    if freq_word == 0 {
        return None;
    }
    let (once, rep) = (next()? as usize * 2, next()? as usize * 2);
    if once + rep > out.len() || once + rep == 0 {
        return None;
    }
    // One carrier period, in ps.
    let period_ps = freq_word * UNIT_PS;
    let carrier = (1_000_000_000_000 / period_ps) as u32;
    for d in out[..once + rep].iter_mut() {
        let cycles = next()? as u64;
        *d = ((cycles * period_ps + 500_000) / 1_000_000) as u32;
    }
    Some((carrier, once, rep))
}

/// Write `once` (then `repeat`, which may be empty) as Pronto hex into `out`. Each must
/// have an even length: the last space of a captured frame is the gap, so add one before
/// calling. Returns the length written.
pub fn format(carrier: u32, once: &[u32], repeat: &[u32], out: &mut [u8]) -> Option<usize> {
    if carrier == 0 || !once.len().is_multiple_of(2) || !repeat.len().is_multiple_of(2) {
        return None;
    }
    let freq_word = ((1_000_000_000_000u64 / carrier as u64 + UNIT_PS / 2) / UNIT_PS).min(0xFFFF);
    let period_ps = freq_word * UNIT_PS;
    let mut n = 0;
    let mut word = |w: u64, out: &mut [u8]| -> Option<()> {
        let w = w.min(0xFFFF) as u16;
        let digits = b"0123456789ABCDEF";
        if n > 0 {
            *out.get_mut(n)? = b' ';
            n += 1;
        }
        for k in (0..4).rev() {
            *out.get_mut(n)? = digits[(w >> (4 * k) & 0xF) as usize];
            n += 1;
        }
        Some(())
    };
    word(0, out)?;
    word(freq_word, out)?;
    word(once.len() as u64 / 2, out)?;
    word(repeat.len() as u64 / 2, out)?;
    for &d in once.iter().chain(repeat) {
        word((d as u64 * 1_000_000 + period_ps / 2) / period_ps, out)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_known_code() {
        // NEC at 38 kHz (0x006D), one pair once, no repeat: 9 ms mark (0x0156 cycles ≈ 342),
        // 4.5 ms space.
        let mut out = [0u32; 8];
        let (carrier, once, rep) = parse("0000 006D 0001 0000 0156 00AB", &mut out).unwrap();
        assert_eq!((once, rep), (2, 0));
        assert!((37_900..38_100).contains(&carrier), "{carrier}");
        assert!(out[0].abs_diff(9000) < 20 && out[1].abs_diff(4500) < 20, "{:?}", &out[..2]);
        assert!(parse("0100 006D 0001 0000 0156 00AB", &mut out).is_none());
        assert!(parse("0000 006D 0002 0000 0156", &mut out).is_none());
    }

    #[test]
    fn round_trips() {
        let t = [9000u32, 4500, 562, 562, 562, 1687, 562, 40_000];
        let mut s = [0u8; 256];
        let n = format(38_000, &t, &[], &mut s).unwrap();
        let text = core::str::from_utf8(&s[..n]).unwrap();
        assert!(text.starts_with("0000 006D 0004 0000 "), "{text}");
        let mut back = [0u32; 8];
        let (c, once, _) = parse(text, &mut back).unwrap();
        assert!(c.abs_diff(38_000) < 100);
        for (a, b) in t.iter().zip(&back[..once]) {
            assert!(a.abs_diff(*b) <= 15, "{a} vs {b}");
        }
        assert!(format(38_000, &t[..3], &[], &mut s).is_none());
    }
}
