//! peripherals/lib/cansock.test.ts, with the same fixtures, plus a few cases it didn't cover.

use super::*;
use serde_json::json;

fn raw16(id: u32, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 16];
    b[0..4].copy_from_slice(&id.to_le_bytes());
    b[4] = data.len() as u8;
    b[8..8 + data.len()].copy_from_slice(data);
    b
}

fn spec(v: Value) -> FrameSpec {
    spec_from_json(&v).unwrap()
}

fn frame(v: Value) -> CanFrame {
    make_frame(&spec(v)).unwrap()
}

fn split(b: &[u8], ts: f64) -> Vec<CanFrame> {
    let mut v = Vec::new();
    split_frames(b, ts, &mut v);
    v
}

// ---------- frame encode/decode ----------

#[test]
fn classic_std_frame_round_trip() {
    let f = frame(json!({ "id": 0x123, "data": "DEADBEEF" }));
    let b = encode_frame(&f);
    assert_eq!(b.len(), 16);
    assert_eq!(&b[..8], &[0x23, 0x01, 0, 0, 4, 0, 0, 0]);
    let g = &split(&b, 1000.0)[0];
    assert_eq!((g.id, g.ext, g.rtr, g.fd, g.len), (0x123, false, false, false, 4));
    assert_eq!(to_hex(&g.data), "DEADBEEF");
}

#[test]
fn extended_and_rtr_flags() {
    let f = frame(json!({ "id": 0x18daf110, "rtr": true, "len": 3 }));
    let b = encode_frame(&f);
    assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 0x18daf110 | CAN_EFF_FLAG | CAN_RTR_FLAG);
    let g = &split(&b, 0.0)[0];
    assert_eq!((g.id, g.ext, g.rtr, g.len), (0x18daf110, true, true, 3));
    assert!(g.data.is_empty());
}

#[test]
fn fd_frame_pads_length_and_sets_fdf_brs() {
    let f = frame(json!({ "id": 0x1234, "fd": true, "brs": true, "data": "00112233445566778899" })); // 10 → 12
    assert_eq!(f.len, 12);
    let b = encode_frame(&f);
    assert_eq!(b.len(), 72);
    assert_eq!(b[5], CANFD_BRS | CANFD_FDF);
}

#[test]
fn coalesced_chunks_as_seen_on_desky() {
    let mut ab = raw16(0x100, &[1]);
    ab.extend(raw16(0x101, &[2, 3]));
    assert_eq!(split(&ab, 0.0).iter().map(|f| f.id).collect::<Vec<_>>(), vec![0x100, 0x101]);
    // The 88-byte chunk captured on desky: can_frame 0x123 [DEADBEEF], then canfd_frame
    // 0x80001234 len 12 flags 5.
    let mut fd = vec![0u8; 72];
    fd[0..4].copy_from_slice(&0x8000_1234u32.to_le_bytes());
    fd[4] = 12;
    fd[5] = 5;
    let mut b = raw16(0x123, &[0xde, 0xad, 0xbe, 0xef]);
    b.extend(fd);
    let fs = split(&b, 0.0);
    assert_eq!(fs.len(), 2);
    assert_eq!((fs[1].id, fs[1].ext, fs[1].fd, fs[1].brs, fs[1].len), (0x1234, true, true, true, 12));
}

#[test]
fn error_frame() {
    let b = raw16(CAN_ERR_FLAG | 0x004 | 0x200, &[0, 0x10, 0, 0, 0, 0, 0x80, 0x7f]);
    let f = &split(&b, 0.0)[0];
    assert!(f.err);
    assert_eq!(f.id, 0x204);
    let d = decode_error(f);
    assert!(d.text.contains("RX passive"));
    assert_eq!(d.tec, Some(0x80));
    assert_eq!(d.rec, Some(0x7f));
}

#[test]
fn decode_error_protocol_violation_with_location() {
    let mut f = frame(json!({ "id": 0 }));
    f.err = true;
    f.id = 0x088;
    f.data = vec![0, 0, 0x04, 0x0a, 0, 0, 0, 0];
    assert_eq!(decode_error(&f).text, "bus error; protocol: stuff error at data");
}

