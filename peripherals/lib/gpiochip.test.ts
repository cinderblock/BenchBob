import { describe, expect, test } from "bun:test";
import {
  CHIPINFO_SIZE, decodeChipInfo, decodeLineInfo, decodeValueBits, encodeLineInfoReq, encodeLineRequest, encodeValues, FLAG,
  GPIO_GET_CHIPINFO_IOCTL, GPIO_V2_GET_LINE_IOCTL, GPIO_V2_GET_LINEINFO_IOCTL, GPIO_V2_LINE_GET_VALUES_IOCTL, LINEINFO_SIZE, LINEREQ_SIZE, requestFd,
} from "./gpiochip.ts";

// Raw gpio_v2_line_info buffers from GPIO_V2_GET_LINEINFO_IOCTL on desky (Pi 4, kernel 6.18,
// 2026-10-19), trailing zeros trimmed. gpioinfo agreed with every one.
const DESKY: Record<string, string> = {
  "0:2": "4750494f3200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000050000",
  "0:4": "4750494f34000000000000000000000000000000000000000000000000000000" + "6f6e657769726540340000000000000000000000000000000000000000000000" + "040000000000000049",
  "0:17": "4750494f3137000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001100000000000000040000",
  "0:42": "5354415455535f4c45445f475f434c4b00000000000000000000000000000000" + "4143540000000000000000000000000000000000000000000000000000000000" + "2a0000000000000009",
};
const buf = (hex: string) => {
  const b = new Uint8Array(LINEINFO_SIZE);
  b.set(Buffer.from(hex, "hex"));
  return b;
};

describe("gpiochip v2 uAPI", () => {
  test("ioctl numbers match the kernel's (include/uapi/linux/gpio.h)", () => {
    expect(GPIO_GET_CHIPINFO_IOCTL).toBe(0x8044b401);
    expect(GPIO_V2_GET_LINEINFO_IOCTL).toBe(0xc100b405);
    expect(GPIO_V2_GET_LINE_IOCTL).toBe(0xc250b407);
    expect(GPIO_V2_LINE_GET_VALUES_IOCTL).toBe(0xc010b40e);
    expect([CHIPINFO_SIZE, LINEINFO_SIZE, LINEREQ_SIZE]).toEqual([68, 256, 592]);
  });

  test("chip info", () => {
    const b = new Uint8Array(CHIPINFO_SIZE);
    b.set(new TextEncoder().encode("gpiochip0"), 0);
    b.set(new TextEncoder().encode("pinctrl-bcm2711"), 32);
    new DataView(b.buffer).setUint32(64, 58, true);
    expect(decodeChipInfo(b)).toEqual({ name: "gpiochip0", label: "pinctrl-bcm2711", lines: 58 });
  });

  test("line info decodes desky's lines like gpioinfo", () => {
    expect(decodeLineInfo(buf(DESKY["0:17"]))).toMatchObject({ offset: 17, name: "GPIO17", consumer: null, used: false, direction: "input", flags: ["input"] });
    // Muxed to I²C1 with no consumer: "used", and shown as "kernel" like libgpiod does.
    expect(decodeLineInfo(buf(DESKY["0:2"]))).toMatchObject({ offset: 2, name: "GPIO2", consumer: "kernel", used: true, flags: ["used", "input"] });
    expect(decodeLineInfo(buf(DESKY["0:4"]))).toMatchObject({ offset: 4, consumer: "onewire@4", direction: "output", drive: "open-drain", flags: ["used", "output", "open-drain"] });
    expect(decodeLineInfo(buf(DESKY["0:42"]))).toMatchObject({ offset: 42, name: "STATUS_LED_G_CLK", consumer: "ACT", direction: "output" });
  });

  test("bias, active-low, edges and debounce", () => {
    const b = new Uint8Array(LINEINFO_SIZE);
    const v = new DataView(b.buffer);
    v.setUint32(64, 7, true);
    v.setUint32(68, 1, true); // one attribute
    v.setUint32(72, FLAG.INPUT | FLAG.ACTIVE_LOW | FLAG.BIAS_PULL_UP | FLAG.EDGE_RISING | FLAG.EDGE_FALLING, true);
    v.setUint32(80, 3, true); // GPIO_V2_LINE_ATTR_ID_DEBOUNCE
    v.setUint32(88, 5000, true);
    expect(decodeLineInfo(b)).toMatchObject({ offset: 7, bias: "pull-up", activeLow: true, edge: "both", debounceUs: 5000, flags: ["input", "active-low", "pull-up", "edges=both", "debounce=5000us"] });
  });

  test("line request layout: offsets, consumer, as-is flags, num_lines, fd", () => {
    const r = encodeLineRequest([17, 27], "benchbob");
    const v = new DataView(r.buffer);
    expect(r.length).toBe(592);
    expect([v.getUint32(0, true), v.getUint32(4, true)]).toEqual([17, 27]);
    expect(new TextDecoder().decode(r.subarray(256, 264))).toBe("benchbob");
    expect(v.getBigUint64(288, true)).toBe(0n); // as-is: no direction flag
    expect(v.getUint32(560, true)).toBe(2);
    expect(requestFd(r)).toBe(-1);
    v.setInt32(588, 23, true);
    expect(requestFd(r)).toBe(23);
    expect(() => encodeLineRequest([], "x")).toThrow();
    // A consumer longer than the field is cut, keeping the NUL.
    expect(encodeLineRequest([1], "x".repeat(40))[256 + 31]).toBe(0);
  });

  test("values", () => {
    const b = encodeValues(1n);
    expect(new DataView(b.buffer).getBigUint64(8, true)).toBe(1n);
    new DataView(b.buffer).setBigUint64(0, 1n, true);
    expect(decodeValueBits(b)).toBe(1n);
    expect(encodeLineInfoReq(42)[64]).toBe(42);
  });
});
