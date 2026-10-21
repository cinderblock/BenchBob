//! Auto-baud scoring (serial.ts): how much a sample looks like text from a correctly timed
//! UART, and picking the best rate of a sweep.

use benchbob_core::http::num;
use serde_json::{Value, json};

use crate::util::to_fixed;

pub const AUTOBAUD_RATES: &[u32] =
    &[1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 74880, 115200, 230400, 250000, 460800, 921600, 1000000, 1500000, 2000000];

/// 0..1. At the wrong rate the receiver sees framing errors (read as NUL), high-bit bytes
/// and control junk.
pub fn score_sample(b: &[u8]) -> f64 {
    if b.len() < 4 {
        return 0.0;
    }
    let (mut printable, mut wordy, mut newlines, mut nul) = (0usize, 0usize, 0usize, 0usize);
    for &x in b {
        if (0x20..0x7f).contains(&x) || x == 9 || x == 10 || x == 13 {
            printable += 1;
        }
        if x.is_ascii_alphanumeric() || x == 0x20 {
            wordy += 1;
        }
        if x == 10 {
            newlines += 1;
        }
        if x == 0 {
            nul += 1;
        }
    }
    let n = b.len() as f64;
    let p = printable as f64 / n;
    let w = wordy as f64 / n;
    // Line structure: real logs break into lines of sensible length.
    let avg_line = if newlines > 0 { n / newlines as f64 } else { f64::INFINITY };
    let lines = if n < 40.0 {
        0.75
    } else if newlines > 0 && (2.0..=200.0).contains(&avg_line) {
        1.0
    } else {
        0.5
    };
    let s = p * p * (0.55 + 0.45 * (w / 0.6).min(1.0)) * (0.85 + 0.15 * lines) * (1.0 - (nul as f64 / n).min(0.5));
    s.clamp(0.0, 1.0)
}

fn printable_sample(b: &[u8], n: usize) -> String {
    b.iter()
        .take(n)
        .map(|&x| {
            if (0x20..0x7f).contains(&x) {
                x as char
            } else if x == 10 {
                '⏎'
            } else {
                '·'
            }
        })
        .collect()
}

pub struct Best {
    pub baud: u32,
    pub score: f64,
    pub bytes: usize,
    pub confidence: f64,
}

pub struct Rate {
    pub baud: u32,
    pub bytes: usize,
    pub score: f64,
    pub sample: String,
}

pub struct AutobaudResult {
    pub best: Option<Best>,
    pub results: Vec<Rate>,
}

impl AutobaudResult {
    /// `{ best, results }` in the TypeScript's key order.
    pub fn to_json(&self) -> Value {
        json!({
            "best": self.best.as_ref().map(|b| json!({ "baud": b.baud, "score": num(b.score), "bytes": b.bytes, "confidence": num(b.confidence) })),
            "results": self.results.iter().map(|r| json!({ "baud": r.baud, "bytes": r.bytes, "score": num(r.score), "sample": r.sample })).collect::<Vec<_>>(),
        })
    }
}

/// Pick the best rate. Confidence needs a decent score, enough bytes, and a clear winner.
pub fn pick_baud(samples: &[(u32, Vec<u8>)]) -> AutobaudResult {
    let results: Vec<Rate> = samples
        .iter()
        .map(|(baud, data)| Rate {
            baud: *baud,
            bytes: data.len(),
            score: to_fixed(score_sample(data), 3),
            sample: printable_sample(data, 48),
        })
        .collect();
    let mut ranked: Vec<&Rate> = results.iter().collect();
    // Stable, like Array.prototype.sort.
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap().then(b.bytes.cmp(&a.bytes)));
    let best = match ranked.first() {
        Some(top) if top.score != 0.0 => {
            let second = ranked.get(1).map(|r| r.score).unwrap_or(0.0);
            let size = (top.bytes as f64 / 48.0).min(1.0);
            let margin = ((top.score - second) / top.score * 2.0).min(1.0);
            Some(Best { baud: top.baud, score: top.score, bytes: top.bytes, confidence: to_fixed(top.score * size * margin, 2) })
        }
        _ => None,
    };
    AutobaudResult { best, results }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transmit 8N1 frames at tx_baud and receive them the way a UART does at rx_baud: wait
    /// for a falling edge, sample each bit mid-cell, and turn a bad stop bit into a NUL (what
    /// Linux delivers for a framing error without PARMRK).
    fn uart(bytes: &[u8], tx_baud: f64, rx_baud: f64) -> Vec<u8> {
        let os = 16usize;
        let mut level: Vec<u8> = Vec::new();
        let mut push = |v: u8, n: usize| level.extend(std::iter::repeat_n(v, n * os));
        push(1, 4);
        for &b in bytes {
            push(0, 1);
            for i in 0..8 {
                push((b >> i) & 1, 1);
            }
            push(1, 1);
        }
        push(1, 20);
        let at = |t: f64| -> u8 { level.get(((t * tx_baud * os as f64).floor() as usize).min(level.len() - 1)).copied().unwrap_or(1) };
        let mut out = Vec::new();
        let dt = 1.0 / (tx_baud * os as f64);
        let rx_bit = 1.0 / rx_baud;
        let mut t = 0.0;
        let end = level.len() as f64 * dt;
        while t < end {
            if at(t) == 1 {
                t += dt;
                continue;
            }
            let mut v = 0u8;
            for i in 0..8 {
                v |= at(t + rx_bit * (1.5 + i as f64)) << i;
            }
            let stop = at(t + rx_bit * 9.5);
            out.push(if stop == 1 { v } else { 0 });
            t += rx_bit * 9.5 + dt; // resync after the middle of the stop bit
        }
        out
    }

    const BOOT: &[u8] = b"ets Jun  8 2016 00:22:57\r\n\r\nrst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)\r\nconfigsip: 0, SPIWP:0xee\r\nclk_drv:0x00,q_drv:0x00,d_drv:0x00\r\nmode:DIO, clock div:2\r\nload:0x3fff0030,len:1184\r\nI (29) boot: ESP-IDF v5.1 2nd stage bootloader\r\nI (29) boot: compile time 12:00:00\r\nHello from the device, uptime 12 s\r\n";

    #[test]
    fn uart_model_is_faithful_at_the_right_rate() {
        assert_eq!(uart(BOOT, 115200.0, 115200.0), BOOT);
    }

    #[test]
    fn auto_baud_picks_the_rate_from_a_sweep_of_resampled_traffic() {
        for tx in [9600u32, 74880, 115200, 921600] {
            let samples: Vec<(u32, Vec<u8>)> = AUTOBAUD_RATES.iter().map(|&rx| (rx, uart(BOOT, tx as f64, rx as f64))).collect();
            let r = pick_baud(&samples);
            let b = r.best.unwrap();
            assert_eq!(b.baud, tx);
            assert!(b.confidence > 0.5, "{tx}: {}", b.confidence);
        }
    }

    #[test]
    fn auto_baud_with_silence_has_no_answer() {
        let r = pick_baud(&AUTOBAUD_RATES.iter().map(|&b| (b, Vec::new())).collect::<Vec<_>>());
        assert!(r.best.is_none());
        assert_eq!(r.to_json()["best"], Value::Null);
    }

    #[test]
    fn binary_noise_scores_low() {
        let noise: Vec<u8> = (0..400u32).map(|i| ((i * 167 + 13) & 0xff) as u8).collect();
        assert!(score_sample(&noise) < 0.3);
        assert!(score_sample(BOOT) > 0.8);
    }
}
