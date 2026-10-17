import { test, expect } from "bun:test";
import { Ring, scoreSample, pickBaud, AUTOBAUD_RATES, FlashProgress, flashCommand, identify, stableId, resetsFor, resetSequence, payload, parseHex, stripAnsi } from "./serial.ts";

// ---------- UART model for auto-baud fixtures ----------
// Transmit 8N1 frames at txBaud and receive them the way a UART does at rxBaud: wait for a
// falling edge, sample each bit mid-cell, and turn a bad stop bit into a NUL (what Linux
// delivers for a framing error without PARMRK).
function uart(bytes: Uint8Array, txBaud: number, rxBaud: number, gapBits = 0): Uint8Array {
  const level: number[] = []; // one sample per 1/16 of a tx bit
  const os = 16;
  const push = (v: number, n = 1) => {
    for (let i = 0; i < n * os; i++) level.push(v);
  };
  push(1, 4);
  for (const b of bytes) {
    push(0);
    for (let i = 0; i < 8; i++) push((b >> i) & 1);
    push(1, 1 + gapBits);
  }
  push(1, 20);
  const at = (t: number) => level[Math.min(level.length - 1, Math.floor((t * txBaud * os)))] ?? 1;
  const out: number[] = [];
  const dt = 1 / (txBaud * os);
  const rxBit = 1 / rxBaud;
  let t = 0;
  const end = level.length * dt;
  while (t < end) {
    if (at(t) === 1) {
      t += dt;
      continue;
    }
    let v = 0;
    for (let i = 0; i < 8; i++) v |= at(t + rxBit * (1.5 + i)) << i;
    const stop = at(t + rxBit * 9.5);
    out.push(stop ? v : 0);
    t += rxBit * 9.5 + dt; // resync after the middle of the stop bit
  }
  return Uint8Array.from(out);
}

const BOOT = new TextEncoder().encode(
  "ets Jun  8 2016 00:22:57\r\n\r\nrst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)\r\nconfigsip: 0, SPIWP:0xee\r\nclk_drv:0x00,q_drv:0x00,d_drv:0x00\r\nmode:DIO, clock div:2\r\nload:0x3fff0030,len:1184\r\nI (29) boot: ESP-IDF v5.1 2nd stage bootloader\r\nI (29) boot: compile time 12:00:00\r\nHello from the device, uptime 12 s\r\n",
);

test("UART model is faithful at the right rate", () => {
  expect(new TextDecoder().decode(uart(BOOT, 115200, 115200))).toBe(new TextDecoder().decode(BOOT));
});

for (const tx of [9600, 74880, 115200, 921600]) {
  test(`auto-baud picks ${tx} from a sweep of resampled traffic`, () => {
    const samples = AUTOBAUD_RATES.map((rx) => ({ baud: rx, data: uart(BOOT, tx, rx) }));
    const r = pickBaud(samples);
    expect(r.best?.baud).toBe(tx);
    expect(r.best!.confidence).toBeGreaterThan(0.5);
  });
}

test("auto-baud with silence has no answer", () => {
  const r = pickBaud(AUTOBAUD_RATES.map((baud) => ({ baud, data: new Uint8Array(0) })));
  expect(r.best).toBeNull();
});

test("binary noise scores low", () => {
  const noise = Uint8Array.from({ length: 400 }, (_, i) => (i * 167 + 13) & 0xff);
  expect(scoreSample(noise)).toBeLessThan(0.3);
  expect(scoreSample(BOOT)).toBeGreaterThan(0.8);
});

// ---------- Ring ----------

test("ring keeps absolute cursors and drops the oldest", () => {
  const r = new Ring(100);
  for (let i = 0; i < 30; i++) r.push(new Uint8Array(10).fill(i), i * 100);
  expect(r.end).toBe(300);
  expect(r.size).toBeLessThanOrEqual(110);
  const s = r.since(0);
  expect(s.truncated).toBe(true);
  expect(s.from).toBe(r.start);
  expect(s.data[s.data.length - 1]).toBe(29);
  const t = r.since(295);
  expect([...t.data]).toEqual([29, 29, 29, 29, 29]);
  expect(r.since(300).data.length).toBe(0);
  expect(r.since(280, 4).to).toBe(284);
});

test("ring coalesces a trickle but not slow data", () => {
  const r = new Ring();
  for (let i = 0; i < 100; i++) r.push(Uint8Array.of(65 + (i % 26)), 1000 + i);
  expect(r.chunks.length).toBe(2); // 100 bytes over 100 ms: a chunk spans at most 50 ms so timestamps stay honest
  r.push(Uint8Array.of(1), 5000);
  expect(r.chunks.length).toBe(3);
  const tail = r.tail(3);
  expect(tail.map((c) => c.data.length).reduce((a, b) => a + b)).toBe(3);
  expect(tail[tail.length - 1].t).toBe(5000);
});