#[test]
fn decode_error_extras() {
    let mut f = frame(json!({ "id": 0 }));
    f.err = true;
    f.id = 0x002 | 0x010 | 0x040;
    f.data = vec![5, 0, 0, 0, 0x33, 0, 0, 0];
    let d = decode_error(&f);
    assert_eq!(d.text, "lost arbitration; bus off; arbitration lost at bit 5; transceiver: 0x33");
    assert!(d.bus_off);
    assert_eq!((d.tec, d.rec), (None, None));
    f.id = 0;
    f.data = vec![];
    assert_eq!(decode_error(&f).text, "error frame");
}

#[test]
fn validation() {
    assert!(make_frame(&spec(json!({ "id": 0x800 + 0x1fffffff }))).is_err());
    assert!(make_frame(&spec(json!({ "id": 1, "data": "001122334455667788" }))).unwrap_err().contains("at most 8"));
    assert!(make_frame(&spec(json!({ "id": 1, "fd": true, "rtr": true }))).is_err());
    assert!(frame(json!({ "id": 0x800 })).ext);
    assert!(parse_hex("abc").unwrap_err().contains("Odd"));
    assert_eq!(to_hex(&parse_hex("de ad:BE,ef").unwrap()), "DEADBEEF");
    assert_eq!(to_hex(&parse_hex("0xDE,0xAD").unwrap()), "DEAD");
    assert_eq!(parse_hex("zz").unwrap_err(), "Not hex: \"zz\"");
    assert_eq!(make_frame(&spec(json!({ "id": 1.5 }))).unwrap_err(), "id must be a non-negative integer");
    assert_eq!(make_frame(&spec(json!({ "id": 0x20000000 }))).unwrap_err(), "id 0x20000000 is out of range");
    assert_eq!(make_frame(&spec(json!({ "id": 1, "rtr": true, "len": 9 }))).unwrap_err(), "RTR length must be 0–8");
    assert_eq!(to_hex(&frame(json!({ "id": 1, "data": [1, 255, 256] })).data), "01FF00");
}

// ---------- candump / cansend formats ----------

#[test]
fn candump_lines_match_can_utils_output() {
    let mut f = frame(json!({ "id": 0x123, "fd": true, "brs": true, "data": "DEADBEEF" }));
    f.ts = 1792225996360.433;
    // Exactly what `candump -L` printed on desky for `cansend vcan0 123##1DEADBEEF`.
    assert_eq!(candump_line(&f, "vcan0"), "(1792225996.360433) vcan0 123##5DEADBEEF");
    assert_eq!(candump_body(&frame(json!({ "id": 0x12345678, "rtr": true }))), "12345678#R");
    assert_eq!(candump_body(&frame(json!({ "id": 0x123, "rtr": true, "len": 3 }))), "123#R3");
    assert_eq!(candump_body(&frame(json!({ "id": 0x7ff, "data": "" }))), "7FF#");
}

#[test]
fn cansend_parsing() {
    let s = parse_cansend("123#DEADBEEF").unwrap();
    assert_eq!((s.id, s.ext), (291.0, false));
    let s = parse_cansend("00000123#11").unwrap();
    assert_eq!((s.id, s.ext), (291.0, true));
    let s = parse_cansend("123#R4").unwrap();
    assert_eq!((s.rtr, s.len), (true, Some(4.0)));
    let s = parse_cansend("123##1DEAD").unwrap();
    assert_eq!((s.fd, s.brs), (true, true));
    assert!(parse_cansend("garbage").is_err());
    assert!(parse_cansend("  123#11  ").is_ok());
    assert_eq!(parse_cansend("123##").unwrap_err(), "CAN FD frame needs a flags nibble after ##");
}

#[test]
fn candump_line_parse_round_trip() {
    for l in [
        "(1700000000.000001) can0 123#DEADBEEF",
        "(1700000000.500000) can0 18DAF110#R",
        "(1700000000.250000) vcan0 123##5DEADBEEF",
        "(1700000001.000000) can0 20000080#0000000000000000",
    ] {
        let p = parse_candump_line(l).unwrap().unwrap();
        assert_eq!(candump_line(&p.frame, &p.iface), l);
    }
    assert!(parse_candump_line("nope").unwrap().is_none());
}

// ---------- bus load bits ----------

