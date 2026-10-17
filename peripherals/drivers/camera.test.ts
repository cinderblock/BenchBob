import { expect, test } from "bun:test";
import { capsCanCapture, cameraKind, cameraKey, chooseStream, isUsbCaptureNode, isUserControl, parseControls, parseDeviceCaps, parseFormats, parseLibcameraList } from "../lib/camera.ts";

// Real udev properties from desky (Pi 4, kernel 6.18): codec, ISP capture output, HEVC.
const bcmCodec = {
  DEVPATH: "/devices/platform/soc/fe00b840.mailbox/bcm2835-codec/video4linux/video10",
  DEVNAME: "/dev/video10", SUBSYSTEM: "video4linux", ID_V4L_PRODUCT: "bcm2835-codec-decode", ID_V4L_CAPABILITIES: ":", ID_PATH: "platform-fe00b840.mailbox",
};
const bcmIsp = {
  DEVPATH: "/devices/platform/soc/fe00b840.mailbox/bcm2835-isp/video4linux/video14",
  DEVNAME: "/dev/video14", SUBSYSTEM: "video4linux", ID_V4L_PRODUCT: "bcm2835-isp", ID_V4L_CAPABILITIES: ":capture:", ID_PATH: "platform-fe00b840.mailbox",
};
const hevc = {
  DEVPATH: "/devices/platform/scb/feb00000.codec/video4linux/video19",
  DEVNAME: "/dev/video19", SUBSYSTEM: "video4linux", ID_V4L_PRODUCT: "rpi-hevc-dec", ID_V4L_CAPABILITIES: ":", ID_PATH: "platform-feb00000.codec",
};
// Representative UVC webcam: capture node + its metadata twin (systemd ≥ 247 marks the twin ":").
const usbDev = { DEVPATH: "/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0/usb1/1-1/1-1.3", SUBSYSTEM: "usb", DEVTYPE: "usb_device", ID_VENDOR_ID: "046d", ID_MODEL_ID: "082d" };
const uvc = {
  DEVPATH: usbDev.DEVPATH + "/1-1.3:1.0/video4linux/video0", DEVNAME: "/dev/video0", SUBSYSTEM: "video4linux",
  ID_V4L_PRODUCT: "HD Pro Webcam C920", ID_V4L_CAPABILITIES: ":capture:", ID_VENDOR_ID: "046d", ID_MODEL_ID: "082d", ID_SERIAL_SHORT: "A1B2C3D4",
  ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0",
};
const uvcMeta = { ...uvc, DEVPATH: usbDev.DEVPATH + "/1-1.3:1.0/video4linux/video1", DEVNAME: "/dev/video1", ID_V4L_CAPABILITIES: ":" };

test("detection keeps USB capture nodes and drops Pi codec/ISP/metadata nodes", () => {
  expect(isUsbCaptureNode(bcmCodec, null)).toBe(false);
  expect(isUsbCaptureNode(bcmIsp, null)).toBe(false); // ":capture:" but no USB parent
  expect(isUsbCaptureNode(hevc, null)).toBe(false);
  expect(isUsbCaptureNode(uvc, usbDev)).toBe(true);
  expect(isUsbCaptureNode(uvcMeta, usbDev)).toBe(false);
  expect(isUsbCaptureNode({ ...uvc, DEVNAME: "/dev/v4l-subdev0" }, usbDev)).toBe(false);
});

// Real `v4l2-ctl -d /dev/video14 -D` from desky.
const capsIsp = `Driver Info:
\tDriver name      : bcm2835-isp
\tCard type        : bcm2835-isp
\tBus info         : platform:bcm2835-isp
\tDriver version   : 6.18.50
\tCapabilities     : 0x84200001
\t\tVideo Capture
\t\tStreaming
\t\tExtended Pix Format
\t\tDevice Capabilities
\tDevice Caps      : 0x04200001
\t\tVideo Capture
\t\tStreaming
\t\tExtended Pix Format
Media Driver Info:
\tDriver name      : bcm2835-isp
`;
// A UVC metadata node: the device as a whole can capture, this node can't.
const capsMeta = `Driver Info:
\tDriver name      : uvcvideo
\tCard type        : HD Pro Webcam C920
\tBus info         : usb-0000:01:00.0-1.3
\tDriver version   : 6.18.50
\tCapabilities     : 0x84a00001
\t\tVideo Capture
\t\tMetadata Capture
\t\tStreaming
\t\tExtended Pix Format
\t\tDevice Capabilities
\tDevice Caps      : 0x04a00000
\t\tMetadata Capture
\t\tStreaming
\t\tExtended Pix Format
`;