// ---------- Flash ----------

test("esptool commands", () => {
  expect(flashCommand("/dev/ttyUSB0", { tool: "esptool", action: "info" })).toEqual(["/usr/bin/esptool", "--chip", "auto", "--port", "/dev/ttyUSB0", "--baud", "115200", "flash_id"]);
  expect(flashCommand("/dev/ttyUSB0", { tool: "esptool", action: "write", chip: "esp32", files: [{ offset: 0x1000, path: "/t/b.bin", name: "b.bin" }, { offset: 0x10000, path: "/t/a.bin", name: "a.bin" }] }).slice(1)).toEqual(["--chip", "esp32", "--port", "/dev/ttyUSB0", "--baud", "460800", "write_flash", "-z", "0x01000", "/t/b.bin", "0x10000", "/t/a.bin"]);
  expect(flashCommand("/dev/ttyACM0", { tool: "esptool", action: "erase", baud: 921600 }).slice(-3)).toEqual(["--baud", "921600", "erase_flash"]);
  expect(() => flashCommand("/dev/x", { tool: "esptool", action: "info", chip: "z80" })).toThrow(/Unknown chip/);
  expect(() => flashCommand("/dev/x", { tool: "esptool", action: "write" })).toThrow(/No firmware/);
});

test("avrdude presets", () => {
  const f = [{ offset: 0, path: "/t/0-blink.hex", name: "blink.hex" }];
  expect(flashCommand("/dev/ttyUSB0", { tool: "avrdude", action: "write", preset: "uno", files: f })).toEqual(["/usr/bin/avrdude", "-p", "atmega328p", "-c", "arduino", "-P", "/dev/ttyUSB0", "-b", "115200", "-D", "-U", "flash:w:/t/0-blink.hex:i"]);
  expect(flashCommand("/dev/ttyUSB0", { tool: "avrdude", action: "write", preset: "nano-old", files: f }).slice(7, 9)).toEqual(["-b", "57600"]);
  const mega = flashCommand("/dev/ttyACM0", { tool: "avrdude", action: "write", preset: "mega", files: [{ offset: 0, path: "/t/x.bin", name: "x.bin" }] });
  expect(mega.slice(1, 5)).toEqual(["-p", "atmega2560", "-c", "wiring"]);
  expect(mega[mega.length - 1]).toBe("flash:w:/t/x.bin:r");
  expect(() => flashCommand("/dev/x", { tool: "avrdude", action: "write", preset: "due", files: f })).toThrow(/preset/);
});

const ESPTOOL_OK = `esptool.py v4.7.0
Serial port /dev/ttyUSB0
Connecting....
Chip is ESP32-D0WD-V3 (revision v3.1)
Features: WiFi, BT, Dual Core, 240MHz, VRef calibration in efuse, Coding Scheme None
Crystal is 40MHz
MAC: 24:6f:28:aa:bb:cc
Uploading stub...
Running stub...
Stub running...
Changing baud rate to 460800
Changed.
Configuring flash size...
Flash will be erased from 0x00001000 to 0x00007fff...
Flash will be erased from 0x00010000 to 0x000fffff...
Compressed 26640 bytes to 16691...
Writing at 0x00001000... (50 %)\rWriting at 0x000076a1... (100 %)
Wrote 26640 bytes (16691 compressed) at 0x00001000 in 0.8 seconds (effective 263.0 kbit/s)...
Hash of data verified.
Compressed 1000000 bytes to 500000...
Writing at 0x00010000... (3 %)
`;

test("esptool progress across two files", () => {
  const fp = new FlashProgress("esptool", 2);
  const lines = fp.feed(ESPTOOL_OK);
  expect(lines).toContain("Writing at 0x000076a1... (100 %)");
  expect(fp.info).toMatchObject({ chip: "ESP32-D0WD-V3 (revision v3.1)", mac: "24:6f:28:aa:bb:cc", crystal: "40MHz", verified: "yes" });
  expect(fp.phase).toBe("writing");
  expect(fp.progress).toBeCloseTo(51.5, 1);
  fp.feed("Writing at 0x000f0000... (100 %)\nWrote 1000000 bytes\nHash of data verified.\n\nLeaving...\nHard resetting via RTS pin...\n");
  expect(fp.progress).toBe(100);
  expect(fp.phase).toBe("resetting");
  expect(fp.error).toBe("");
});

test("esptool flash_id info", () => {
  const fp = new FlashProgress("esptool", 1, "info");
  fp.feed("Connecting...\nDetecting chip type... ESP32-C3\nChip is ESP32-C3 (QFN32) (revision v0.4)\nMAC: 58:cf:79:00:00:01\nManufacturer: 20\nDevice: 4016\nDetected flash size: 4MB\n");
  expect(fp.info).toMatchObject({ chipType: "ESP32-C3", flashManufacturer: "20", flashDevice: "4016", flashSize: "4MB" });
});

