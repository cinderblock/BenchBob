//! ble-decode.test.ts, with the same crafted and published payloads, plus the real capture.

use serde_json::json;

use super::*;

fn fields(b: Option<Beacon>) -> Value {
    Value::Object(b.unwrap().fields.unwrap())
}

/// The capture is real over-the-air traffic, so it carries neighbours' device addresses and
/// names. It stays out of the (public) repo: gitignored, and this test skips without it.
/// `BENCHBOB_BLE_CAPTURE` can point at a copy elsewhere.
fn capture() -> Option<Vec<u8>> {
    let p = std::env::var("BENCHBOB_BLE_CAPTURE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/ble-scan.btsnoop"));
    std::fs::read(p).ok()
}

#[test]
fn real_capture_chunked_arbitrarily() {
    let Some(bytes) = capture() else {
        eprintln!("skipped: no ble-scan.btsnoop capture");
        return;
    };
    let mut r = BtsnoopReader::default();
    let mut recs = Vec::new();
    for c in bytes.chunks(777) {
        recs.extend(r.push(c).unwrap());
    }
    assert_eq!(r.datalink, 2001);
    let ni = recs.iter().find(|x| x.opcode == mon::NEW_INDEX).unwrap();
    assert_eq!(parse_new_index(&ni.data), ("DC:A6:32:03:FC:38".to_string(), "hci0".to_string()));
    let reports: Vec<AdvReport> =
        recs.iter().filter(|x| x.opcode == mon::EVENT).flat_map(|x| parse_hci_event(&x.data, x.ts, x.index)).collect();
    // btmon -r on the same file counts 289 ADV_NONCONN_IND + 255 ADV_IND + 244 SCAN_RSP.
    assert_eq!(reports.len(), 788);
    // Timestamps are real wall-clock times (2026): after 2025-01-01.
    assert!(reports[0].ts > 1_735_689_600_000.0);
    assert!(reports.iter().all(|x| x.rssi.is_none_or(|r| r < 0 && r > -128)));

    let mut t = BleTracker::default();
    for x in &reports {
        t.ingest(x, &format!("hci{}", x.index));
    }
    let list = t.list(None, None, None);
    assert!(list.len() > 10);
    let companies: Vec<&str> = list.iter().flat_map(|d| d.mfr.iter().map(|m| m.company_name)).collect();
    for c in ["Microsoft", "Apple", "Tuya"] {
        assert!(companies.contains(&c), "{c}");
    }
    assert!(list.iter().any(|d| d.connectable));
    assert!(list.iter().any(|d| !d.scan_rsp.is_empty()));
    assert!(list.iter().any(|d| d.beacons.iter().any(|b| b.kind == "ms-cdp")));
    assert!(list.iter().any(|d| d.beacons.iter().any(|b| b.kind == "apple")));
    // RSSI history is bucketed per second, so ≤ ~11 slots for a 10 s capture.
    assert!(list.iter().map(|d| d.rssi_hist.len()).max().unwrap() <= 12);
    for d in &list {
        let _ = device_kind(d);
    }
}

#[test]
fn legacy_report() {
    // 3E len 02 num=1 type=0 addrType=1 addr(LE) dataLen=3 data rssi
    let ev = from_hex("3e 0f 02 01 00 01 66 55 44 33 22 c1 03 02 01 06 b5");
    let r = &parse_hci_event(&ev, 0.0, 0)[0];
    assert_eq!(r.addr, "C1:22:33:44:55:66");
    assert_eq!(r.pdu, "ADV_IND");
    assert!(r.connectable);
    assert_eq!(r.rssi, Some(-75));
    assert_eq!(addr_kind(&r.addr, r.addr_type), "random static");
    assert_eq!(parse_ad(&r.data).flags, Some(6));
}

#[test]
fn extended_report_with_tx_power() {
    // et=0x0013 (connectable, scannable, legacy)… addr, phys, sid, tx=-8, rssi=-60, interval, direct, len 0
    let ev = from_hex("3e 1a 0d 01 13 00 00 01 02 03 04 05 06 01 00 ff f8 c4 00 00 00 00 00 00 00 00 00 00 00");
    let r = &parse_hci_event(&ev, 0.0, 0)[0];
    assert_eq!(r.addr, "06:05:04:03:02:01");
    assert_eq!(r.pdu, "ADV_IND");
    assert_eq!(r.tx_power, Some(-8));
    assert_eq!(r.rssi, Some(-60));
}

