// ==================== BLE advertisement decoding ====================
//
// Pure functions, no I/O, so everything here is unit-tested against captured and crafted
// bytes. The pipeline is:
//
//   btmon -w (btsnoop, Linux monitor format) → BtsnoopReader → HCI LE advertising reports
//   → AD structures → beacon decoders → BleTracker (per-advertiser aggregate)
//
// Working from raw HCI reports rather than BlueZ's D-Bus view is deliberate: D-Bus merges
// advertisements and scan responses, hides the PDU type (so "connectable" is unknowable)
// and only keeps the latest manufacturer data per company.

import { APPLE_AUDIO_MODELS, APPLE_TYPES, MS_CDP_DEVICES, XIAOMI_PRODUCTS, companyName, uuid16Name } from "./ble-ids.ts";

// ---------- small byte helpers ----------

export function hex(b: Uint8Array, sep = ""): string {
  let s = "";
  for (let i = 0; i < b.length; i++) s += (i && sep ? sep : "") + b[i].toString(16).padStart(2, "0");
  return s;
}

export function fromHex(s: string): Uint8Array {
  const c = s.replace(/[^0-9a-fA-F]/g, "");
  const out = new Uint8Array(c.length >> 1);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(c.substr(i * 2, 2), 16);
  return out;
}

const dv = (b: Uint8Array) => new DataView(b.buffer, b.byteOffset, b.byteLength);
const u16le = (b: Uint8Array, o: number) => b[o] | (b[o + 1] << 8);
const u16be = (b: Uint8Array, o: number) => (b[o] << 8) | b[o + 1];
const s16le = (b: Uint8Array, o: number) => (u16le(b, o) << 16) >> 16;
const s16be = (b: Uint8Array, o: number) => (u16be(b, o) << 16) >> 16;
const u24le = (b: Uint8Array, o: number) => b[o] | (b[o + 1] << 8) | (b[o + 2] << 16);
const u32le = (b: Uint8Array, o: number) => (b[o] | (b[o + 1] << 8) | (b[o + 2] << 16) | (b[o + 3] << 24)) >>> 0;
const u32be = (b: Uint8Array, o: number) => ((b[o] << 24) | (b[o + 1] << 16) | (b[o + 2] << 8) | b[o + 3]) >>> 0;
const s8 = (v: number) => (v << 24) >> 24;
const round = (v: number, d = 2) => Math.round(v * 10 ** d) / 10 ** d;

/** Bluetooth addresses travel little-endian; display is big-endian with colons. */
export function addrStr(b: Uint8Array, o = 0): string {
  const p: string[] = [];
  for (let i = 5; i >= 0; i--) p.push(b[o + i].toString(16).padStart(2, "0"));
  return p.join(":").toUpperCase();
}