test("Device Caps decide capture vs metadata", () => {
  const a = parseDeviceCaps(capsIsp);
  expect(a.driver).toBe("bcm2835-isp");
  expect(a.caps).toEqual(["Video Capture", "Streaming", "Extended Pix Format"]);
  expect(capsCanCapture(a.caps)).toBe(true);
  const b = parseDeviceCaps(capsMeta);
  expect(b.card).toBe("HD Pro Webcam C920");
  expect(capsCanCapture(b.caps)).toBe(false);
});

test("HDMI capture sticks are labelled", () => {
  expect(cameraKind("534d", "2109", "USB Video")).toBe("hdmi");
  expect(cameraKind("345f", "2130", "UGREEN")).toBe("hdmi");
  expect(cameraKind("046d", "082d", "HD Pro Webcam C920")).toBe("webcam");
  expect(cameraKind("1234", "5678", "USB3.0 Video Capture")).toBe("hdmi");
});

test("settings key prefers serial, falls back to port path", () => {
  expect(cameraKey(uvc)).toBe("usb:046d:082d:A1B2C3D4");
  expect(cameraKey({ ...uvc, ID_SERIAL_SHORT: "" })).toBe("path:platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0");
});

// v4l2-ctl 1.30 --list-formats-ext for an MS2109 stick.
const formatsMs2109 = `ioctl: VIDIOC_ENUM_FMT
\tType: Video Capture

\t[0]: 'MJPG' (Motion-JPEG, compressed)
\t\tSize: Discrete 1920x1080
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\t\tInterval: Discrete 0.040s (25.000 fps)
\t\tSize: Discrete 1280x720
\t\t\tInterval: Discrete 0.017s (60.000 fps)
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\tSize: Discrete 640x480
\t\t\tInterval: Discrete 0.017s (60.000 fps)
\t[1]: 'YUYV' (YUYV 4:2:2)
\t\tSize: Discrete 1920x1080
\t\t\tInterval: Discrete 0.200s (5.000 fps)
\t\tSize: Discrete 1280x720
\t\t\tInterval: Discrete 0.100s (10.000 fps)
\t\tSize: Discrete 640x480
\t\t\tInterval: Discrete 0.033s (30.000 fps)
`;
// Real desky output head (ISP node): stepwise sizes.
const formatsIsp = `ioctl: VIDIOC_ENUM_FMT
\tType: Video Capture

\t[0]: 'YUYV' (YUYV 4:2:2)
\t\tSize: Stepwise 64x64 - 16384x16384 with step 2/2
\t[1]: 'RGB3' (24-bit RGB 8-8-8)
\t\tSize: Stepwise 64x64 - 16384x16384 with step 1/1
`;

test("--list-formats-ext parsing", () => {
  const f = parseFormats(formatsMs2109);
  expect(f.map((x) => x.fourcc)).toEqual(["MJPG", "YUYV"]);
  expect(f[0].compressed).toBe(true);
  expect(f[0].sizes[0]).toEqual({ w: 1920, h: 1080, fps: [30, 25] });
  expect(f[0].sizes[1].fps).toEqual([60, 30]);
  expect(f[1].sizes[2]).toEqual({ w: 640, h: 480, fps: [30] });
  const s = parseFormats(formatsIsp);
  expect(s[0].stepwise).toEqual({ minW: 64, minH: 64, maxW: 16384, maxH: 16384, stepW: 2, stepH: 2 });
  expect(s[1].fourcc).toBe("RGB3");
});

