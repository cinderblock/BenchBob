//! Recent rx with timestamps (serial.ts `Ring`). Positions are absolute byte offsets since
//! the port was first opened, so a cursor stays meaningful as old data falls off the front.

use std::collections::VecDeque;

pub const RING_CAP: usize = 256 * 1024;

pub struct Chunk {
    pub t: u64,
    pub pos: u64,
    pub data: Vec<u8>,
}

pub struct Ring {
    pub chunks: VecDeque<Chunk>,
    pub size: usize,
    pub end: u64,
    pub cap: usize,
}

pub struct Since {
    pub from: u64,
    pub to: u64,
    pub data: Vec<u8>,
    pub truncated: bool,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new(RING_CAP)
    }
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self { chunks: VecDeque::new(), size: 0, end: 0, cap }
    }

    pub fn start(&self) -> u64 {
        self.end - self.size as u64
    }

    pub fn push(&mut self, data: &[u8], t: u64) {
        if data.is_empty() {
            return;
        }
        let end = self.end;
        match self.chunks.back_mut() {
            // Coalesce a trickle (a byte per keystroke) so the chunk count stays bounded.
            Some(last) if last.data.len() + data.len() <= 4096 && (t as i64 - last.t as i64) < 50 => last.data.extend_from_slice(data),
            _ => self.chunks.push_back(Chunk { t, pos: end, data: data.to_vec() }),
        }
        self.size += data.len();
        self.end += data.len() as u64;
        while self.size > self.cap && self.chunks.len() > 1 {
            let c = self.chunks.pop_front().unwrap();
            self.size -= c.data.len();
        }
    }

    /// Bytes from `cursor` (clamped to what's still held) up to `max` bytes.
    pub fn since(&self, cursor: f64, max: f64) -> Since {
        let start = self.start();
        let from = if cursor.is_nan() || cursor < start as f64 { start } else { cursor as u64 };
        let to = if max.is_finite() { self.end.min(from.saturating_add(max.max(0.0) as u64)) } else { self.end };
        let mut out = vec![0u8; to.saturating_sub(from) as usize];
        if out.is_empty() {
            return Since { from, to, data: out, truncated: cursor < start as f64 };
        }
        for c in &self.chunks {
            let c_end = c.pos + c.data.len() as u64;
            if c_end <= from || c.pos >= to {
                continue;
            }
            let a = from.max(c.pos);
            let b = to.min(c_end);
            out[(a - from) as usize..(b - from) as usize].copy_from_slice(&c.data[(a - c.pos) as usize..(b - c.pos) as usize]);
        }
        Since { from, to, data: out, truncated: cursor < start as f64 }
    }

    /// Timestamped chunks covering the last `bytes` bytes, for history replay.
    pub fn tail(&self, bytes: usize) -> Vec<(u64, &[u8])> {
        let from = self.start().max(self.end.saturating_sub(bytes as u64));
        let mut out = Vec::new();
        for c in &self.chunks {
            let c_end = c.pos + c.data.len() as u64;
            if c_end <= from {
                continue;
            }
            out.push((c.t, if c.pos >= from { &c.data[..] } else { &c.data[(from - c.pos) as usize..] }));
        }
        out
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_absolute_cursors_and_drops_the_oldest() {
        let mut r = Ring::new(100);
        for i in 0..30u8 {
            r.push(&[i; 10], i as u64 * 100);
        }
        assert_eq!(r.end, 300);
        assert!(r.size <= 110);
        let s = r.since(0.0, f64::INFINITY);
        assert!(s.truncated);
        assert_eq!(s.from, r.start());
        assert_eq!(*s.data.last().unwrap(), 29);
        let t = r.since(295.0, f64::INFINITY);
        assert_eq!(t.data, vec![29; 5]);
        assert_eq!(r.since(300.0, f64::INFINITY).data.len(), 0);
        assert_eq!(r.since(280.0, 4.0).to, 284);
    }

    #[test]
    fn ring_coalesces_a_trickle_but_not_slow_data() {
        let mut r = Ring::default();
        for i in 0..100u64 {
            r.push(&[65 + (i % 26) as u8], 1000 + i);
        }
        assert_eq!(r.chunks.len(), 2); // 100 bytes over 100 ms: a chunk spans at most 50 ms so timestamps stay honest
        r.push(&[1], 5000);
        assert_eq!(r.chunks.len(), 3);
        let tail = r.tail(3);
        assert_eq!(tail.iter().map(|c| c.1.len()).sum::<usize>(), 3);
        assert_eq!(tail.last().unwrap().0, 5000);
    }

    #[test]
    fn a_cursor_past_the_end_reads_nothing() {
        let mut r = Ring::default();
        r.push(b"hello", 1);
        let s = r.since(10.0, f64::INFINITY);
        assert!(s.data.is_empty() && !s.truncated);
    }
}