#[test]
fn classic_worst_case_matches_the_textbook_figures() {
    // 8-byte std frame: 111 bits + 24 stuff bits = 135; 8-byte ext: 131 + 29 = 160 (incl. 3-bit IFS).
    assert_eq!(frame_bits(false, false, false, false, 8).0, 135);
    assert_eq!(frame_bits(true, false, false, false, 8).0, 160);
    assert_eq!(frame_bits(false, false, false, false, 0).0, 47 + 8);
}

#[test]
fn fd_with_brs_splits_phases() {
    let (nom, data) = frame_bits(false, false, true, true, 64);
    assert!(nom > 25);
    assert!(data > 64 * 8);
    let f = frame(json!({ "id": 1, "fd": true, "brs": true, "data": "00".repeat(64) }));
    let t = frame_time(&f, 500000.0, Some(2000000.0));
    assert!((t - (nom as f64 / 500000.0 + data as f64 / 2000000.0)).abs() < 1e-12);
}

// ---------- filters ----------

#[test]
fn filter_string_syntax() {
    let f = parse_filter_string(Some("123, 7E0:7F8 x18DAF100:1FFFFF00")).unwrap().unwrap();
    assert_eq!(
        f,
        vec![
            IdFilter { id: 0x123, mask: None, ext: None },
            IdFilter { id: 0x7e0, mask: Some(0x7f8), ext: None },
            IdFilter { id: 0x18daf100, mask: Some(0x1fffff00), ext: Some(true) },
        ]
    );
    let m = |id: u32, ext: bool| match_filters(id, ext, false, Some(&f));
    assert!(m(0x123, false));
    assert!(!m(0x7e8, false)); // 0x7E8 & 0x7F8 = 0x7E8 ≠ 0x7E0
    assert!(m(0x7e7, false));
    assert!(m(0x18daf1aa, true));
    assert!(!m(0x18daf1aa, false));
    assert!(!match_filters(4, false, true, Some(&f)));
    assert!(match_filters(4, false, true, None));
    assert!(parse_filter_string(Some("zz")).is_err());
    assert_eq!(parse_filter_string(Some("  ")).unwrap(), None);
    assert_eq!(parse_filter_string(Some("0x1234")).unwrap().unwrap()[0].ext, Some(true));
}

#[test]
fn json_filters() {
    let f = parse_filters(&json!({ "id": "0x7E8", "mask": "7F8" })).unwrap().unwrap();
    assert_eq!(f, vec![IdFilter { id: 0x7e8, mask: Some(0x7f8), ext: None }]);
    assert_eq!(parse_filters(&json!([])).unwrap(), None);
    assert_eq!(parse_filters(&json!({})).unwrap_err(), "filter id must be a number or hex string");
    let f = parse_filters(&json!([{ "id": 5, "ext": 1 }])).unwrap().unwrap();
    assert_eq!(f[0].ext, Some(true));
}

#[test]
fn json_frame_specs() {
    assert_eq!(spec(json!({ "id": "0x7DF", "data": "0201" })).id, 0x7df as f64);
    assert_eq!(spec(json!({ "id": 291 })).id, 291.0);
    assert_eq!(spec(json!({ "cansend": "1#00" })).id, 1.0);
    assert!(spec_from_json(&json!({})).is_err());
    assert_eq!(spec_from_json(&json!({ "id": "zz" })).unwrap_err(), "bad id \"zz\"");
    assert_eq!(spec_from_json(&json!(5)).unwrap_err(), "frame must be an object");
}

// ---------- auto-bitrate decision ----------

fn t(bitrate: f64, frames: u64, errors: u64, bus_errors: u64) -> BitrateTrial {
    BitrateTrial { bitrate, frames, errors, bus_errors, skipped: None }
}

#[test]
fn clear_winner_among_error_ridden_wrong_rates() {
    let v = pick_bitrate(&[t(1e6, 0, 40, 200), t(8e5, 0, 35, 150), t(5e5, 120, 0, 0), t(2.5e5, 1, 30, 90), t(1.25e5, 0, 20, 40)]);
    assert_eq!((v.result, v.bitrate, v.confidence), ("found", Some(500000.0), Some("high")));
    assert_eq!(v.text, "500k: 120 frames, 0 errors");
}