#[test]
fn address_kinds() {
    assert_eq!(addr_kind("4A:00:00:00:00:00", 1), "random resolvable");
    assert_eq!(addr_kind("0A:00:00:00:00:00", 1), "random non-resolvable");
    assert_eq!(addr_kind("DC:A6:32:03:FC:38", 0), "public");
}

#[test]
fn ad_structures() {
    let ad = parse_ad(&from_hex("020106 0509 54657374 0303 0F18 020A F4 0716 AAFE 10E70001 05FF 4C00 1002"));
    assert_eq!(ad.name.as_deref(), Some("Test"));
    assert_eq!(ad.uuids, ["180F"]);
    assert_eq!(ad.tx_power, Some(-12));
    assert_eq!(ad.svc[0].0, "FEAA");
    assert_eq!(ad.mfr[0].0, 0x004c);
}

#[test]
fn ibeacon() {
    let f = fields(decode_ibeacon(0x004c, &from_hex("0215 E2C56DB5DFFB48D2B060D0F5A71096E0 0001 0002 C5")));
    assert_eq!(f, json!({ "uuid": "E2C56DB5-DFFB-48D2-B060-D0F5A71096E0", "major": 1, "minor": 2, "measuredPower_dBm": -59 }));
}

#[test]
fn eddystone() {
    let uid = fields(decode_eddystone("FEAA", &from_hex("00E7 00112233445566778899 AABBCCDDEEFF 0000")));
    assert_eq!(uid, json!({ "txPower0m_dBm": -25, "namespace": "00112233445566778899", "instance": "AABBCCDDEEFF" }));
    assert_eq!(fields(decode_eddystone("FEAA", &from_hex("10E7 00 676f6f676c65 07")))["url"], "http://www.google.com");
    assert_eq!(fields(decode_eddystone("FEAA", &from_hex("10E7 03 6578616d706c65 00 6162")))["url"], "https://example.com/ab");
    let tlm = fields(decode_eddystone("FEAA", &from_hex("2000 0BB8 1980 00000064 00000E10")));
    assert_eq!(tlm, json!({ "battery_mV": 3000, "temperature_C": 25.5, "advCount": 100, "uptime_s": 360 }));
}

#[test]
fn bthome_spec_examples() {
    assert_eq!(fields(decode_bthome("FCD2", &from_hex("40 02CA09 03BF13"))), json!({ "temperature_C": 25.06, "humidity": 50.55 }));
    assert_eq!(
        fields(decode_bthome("FCD2", &from_hex("44 0161 3A01"))),
        json!({ "battery": 97, "button": "press", "trigger_based": true })
    );
    assert_eq!(fields(decode_bthome("FCD2", &from_hex("40 02CA09 02F6FF"))), json!({ "temperature_C": 25.06, "temperature_C_2": -0.1 }));
    assert_eq!(fields(decode_bthome("FCD2", &from_hex("41 0102"))), json!({ "encrypted": true }));
    assert_eq!(fields(decode_bthome("FCD2", &from_hex("40 5304 74657374"))), json!({ "text": "test" }));
}

#[test]
fn ruuvi_rawv2() {
    let f = fields(decode_ruuvi(0x0499, &from_hex("0512FC5394C37C0004FFFC040CAC364200CDCBB8334C884F")));
    assert_eq!(
        f,
        json!({
            "temperature_C": 24.3, "humidity": 53.49, "pressure_hPa": 1000.44, "accelX_mg": 4, "accelY_mg": -4, "accelZ_mg": 1036,
            "battery_mV": 2977, "txPower_dBm": 4, "movements": 66, "sequence": 205, "mac": "CB:B8:33:4C:88:4F",
        })
    );
}

#[test]
fn govee() {
    assert_eq!(
        fields(decode_govee(0xec88, &from_hex("000351B46400"), "GVH5075_1A2B")),
        json!({ "temperature_C": 21.7, "humidity": 52.4, "battery": 100 })
    );
    assert_eq!(fields(decode_govee(0xec88, &from_hex("0080C6703700"), "")), json!({ "temperature_C": -5, "humidity": 80, "battery": 55 }));
    assert!(decode_govee(0xec88, &from_hex("000351B46400"), "GVH5075_1A2B").unwrap().label.contains("GVH5075"));
}

#[test]
fn atc_and_pvvx() {
    assert_eq!(
        fields(decode_atc("181A", &from_hex("A4C138AABBCC 00E1 2D 5A 0B86 07"))),
        json!({ "mac": "A4:C1:38:AA:BB:CC", "temperature_C": 22.5, "humidity": 45, "battery": 90, "battery_mV": 2950, "frameCounter": 7 })
    );
    let p = decode_atc("181A", &from_hex("CCBBAA38C1A4 CA08 A011 860B 5A 07 05")).unwrap();
    assert_eq!(p.kind, "pvvx");
    assert_eq!(
        Value::Object(p.fields.unwrap()),
        json!({ "mac": "A4:C1:38:AA:BB:CC", "temperature_C": 22.5, "humidity": 45.12, "battery_mV": 2950, "battery": 90, "frameCounter": 7, "flags": 5 })
    );
}

