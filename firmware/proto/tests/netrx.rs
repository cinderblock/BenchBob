//! Network input: sACN / Art-Net / DDP packets through `Leds::receive` and `tick`.

use benchbob_pixels::timing::Timing;
use benchbob_proto::api::leds::NET_TIMEOUT_MS;
use benchbob_proto::api::*;
use benchbob_proto::json::{self, Buf, Json, Value};

static PINS: [PinInfo; 4] = [
    PinInfo { gpio: 2, header: Some(4), func: None, note: None },
    PinInfo { gpio: 3, header: Some(5), func: None, note: None },
    PinInfo { gpio: 4, header: Some(6), func: None, note: None },
    PinInfo { gpio: 5, header: Some(7), func: None, note: None },
];
static LEDS: LedsInfo =
    LedsInfo { max_strings: 4, max_pixels: 1024, one_wire: true, clocked: true, check_timing: ok_timing, max_clock_hz: 10_000_000 };
static INFO: BoardInfo = BoardInfo {
    board: "mock",
    board_name: "Mock",
    chip: "MOCK",
    firmware_version: "0",
    build: "0",
    pins: &PINS,
    i2c: &[],
    leds: Some(&LEDS),
    onboard_leds: &[],
};

fn ok_timing(_: &Timing) -> Result<(), &'static str> {
    Ok(())
}

fn add(l: &mut Leds, body: &str) -> i64 {
    let v = json::parse(body).unwrap();
    l.configure(&INFO, None, Some(&v), 0).map_err(|e| e.message().to_string()).unwrap()
}

fn string(l: &Leds, id: i64) -> String {
    let mut mem = vec![0u8; 8192];
    let mut b = Buf::new(&mut mem);
    l.string_json(id, &mut Json::new(&mut b)).ok().unwrap();
    b.as_str().to_string()
}

fn field<'a>(s: &'a str, path: &[&str]) -> Value<'a> {
    let mut v = json::parse(s).unwrap();
    for k in path {
        v = v.get(k).unwrap();
    }
    v
}

fn sacn(universe: u16, seq: u8, priority: u8, cid: u8, data: &[u8]) -> Vec<u8> {
    let end = 126 + data.len();
    let mut p = vec![0u8; end];
    p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
    p[4..16].copy_from_slice(b"ASC-E1.17\0\0\0");
    p[16..18].copy_from_slice(&(0x7000 | (end - 16) as u16).to_be_bytes());
    p[18..22].copy_from_slice(&4u32.to_be_bytes());
    p[22..38].fill(cid);
    p[38..40].copy_from_slice(&(0x7000 | (end - 38) as u16).to_be_bytes());
    p[40..44].copy_from_slice(&2u32.to_be_bytes());
    p[44..50].copy_from_slice(b"xLight");
    p[108] = priority;
    p[111] = seq;
    p[113..115].copy_from_slice(&universe.to_be_bytes());
    p[115..117].copy_from_slice(&(0x7000 | (end - 115) as u16).to_be_bytes());
    p[117] = 0x02;
    p[118] = 0xA1;
    p[121..123].copy_from_slice(&1u16.to_be_bytes());
    p[123..125].copy_from_slice(&(1 + data.len() as u16).to_be_bytes());
    p[126..].copy_from_slice(data);
    p
}

fn artdmx(port_address: u16, data: &[u8]) -> Vec<u8> {
    let mut p = b"Art-Net\0".to_vec();
    p.extend_from_slice(&0x5000u16.to_le_bytes());
    p.extend_from_slice(&[0, 14, 0, 0]); // version, sequence, physical
    p.push(port_address as u8);
    p.push((port_address >> 8) as u8);
    p.extend_from_slice(&(data.len() as u16).to_be_bytes());
    p.extend_from_slice(data);
    p
}

fn ddp(offset: u32, push: bool, data: &[u8]) -> Vec<u8> {
    let mut p = vec![0x40 | push as u8, 0, 1, 1];
    p.extend_from_slice(&offset.to_be_bytes());
    p.extend_from_slice(&(data.len() as u16).to_be_bytes());
    p.extend_from_slice(data);
    p
}

/// The frame as wire bytes (GRB here), output on.
fn frame(l: &mut Leds, slot: usize) -> Vec<u8> {
    let mut out = vec![0u8; 4096];
    let n = l.render(slot, 0, &mut out);
    out.truncate(n);
    out
}