test("stream choice prefers MJPEG and honours saved settings", () => {
  const f = parseFormats(formatsMs2109);
  expect(chooseStream(f)).toEqual({ fourcc: "MJPG", w: 1920, h: 1080, fps: 30 });
  expect(chooseStream(f, { fourcc: "MJPG", w: 1280, h: 720, fps: 60 })).toEqual({ fourcc: "MJPG", w: 1280, h: 720, fps: 60 });
  expect(chooseStream(f, { fourcc: "YUYV", w: 640, h: 480, fps: 99 })).toEqual({ fourcc: "YUYV", w: 640, h: 480, fps: 30 });
  // A saved size the camera no longer offers falls back to the default pick.
  expect(chooseStream(f, { fourcc: "MJPG", w: 800, h: 600, fps: 30 })?.w).toBe(1920);
  // Uncompressed-only camera: capped at 720p because the Pi has to encode.
  expect(chooseStream([f[1]])).toEqual({ fourcc: "YUYV", w: 1280, h: 720, fps: 10 });
  expect(chooseStream(parseFormats(formatsIsp))).toEqual({ fourcc: "YUYV", w: 1280, h: 720, fps: 30 });
  expect(chooseStream([])).toBeNull();
});

// Real desky output (codec node) followed by typical UVC controls.
const ctrls = `
User Controls

                     brightness 0x00980900 (int)    : min=0 max=255 step=1 default=128 value=140
                       contrast 0x00980901 (int)    : min=0 max=255 step=1 default=128 value=128
white_balance_automatic 0x0098090c (bool)   : default=1 value=1
           power_line_frequency 0x00980918 (menu)   : min=0 max=2 default=2 value=2 (60 Hz)
\t\t\t\t0: Disabled
\t\t\t\t1: 50 Hz
\t\t\t\t2: 60 Hz
      white_balance_temperature 0x0098091a (int)    : min=2000 max=6500 step=1 default=4000 value=4000 flags=inactive
  min_number_of_capture_buffers 0x00980927 (int)    : min=1 max=1 step=1 default=1 value=1 flags=read-only, has-min-max

Camera Controls

                  auto_exposure 0x009a0901 (menu)   : min=0 max=3 default=3 value=3 (Aperture Priority Mode)
\t\t\t\t1: Manual Mode
\t\t\t\t3: Aperture Priority Mode
         exposure_time_absolute 0x009a0902 (int)    : min=3 max=2047 step=1 default=250 value=250 flags=inactive
                 focus_absolute 0x009a090a (int)    : min=0 max=250 step=5 default=0 value=0 flags=inactive

Codec Controls

                     h264_level 0x00990a67 (menu)   : min=0 max=15 default=11 value=11 (4) flags=read-only, has-min-max
\t\t\t\t0: 1
\t\t\t\t11: 4
`;

test("--list-ctrls-menus parsing", () => {
  const c = parseControls(ctrls);
  const by = Object.fromEntries(c.map((x) => [x.name, x]));
  expect(by.brightness).toMatchObject({ type: "int", min: 0, max: 255, step: 1, default: 128, value: 140, group: "User Controls", flags: [] });
  expect(by.white_balance_automatic).toMatchObject({ type: "bool", default: 1, value: 1 });
  expect(by.power_line_frequency.menu).toEqual([{ value: 0, label: "Disabled" }, { value: 1, label: "50 Hz" }, { value: 2, label: "60 Hz" }]);
  expect(by.white_balance_temperature.flags).toEqual(["inactive"]);
  expect(by.auto_exposure).toMatchObject({ group: "Camera Controls", value: 3 });
  expect(by.auto_exposure.menu?.length).toBe(2);
  expect(by.min_number_of_capture_buffers.flags).toEqual(["read-only", "has-min-max"]);
  expect(by.h264_level.group).toBe("Codec Controls");
  const shown = c.filter(isUserControl).map((x) => x.name);
  expect(shown).toEqual(["brightness", "contrast", "white_balance_automatic", "power_line_frequency", "white_balance_temperature", "auto_exposure", "exposure_time_absolute", "focus_absolute"]);
});

test("libcamera listing", () => {
  expect(parseLibcameraList("No cameras available!")).toEqual([]);
  const t = `Available cameras
-----------------
0 : imx708 [4608x2592 10-bit RGGB] (/base/soc/i2c0mux/i2c@1/imx708@1a)
    Modes: 'SRGGB10_CSI2P' : 1536x864 [120.13 fps - (768, 432)/3072x1728 crop]
`;
  expect(parseLibcameraList(t)).toEqual([{ index: 0, sensor: "imx708", detail: "4608x2592 10-bit RGGB /base/soc/i2c0mux/i2c@1/imx708@1a" }]);
});