function uuid128Str(b: Uint8Array, o: number): string {
  const h = hex(b.slice(o, o + 16).reverse());
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`.toUpperCase();
}

// ---------- btsnoop stream ----------

export interface BtsnoopRecord {
  ts: number; // ms since the Unix epoch
  index: number; // controller index (hciN); 0xFFFF for system notes
  opcode: number; // monitor opcode: 0 new index, 1 del index, 3 event, 12 system note…
  data: Uint8Array;
}

export const MON = { NEW_INDEX: 0, DEL_INDEX: 1, COMMAND: 2, EVENT: 3, INDEX_INFO: 10, SYSTEM_NOTE: 12 };

// btsnoop timestamps count microseconds from 0000-01-01; this is 1970-01-01 on that scale.
const BTSNOOP_EPOCH_US = 0x00dcddb30f2f8000n;

/** Incremental btsnoop parser: feed it pipe chunks, get whole records back. */
export class BtsnoopReader {
  private buf = new Uint8Array(0);
  private headerSeen = false;
  datalink = 0;

  push(chunk: Uint8Array): BtsnoopRecord[] {
    const merged = new Uint8Array(this.buf.length + chunk.length);
    merged.set(this.buf);
    merged.set(chunk, this.buf.length);
    let o = 0;
    const out: BtsnoopRecord[] = [];
    if (!this.headerSeen) {
      if (merged.length < 16) {
        this.buf = merged;
        return out;
      }
      if (new TextDecoder().decode(merged.slice(0, 7)) !== "btsnoop") throw new Error("not a btsnoop stream");
      this.datalink = dv(merged).getUint32(12);
      this.headerSeen = true;
      o = 16;
    }
    const v = dv(merged);
    while (merged.length - o >= 24) {
      const incl = v.getUint32(o + 4);
      if (merged.length - o < 24 + incl) break;
      const flags = v.getUint32(o + 8);
      const tsUs = v.getBigUint64(o + 16);
      const data = merged.slice(o + 24, o + 24 + incl);
      const ts = Number(tsUs - BTSNOOP_EPOCH_US) / 1000;
      if (this.datalink === 2001) out.push({ ts, index: flags >>> 16, opcode: flags & 0xffff, data });
      else if (this.datalink === 1002 && data[0] === 0x04) out.push({ ts, index: 0, opcode: MON.EVENT, data: data.slice(1) }); // H4 UART
      else if (this.datalink === 1001 && flags & 2 && flags & 1) out.push({ ts, index: 0, opcode: MON.EVENT, data });
      o += 24 + incl;
    }
    this.buf = merged.slice(o);
    return out;
  }
}

/** NEW_INDEX payload: type, bus, bdaddr, 8-byte name ("hci0"). */
export function parseNewIndex(d: Uint8Array): { addr: string; name: string } {
  return { addr: addrStr(d, 2), name: new TextDecoder().decode(d.slice(8, 16)).replace(/\0.*$/s, "") };
}

// ---------- HCI LE advertising reports ----------

export interface AdvReport {
  ts: number;
  index: number;
  addr: string;
  addrType: number; // HCI: 0 public, 1 random, 2 public identity, 3 random static identity
  pdu: string; // ADV_IND, ADV_NONCONN_IND, SCAN_RSP, EXT…
  connectable: boolean;
  scannable: boolean;
  scanRsp: boolean;
  rssi: number | null;
  txPower: number | null; // extended reports carry it in the header
  data: Uint8Array;
}

const LEGACY_PDU = ["ADV_IND", "ADV_DIRECT_IND", "ADV_SCAN_IND", "ADV_NONCONN_IND", "SCAN_RSP"];

/** Decode an HCI event packet; returns the advertising reports it carries (usually zero or one). */
export function parseHciEvent(ev: Uint8Array, ts = 0, index = 0): AdvReport[] {
  if (ev.length < 3 || ev[0] !== 0x3e) return [];
  const sub = ev[2];
  const out: AdvReport[] = [];
  let o = 4;
  const n = ev[3];
  if (sub === 0x02) {
    // LE Advertising Report. Controllers send one report per event in practice; BlueZ parses
    // multiple sequentially, so do the same.
    for (let i = 0; i < n && o + 9 <= ev.length; i++) {
      const type = ev[o];
      const addrType = ev[o + 1];
      const addr = addrStr(ev, o + 2);
      const len = ev[o + 8];
      const data = ev.slice(o + 9, o + 9 + len);
      const r = ev[o + 9 + len];
      o += 10 + len;
      out.push({
        ts, index, addr, addrType,
        pdu: LEGACY_PDU[type] || `type ${type}`,
        connectable: type === 0 || type === 1,
        scannable: type === 0 || type === 2,
        scanRsp: type === 4,
        rssi: r === undefined || r === 127 ? null : s8(r),
        txPower: null,
        data,
      });
    }
  } else if (sub === 0x0d) {
    // LE Extended Advertising Report (BT 5).
    for (let i = 0; i < n && o + 24 <= ev.length; i++) {
      const et = u16le(ev, o);
      const addrType = ev[o + 2];
      const addr = addrStr(ev, o + 3);
      const tx = s8(ev[o + 12]);
      const r = s8(ev[o + 13]);
      const len = ev[o + 23];
      const data = ev.slice(o + 24, o + 24 + len);
      o += 24 + len;
      const legacy = !!(et & 0x10);
      out.push({
        ts, index, addr, addrType,
        pdu: legacy ? (et & 0x08 ? "SCAN_RSP" : et & 0x01 ? "ADV_IND" : et & 0x02 ? "ADV_SCAN_IND" : "ADV_NONCONN_IND") : `EXT${et & 0x08 ? " SCAN_RSP" : ""}`,
        connectable: !!(et & 0x01),
        scannable: !!(et & 0x02),
        scanRsp: !!(et & 0x08),
        rssi: r === 127 ? null : r,
        txPower: tx === 127 ? null : tx,
        data,
      });
    }
  }
  return out;
}

/** Human address type, including the random sub-type from the top two address bits. */
export function addrKind(addr: string, hciType: number): string {
  if (hciType === 0) return "public";
  if (hciType === 2) return "public (resolved)";
  if (hciType === 3) return "random static (resolved)";
  const top = parseInt(addr.slice(0, 2), 16) >> 6;
  return top === 3 ? "random static" : top === 1 ? "random resolvable" : top === 0 ? "random non-resolvable" : "random";
}

// ---------- AD structures ----------

export interface AdInfo {
  flags?: number;
  name?: string;
  nameComplete?: boolean;
  txPower?: number;
  appearance?: number;
  uuids: string[]; // "180F" for 16-bit, full form for 128-bit
  mfr: { company: number; data: Uint8Array }[];
  svc: { uuid: string; data: Uint8Array }[];
}

export function parseAd(d: Uint8Array): AdInfo {
  const ad: AdInfo = { uuids: [], mfr: [], svc: [] };
  let o = 0;
  while (o < d.length) {
    const len = d[o];
    if (!len || o + 1 + len > d.length) break;
    const t = d[o + 1];
    const v = d.slice(o + 2, o + 1 + len);
    o += 1 + len;
    switch (t) {
      case 0x01:
        ad.flags = v[0];
        break;
      case 0x02:
      case 0x03:
        for (let i = 0; i + 1 < v.length; i += 2) ad.uuids.push(u16le(v, i).toString(16).toUpperCase().padStart(4, "0"));
        break;
      case 0x04:
      case 0x05:
        for (let i = 0; i + 3 < v.length; i += 4) ad.uuids.push(u32le(v, i).toString(16).toUpperCase().padStart(8, "0"));
        break;
      case 0x06:
      case 0x07:
        for (let i = 0; i + 15 < v.length; i += 16) ad.uuids.push(uuid128Str(v, i));
        break;
      case 0x08:
      case 0x09: {
        const name = new TextDecoder().decode(v).replace(/\0+$/, "");
        // A complete name beats a shortened one regardless of which packet came first.
        if (name && (t === 0x09 || !ad.nameComplete)) {
          ad.name = name;
          ad.nameComplete = t === 0x09;
        }
        break;
      }
      case 0x0a:
        ad.txPower = s8(v[0]);
        break;
      case 0x19:
        if (v.length >= 2) ad.appearance = u16le(v, 0);
        break;
      case 0x16:
        if (v.length >= 2) ad.svc.push({ uuid: u16le(v, 0).toString(16).toUpperCase().padStart(4, "0"), data: v.slice(2) });
        break;
      case 0x20:
        if (v.length >= 4) ad.svc.push({ uuid: u32le(v, 0).toString(16).toUpperCase().padStart(8, "0"), data: v.slice(4) });
        break;
      case 0x21:
        if (v.length >= 16) ad.svc.push({ uuid: uuid128Str(v, 0), data: v.slice(16) });
        break;
      case 0xff:
        if (v.length >= 2) ad.mfr.push({ company: u16le(v, 0), data: v.slice(2) });
        break;
    }
  }
  return ad;
}

/** "180F" → "Battery"; 128-bit UUIDs built on the SIG base are shortened first. */
export function uuidName(uuid: string): string {
  const m = /^0000([0-9A-F]{4})-0000-1000-8000-00805F9B34FB$/i.exec(uuid);
  const short = m ? m[1] : uuid.length === 4 ? uuid : null;
  return short ? uuid16Name(parseInt(short, 16)) : "";
}

// ---------- Beacon decoders ----------

export interface Beacon {
  kind: string; // stable id: "ibeacon", "eddystone-tlm", "bthome", "apple", …
  label: string; // one-line human summary
  fields?: Record<string, any>; // decoded values (units in the key names)
}

export function decodeIBeacon(company: number, d: Uint8Array): Beacon | null {
  if (company !== 0x004c || d.length < 23 || d[0] !== 0x02 || d[1] !== 0x15) return null;
  const h = hex(d.slice(2, 18)).toUpperCase();
  const uuid = `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  const major = u16be(d, 18);
  const minor = u16be(d, 20);
  const measuredPower = s8(d[22]);
  return { kind: "ibeacon", label: `iBeacon ${uuid} major ${major} minor ${minor} @1m ${measuredPower} dBm`, fields: { uuid, major, minor, measuredPower_dBm: measuredPower } };
}

