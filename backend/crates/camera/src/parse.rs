//! Pure helpers (`peripherals/lib/camera.ts`): which V4L2 nodes are cameras, and parsers for
//! v4l2-ctl's text output.

use std::sync::OnceLock;

use benchbob_core::http::num;
use benchbob_core::udev::Props;
use benchbob_core::util::{js_round, prop};
use regex::Regex;
use serde_json::{Map, Value, json};

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

// ---------- detection ----------

/// A node is a camera worth listing when udev says it can capture and it hangs off USB. The
/// USB requirement keeps out the Pi's bcm2835-isp outputs (memory-to-memory stages udev also
/// marks `:capture:`). CSI cameras are found separately via libcamera.
pub fn is_usb_capture_node(p: &Props, usb_parent: Option<&Props>) -> bool {
    static DEV: OnceLock<Regex> = OnceLock::new();
    if prop(p, "SUBSYSTEM") != "video4linux" || !re(&DEV, r"^/dev/video\d+$").is_match(prop(p, "DEVNAME")) {
        return false;
    }
    if !prop(p, "ID_V4L_CAPABILITIES").contains(":capture:") || usb_parent.is_none() {
        return false;
    }
    // Belt and braces: the codec/ISP drivers never sit under USB.
    let prod = prop(p, "ID_V4L_PRODUCT");
    !(prod.starts_with("bcm2835") || prod.starts_with("rpi-"))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceCaps {
    pub driver: String,
    pub card: String,
    pub bus: String,
    pub caps: Vec<String>,
}

/// Device Caps from `v4l2-ctl -D`: the per-node capabilities (a UVC metadata node can't capture).
pub fn parse_device_caps(text: &str) -> DeviceCaps {
    static CAPS: OnceLock<Regex> = OnceLock::new();
    let field = |k: &str| {
        Regex::new(&format!(r"(?m)^\s*{}\s*:\s*(.*)$", regex::escape(k)))
            .unwrap()
            .captures(text)
            .map(|m| m[1].trim().to_string())
            .unwrap_or_default()
    };
    let mut caps = Vec::new();
    if let Some(m) = re(&CAPS, r"(?i)Device Caps\s*:\s*0x[0-9a-f]+\n((?:\t\t.*\n?)*)").captures(text) {
        caps = m[1].split('\n').map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    }
    DeviceCaps { driver: field("Driver name"), card: field("Card type"), bus: field("Bus info"), caps }
}

pub fn caps_can_capture(caps: &[String]) -> bool {
    caps.iter().any(|c| c == "Video Capture" || c == "Video Capture Multiplanar")
}

/// USB IDs of the cheap MS2109/MS2130-class HDMI-to-USB capture sticks.
const HDMI_CAPTURE_IDS: [&str; 6] = ["534d:2109", "534d:2130", "345f:2130", "345f:2131", "1d6c:0103", "eba4:7588"];

pub fn camera_kind(vid: &str, pid: &str, model: &str) -> &'static str {
    static HDMI: OnceLock<Regex> = OnceLock::new();
    if HDMI_CAPTURE_IDS.contains(&format!("{vid}:{pid}").to_lowercase().as_str()) {
        return "hdmi";
    }
    // Rebadged sticks keep the chip but change the strings; the strings usually still say so.
    if re(&HDMI, r"(?i)hdmi|capture|usb3\.0 video|cam link|ms210|ms213").is_match(model) {
        return "hdmi";
    }
    "webcam"
}

/// Settings key that survives replugging into the same port, and moving ports when there's a serial.
pub fn camera_key(p: &Props) -> String {
    if !prop(p, "ID_SERIAL_SHORT").is_empty() && !prop(p, "ID_VENDOR_ID").is_empty() {
        return format!("usb:{}:{}:{}", prop(p, "ID_VENDOR_ID"), prop(p, "ID_MODEL_ID"), prop(p, "ID_SERIAL_SHORT"));
    }
    format!("path:{}", if prop(p, "ID_PATH").is_empty() { prop(p, "DEVPATH") } else { prop(p, "ID_PATH") })
}