#[test]
fn sacn_takes_over_then_lets_go() {
    let mut l = Leds::new();
    l.set_output(true);
    // 200 GRB pixels from universe 5: 170 in the first universe, 30 in the next.
    let id = add(&mut l, r#"{"chip":"ws2812b","pin":2,"pixels":200,"gamma":false,"network":{"protocol":"sacn","universe":5}}"#);
    let mut u = [0u16; 8];
    let n = l.sacn_universes(&mut u);
    assert_eq!(&u[..n], &[5, 6]);
    assert_eq!(l.protocols(), 1);
    let g0 = l.job(0).unwrap().content_gen;

    assert!(l.receive(5568, &sacn(5, 1, 100, 1, &[255, 0, 0, 0, 255, 0]), "10.0.0.2", 100));
    assert!(l.receive(5568, &sacn(6, 9, 100, 1, &[0, 0, 255]), "10.0.0.2", 110));
    assert_ne!(l.job(0).unwrap().content_gen, g0);
    let f = frame(&mut l, 0);
    assert_eq!(&f[..6], &[0, 255, 0, 255, 0, 0], "pixel 0 red, pixel 1 green, in GRB");
    assert_eq!(&f[170 * 3..171 * 3], &[0, 0, 255], "pixel 170 from the second universe");
    let s = string(&l, id);
    assert!(field(&s, &["source"]).str_eq("network"));
    assert!(field(&s, &["receiving", "from"]).str_eq("xLight"));
    assert_eq!(field(&s, &["receiving", "packets"]).as_i64(), Some(2));
    assert!(field(&s, &["pattern", "name"]).str_eq("rainbow"), "the pattern that comes back");

    // A repeat (same sequence) is dropped; a lower-priority source too.
    assert!(!l.receive(5568, &sacn(5, 1, 100, 1, &[1, 1, 1]), "10.0.0.2", 120));
    assert!(!l.receive(5568, &sacn(5, 50, 50, 2, &[1, 1, 1]), "10.0.0.3", 130));
    // Other universes and ports aren't ours.
    assert!(!l.receive(5568, &sacn(9, 2, 100, 1, &[1, 1, 1]), "10.0.0.2", 140));
    assert!(!l.receive(6454, &artdmx(5, &[1, 1, 1]), "10.0.0.2", 140));

    // Setting a pattern while receiving changes what comes back, not the picture.
    let v = json::parse(r##"{"name":"solid","color":"#0000ff"}"##).unwrap();
    l.set_pattern(id, Some(&v), 150).ok().unwrap();
    assert!(field(&string(&l, id), &["source"]).str_eq("network"));
    let v = json::parse(r##"{"fill":"#ffffff"}"##).unwrap();
    assert_eq!(l.set_pixels(id, Some(&v)).err().unwrap().status, 409);

    // Quiet for the timeout: the pattern is back.
    assert!(!l.tick(110 + NET_TIMEOUT_MS));
    assert!(l.tick(111 + NET_TIMEOUT_MS));
    let s = string(&l, id);
    assert!(field(&s, &["source"]).str_eq("pattern"));
    assert!(field(&s, &["pattern", "name"]).str_eq("solid"));
    assert!(!field(&s, &["receiving", "active"]).as_bool().unwrap());
}

#[test]
fn artnet_with_a_start_channel_and_rgbw() {
    let mut l = Leds::new();
    l.set_output(true);
    add(
        &mut l,
        r#"{"chip":"sk6812","pin":2,"pixels":3,"order":"GRBW","gamma":false,"network":{"protocol":"artnet","universe":17,"startChannel":4}}"#,
    );
    let mut u = [0u16; 4];
    assert_eq!(l.artnet_universes(&mut u), 1);
    assert_eq!(u[0], 17);
    // Channels 1–3 belong to something else; then R G B W per pixel.
    assert!(l.receive(6454, &artdmx(17, &[9, 9, 9, 10, 20, 30, 40, 0, 0, 0, 0]), "10.0.0.9", 5));
    let f = frame(&mut l, 0);
    assert_eq!(&f[..4], &[20, 10, 30, 40]);
    assert!(field(&string(&l, 1), &["receiving", "from"]).str_eq("10.0.0.9"));
}

#[test]
fn ddp_renders_on_push_once_the_sender_uses_it() {
    let mut l = Leds::new();
    l.set_output(true);
    add(&mut l, r#"{"chip":"ws2812b","pin":3,"pixels":4,"order":"RGB","gamma":false,"network":{"protocol":"ddp","ddpOffset":3}}"#);
    // Without a push seen yet, every packet renders.
    assert!(l.receive(4048, &ddp(3, false, &[1, 2, 3, 4, 5, 6]), "x", 1));
    assert!(l.receive(4048, &ddp(9, true, &[7, 8, 9, 10, 11, 12]), "x", 2));
    // From now on only pushes do.
    assert!(!l.receive(4048, &ddp(3, false, &[0; 6]), "x", 3));
    assert!(l.receive(4048, &ddp(9, true, &[0; 6]), "x", 4));
    assert_eq!(frame(&mut l, 0), [0; 12]);
}

#[test]
fn network_config_validates_and_round_trips() {
    let mut l = Leds::new();
    let bad = json::parse(r#"{"chip":"ws2812b","pin":2,"pixels":10,"network":{"protocol":"sacn","universe":0}}"#).unwrap();
    assert_eq!(l.configure(&INFO, None, Some(&bad), 0).err().unwrap().status, 400);
    let bad = json::parse(r#"{"chip":"ws2812b","pin":2,"pixels":10,"network":{"protocol":"osc"}}"#).unwrap();
    assert!(l.configure(&INFO, None, Some(&bad), 0).err().unwrap().message().contains("sacn, artnet or ddp"));
    let id = add(&mut l, r#"{"chip":"ws2812b","pin":2,"pixels":10,"network":{"protocol":"artnet"}}"#);
    let s = string(&l, id);
    assert!(field(&s, &["network", "protocol"]).str_eq("artnet"));
    assert_eq!(field(&s, &["network", "universe"]).as_i64(), Some(0));
    assert!(field(&s, &["receiving"]).is_null());
    // Changing only the universe keeps the protocol; null turns it off.
    let v = json::parse(r#"{"network":{"universe":3}}"#).unwrap();
    l.configure(&INFO, Some(id), Some(&v), 0).ok().unwrap();
    assert_eq!(field(&string(&l, id), &["network", "universe"]).as_i64(), Some(3));
    let v = json::parse(r#"{"network":null}"#).unwrap();
    l.configure(&INFO, Some(id), Some(&v), 0).ok().unwrap();
    assert!(field(&string(&l, id), &["network"]).is_null());
    assert_eq!(l.protocols(), 0);
}

fn persisted(l: &Leds, id: i64) -> String {
    let mut mem = vec![0u8; 2048];
    let mut b = Buf::new(&mut mem);
    l.persist_json(id, &mut Json::new(&mut b)).ok().unwrap();
    b.as_str().to_string()
}

#[test]
fn settings_persist_and_restore_without_live_state() {
    let mut l = Leds::new();
    let g0 = l.persist_gen();
    let a = add(
        &mut l,
        r#"{"chip":"custom","pin":2,"pixels":12,"name":"desk","order":"GRBW","timing":{"t0hNs":350,"t1hNs":700,"periodNs":1250,"resetUs":80},"power":{"maxMa":500},"network":{"protocol":"artnet","universe":4}}"#,
    );
    let b = add(&mut l, r#"{"chip":"apa102","pin":4,"clockPin":5,"pixels":5}"#);
    assert_ne!(l.persist_gen(), g0);
    let v = json::parse(r#"{"name":"chase","speed":20}"#).unwrap();
    l.set_pattern(a, Some(&v), 0).ok().unwrap();
    let g1 = l.persist_gen();
    // Pixels and network data aren't settings.
    let v = json::parse(r##"{"fill":"#ff0000"}"##).unwrap();
    let _ = l.set_pixels(b, Some(&v));
    l.receive(6454, &artdmx(4, &[1, 2, 3, 4]), "x", 10);
    assert_eq!(l.persist_gen(), g1);

    let pa = persisted(&l, a);
    let pb = persisted(&l, b);
    assert!(!pa.contains("state") && !pa.contains("receiving") && !pa.contains("estimateMa"));
    assert!(pa.contains(r#""resetUs":80"#) && pa.contains(r#""name":"chase""#), "{pa}");
    // A clocked string's clock; a preset chip's timing isn't stored; nor pixels.
    assert!(pb.contains(r#""clockHz":"#) && !pb.contains("timing") && pb.contains(r#""pattern":{"name":"off"}"#), "{pb}");

    let mut r = Leds::new();
    assert_eq!(r.restore_json(&INFO, pa.as_bytes(), 0).ok(), Some(a));
    assert_eq!(r.restore_json(&INFO, pb.as_bytes(), 0).ok(), Some(b));
    assert_eq!(persisted(&r, a), pa);
    assert_eq!(persisted(&r, b), pb);
    assert!(r.restore_json(&INFO, b"{}", 0).is_err());
}

#[test]
fn preview_downsamples_what_the_string_shows() {
    let mut l = Leds::new();
    let id = add(&mut l, r#"{"chip":"sk6812","pin":2,"pixels":300,"order":"GRBW","brightness":128}"#);
    let v = json::parse(r##"{"name":"solid","color":"#ff000080"}"##).unwrap();
    l.set_pattern(id, Some(&v), 0).ok().unwrap();
    let mut mem = vec![0u8; 4096];
    let mut b = Buf::new(&mut mem);
    l.preview(id, 0, 100, &mut Json::new(&mut b)).ok().unwrap();
    let s = b.as_str().to_string();
    assert_eq!(field(&s, &["step"]).as_i64(), Some(3));
    assert!(!field(&s, &["output"]).as_bool().unwrap());
    let colors = field(&s, &["colors"]);
    let c = colors.as_plain_str().unwrap();
    assert_eq!(c.len(), 100 * 6);
    // Red + half white, at half brightness: (255+128 → 255) / 2, 128 / 2, 128 / 2.
    assert_eq!(&c[..6], "804040");
}