/** Apple Continuity: a sequence of type/length/value messages. Labels only — the payloads are mostly encrypted. */
export function decodeApple(company: number, d: Uint8Array): Beacon | null {
  if (company !== 0x004c || d.length < 2) return null;
  const msgs: string[] = [];
  const fields: Record<string, any> = {};
  let o = 0;
  while (o + 2 <= d.length) {
    const t = d[o];
    const len = d[o + 1];
    const v = d.slice(o + 2, o + 2 + len);
    o += 2 + len;
    if (t === 0x02) continue; // iBeacon has its own decoder
    let label = APPLE_TYPES[t] || `type 0x${t.toString(16).padStart(2, "0")}`;
    if (t === 0x07 && v.length >= 3) {
      const model = APPLE_AUDIO_MODELS[u16be(v, 1)];
      if (model) {
        label += `: ${model}`;
        fields.audioModel = model;
      }
    } else if (t === 0x12) {
      // Find My (offline finding). A 25-byte payload carries the rotating public key and is
      // only broadcast while the accessory is separated from its owner; 2 bytes = near owner.
      const separated = len >= 25;
      const battery = v.length ? ["full", "medium", "low", "critical"][v[0] >> 6] : undefined;
      label += separated ? " (separated from owner)" : " (near owner)";
      fields.findMySeparated = separated;
      if (battery && separated) fields.findMyBattery = battery;
    } else if (t === 0x10 && v.length >= 1) {
      fields.nearbyInfoStatus = v[0];
    }
    msgs.push(label);
  }
  if (!msgs.length) return null;
  return { kind: "apple", label: `Apple: ${msgs.join(", ")}`, fields: { messages: msgs, ...fields } };
}