// ---------- --list-formats-ext ----------

#[derive(Debug, Clone, PartialEq)]
pub struct FrameSize {
    pub w: u32,
    pub h: u32,
    /// Discrete rates, highest first; empty when the driver reports none.
    pub fps: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stepwise {
    pub min_w: u32,
    pub min_h: u32,
    pub max_w: u32,
    pub max_h: u32,
    pub step_w: u32,
    pub step_h: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// "MJPG", "YUYV"
    pub fourcc: String,
    pub description: String,
    pub compressed: bool,
    pub sizes: Vec<FrameSize>,
    pub stepwise: Option<Stepwise>,
}

impl Format {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("fourcc".into(), self.fourcc.clone().into());
        m.insert("description".into(), self.description.clone().into());
        m.insert("compressed".into(), self.compressed.into());
        m.insert(
            "sizes".into(),
            self.sizes.iter().map(|s| json!({ "w": s.w, "h": s.h, "fps": s.fps.iter().map(|f| num(*f)).collect::<Vec<_>>() })).collect(),
        );
        if let Some(s) = &self.stepwise {
            m.insert(
                "stepwise".into(),
                json!({ "minW": s.min_w, "minH": s.min_h, "maxW": s.max_w, "maxH": s.max_h, "stepW": s.step_w, "stepH": s.step_h }),
            );
        }
        Value::Object(m)
    }
}

pub fn parse_formats(text: &str) -> Vec<Format> {
    static FMT: OnceLock<Regex> = OnceLock::new();
    static DISC: OnceLock<Regex> = OnceLock::new();
    static STEP: OnceLock<Regex> = OnceLock::new();
    static IVAL: OnceLock<Regex> = OnceLock::new();
    let mut out: Vec<Format> = Vec::new();
    // Whether the last line opened a size that intervals belong to.
    let mut in_size = false;
    for line in text.split('\n') {
        if let Some(m) = re(&FMT, r"^\s*\[\d+\]: '(.{1,4})' \((.*)\)\s*$").captures(line) {
            out.push(Format {
                fourcc: m[1].trim().into(),
                description: m[2].into(),
                compressed: m[2].to_lowercase().contains("compressed"),
                sizes: Vec::new(),
                stepwise: None,
            });
            in_size = false;
        } else if let (Some(f), Some(m)) = (out.last_mut(), re(&DISC, r"^\s*Size: Discrete (\d+)x(\d+)").captures(line)) {
            f.sizes.push(FrameSize { w: m[1].parse().unwrap_or(0), h: m[2].parse().unwrap_or(0), fps: Vec::new() });
            in_size = true;
        } else if let (Some(f), Some(m)) = (
            out.last_mut(),
            re(&STEP, r"^\s*Size: (?:Stepwise|Continuous) (\d+)x(\d+) - (\d+)x(\d+)(?: with step (\d+)/(\d+))?").captures(line),
        ) {
            let n = |i: usize| m.get(i).and_then(|g| g.as_str().parse().ok()).unwrap_or(1);
            f.stepwise = Some(Stepwise { min_w: n(1), min_h: n(2), max_w: n(3), max_h: n(4), step_w: n(5), step_h: n(6) });
            in_size = false;
        } else if in_size && let Some(m) = re(&IVAL, r"^\s*Interval: Discrete [\d.]+s \(([\d.]+) fps\)").captures(line) {
            let fps = js_round(m[1].parse::<f64>().unwrap_or(f64::NAN) * 1000.0) / 1000.0;
            if let Some(s) = out.last_mut().and_then(|f| f.sizes.last_mut())
                && !s.fps.contains(&fps)
            {
                s.fps.push(fps);
            }
        }
    }
    for f in &mut out {
        for s in &mut f.sizes {
            s.fps.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        }
        f.sizes.sort_by_key(|s| std::cmp::Reverse(s.w as u64 * s.h as u64));
    }
    out
}

