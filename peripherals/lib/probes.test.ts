import { describe, expect, test } from "bun:test";
import * as L from "./probes.ts";

// ---------- Fixtures ----------
//
// REAL = captured from openocd 0.12.0+dev-snapshot (2026-02-16) on desky with no probe attached.
// SYNTHETIC = written from OpenOCD's source/typical sessions; message texts match 0.12, but
// these weren't captured from hardware.

const REAL_CMSIS_NO_ADAPTER = `Open On-Chip Debugger 0.12.0+dev-snapshot (2026-02-16-16:07)
Licensed under GNU GPL v2
Warn : Transport "swd" was already selected
Info : [rp2040.core0] Hardware thread awareness created
Info : [rp2040.core1] Hardware thread awareness created
adapter speed: 4000 kHz
Error: unable to find a matching CMSIS-DAP device`;

const REAL_STLINK_NO_ADAPTER = `adapter speed: 4000 kHz
Error: open failed`;

const REAL_JLINK_BAD_SERIAL = `adapter speed: 4000 kHz
Error: Invalid serial number: ABC123`;

const REAL_JLINK_WRONG_TRANSPORT = `Warn : DEPRECATED: auto-selecting transport "jtag". Use 'transport select jtag' to suppress this message.
newtap: nrf52.cpu missing IR length`;

const REAL_DUMMY_JTAG = `Info : TAP chip.cpu does not have valid IDCODE (idcode=0x0)
Warn : Unexpected idcode after end of chain: 21 0xfffff800
Error: double-check your JTAG setup (interface, speed, ...)
Error: Trying to use configured scan chain anyway...
Error: chip.cpu: IR capture error; saw 0x0f not 0x01
Warn : Bypassing JTAG setup events due to errors
Error: Invalid ACK (7) in DAP response
Error: JTAG-DP STICKY ERROR`;

const REAL_DUMMY_READY = `Warn : gdb services need one or more targets defined
DESKYPI-READY
Info : Listening on port 16667 for tcl connections
Info : telnet server disabled`;

// SYNTHETIC: ST-Link V2 + STM32F4 Discovery, healthy (0.12 bracketed target names).
const SYN_STLINK_OK = `Info : Listening on port 6666 for tcl connections
Info : Listening on port 4444 for telnet connections
Info : clock speed 2000 kHz
Info : STLINK V2J37S7 (API v2) VID:PID 0483:3748
Info : Target voltage: 3.238278
Info : [stm32f4x.cpu] Cortex-M4 r0p1 processor detected
Info : [stm32f4x.cpu] target has 6 breakpoints, 4 watchpoints
Info : starting gdb server for stm32f4x.cpu on 3333
Info : Listening on port 3333 for gdb connections
Info : accepting 'gdb' connection on tcp/3333
Info : device id = 0x10076413
Info : flash size = 1024 KiB`;

// SYNTHETIC: ST-Link with the target unpowered / unwired.
const SYN_STLINK_NO_TARGET = `Info : STLINK V2J37S7 (API v2) VID:PID 0483:3748
Info : Target voltage: 0.003182
Error: target voltage may be too low for reliable debugging
Error: init mode failed (unable to connect to the target)`;

// SYNTHETIC: Raspberry Pi Debug Probe + RP2040, healthy (pre-0.12 "name:" form for the core line).
const SYN_CMSIS_OK = `Info : Using CMSIS-DAPv2 interface with VID:PID=0x2e8a:0x000c, serial=E6614C311B597D32
Info : CMSIS-DAP: SWD supported
Info : CMSIS-DAP: FW Version = 2.0.0
Info : CMSIS-DAP: Interface Initialised (SWD)
Info : SWD DPIDR 0x0bc12477, DLPIDR 0x00000001
Info : rp2040.core0: Cortex-M0+ r0p1 processor detected
Info : [rp2040.core1] Cortex-M0+ r0p1 processor detected
Info : RP2040 Flash Probe: 2097152 bytes @0x10000000, in 32 sectors`;

// SYNTHETIC: CMSIS-DAP with SWD lines not connected.
const SYN_CMSIS_NO_TARGET = `Info : Using CMSIS-DAPv2 interface with VID:PID=0x2e8a:0x000c, serial=E6614C311B597D32
Info : CMSIS-DAP: FW Version = 2.0.0
Error: Error connecting DP: cannot read IDR`;

function feed(log: string) {
  const st = L.newOcdState();
  for (const l of log.split("\n")) L.parseOcdLine(st, l);
  return st;
}