/** Microsoft: Swift Pair (beacon id 0x03) and CDP (0x01, Windows/Xbox "nearby devices"). */
export function decodeMicrosoft(company: number, d: Uint8Array): Beacon | null {
  if (company !== 0x0006 || d.length < 2) return null;
  if (d[0] === 0x03) {
    // Swift Pair: 0x03, sub-scenario, reserved RSSI byte, then [BR/EDR addr + CoD] + display name.
    const sub = d[1];
    let nameAt = 3;
    if (sub === 0x01) nameAt = 3 + 6 + 3;
    const name = d.length > nameAt ? new TextDecoder().decode(d.slice(nameAt)).replace(/\0+$/, "") : "";
    const mode = sub === 0x00 ? "LE" : sub === 0x01 ? "BR/EDR via LE" : sub === 0x02 ? "LE & BR/EDR" : `scenario ${sub}`;
    return { kind: "swiftpair", label: `Microsoft Swift Pair (${mode})${name ? `: ${name}` : ""}`, fields: { scenario: mode, displayName: name || undefined } };
  }
  if (d[0] === 0x01) {
    const dev = MS_CDP_DEVICES[d[1] & 0x1f] || `device type ${d[1] & 0x1f}`;
    return { kind: "ms-cdp", label: `Microsoft CDP beacon (${dev})`, fields: { deviceType: dev } };
  }
  return null;
}

/** Ruuvi RAWv2 (data format 5). */
export function decodeRuuvi(company: number, d: Uint8Array): Beacon | null {
  if (company !== 0x0499 || d.length < 24 || d[0] !== 0x05) return null;
  const f: Record<string, any> = {};
  const t = s16be(d, 1);
  if (t !== -32768) f.temperature_C = round(t * 0.005, 3);
  const h = u16be(d, 3);
  if (h !== 0xffff) f.humidity = round(h * 0.0025, 2);
  const p = u16be(d, 5);
  if (p !== 0xffff) f.pressure_hPa = round((p + 50000) / 100, 2);
  const ax = s16be(d, 7), ay = s16be(d, 9), az = s16be(d, 11);
  if (ax !== -32768) Object.assign(f, { accelX_mg: ax, accelY_mg: ay, accelZ_mg: az });
  const pw = u16be(d, 13);
  if (pw >> 5 !== 0x7ff) f.battery_mV = (pw >> 5) + 1600;
  if ((pw & 0x1f) !== 0x1f) f.txPower_dBm = (pw & 0x1f) * 2 - 40;
  if (d[15] !== 0xff) f.movements = d[15];
  if (u16be(d, 16) !== 0xffff) f.sequence = u16be(d, 16);
  f.mac = hex(d.slice(18, 24), ":").toUpperCase();
  return { kind: "ruuvi", label: `RuuviTag ${sensorLabel(f)}`, fields: f };
}

/** Govee H5072/H5075 (and the many models sharing the 3-byte packed format). */
export function decodeGovee(company: number, d: Uint8Array, name = ""): Beacon | null {
  if (company !== 0xec88 || d.length < 5) return null;
  // Packed as one 24-bit big-endian number: temp*10000 + humidity*10, bit 23 = negative.
  let packed = (d[1] << 16) | (d[2] << 8) | d[3];
  const neg = !!(packed & 0x800000);
  packed &= 0x7fffff;
  const f = {
    temperature_C: round((neg ? -1 : 1) * Math.floor(packed / 1000) / 10, 1),
    humidity: round((packed % 1000) / 10, 1),
    battery: d[4] & 0x7f,
  };
  const model = /GVH5\d{3}|Govee_H5\d{3}/i.exec(name)?.[0] || "H5075/H5072";
  return { kind: "govee", label: `Govee ${model} ${sensorLabel(f)}`, fields: f };
}