test("esptool errors (fixtures captured on desky)", () => {
  const a = new FlashProgress("esptool");
  a.feed("esptool.py v4.7.0\nSerial port /dev/ttyNOPE\n\nA fatal error occurred: Could not open /dev/ttyNOPE, the port is busy or doesn't exist.\n([Errno 2] could not open port /dev/ttyNOPE)\n");
  expect(a.error).toBe("Could not open /dev/ttyNOPE, the port is busy or doesn't exist.");
  const b = new FlashProgress("esptool");
  b.feed('Traceback (most recent call last):\n  File "/usr/bin/esptool", line 37, in <module>\nOSError: [Errno 25] Inappropriate ioctl for device\n');
  expect(b.error).toBe("OSError: [Errno 25] Inappropriate ioctl for device");
});

test("avrdude bar parsing, partial and complete", () => {
  const fp = new FlashProgress("avrdude");
  fp.feed("avrdude: AVR device initialized and ready to accept instructions\n\nReading | ################################################## | 100% 0.00 s\n\navrdude: Device signature = 0x1e950f (probably m328p)\n");
  expect(fp.info).toMatchObject({ signature: "0x1e950f", part: "m328p" });
  expect(fp.progress).toBe(5);
  fp.feed("avrdude: writing 924 bytes flash ...\n\nWriting | #########################");
  expect(fp.phase).toBe("writing");
  expect(fp.progress).toBeCloseTo(40, 0);
  fp.feed("######################### | 100% 0.15 s\n\navrdude: 924 bytes of flash written\n\nReading | ##########");
  expect(fp.phase).toBe("verifying");
  expect(fp.progress).toBeCloseTo(80, 0);
  fp.feed("######################################## | 100% 0.12 s\n\navrdude: 924 bytes of flash verified\n\navrdude done.  Thank you.\n");
  expect(fp.progress).toBe(100);
  expect(fp.error).toBe("");
});

test("avrdude errors (fixture captured on desky)", () => {
  const fp = new FlashProgress("avrdude");
  fp.feed('avrdude OS error: ioctl("TIOCMGET"): Inappropriate ioctl for device\navrdude error: programmer is not responding\navrdude warning: attempt 1 of 10: not in sync: resp=0x00\navrdude error: unable to open programmer arduino on port /dev/pts/1\n\navrdude done.  Thank you.\n');
  expect(fp.error).toBe("unable to open programmer arduino on port /dev/pts/1");
});

// ---------- Identification and helpers ----------

test("known devices", () => {
  expect(identify("303A", "1001")?.family).toBe("esp-usbjtag");
  expect(identify("10c4", "ea60")?.name).toBe("CP2102/CP2104");
  expect(identify("1a86", "55d4")?.name).toBe("CH9102");
  expect(identify("2341", "0043")?.name).toBe("Arduino Uno R3");
  expect(identify("2341", "beef")?.name).toBe("Arduino");
  expect(identify("2e8a", "000a")?.family).toBe("pico");
  expect(identify("0483", "5740")?.family).toBe("stm32");
  expect(identify("1366", "0105")?.name).toBe("SEGGER J-Link VCOM");
  expect(identify("dead", "beef")).toBeNull();
  expect(resetsFor("pico")).toEqual(["touch1200"]);
  expect(resetSequence("esp-boot", "esp-usbjtag")[0]).toEqual({ rts: false, dtr: false });
  expect(resetSequence("esp-boot", "bridge")).toEqual([{ dtr: false, rts: true }, { ms: 100 }, { dtr: true, rts: false }, { ms: 50 }, { dtr: false }]);
});

test("stable ids", () => {
  expect(stableId({ ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0" })).toBe("usb-0-1.3-1.0");
  expect(stableId({ ID_SERIAL: "Silicon_Labs_CP2102_0001", ID_USB_INTERFACE_NUM: "00" })).toBe("sn-Silicon-Labs-CP2102-0001-00");
  expect(stableId({ DEVNAME: "/dev/ttyACM0" })).toBe("ttyACM0");
});

test("payloads", () => {
  expect([...payload({ text: "AT", eol: "crlf" })]).toEqual([65, 84, 13, 10]);
  expect([...payload({ hex: "0xde ad:BE,ef" })]).toEqual([0xde, 0xad, 0xbe, 0xef]);
  expect([...payload({ base64: "AQI=" })]).toEqual([1, 2]);
  expect(() => parseHex("abc")).toThrow(/pairs/);
  expect(() => payload({})).toThrow(/text, hex or base64/);
  expect(() => payload({ text: "x", eol: "nl" })).toThrow(/eol/);
  expect(stripAnsi("\x1b[1;32mI (29) boot\x1b[0m\x1b]0;title\x07 ok")).toBe("I (29) boot ok");
});