/// v4l2 fourcc → ustreamer --format name; formats ustreamer can't ingest map to None.
pub fn ustreamer_format(fourcc: &str) -> Option<&'static str> {
    Some(match fourcc {
        "MJPG" => "MJPEG",
        "JPEG" => "JPEG",
        "YUYV" => "YUYV",
        "UYVY" => "UYVY",
        "RGBP" => "RGB565",
        "RGB3" => "RGB24",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamChoice {
    pub fourcc: String,
    pub w: f64,
    pub h: f64,
    pub fps: f64,
}

impl StreamChoice {
    pub fn to_json(&self) -> Value {
        json!({ "fourcc": self.fourcc, "w": num(self.w), "h": num(self.h), "fps": num(self.fps) })
    }
}

/// A saved or requested choice, with JavaScript's loose fields (NaN when missing).
#[derive(Debug, Clone)]
pub struct Want {
    pub fourcc: Value,
    pub w: f64,
    pub h: f64,
    pub fps: f64,
}

impl Want {
    pub fn from_json(v: &Value) -> Self {
        let n = |k: &str| match v.get(k) {
            Some(Value::Number(x)) => x.as_f64().unwrap_or(f64::NAN),
            Some(x) => benchbob_core::http::js_number(x),
            None => f64::NAN,
        };
        Self { fourcc: v.get("fourcc").cloned().unwrap_or(Value::Null), w: n("w"), h: n("h"), fps: n("fps") }
    }
}

fn truthy_num(x: f64) -> bool {
    x != 0.0 && !x.is_nan()
}

/// What to stream: the saved choice if the camera still offers it, else MJPEG at the largest
/// size up to 1080p, else an uncompressed format ustreamer can encode, kept to 720p.
pub fn choose_stream(formats: &[Format], saved: Option<&Want>) -> Option<StreamChoice> {
    let usable: Vec<&Format> =
        formats.iter().filter(|f| ustreamer_format(&f.fourcc).is_some() && (!f.sizes.is_empty() || f.stepwise.is_some())).collect();
    if usable.is_empty() {
        return None;
    }
    if let Some(s) = saved
        && benchbob_core::http::truthy(&s.fourcc)
        && truthy_num(s.w)
        && truthy_num(s.h)
    {
        let fc = s.fourcc.as_str().unwrap_or("");
        let f = usable.iter().find(|f| f.fourcc == fc);
        if let Some(f) = f
            && let Some(sz) = f.sizes.iter().find(|z| z.w as f64 == s.w && z.h as f64 == s.h)
        {
            let fps = if sz.fps.contains(&s.fps) { s.fps } else { sz.fps.first().copied().unwrap_or(30.0) };
            return Some(StreamChoice { fourcc: f.fourcc.clone(), w: sz.w as f64, h: sz.h as f64, fps });
        }
        if let Some(f) = f
            && let Some(st) = &f.stepwise
            && s.w <= st.max_w as f64
            && s.h <= st.max_h as f64
        {
            return Some(StreamChoice { fourcc: f.fourcc.clone(), w: s.w, h: s.h, fps: if truthy_num(s.fps) { s.fps } else { 30.0 } });
        }
    }
    let f = usable.iter().find(|f| f.fourcc == "MJPG").or_else(|| usable.iter().find(|f| f.fourcc == "JPEG")).unwrap_or(&usable[0]);
    let cap = if f.compressed { 1920 * 1080 } else { 1280 * 720 };
    if f.sizes.is_empty()
        && let Some(st) = &f.stepwise
    {
        let w = st.max_w.min(if f.compressed { 1920 } else { 1280 });
        let h = st.max_h.min(if f.compressed { 1080 } else { 720 });
        return Some(StreamChoice { fourcc: f.fourcc.clone(), w: w as f64, h: h as f64, fps: 30.0 });
    }
    let s = f.sizes.iter().find(|s| s.w as u64 * s.h as u64 <= cap).unwrap_or_else(|| f.sizes.last().unwrap());
    Some(StreamChoice { fourcc: f.fourcc.clone(), w: s.w as f64, h: s.h as f64, fps: s.fps.first().copied().unwrap_or(30.0) })
}

// ---------- --list-ctrls-menus ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    /// v4l2-ctl name: "brightness", "auto_exposure"
    pub name: String,
    pub id: String,
    /// int, bool, menu, intmenu, button, int64, bitmask…
    pub kind: String,
    /// "User Controls", "Camera Controls"
    pub group: String,
    /// min, max, step, default, value, as present.
    pub nums: Vec<(&'static str, f64)>,
    /// inactive, read-only, …
    pub flags: Vec<String>,
    pub menu: Option<Vec<(f64, String)>>,
}

impl Control {
    pub fn get(&self, k: &str) -> Option<f64> {
        self.nums.iter().find(|(n, _)| *n == k).map(|(_, v)| *v)
    }
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), self.name.clone().into());
        m.insert("id".into(), self.id.clone().into());
        m.insert("type".into(), self.kind.clone().into());
        m.insert("group".into(), self.group.clone().into());
        m.insert("flags".into(), json!(self.flags));
        for (k, v) in &self.nums {
            m.insert((*k).into(), num(*v));
        }
        if let Some(menu) = &self.menu {
            m.insert("menu".into(), menu.iter().map(|(v, l)| json!({ "value": num(*v), "label": l })).collect());
        }
        Value::Object(m)
    }
}