// ---------- Detection ----------

describe("probeType", () => {
  test("ST-Link variants", () => {
    expect(L.probeType("0483", "3748")?.name).toBe("ST-LINK/V2");
    expect(L.probeType("0483", "374B")?.name).toBe("ST-LINK/V2-1");
    expect(L.probeType("0483", "374e")?.kind).toBe("stlink");
    expect(L.probeType("0483", "df11")).toBeNull(); // DFU is a bootloader, not a probe
  });
  test("J-Link by vendor", () => {
    for (const pid of ["0101", "0105", "1015", "1020", "1050"]) expect(L.probeType("1366", pid)?.iface).toBe("interface/jlink.cfg");
  });
  test("CMSIS-DAP by id and by string", () => {
    expect(L.probeType("2e8a", "000c")?.name).toBe("Raspberry Pi Debug Probe");
    expect(L.probeType("0d28", "0204")?.kind).toBe("cmsis-dap");
    expect(L.probeType("c251", "f001", ["LPC-Link CMSIS-DAP"])?.kind).toBe("cmsis-dap");
    expect(L.probeType("c251", "f001", ["Keyboard"])).toBeNull();
  });
  test("others", () => {
    expect(L.probeType("0403", "6010")?.iface).toBe("interface/ftdi/esp32_devkitj_v1.cfg");
    expect(L.probeType("303a", "1001")?.transports).toEqual(["jtag"]);
    expect(L.probeType("1d50", "6018")?.gdbBuiltIn).toBe(true);
    expect(L.probeType("1a86", "8010")?.iface).toBeNull();
    expect(L.probeType("1a86", "8012")?.iface).toBe("interface/cmsis-dap.cfg");
    for (const pid of ["2141", "2111", "2145"]) expect(L.probeType("03eb", pid)?.kind).toBe("atmel");
  });
  test("bootloaders", () => {
    expect(L.bootType("2e8a", "0003")?.chip).toBe("rp2040");
    expect(L.bootType("2e8a", "000f")?.chip).toBe("rp2350");
    expect(L.bootType("0483", "df11")?.tool).toBe("dfu-util");
    expect(L.bootType("2e8a", "000c")).toBeNull();
  });
  test("udev strings from fixture props", () => {
    const props = { ID_MODEL: "Debugprobe_on_Pico__CMSIS-DAP_", ID_MODEL_ENC: "Debugprobe\\x20on\\x20Pico\\x20\\x28CMSIS-DAP\\x29" };
    expect(L.usbStrings(props)).toContain("Debugprobe on Pico (CMSIS-DAP)");
    expect(L.probeType("1234", "5678", L.usbStrings(props))?.kind).toBe("cmsis-dap");
  });
});