/** Eddystone frames (service data 0xFEAA). */
export function decodeEddystone(uuid: string, d: Uint8Array): Beacon | null {
  if (uuid !== "FEAA" || !d.length) return null;
  const type = d[0];
  if (type === 0x00 && d.length >= 18) {
    const ns = hex(d.slice(2, 12)).toUpperCase();
    const inst = hex(d.slice(12, 18)).toUpperCase();
    return { kind: "eddystone-uid", label: `Eddystone-UID ${ns} / ${inst}`, fields: { txPower0m_dBm: s8(d[1]), namespace: ns, instance: inst } };
  }
  if (type === 0x10 && d.length >= 3) {
    const schemes = ["http://www.", "https://www.", "http://", "https://"];
    const exp = [".com/", ".org/", ".edu/", ".net/", ".info/", ".biz/", ".gov/", ".com", ".org", ".edu", ".net", ".info", ".biz", ".gov"];
    let url = schemes[d[2]] ?? "";
    for (const c of d.slice(3)) url += c < exp.length ? exp[c] : c > 0x20 && c < 0x7f ? String.fromCharCode(c) : "";
    return { kind: "eddystone-url", label: `Eddystone-URL ${url}`, fields: { txPower0m_dBm: s8(d[1]), url } };
  }
  if (type === 0x20 && d.length >= 14 && d[1] === 0x00) {
    const f: Record<string, any> = {};
    const mv = u16be(d, 2);
    if (mv) f.battery_mV = mv;
    if (u16be(d, 4) !== 0x8000) f.temperature_C = round(s16be(d, 4) / 256, 2);
    f.advCount = u32be(d, 6);
    f.uptime_s = round(u32be(d, 10) / 10, 1);
    return { kind: "eddystone-tlm", label: `Eddystone-TLM ${sensorLabel(f)} up ${fmtDur(f.uptime_s)}`, fields: f };
  }
  if (type === 0x20 && d[1] === 0x01) return { kind: "eddystone-tlm", label: "Eddystone-TLM (encrypted)" };
  if (type === 0x30) return { kind: "eddystone-eid", label: `Eddystone-EID ${hex(d.slice(2, 10))}` };
  return null;
}

// BTHome v2 object table: id → [name, byte length, signed, factor]. Text/raw (0x53/0x54)
// are length-prefixed and handled inline. An unknown id ends parsing because its size
// isn't known.
const BTHOME: Record<number, [string, number, boolean, number]> = {
  0x00: ["packet_id", 1, false, 1],
  0x01: ["battery", 1, false, 1],
  0x02: ["temperature_C", 2, true, 0.01],
  0x03: ["humidity", 2, false, 0.01],
  0x04: ["pressure_hPa", 3, false, 0.01],
  0x05: ["illuminance_lux", 3, false, 0.01],
  0x06: ["mass_kg", 2, false, 0.01],
  0x07: ["mass_lb", 2, false, 0.01],
  0x08: ["dewpoint_C", 2, true, 0.01],
  0x09: ["count", 1, false, 1],
  0x0a: ["energy_kWh", 3, false, 0.001],
  0x0b: ["power_W", 3, false, 0.01],
  0x0c: ["voltage_V", 2, false, 0.001],
  0x0d: ["pm2_5_ugm3", 2, false, 1],
  0x0e: ["pm10_ugm3", 2, false, 1],
  0x0f: ["generic", 1, false, 1],
  0x10: ["power_on", 1, false, 1],
  0x11: ["opening", 1, false, 1],
  0x12: ["co2_ppm", 2, false, 1],
  0x13: ["tvoc_ugm3", 2, false, 1],
  0x14: ["moisture", 2, false, 0.01],
  0x15: ["battery_low", 1, false, 1],
  0x16: ["battery_charging", 1, false, 1],
  0x17: ["carbon_monoxide", 1, false, 1],
  0x18: ["cold", 1, false, 1],
  0x19: ["connectivity", 1, false, 1],
  0x1a: ["door", 1, false, 1],
  0x1b: ["garage_door", 1, false, 1],
  0x1c: ["gas", 1, false, 1],
  0x1d: ["heat", 1, false, 1],
  0x1e: ["light", 1, false, 1],
  0x1f: ["lock", 1, false, 1],
  0x20: ["moisture_detected", 1, false, 1],
  0x21: ["motion", 1, false, 1],
  0x22: ["moving", 1, false, 1],
  0x23: ["occupancy", 1, false, 1],
  0x24: ["plug", 1, false, 1],
  0x25: ["presence", 1, false, 1],
  0x26: ["problem", 1, false, 1],
  0x27: ["running", 1, false, 1],
  0x28: ["safety", 1, false, 1],
  0x29: ["smoke", 1, false, 1],
  0x2a: ["sound", 1, false, 1],
  0x2b: ["tamper", 1, false, 1],
  0x2c: ["vibration", 1, false, 1],
  0x2d: ["window", 1, false, 1],
  0x2e: ["humidity", 1, false, 1],
  0x2f: ["moisture", 1, false, 1],
  0x3a: ["button", 1, false, 1],
  0x3c: ["dimmer", 2, false, 1],
  0x3d: ["count", 2, false, 1],
  0x3e: ["count", 4, false, 1],
  0x3f: ["rotation_deg", 2, true, 0.1],
  0x40: ["distance_mm", 2, false, 1],
  0x41: ["distance_m", 2, false, 0.1],
  0x42: ["duration_s", 3, false, 0.001],
  0x43: ["current_A", 2, false, 0.001],
  0x44: ["speed_ms", 2, false, 0.01],
  0x45: ["temperature_C", 2, true, 0.1],
  0x46: ["uv_index", 1, false, 0.1],
  0x47: ["volume_L", 2, false, 0.1],
  0x48: ["volume_mL", 2, false, 1],
  0x49: ["flow_m3h", 2, false, 0.001],
  0x4a: ["voltage_V", 2, false, 0.1],
  0x4b: ["gas_m3", 3, false, 0.001],
  0x4c: ["gas_m3", 4, false, 0.001],
  0x4d: ["energy_kWh", 4, false, 0.001],
  0x4e: ["volume_L", 4, false, 0.001],
  0x4f: ["water_L", 4, false, 0.001],
  0x50: ["timestamp", 4, false, 1],
  0x51: ["acceleration_ms2", 2, false, 0.001],
  0x52: ["gyroscope_dps", 2, false, 0.001],
  0x55: ["volume_storage_L", 4, false, 0.001],
  0x56: ["conductivity_uScm", 2, false, 1],
  0x57: ["temperature_C", 1, true, 1],
  0x58: ["temperature_C", 1, true, 0.35],
  0x59: ["count", 1, true, 1],
  0x5a: ["count", 2, true, 1],
  0x5b: ["count", 4, true, 1],
  0x5c: ["power_W", 4, true, 0.01],
  0x5d: ["current_A", 2, true, 0.001],
  0x5e: ["direction_deg", 2, false, 0.01],
  0x5f: ["precipitation_mm", 2, false, 0.1],
  0x60: ["channel", 1, false, 1],
  0x61: ["rotational_speed_rpm", 2, false, 1],
  0xf0: ["device_type_id", 2, false, 1],
  0xf1: ["firmware_version", 4, false, 1],
  0xf2: ["firmware_version", 3, false, 1],
};
const BTHOME_BUTTON = ["none", "press", "double_press", "triple_press", "long_press", "long_double_press", "long_triple_press", "hold_press"];