#[test]
fn a_fluke_frame_at_a_wrong_rate_doesnt_win() {
    assert_eq!(pick_bitrate(&[t(5e5, 2, 50, 0), t(2.5e5, 0, 0, 0)]).result, "none");
}

#[test]
fn few_frames_low_confidence() {
    let v = pick_bitrate(&[t(125000.0, 1, 0, 0), t(250000.0, 0, 5, 0)]);
    assert_eq!((v.bitrate, v.confidence), (Some(125000.0), Some("low")));
}

#[test]
fn silent_bus() {
    let trials: Vec<_> = STD_BITRATES.iter().map(|b| t(*b as f64, 0, 0, 0)).collect();
    assert_eq!(pick_bitrate(&trials).result, "silent");
}

#[test]
fn bitrates_print_like_the_dashboard() {
    assert_eq!(fmt_bitrate(1e6), "1M");
    assert_eq!(fmt_bitrate(83333.0), "83.333k");
    assert_eq!(fmt_bitrate(500.0), "500");
    assert_eq!(fmt_bitrate(0.0), "?");
    assert_eq!(fmt_bitrate(2_500_000.0), "2.5M");
}

// ---------- socketcand protocol ----------

#[test]
fn element_framing_like_socketcands_receive_command() {
    let (cmds, rest) = sc_take("junk< open can0 >< rawmode >< send 12");
    assert_eq!(cmds, vec!["< open can0 >", "< rawmode >"]);
    assert_eq!(rest, "< send 12");
    assert_eq!(sc_words("< send 123 2 a 0B >"), vec!["send", "123", "2", "a", "0B"]);
    let (cmds, _) = sc_take("<\topen\n\n can0>");
    assert_eq!(cmds, vec!["< open can0>"]);
    assert_eq!(sc_words(&cmds[0]), vec!["open", "can0"]);
}

#[test]
fn raw_mode_frame_lines_are_what_python_can_parses() {
    let mut f = frame(json!({ "id": 0x123, "data": "DEADBEEF" }));
    f.ts = 1700000000123.456;
    assert_eq!(sc_frame_raw(&f), "< frame 123 1700000000.123456 DEADBEEF >");
    let mut e = frame(json!({ "id": 0x1abcdef, "ext": true, "data": "" }));
    e.ts = 1700000000000.0;
    assert_eq!(sc_frame_raw(&e), "< frame 01ABCDEF 1700000000.000000  >");
    assert_eq!(sc_frame_bcm(&f), "< frame 123 1700000000.123456 DE AD BE EF  >");
}

#[test]
fn send_parsing_python_can_and_savvycan_styles() {
    let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let f = sc_parse_id_len_data(&w(&["123", "2", "de", "a"])).unwrap();
    assert_eq!((f.id, f.ext, f.len), (0x123, false, 2));
    assert_eq!(to_hex(&f.data), "DE0A");
    let f = sc_parse_id_len_data(&w(&["00000123", "0"])).unwrap();
    assert_eq!((f.id, f.ext), (0x123, true));
    let f = sc_parse_id_len_data(&w(&["80000123", "1", "ff"])).unwrap(); // SavvyCAN sets bit 31
    assert_eq!((f.id, f.ext), (0x123, true));
    assert!(sc_parse_id_len_data(&w(&["123", "3", "00"])).is_err());
}

#[test]
fn beacon_xml_format() {
    assert_eq!(
        beacon_xml("benchbob", "10.0.0.2", 29536, &["can0".into(), "vcan0".into()]),
        "<CANBeacon name=\"benchbob\" type=\"SocketCAN\" description=\"BenchBob socketcand\">\n<URL>can://10.0.0.2:29536</URL><Bus name=\"can0\"/><Bus name=\"vcan0\"/></CANBeacon>"
    );
}

#[test]
fn javascript_conversions() {
    assert_eq!(to_uint32(-1.0), 0xffff_ffff);
    assert_eq!(to_uint32(f64::NAN), 0);
    assert_eq!(parse_int_hex("0x1Fzz"), Some(31.0));
    assert_eq!(parse_int_hex("zz"), None);
    assert_eq!(fmt_secs(1700000000000.9999), "1700000000.001000");
    assert_eq!(fmt_secs(1700000000999.9999), "1700000001.000000");
}