#[test]
fn mibeacon() {
    let f = fields(decode_mibeacon("FE95", &from_hex("5050 5B05 01 CCBBAA38C1A4 0D10 04 E100 C201")));
    assert_eq!(f, json!({ "product": "LYWSD03MMC", "frameCounter": 1, "mac": "A4:C1:38:AA:BB:CC", "temperature_C": 22.5, "humidity": 45 }));
    assert_eq!(fields(decode_mibeacon("FE95", &from_hex("5050 5B05 02 CCBBAA38C1A4 0A10 01 64")))["battery"], 100);
    assert_eq!(fields(decode_mibeacon("FE95", &from_hex("5858 5B05 01 0102030405060708")))["encrypted"], true);
}

#[test]
fn apple_continuity() {
    let sep = decode_apple(0x004c, &from_hex(&format!("1219{}{}", "10", "00".repeat(24)))).unwrap();
    assert_eq!(sep.fields.as_ref().unwrap()["findMySeparated"], true);
    assert!(sep.label.contains("separated"));
    assert_eq!(fields(decode_apple(0x004c, &from_hex("12022400")))["findMySeparated"], false);
    assert_eq!(fields(decode_apple(0x004c, &from_hex(&format!("0719010E20{}", "00".repeat(22)))))["audioModel"], "AirPods Pro");
    assert_eq!(
        fields(decode_apple(0x004c, &from_hex(&format!("10050B1C4E5F38 0C0E00{}", "00".repeat(14)))))["messages"],
        json!(["Nearby Info", "Handoff"])
    );
    // iBeacon payloads are left to the iBeacon decoder.
    assert_eq!(decode_apple(0x004c, &from_hex("0215E2C56DB5DFFB48D2B060D0F5A71096E000010002C5")), None);
}

#[test]
fn microsoft() {
    let mut d = vec![0x03, 0x00, 0x80];
    d.extend_from_slice(b"Surface Mouse");
    assert_eq!(fields(decode_microsoft(0x0006, &d)), json!({ "scenario": "LE", "displayName": "Surface Mouse" }));
    assert_eq!(fields(decode_microsoft(0x0006, &from_hex("010920")))["deviceType"], "Windows desktop");
}

#[test]
fn dispatch_across_a_full_advertisement() {
    let ad = parse_ad(&from_hex("020106 0C16 D2FC 40 02CA09 03BF13 0161"));
    let b = &decode_beacons(&ad)[0];
    assert_eq!(b.kind, "bthome");
    assert_eq!(Value::Object(b.fields.clone().unwrap()), json!({ "temperature_C": 25.06, "humidity": 50.55, "battery": 97 }));
}

/// Beyond the TypeScript tests: labels, the device JSON's key order, and that truncated
/// payloads decode to nothing instead of failing.
#[test]
fn labels_device_json_and_truncation() {
    let tlm = decode_eddystone("FEAA", &from_hex("2000 0BB8 1980 00000064 00000E10")).unwrap();
    assert_eq!(tlm.label, "Eddystone-TLM 25.5 °C · batt 3000 mV up 6 min");
    assert_eq!(decode_bthome("FCD2", &from_hex("40 02CA09 03BF13")).unwrap().label, "BTHome 25.06 °C · 50.55 %RH");
    assert_eq!(decode_mibeacon("FE95", &from_hex("5050 5B05 01 CCBB")), None);
    let mut t = BleTracker::default();
    let ev = from_hex("3e 0f 02 01 00 01 66 55 44 33 22 c1 03 02 01 06 b5");
    t.ingest(&parse_hci_event(&ev, 1000.5, 0)[0], "hci0");
    let d = t.devices.values().next().unwrap();
    let j = d.json();
    let keys: Vec<&str> = j.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "addr",
            "addrType",
            "controller",
            "name",
            "rssi",
            "rssiHist",
            "txPower",
            "firstSeen",
            "lastSeen",
            "count",
            "connectable",
            "scannable",
            "flags",
            "appearance",
            "uuids",
            "mfr",
            "svc",
            "beacons",
            "lastPdu",
            "adv",
            "scanRsp"
        ]
    );
    assert_eq!(serde_json::to_string(&d.json()["firstSeen"]).unwrap(), "1000.5");
}