export function decodeBTHome(uuid: string, d: Uint8Array): Beacon | null {
  if (uuid !== "FCD2" || !d.length) return null;
  const info = d[0];
  const version = info >> 5;
  if (version !== 2) return { kind: "bthome", label: `BTHome v${version} (unsupported version)` };
  if (info & 1) return { kind: "bthome", label: "BTHome v2 (encrypted)", fields: { encrypted: true } };
  const f: Record<string, any> = {};
  const put = (k: string, v: any) => {
    // Repeated objects (two temperatures, several buttons) get _2, _3… suffixes.
    let key = k;
    for (let i = 2; key in f; i++) key = `${k}_${i}`;
    f[key] = v;
  };
  let o = 1;
  while (o < d.length) {
    const id = d[o++];
    if (id === 0x53 || id === 0x54) {
      const len = d[o++] ?? 0;
      const v = d.slice(o, o + len);
      o += len;
      put(id === 0x53 ? "text" : "raw", id === 0x53 ? new TextDecoder().decode(v) : hex(v));
      continue;
    }
    const spec = BTHOME[id];
    if (!spec || o + spec[1] > d.length) {
      if (!spec) f.unparsed = hex(d.slice(o - 1));
      break;
    }
    const [name, len, signed, factor] = spec;
    let raw = 0;
    for (let i = len - 1; i >= 0; i--) raw = raw * 256 + d[o + i];
    if (signed && raw >= 2 ** (len * 8 - 1)) raw -= 2 ** (len * 8);
    o += len;
    if (id === 0x3a) put(name, BTHOME_BUTTON[raw] || raw);
    else if (id === 0x3c) put(name, { event: raw & 0xff, steps: raw >> 8 });
    else put(name, factor === 1 ? raw : round(raw * factor, 4));
  }
  if (info & 4) f.trigger_based = true;
  return { kind: "bthome", label: `BTHome ${sensorLabel(f) || Object.keys(f).join(", ")}`, fields: f };
}