pub fn parse_controls(text: &str) -> Vec<Control> {
    static CTL: OnceLock<Regex> = OnceLock::new();
    static KV: OnceLock<Regex> = OnceLock::new();
    static FLAGS: OnceLock<Regex> = OnceLock::new();
    static MENU: OnceLock<Regex> = OnceLock::new();
    static GROUP: OnceLock<Regex> = OnceLock::new();
    let mut out: Vec<Control> = Vec::new();
    let mut group = String::new();
    // Whether the last control can still take menu lines.
    let mut cur_menu = false;
    for line in text.split('\n') {
        if let Some(m) = re(&CTL, r"^\s*([a-z0-9_]+) (0x[0-9a-f]+) \(([a-z0-9]+)\)\s*:\s*(.*)$").captures(line) {
            let rest = &m[4];
            let mut kv: Vec<(String, f64)> = Vec::new();
            for k in re(&KV, r"(\w+)=(-?\d+)").captures_iter(rest) {
                let v: f64 = k[2].parse().unwrap_or(f64::NAN);
                match kv.iter_mut().find(|(n, _)| *n == k[1]) {
                    Some(e) => e.1 = v,
                    None => kv.push((k[1].to_string(), v)),
                }
            }
            let flags = re(&FLAGS, r"flags=([\w\-, ]+)$")
                .captures(rest)
                .map(|f| f[1].split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            let nums = ["min", "max", "step", "default", "value"]
                .into_iter()
                .filter_map(|k| kv.iter().find(|(n, _)| n == k).map(|(_, v)| (k, *v)))
                .collect();
            let is_menu = &m[3] == "menu" || &m[3] == "intmenu";
            out.push(Control {
                name: m[1].into(),
                id: m[2].into(),
                kind: m[3].into(),
                group: group.clone(),
                nums,
                flags,
                menu: is_menu.then(Vec::new),
            });
            cur_menu = is_menu;
        } else if cur_menu && let Some(m) = re(&MENU, r"^\t\t\t\t(-?\d+): (.*)$").captures(line) {
            if let Some(menu) = out.last_mut().and_then(|c| c.menu.as_mut()) {
                menu.push((m[1].parse().unwrap_or(f64::NAN), m[2].trim().into()));
            }
        } else if let Some(m) = re(&GROUP, r"^([A-Z][\w ]+ Controls)\s*$").captures(line.trim()) {
            group = m[1].into();
            cur_menu = false;
        }
    }
    out
}

/// Controls a person adjusts on a camera; the rest (codec levels, read-only counters) is noise.
pub fn is_user_control(c: &Control) -> bool {
    if c.flags.iter().any(|f| f == "read-only" || f == "write-only") {
        return false;
    }
    if !["int", "bool", "menu", "intmenu"].contains(&c.kind.as_str()) {
        return false;
    }
    !(c.group.starts_with("Codec") || c.group.starts_with("Detection") || c.group.starts_with("Colorimetry"))
}

// ---------- libcamera ----------

/// `rpicam-hello --list-cameras` → [{ index, sensor, detail }].
pub fn parse_libcamera_list(text: &str) -> Vec<Value> {
    static CAM: OnceLock<Regex> = OnceLock::new();
    re(&CAM, r"(?m)^(\d+) : (\S+) \[([^\]]*)\]\s*\(([^)]*)\)")
        .captures_iter(text)
        .map(|m| json!({ "index": m[1].parse::<u64>().unwrap_or(0), "sensor": &m[2], "detail": format!("{} {}", &m[3], &m[4]).trim() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(kv: &[(&str, &str)]) -> Props {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const USB_DEV: &str = "/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0/usb1/1-1/1-1.3";

    fn uvc() -> Props {
        props(&[
            ("DEVPATH", &format!("{USB_DEV}/1-1.3:1.0/video4linux/video0")),
            ("DEVNAME", "/dev/video0"),
            ("SUBSYSTEM", "video4linux"),
            ("ID_V4L_PRODUCT", "HD Pro Webcam C920"),
            ("ID_V4L_CAPABILITIES", ":capture:"),
            ("ID_VENDOR_ID", "046d"),
            ("ID_MODEL_ID", "082d"),
            ("ID_SERIAL_SHORT", "A1B2C3D4"),
            ("ID_PATH", "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0"),
        ])
    }

    #[test]
    fn detection_keeps_usb_capture_nodes_and_drops_pi_codec_isp_metadata_nodes() {
        // Real udev properties from desky (Pi 4, kernel 6.18): codec, ISP capture output, HEVC.
        let bcm_codec = props(&[
            ("DEVPATH", "/devices/platform/soc/fe00b840.mailbox/bcm2835-codec/video4linux/video10"),
            ("DEVNAME", "/dev/video10"),
            ("SUBSYSTEM", "video4linux"),
            ("ID_V4L_PRODUCT", "bcm2835-codec-decode"),
            ("ID_V4L_CAPABILITIES", ":"),
        ]);
        let bcm_isp = props(&[
            ("DEVPATH", "/devices/platform/soc/fe00b840.mailbox/bcm2835-isp/video4linux/video14"),
            ("DEVNAME", "/dev/video14"),
            ("SUBSYSTEM", "video4linux"),
            ("ID_V4L_PRODUCT", "bcm2835-isp"),
            ("ID_V4L_CAPABILITIES", ":capture:"),
        ]);
        let hevc = props(&[
            ("DEVPATH", "/devices/platform/scb/feb00000.codec/video4linux/video19"),
            ("DEVNAME", "/dev/video19"),
            ("SUBSYSTEM", "video4linux"),
            ("ID_V4L_PRODUCT", "rpi-hevc-dec"),
            ("ID_V4L_CAPABILITIES", ":"),
        ]);
        let usb = props(&[("DEVPATH", USB_DEV), ("SUBSYSTEM", "usb"), ("DEVTYPE", "usb_device")]);
        let mut meta = uvc();
        meta.insert("DEVPATH".into(), format!("{USB_DEV}/1-1.3:1.0/video4linux/video1"));
        meta.insert("DEVNAME".into(), "/dev/video1".into());
        meta.insert("ID_V4L_CAPABILITIES".into(), ":".into());
        assert!(!is_usb_capture_node(&bcm_codec, None));
        assert!(!is_usb_capture_node(&bcm_isp, None));
        assert!(!is_usb_capture_node(&hevc, None));
        assert!(is_usb_capture_node(&uvc(), Some(&usb)));
        assert!(!is_usb_capture_node(&meta, Some(&usb)));
        let mut sub = uvc();
        sub.insert("DEVNAME".into(), "/dev/v4l-subdev0".into());
        assert!(!is_usb_capture_node(&sub, Some(&usb)));
    }

    // Real `v4l2-ctl -d /dev/video14 -D` from desky.
    const CAPS_ISP: &str = "Driver Info:
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
";
    // A UVC metadata node: the device as a whole can capture, this node can't.
    const CAPS_META: &str = "Driver Info:
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
";

    #[test]
    fn device_caps_decide_capture_vs_metadata() {
        let a = parse_device_caps(CAPS_ISP);
        assert_eq!(a.driver, "bcm2835-isp");
        assert_eq!(a.caps, ["Video Capture", "Streaming", "Extended Pix Format"]);
        assert!(caps_can_capture(&a.caps));
        let b = parse_device_caps(CAPS_META);
        assert_eq!(b.card, "HD Pro Webcam C920");
        assert!(!caps_can_capture(&b.caps));
    }

    #[test]
    fn hdmi_capture_sticks_are_labelled() {
        assert_eq!(camera_kind("534d", "2109", "USB Video"), "hdmi");
        assert_eq!(camera_kind("345f", "2130", "UGREEN"), "hdmi");
        assert_eq!(camera_kind("046d", "082d", "HD Pro Webcam C920"), "webcam");
        assert_eq!(camera_kind("1234", "5678", "USB3.0 Video Capture"), "hdmi");
    }

    #[test]
    fn settings_key_prefers_serial_then_port_path() {
        assert_eq!(camera_key(&uvc()), "usb:046d:082d:A1B2C3D4");
        let mut p = uvc();
        p.insert("ID_SERIAL_SHORT".into(), String::new());
        assert_eq!(camera_key(&p), "path:platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0");
    }

    // v4l2-ctl 1.30 --list-formats-ext for an MS2109 stick.
    pub(crate) const FORMATS_MS2109: &str = "ioctl: VIDIOC_ENUM_FMT
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
";
    // Real desky output head (ISP node): stepwise sizes.
    const FORMATS_ISP: &str = "ioctl: VIDIOC_ENUM_FMT
\tType: Video Capture

\t[0]: 'YUYV' (YUYV 4:2:2)
\t\tSize: Stepwise 64x64 - 16384x16384 with step 2/2
\t[1]: 'RGB3' (24-bit RGB 8-8-8)
\t\tSize: Stepwise 64x64 - 16384x16384 with step 1/1
";

    #[test]
    fn list_formats_ext_parsing() {
        let f = parse_formats(FORMATS_MS2109);
        assert_eq!(f.iter().map(|x| x.fourcc.as_str()).collect::<Vec<_>>(), ["MJPG", "YUYV"]);
        assert!(f[0].compressed);
        assert_eq!(f[0].sizes[0], FrameSize { w: 1920, h: 1080, fps: vec![30.0, 25.0] });
        assert_eq!(f[0].sizes[1].fps, [60.0, 30.0]);
        assert_eq!(f[1].sizes[2], FrameSize { w: 640, h: 480, fps: vec![30.0] });
        let s = parse_formats(FORMATS_ISP);
        assert_eq!(s[0].stepwise, Some(Stepwise { min_w: 64, min_h: 64, max_w: 16384, max_h: 16384, step_w: 2, step_h: 2 }));
        assert_eq!(s[1].fourcc, "RGB3");
        assert_eq!(
            s[0].to_json().to_string(),
            r#"{"fourcc":"YUYV","description":"YUYV 4:2:2","compressed":false,"sizes":[],"stepwise":{"minW":64,"minH":64,"maxW":16384,"maxH":16384,"stepW":2,"stepH":2}}"#
        );
    }

    fn want(fourcc: &str, w: f64, h: f64, fps: f64) -> Want {
        Want { fourcc: fourcc.into(), w, h, fps }
    }

    fn sc(fourcc: &str, w: f64, h: f64, fps: f64) -> Option<StreamChoice> {
        Some(StreamChoice { fourcc: fourcc.into(), w, h, fps })
    }

    #[test]
    fn stream_choice_prefers_mjpeg_and_honours_saved_settings() {
        let f = parse_formats(FORMATS_MS2109);
        assert_eq!(choose_stream(&f, None), sc("MJPG", 1920.0, 1080.0, 30.0));
        assert_eq!(choose_stream(&f, Some(&want("MJPG", 1280.0, 720.0, 60.0))), sc("MJPG", 1280.0, 720.0, 60.0));
        assert_eq!(choose_stream(&f, Some(&want("YUYV", 640.0, 480.0, 99.0))), sc("YUYV", 640.0, 480.0, 30.0));
        // A saved size the camera no longer offers falls back to the default pick.
        assert_eq!(choose_stream(&f, Some(&want("MJPG", 800.0, 600.0, 30.0))).unwrap().w, 1920.0);
        // Uncompressed-only camera: capped at 720p because the Pi has to encode.
        assert_eq!(choose_stream(&[f[1].clone()], None), sc("YUYV", 1280.0, 720.0, 10.0));
        assert_eq!(choose_stream(&parse_formats(FORMATS_ISP), None), sc("YUYV", 1280.0, 720.0, 30.0));
        assert_eq!(choose_stream(&[], None), None);
    }

    // Real desky output (codec node) followed by typical UVC controls.
    const CTRLS: &str = "
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
";

    #[test]
    fn list_ctrls_menus_parsing() {
        let c = parse_controls(CTRLS);
        let by = |n: &str| c.iter().find(|x| x.name == n).unwrap();
        assert_eq!(
            by("brightness").to_json(),
            json!({ "name": "brightness", "id": "0x00980900", "type": "int", "group": "User Controls", "flags": [], "min": 0, "max": 255, "step": 1, "default": 128, "value": 140 })
        );
        assert_eq!((by("white_balance_automatic").kind.as_str(), by("white_balance_automatic").get("default")), ("bool", Some(1.0)));
        assert_eq!(
            by("power_line_frequency").to_json()["menu"],
            json!([{ "value": 0, "label": "Disabled" }, { "value": 1, "label": "50 Hz" }, { "value": 2, "label": "60 Hz" }])
        );
        assert_eq!(by("white_balance_temperature").flags, ["inactive"]);
        assert_eq!((by("auto_exposure").group.as_str(), by("auto_exposure").get("value")), ("Camera Controls", Some(3.0)));
        assert_eq!(by("auto_exposure").menu.as_ref().unwrap().len(), 2);
        assert_eq!(by("min_number_of_capture_buffers").flags, ["read-only", "has-min-max"]);
        assert_eq!(by("h264_level").group, "Codec Controls");
        let shown: Vec<&str> = c.iter().filter(|x| is_user_control(x)).map(|x| x.name.as_str()).collect();
        assert_eq!(
            shown,
            [
                "brightness",
                "contrast",
                "white_balance_automatic",
                "power_line_frequency",
                "white_balance_temperature",
                "auto_exposure",
                "exposure_time_absolute",
                "focus_absolute"
            ]
        );
    }

    #[test]
    fn libcamera_listing() {
        assert!(parse_libcamera_list("No cameras available!").is_empty());
        let t = "Available cameras
-----------------
0 : imx708 [4608x2592 10-bit RGGB] (/base/soc/i2c0mux/i2c@1/imx708@1a)
    Modes: 'SRGGB10_CSI2P' : 1536x864 [120.13 fps - (768, 432)/3072x1728 crop]
";
        assert_eq!(
            parse_libcamera_list(t),
            [json!({ "index": 0, "sensor": "imx708", "detail": "4608x2592 10-bit RGGB /base/soc/i2c0mux/i2c@1/imx708@1a" })]
        );
    }
}