describe("ids and pinning", () => {
  test("stableId prefers serial", () => {
    expect(L.stableId({ ID_SERIAL: "STMicroelectronics_STM32_STLink_066DFF38", ID_PATH: "x" })).toBe("STMicroelectronics_STM32_STLink_066DFF38");
    expect(L.stableId({ ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3" })).toBe("platform-fd500000.pcie-pci-0000_01_00.0-usb-0_1.3");
  });
  test("usbLocation", () => {
    expect(L.usbLocation("/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0/usb1/1-1/1-1.3")).toBe("1-1.3");
    expect(L.usbLocation("/devices/virtual/foo")).toBeNull();
  });
  test("pinCommand", () => {
    const dp = "/devices/x/usb1/1-1/1-1.2";
    expect(L.pinCommand("cmsis-dap", "000c", "E6614C311B597D32", dp)).toBe("adapter serial E6614C311B597D32");
    // ST-Link/V2's serial is binary; udev's version is useless to OpenOCD.
    expect(L.pinCommand("stlink", "3748", "U_U_U_", dp)).toBe("adapter usb location 1-1.2");
    expect(L.pinCommand("jlink", "0101", "", dp)).toBe("adapter usb location 1-1.2");
    expect(L.pinCommand("jlink", "0101", "evil}{;", dp)).toBe("adapter usb location 1-1.2");
  });
});

// ---------- Command lines ----------

describe("openocdArgs", () => {
  test("server", () => {
    const a = L.openocdArgs({ iface: "interface/stlink.cfg", target: "target/stm32f4x.cfg", transport: "swd", speed: 4000, pin: "adapter serial 066D", bindto: "0.0.0.0", ports: { gdb: 3333, telnet: 4444, tcl: 6666 } });
    expect(a).toEqual([
      "-f", "interface/stlink.cfg",
      "-c", "transport select swd",
      "-f", "target/stm32f4x.cfg",
      "-c", "adapter speed 4000",
      "-c", "adapter serial 066D",
      "-c", "bindto 0.0.0.0",
      "-c", "gdb port 3333", "-c", "telnet port 4444", "-c", "tcl port 6666",
      "-c", "init", "-c", "echo DESKYPI-READY",
    ]);
    // Transport before the target script, speed after it.
    expect(a.indexOf("transport select swd")).toBeLessThan(a.indexOf("target/stm32f4x.cfg"));
    expect(a.indexOf("adapter speed 4000")).toBeGreaterThan(a.indexOf("target/stm32f4x.cfg"));
  });
  test("one-shot + generic target", () => {
    const a = L.openocdArgs({ iface: "interface/cmsis-dap.cfg", target: L.GENERIC_TARGET, transport: "swd", speed: 1000, bindto: "127.0.0.1", ports: { gdb: null, telnet: null, tcl: 40000 } });
    expect(a).toContain("swd newdap chip cpu -enable");
    expect(a).toContain("target create chip.cpu cortex_m -dap chip.dap");
    expect(a).toContain("gdb port disabled");
    expect(a).toContain("telnet port disabled");
    expect(a.filter((x) => x.startsWith("target/"))).toEqual([]);
  });
  test("no target", () => {
    const a = L.openocdArgs({ iface: "interface/dummy.cfg", target: L.NO_TARGET, transport: "jtag", speed: 1000, bindto: "127.0.0.1", ports: { gdb: null, telnet: null, tcl: 1 } });
    expect(a.filter((x) => x.startsWith("target"))).toEqual([]);
  });
  test("defaults", () => {
    const st = L.probeType("0483", "3748")!;
    const esp = L.probeType("303a", "1001")!;
    expect(L.defaultTransport(st, "target/stm32f1x.cfg")).toBe("swd");
    expect(L.defaultTransport(st, "target/esp32.cfg")).toBe("jtag");
    expect(L.defaultTransport(esp, "target/esp32s3.cfg")).toBe("jtag");
    expect(L.defaultSpeed(esp, "target/esp32s3.cfg")).toBe(40000);
    expect(L.gdbFor("target/esp32s3.cfg")).toBe("xtensa-esp32s3-elf-gdb");
    expect(L.gdbFor("target/esp32.cfg")).toBe("xtensa-esp32-elf-gdb");
    expect(L.gdbFor("target/esp32c3.cfg")).toBe("riscv32-esp-elf-gdb");
    expect(L.gdbFor("target/rp2040.cfg")).toBe("arm-none-eabi-gdb");
  });
  test("shellQuote", () => {
    expect(L.shellQuote("-c")).toBe("-c");
    expect(L.shellQuote("gdb port 3333")).toBe("'gdb port 3333'");
  });
});

// ---------- Log parsing ----------

describe("parseOcdLine", () => {
  test("REAL: no CMSIS-DAP adapter", () => {
    const st = feed(REAL_CMSIS_NO_ADAPTER);
    expect(st.errors).toEqual(["Error: unable to find a matching CMSIS-DAP device"]);
    expect(st.hint).toMatch(/couldn't open the probe/);
    expect(st.cpus).toEqual([]);
  });
  test("REAL: no ST-Link", () => expect(feed(REAL_STLINK_NO_ADAPTER).hint).toMatch(/couldn't open the probe/));
  test("REAL: J-Link bad serial", () => expect(feed(REAL_JLINK_BAD_SERIAL).hint).toMatch(/serial number/));
  test("REAL: wrong transport", () => expect(L.hintFor(REAL_JLINK_WRONG_TRANSPORT.split("\n")[1])).toMatch(/transport/));
  test("REAL: dummy JTAG chain", () => {
    const st = feed(REAL_DUMMY_JTAG);
    expect(st.errors.length).toBe(5);
    expect(st.hint).toMatch(/target doesn't answer|JTAG chain/);
  });
  test("REAL: ready ordering", () => {
    const st = feed(REAL_DUMMY_READY);
    expect(st.listening).toEqual({ tcl: 16667 });
  });
  test("SYNTHETIC: ST-Link healthy", () => {
    const st = feed(SYN_STLINK_OK);
    expect(st.adapter).toBe("STLINK V2J37S7 (API v2)");
    expect(st.voltage).toBe(3.24);
    expect(st.cpus).toEqual(["stm32f4x.cpu: Cortex-M4 r0p1"]);
    expect(st.listening).toEqual({ tcl: 6666, telnet: 4444, gdb: 3333 });
    expect(st.gdbClients).toBe(1);
    expect(st.flash).toBe("1024 KiB");
    expect(st.errors).toEqual([]);
    expect(st.hint).toBe("");
  });
  test("SYNTHETIC: ST-Link no target", () => {
    const st = feed(SYN_STLINK_NO_TARGET);
    expect(st.voltage).toBe(0);
    expect(st.errors.length).toBe(2);
    expect(st.hint).toMatch(/target doesn't answer/);
  });
  test("SYNTHETIC: CMSIS-DAP healthy", () => {
    const st = feed(SYN_CMSIS_OK);
    expect(st.adapter).toBe("CMSIS-DAPv2 FW 2.0.0");
    expect(st.dpidr).toBe("0x0bc12477");
    expect(st.cpus).toEqual(["rp2040.core0: Cortex-M0+ r0p1", "rp2040.core1: Cortex-M0+ r0p1"]);
    expect(st.flash).toBe("2097152 bytes");
  });
  test("SYNTHETIC: CMSIS-DAP no target", () => expect(feed(SYN_CMSIS_NO_TARGET).hint).toMatch(/SWDIO\/SWCLK/));
  test("hints", () => {
    expect(L.hintFor("Error: libusb_open() failed with LIBUSB_ERROR_ACCESS")).toMatch(/plugdev/);
    expect(L.hintFor("Error: Couldn't bind tcl to socket on port 6666: Address already in use")).toMatch(/ports/);
    expect(L.hintFor("Error: JTAG scan chain interrogation failed: all ones")).toMatch(/JTAG chain/);
    expect(L.hintFor("Error: [stm32f4x.cpu] clearing lockup after double fault")).toMatch(/fault/);
    expect(L.hintFor("Info : nothing")).toBe("");
  });
});

// ---------- TCL ----------

describe("tcl", () => {
  test("wrap/unwrap", () => {
    expect(L.tclWrap("targets")).toBe('if {[catch {capture {targets}} __r]} {set __r "@@ERR@@$__r"} else {set __r "@@OK@@$__r"}');
    expect(L.tclUnwrap("@@OK@@hello")).toBe("hello");
    expect(() => L.tclUnwrap('@@ERR@@invalid command name "foo"')).toThrow('invalid command name "foo"');
    expect(() => L.tclUnwrap("missing close-brace")).toThrow();
  });
  test("balanced", () => {
    expect(L.tclBalanced("program {/tmp/a b.elf} verify")).toBe(true);
    expect(L.tclBalanced("echo {")).toBe(false);
    expect(L.tclBalanced("echo }{")).toBe(false);
    expect(L.tclBalanced("echo \\{")).toBe(true);
  });
  test("TclClient against a fake OpenOCD TCL server", async () => {
    // Speaks the 0x1a-terminated protocol: evaluates our wrapper by recognising the inner command.
    const srv = Bun.listen({
      hostname: "127.0.0.1",
      port: 0,
      socket: {
        data(s, d) {
          for (const msg of new TextDecoder().decode(d).split("\x1a").filter(Boolean)) {
            const inner = /catch \{(?:capture \{)?(.*?)\}?\} __r/.exec(msg)?.[1] || "";
            const reply = inner === "version" ? "@@OK@@Open On-Chip Debugger 0.12.0" : inner.startsWith("read_memory") ? "@@OK@@0x41 0x42 0x0" : `@@ERR@@invalid command name "${inner}"`;
            s.write(reply + "\x1a");
          }
        },
      },
    });
    const c = new L.TclClient(srv.port);
    const [v, m] = await Promise.all([c.cmd("version"), c.cmd("read_memory 0x0 8 3", { capture: false })]);
    expect(v).toBe("Open On-Chip Debugger 0.12.0");
    expect(L.parseReadMemory(m)).toEqual([0x41, 0x42, 0]);
    await expect(c.cmd("bogus")).rejects.toThrow('invalid command name "bogus"');
    c.close();
    srv.stop(true);
  });
});

// ---------- Memory / files ----------

describe("memory + firmware", () => {
  test("parseNum", () => {
    expect(L.parseNum("0x08000000")).toBe(0x08000000);
    expect(L.parseNum("1024")).toBe(1024);
    expect(L.parseNum("0x2000_0000")).toBe(0x20000000);
    expect(L.parseNum("12abc")).toBeNull();
    expect(L.parseNum("")).toBeNull();
  });
  test("hexdump", () => {
    const d = L.hexdump([0x48, 0x69, 0, 0xff, ...Array(14).fill(0x41)], 0x20000000);
    expect(d.split("\n")[0]).toBe("20000000  48 69 00 ff 41 41 41 41 41 41 41 41 41 41 41 41  Hi..AAAAAAAAAAAA");
    expect(d.split("\n")[1]).toBe("20000010  41 41" + " ".repeat(42) + "  AA");
  });
  test("firmwareKind", () => {
    expect(L.firmwareKind(new Uint8Array([0x7f, 0x45, 0x4c, 0x46, 1]))).toBe("elf");
    expect(L.firmwareKind(new TextEncoder().encode(":020000040800F2\n"))).toBe("hex");
    expect(L.firmwareKind(new TextEncoder().encode("S00F0000"))).toBe("srec");
    expect(L.firmwareKind(new Uint8Array([0, 0x20, 0, 0x20]), "a.bin")).toBe("bin");
    expect(L.firmwareKind(makeUf2(1, 0xe48bff56))).toBe("uf2");
  });
});

function makeUf2(n: number, family: number | null, addr = 0x10000000): Uint8Array {
  const buf = new Uint8Array(512 * n);
  const dv = new DataView(buf.buffer);
  for (let i = 0; i < n; i++) {
    const o = i * 512;
    dv.setUint32(o, 0x0a324655, true);
    dv.setUint32(o + 4, 0x9e5d5157, true);
    dv.setUint32(o + 8, family === null ? 0 : 0x2000, true);
    dv.setUint32(o + 12, addr + i * 256, true);
    dv.setUint32(o + 16, 256, true);
    dv.setUint32(o + 20, i, true);
    dv.setUint32(o + 24, n, true);
    dv.setUint32(o + 28, family ?? 0, true);
    dv.setUint32(o + 508, 0x0ab16f30, true);
  }
  return buf;
}

describe("uf2", () => {
  test("parse", () => {
    const u = L.parseUf2(makeUf2(4, 0xe48bff56));
    expect(u).toEqual({ blocks: 4, familyIds: [0xe48bff56], families: ["RP2040"], start: 0x10000000, end: 0x10000400, payload: 1024 });
  });
  test("rejects non-UF2", () => {
    expect(() => L.parseUf2(new Uint8Array(100))).toThrow(/512-byte/);
    const bad = makeUf2(2, null);
    bad[512 + 508] = 0;
    expect(() => L.parseUf2(bad)).toThrow(/block 1/);
  });
  test("family acceptance", () => {
    expect(L.uf2Accepts("rp2040", [0xe48bff56])).toBe(true);
    expect(L.uf2Accepts("rp2040", [0xe48bff59])).toBe(false);
    expect(L.uf2Accepts("rp2350", [0xe48bff59, 0xe48bff58])).toBe(true);
    expect(L.uf2Accepts("rp2350", [])).toBe(true);
  });
  test("INFO_UF2.TXT", () => {
    // Real RP2040 bootrom content.
    expect(L.parseInfoUf2("UF2 Bootloader v3.0\r\nModel: Raspberry Pi RP2\r\nBoard-ID: RPI-RP2\r\n")).toEqual({ bootloader: "UF2 Bootloader v3.0", fields: { Model: "Raspberry Pi RP2", "Board-ID": "RPI-RP2" } });
  });
  test("dfu progress", () => {
    expect(L.parseDfuProgress("Erase   \t[=====     ]  45%\rDownload\t[==        ]  12%        8192 bytes")).toEqual({ phase: "Download", pct: 12 });
    expect(L.parseDfuProgress("Opening DFU capable USB device...")).toBeNull();
  });
});

describe("snippets", () => {
  test("clients", () => {
    const s = L.clientSnippets({ host: "deskypi.tsl", ports: { gdb: 3333, telnet: 4444, tcl: 6666 }, target: "target/stm32f4x.cfg", id: "abc", web: "http://deskypi.tsl" });
    expect(s.gdb).toBe('arm-none-eabi-gdb -ex "target extended-remote deskypi.tsl:3333" build/app.elf');
    expect(s.telnet).toBe("telnet deskypi.tsl 4444");
    expect(JSON.parse(s.cortexDebug).gdbTarget).toBe("deskypi.tsl:3333");
    expect(s.platformio).toContain("debug_port = deskypi.tsl:3333");
    expect(s.platformio).toContain("http://deskypi.tsl/api/probes/abc/flash");
  });
});