/** Xiaomi MiBeacon (service data 0xFE95), unencrypted objects only. */
export function decodeMiBeacon(uuid: string, d: Uint8Array): Beacon | null {
  if (uuid !== "FE95" || d.length < 5) return null;
  const fc = u16le(d, 0);
  const pid = u16le(d, 2);
  const product = XIAOMI_PRODUCTS[pid] || `product 0x${pid.toString(16).padStart(4, "0")}`;
  const fields: Record<string, any> = { product, frameCounter: d[4] };
  if (fc & 0x0008) return { kind: "mibeacon", label: `Xiaomi ${product} (encrypted, needs bind key)`, fields: { ...fields, encrypted: true } };
  let o = 5;
  if (fc & 0x0010) {
    fields.mac = addrStr(d, o);
    o += 6;
  }
  if (fc & 0x0020) {
    const cap = d[o++];
    if ((cap & 0x18) === 0x18) o += 2; // capability carries an extra I/O capability word
  }
  if (fc & 0x0040) {
    while (o + 3 <= d.length) {
      const id = u16le(d, o);
      const len = d[o + 2];
      const v = d.slice(o + 3, o + 3 + len);
      o += 3 + len;
      if (v.length < len) break;
      if (id === 0x1004 && len >= 2) fields.temperature_C = round(s16le(v, 0) / 10, 1);
      else if (id === 0x1006 && len >= 2) fields.humidity = round(u16le(v, 0) / 10, 1);
      else if (id === 0x100a && len >= 1) fields.battery = v[0];
      else if (id === 0x100d && len >= 4) Object.assign(fields, { temperature_C: round(s16le(v, 0) / 10, 1), humidity: round(u16le(v, 2) / 10, 1) });
      else if (id === 0x1007 && len >= 3) fields.illuminance_lux = u24le(v, 0);
      else if (id === 0x1008 && len >= 1) fields.moisture = v[0];
      else if (id === 0x1009 && len >= 2) fields.conductivity_uScm = u16le(v, 0);
      else if (id === 0x4c01 && len >= 4) fields.temperature_C = round(dv(v).getFloat32(0, true), 2);
      else if (id === 0x4c02 && len >= 1) fields.humidity = v[0];
      else if (id === 0x4803 && len >= 1) fields.battery = v[0];
      else fields[`obj_${id.toString(16).padStart(4, "0")}`] = hex(v);
    }
  }
  return { kind: "mibeacon", label: `Xiaomi ${product} ${sensorLabel(fields)}`.trim(), fields };
}

/** ATC1441 and pvvx custom firmware for LYWSD03MMC (service data 0x181A), told apart by length. */
export function decodeAtc(uuid: string, d: Uint8Array): Beacon | null {
  if (uuid !== "181A") return null;
  if (d.length === 13) {
    // ATC1441: big-endian, MAC in display order.
    const f = { mac: hex(d.slice(0, 6), ":").toUpperCase(), temperature_C: round(s16be(d, 6) / 10, 1), humidity: d[8], battery: d[9], battery_mV: u16be(d, 10), frameCounter: d[12] };
    return { kind: "atc", label: `ATC thermometer ${sensorLabel(f)}`, fields: f };
  }
  if (d.length === 15 || d.length === 16) {
    // pvvx "custom": little-endian, MAC reversed.
    const f = { mac: addrStr(d, 0), temperature_C: round(s16le(d, 6) / 100, 2), humidity: round(u16le(d, 8) / 100, 2), battery_mV: u16le(d, 10), battery: d[12], frameCounter: d[13], flags: d[14] };
    return { kind: "pvvx", label: `pvvx thermometer ${sensorLabel(f)}`, fields: f };
  }
  return null;
}

/** Run every decoder over a parsed advertisement. */
export function decodeBeacons(ad: AdInfo): Beacon[] {
  const out: Beacon[] = [];
  for (const m of ad.mfr) {
    for (const fn of [decodeIBeacon, decodeApple, decodeMicrosoft, decodeRuuvi]) {
      const b = fn(m.company, m.data);
      if (b) out.push(b);
    }
    const g = decodeGovee(m.company, m.data, ad.name);
    if (g) out.push(g);
  }
  for (const s of ad.svc) {
    for (const fn of [decodeEddystone, decodeBTHome, decodeMiBeacon, decodeAtc]) {
      const b = fn(s.uuid, s.data);
      if (b) out.push(b);
    }
  }
  return out;
}

function sensorLabel(f: Record<string, any>): string {
  const p: string[] = [];
  if (f.temperature_C != null) p.push(`${f.temperature_C} °C`);
  if (f.humidity != null) p.push(`${f.humidity} %RH`);
  if (f.pressure_hPa != null) p.push(`${f.pressure_hPa} hPa`);
  if (f.battery != null) p.push(`batt ${f.battery} %`);
  else if (f.battery_mV != null) p.push(`batt ${f.battery_mV} mV`);
  return p.join(" · ");
}

function fmtDur(s: number): string {
  if (s < 120) return `${Math.round(s)} s`;
  if (s < 7200) return `${Math.round(s / 60)} min`;
  if (s < 172800) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86400)} d`;
}

// ---------- Per-advertiser aggregation ----------

export interface BleDevice {
  addr: string;
  addrType: string;
  controller: string;
  name: string;
  rssi: number | null;
  rssiHist: number[]; // one sample per RSSI_STEP_MS, newest last
  txPower: number | null;
  firstSeen: number;
  lastSeen: number;
  count: number;
  connectable: boolean;
  scannable: boolean;
  flags: number | null;
  appearance: number | null;
  uuids: { uuid: string; name: string }[];
  mfr: { company: number; companyName: string; data: string }[];
  svc: { uuid: string; name: string; data: string }[];
  beacons: Beacon[];
  lastPdu: string;
  adv: string; // latest raw advertising data (hex)
  scanRsp: string; // latest raw scan response (hex)
}

const RSSI_STEP_MS = 1000;
const RSSI_HIST = 60;

export class BleTracker {
  devices = new Map<string, BleDevice>();
  packets = 0;

  ingest(r: AdvReport, controller = `hci${r.index}`): BleDevice {
    this.packets++;
    let d = this.devices.get(r.addr);
    if (!d) {
      d = {
        addr: r.addr, addrType: addrKind(r.addr, r.addrType), controller, name: "", rssi: null, rssiHist: [], txPower: null,
        firstSeen: r.ts, lastSeen: r.ts, count: 0, connectable: false, scannable: false, flags: null, appearance: null,
        uuids: [], mfr: [], svc: [], beacons: [], lastPdu: "", adv: "", scanRsp: "",
      };
      this.devices.set(r.addr, d);
    }
    d.count++;
    d.controller = controller;
    d.lastPdu = r.pdu;
    if (r.connectable) d.connectable = true;
    if (r.scannable) d.scannable = true;
    if (r.rssi != null) {
      // One history slot per second: a phone can send 30 adverts/s, and a sparkline of raw
      // packets would only show the last second. Keep the strongest reading in each slot.
      const slotNew = !d.rssiHist.length || r.ts - d.lastSeen >= RSSI_STEP_MS || Math.floor(r.ts / RSSI_STEP_MS) !== Math.floor(d.lastSeen / RSSI_STEP_MS);
      if (slotNew) {
        d.rssiHist.push(r.rssi);
        if (d.rssiHist.length > RSSI_HIST) d.rssiHist.shift();
      } else d.rssiHist[d.rssiHist.length - 1] = Math.max(d.rssiHist[d.rssiHist.length - 1], r.rssi);
      d.rssi = r.rssi;
    }
    d.lastSeen = r.ts;
    if (r.scanRsp) d.scanRsp = hex(r.data);
    else d.adv = hex(r.data);

    const ad = parseAd(r.data);
    if (ad.name && (!d.name || ad.nameComplete || d.name.length < ad.name.length)) d.name = ad.name;
    if (ad.txPower != null) d.txPower = ad.txPower;
    else if (r.txPower != null) d.txPower = r.txPower;
    if (ad.flags != null) d.flags = ad.flags;
    if (ad.appearance != null) d.appearance = ad.appearance;
    for (const u of ad.uuids) if (!d.uuids.some((x) => x.uuid === u)) d.uuids.push({ uuid: u, name: uuidName(u) });
    for (const m of ad.mfr) {
      const e = { company: m.company, companyName: companyName(m.company), data: hex(m.data) };
      const i = d.mfr.findIndex((x) => x.company === m.company);
      if (i >= 0) d.mfr[i] = e;
      else d.mfr.push(e);
    }
    for (const s of ad.svc) {
      const e = { uuid: s.uuid, name: uuidName(s.uuid), data: hex(s.data) };
      const i = d.svc.findIndex((x) => x.uuid === s.uuid);
      if (i >= 0) d.svc[i] = e;
      else d.svc.push(e);
    }
    // Latest decode per beacon kind; an Apple device alternating Nearby Info and Find My
    // keeps both.
    for (const b of decodeBeacons(ad)) {
      const i = d.beacons.findIndex((x) => x.kind === b.kind);
      if (i >= 0) d.beacons[i] = b;
      else d.beacons.push(b);
    }
    return d;
  }

  /** Forget advertisers not heard from in `maxAgeMs`. */
  prune(now: number, maxAgeMs: number) {
    for (const [k, d] of this.devices) if (now - d.lastSeen > maxAgeMs) this.devices.delete(k);
  }

  list(opts: { since?: number; minRssi?: number; limit?: number } = {}): BleDevice[] {
    let l = [...this.devices.values()];
    if (opts.since) l = l.filter((d) => d.lastSeen >= opts.since!);
    if (opts.minRssi != null) l = l.filter((d) => d.rssi != null && d.rssi >= opts.minRssi!);
    l.sort((a, b) => (b.rssi ?? -999) - (a.rssi ?? -999));
    return opts.limit ? l.slice(0, opts.limit) : l;
  }
}

/** One-line description of what a device is, for lists. */
export function deviceKind(d: BleDevice): string {
  if (d.beacons.length) return d.beacons.map((b) => b.label).join(" | ");
  const parts: string[] = [];
  for (const m of d.mfr) parts.push(m.companyName || `company 0x${m.company.toString(16).padStart(4, "0")}`);
  for (const s of d.svc) parts.push(s.name || s.uuid);
  for (const u of d.uuids) if (u.name && !parts.includes(u.name)) parts.push(u.name);
  return parts.join(", ");
}
