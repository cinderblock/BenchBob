//! peripherals/lib/usb.test.ts on the same fixtures, plus the sysfs reader on a fake tree
//! and the driver's JSON against a fixture inventory.
//!
//! desky-pi4.json / uhubctl-desky-pi4.txt are real captures from desky (Pi 4, nothing
//! plugged in). The "busy" Pi 4 extends that capture with synthetic devices;
//! uhubctl-examples.txt follows the output format shown in the uhubctl README.

use std::collections::HashMap;

use benchbob_core::udev::Props;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::model::*;

macro_rules! fx {
    ($f:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/", $f))
    };
}

const PI4: &str = "Raspberry Pi 4 Model B Rev 1.1";

fn desky() -> (Vec<RawUsbDevice>, Vec<Props>) {
    let v: Value = serde_json::from_str(fx!("desky-pi4.json")).unwrap();
    (serde_json::from_value(v["raw"].clone()).unwrap(), serde_json::from_value(v["udev"].clone()).unwrap())
}

fn req(hub: &str, port: Option<i64>, action: &str) -> PowerReq {
    PowerReq { hub: hub.into(), port, action: PowerAction::parse(action).unwrap() }
}

fn none() -> IndexMap<String, Vec<String>> {
    IndexMap::new()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn desky_tree() {
    let (raw, udev) = desky();
    let inv = build_inventory(&raw, &udev);
    let order: Vec<String> = inv.devices.iter().map(|d| format!("{}@{}", d.busid, d.depth)).collect();
    assert_eq!(order, ["usb1@0", "1-1@1", "usb2@0"]);
    assert_eq!(inv.controllers.len(), 1);
    let c = &inv.controllers[0];
    assert_eq!(
        (c.id.as_str(), c.pci_id.as_str(), c.driver.as_str(), c.buses.clone()),
        ("0000:01:00.0", "1106:3483", "xhci_hcd", vec![1, 2])
    );
    assert!(c.name.contains("VL805"));
    let via = inv.get("1-1").unwrap();
    assert!(via.hub);
    assert_eq!(via.nports, 4);
    assert_eq!(via.vidpid, "2109:3431");
    assert_eq!(via.vendor, "VIA Labs, Inc.");
    assert_eq!(via.speed, 480.0);
    assert_eq!(via.version, "2.10");
    assert_eq!(via.max_power_ma, 100);
    assert_eq!((via.parent.as_deref(), via.port), (Some("usb1"), Some(1)));
    assert_eq!((via.interfaces[0].cls_name.as_str(), via.interfaces[0].driver.as_deref()), ("Hub", Some("hub")));
    assert_eq!(inv.devices[0].ports, vec![UsbPort { port: 1, child: Some("1-1".into()), connect_type: "unknown".into(), over_current: 0 }]);
    assert_eq!(inv.get("usb2").unwrap().speed_label, "5 Gb/s SuperSpeed");
}

#[test]
fn desky_uhubctl_listing() {
    let hubs = parse_uhubctl(fx!("uhubctl-desky-pi4.txt"));
    let l: Vec<String> = hubs.iter().map(|h| format!("{}:{}:{}", h.loc, h.mode, h.nports)).collect();
    assert_eq!(l, ["2:ppps:4", "1-1:ppps:4", "1:ppps:1"]);
    let p = &hubs[0].ports[0];
    assert!(p.power && !p.connect && p.attached.is_none());
    assert_eq!((p.speed.as_str(), p.link.as_str()), ("5gbps", "Rx.Detect"));
    let p = &hubs[2].ports[0];
    assert!(p.power && p.suspend && p.enable && p.connect);
    assert_eq!(p.speed, "highspeed");
    assert_eq!(p.attached, Some(Attached { vidpid: "2109:3431".into(), desc: "USB2.0 Hub, USB 2.10, 4 ports, ppps".into() }));
    assert_eq!(hubs[0].desc, "Linux 6.18.50+rpt-rpi-v8 xhci-hcd xHCI Host Controller 0000:01:00.0");
}

#[test]
fn desky_vl805_quirk() {
    let (raw, udev) = desky();
    let inv = build_inventory(&raw, &udev);
    let hubs = parse_uhubctl(fx!("uhubctl-desky-pi4.txt"));
    let topo = power_topology(&inv, &hubs, PI4);
    let g: Vec<(String, Vec<String>)> = topo.groups.iter().map(|g| (g.id.clone(), g.hubs.clone())).collect();
    assert_eq!(g, vec![("vl805-0000:01:00.0".to_string(), s(&["1-1", "2"]))]);
    assert_eq!(topo.internal.keys().cloned().collect::<Vec<_>>(), ["1:1"]);

    // Switching one Pi 4 port widens to all eight and needs ganged confirmation.
    let p = plan_power(&inv, &hubs, &topo, &none(), &req("1-1", Some(2), "off")).unwrap();
    assert_eq!(p.needs, "ganged");
    assert_eq!(p.ports.len(), 8);
    assert_eq!(p.commands, vec![s(&["-l", "1-1", "-a", "off"]), s(&["-l", "2", "-a", "off"])]);
    assert_eq!(plan_power(&inv, &hubs, &topo, &none(), &req("2", Some(3), "on")).unwrap().needs, "none");
    let cyc = plan_power(&inv, &hubs, &topo, &none(), &req("2", None, "cycle")).unwrap();
    let c: Vec<String> = cyc.commands.iter().map(|c| c.join(" ")).collect();
    assert_eq!(c, ["-l 1-1 -a off", "-l 2 -a off", "-l 1-1 -a on", "-l 2 -a on"]);

    // The internal link is unsafe.
    let p = plan_power(&inv, &hubs, &topo, &none(), &req("1", Some(1), "off")).unwrap();
    assert_eq!(p.needs, "unsafe");
    assert_eq!(p.devices, ["1-1"]);

    // Bad requests.
    assert!(plan_power(&inv, &hubs, &topo, &none(), &req("9", None, "off")).unwrap_err().contains("not listed"));
    assert!(plan_power(&inv, &hubs, &topo, &none(), &req("1-1", Some(7), "off")).unwrap_err().contains("ports 1"));
}

#[test]
fn plan_json_shape() {
    let (raw, udev) = desky();
    let inv = build_inventory(&raw, &udev);
    let hubs = parse_uhubctl(fx!("uhubctl-desky-pi4.txt"));
    let topo = power_topology(&inv, &hubs, PI4);
    let p = plan_power(&inv, &hubs, &topo, &none(), &req("1", Some(1), "off")).unwrap();
    let v = serde_json::to_string(&p).unwrap();
    assert_eq!(
        v,
        r#"{"action":"off","hub":"1","port":1,"commands":[["-l","1","-p","1","-a","off"]],"group":null,"ports":["1:1"],"devices":["1-1"],"warnings":["Port 1:1: Internal link to the onboard VIA hub. Cutting it drops every USB socket and confuses the hub."],"needs":"unsafe"}"#
    );
}

// ---------- A busy Pi 4, built on the real capture ----------

fn dev(name: &str, devpath: &str, a: &[(&str, &str)], ifs: &[(u32, u32, u32, &str)]) -> RawUsbDevice {
    let mut attrs: HashMap<String, String> = [
        ("busnum", name.split('-').next().unwrap()),
        ("devnum", "5"),
        ("bConfigurationValue", "1"),
        ("bDeviceClass", "00"),
        ("version", " 2.00"),
        ("maxchild", "0"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in a {
        attrs.insert(k.to_string(), v.to_string());
    }
    RawUsbDevice {
        name: name.into(),
        devpath: devpath.into(),
        attrs,
        driver: "usb".into(),
        interfaces: ifs
            .iter()
            .enumerate()
            .map(|(i, (cls, sub, proto, driver))| RawInterface {
                name: format!("{name}:1.{i}"),
                cls: *cls,
                sub: *sub,
                proto: *proto,
                num: i as u32,
                alt: 0,
                driver: driver.to_string(),
            })
            .collect(),
        ports: Vec::new(),
        usbip_status: None,
        controller: None,
    }
}

fn props(kv: &[(&str, &str)]) -> Props {
    kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn busy() -> (Vec<RawUsbDevice>, Vec<Props>) {
    let (mut raw, mut udev) = desky();
    let root1 = raw.iter().find(|r| r.name == "usb1").unwrap().devpath.clone();
    let root2 = raw.iter().find(|r| r.name == "usb2").unwrap().devpath.clone();
    let p11 = format!("{root1}/1-1");
    raw.push(dev(
        "1-1.2",
        &format!("{p11}/1-1.2"),
        &[
            ("idVendor", "0403"),
            ("idProduct", "6001"),
            ("speed", "12"),
            ("bMaxPower", "90mA"),
            ("serial", "A50285BI"),
            ("product", "FT232R USB UART"),
        ],
        &[(0xff, 0xff, 0xff, "ftdi_sio")],
    ));
    let mut hub = dev(
        "1-1.3",
        &format!("{p11}/1-1.3"),
        &[("idVendor", "0bda"), ("idProduct", "5411"), ("speed", "480"), ("maxchild", "4"), ("bDeviceClass", "09")],
        &[(9, 0, 0, "hub")],
    );
    hub.ports = (1..=4).map(|port| RawPort { port, connect_type: "hotplug".into(), over_current: if port == 4 { 2 } else { 0 } }).collect();
    raw.push(hub);
    raw.push(dev(
        "1-1.3.1",
        &format!("{p11}/1-1.3/1-1.3.1"),
        &[("idVendor", "1d50"), ("idProduct", "606f"), ("speed", "12")],
        &[(0xff, 0xff, 0xff, "gs_usb")],
    ));
    raw.push(dev(
        "1-1.3.4",
        &format!("{p11}/1-1.3/1-1.3.4"),
        &[("idVendor", "ffff"), ("idProduct", "0001"), ("speed", "12"), ("product", "Mystery")],
        &[(0xff, 0, 0, "")],
    ));
    let mut st = dev(
        "1-1.4",
        &format!("{p11}/1-1.4"),
        &[("idVendor", "0483"), ("idProduct", "374b"), ("speed", "480")],
        &[(0xff, 0xff, 0xff, ""), (8, 6, 0x50, ""), (2, 2, 1, ""), (0x0a, 0, 0, "")],
    );
    st.driver = "usbip-host".into();
    st.usbip_status = Some(2);
    raw.push(st);
    raw.push(dev(
        "2-1",
        &format!("{root2}/2-1"),
        &[("idVendor", "0781"), ("idProduct", "5581"), ("speed", "5000"), ("version", " 3.20"), ("bMaxPower", "896mA")],
        &[(8, 6, 0x62, "uas")],
    ));
    let sda = format!("{root2}/2-1/2-1:1.0/host0/target0:0:0/0:0:0:0/block/sda");
    udev.extend([
        props(&[
            ("DEVPATH", &format!("{p11}/1-1.2")),
            ("SUBSYSTEM", "usb"),
            ("DEVTYPE", "usb_device"),
            ("ID_VENDOR_FROM_DATABASE", "Future Technology Devices International, Ltd"),
            ("ID_MODEL_FROM_DATABASE", "FT232 Serial (UART) IC"),
            ("ID_PATH", "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.2"),
        ]),
        props(&[("DEVPATH", &format!("{p11}/1-1.2/1-1.2:1.0/ttyUSB0/tty/ttyUSB0")), ("SUBSYSTEM", "tty"), ("DEVNAME", "/dev/ttyUSB0")]),
        props(&[
            ("DEVPATH", &format!("{p11}/1-1.3/1-1.3.1/1-1.3.1:1.0/net/can0")),
            ("SUBSYSTEM", "net"),
            ("INTERFACE", "can0"),
            ("ID_NET_DRIVER", "gs_usb"),
        ]),
        props(&[("DEVPATH", &sda), ("SUBSYSTEM", "block"), ("DEVTYPE", "disk"), ("DEVNAME", "/dev/sda")]),
        props(&[("DEVPATH", &format!("{sda}/sda1")), ("SUBSYSTEM", "block"), ("DEVTYPE", "partition"), ("DEVNAME", "/dev/sda1")]),
        props(&[("DEVPATH", &format!("{sda}/sda2")), ("SUBSYSTEM", "block"), ("DEVTYPE", "partition"), ("DEVNAME", "/dev/sda2")]),
    ]);
    (raw, udev)
}

fn node(dev: &str, kind: &str, panel: &str, label: Option<&str>) -> UsbNode {
    UsbNode { dev: dev.into(), kind: kind.into(), panel: Some(panel.into()), label: label.map(Into::into) }
}

#[test]
fn busy_order_and_depths() {
    let (raw, udev) = busy();
    let inv = build_inventory(&raw, &udev);
    let order: Vec<String> = inv.devices.iter().map(|d| format!("{}@{}", d.busid, d.depth)).collect();
    assert_eq!(order, ["usb1@0", "1-1@1", "1-1.2@2", "1-1.3@2", "1-1.3.1@3", "1-1.3.4@3", "1-1.4@2", "usb2@0", "2-1@1"]);
    let kids: Vec<Option<&str>> = inv.get("1-1").unwrap().ports.iter().map(|p| p.child.as_deref()).collect();
    assert_eq!(kids, [None, Some("1-1.2"), Some("1-1.3"), Some("1-1.4")]);
    assert_eq!(inv.get("1-1.3").unwrap().ports[3].over_current, 2);
}

#[test]
fn busy_names_nodes_hints() {
    let (raw, udev) = busy();
    let inv = build_inventory(&raw, &udev);
    let by = |b: &str| inv.get(b).unwrap().clone();
    let ft = by("1-1.2");
    assert_eq!(ft.vendor, "Future Technology Devices International, Ltd");
    assert_eq!(ft.product, "FT232 Serial (UART) IC");
    assert_eq!(ft.serial, "A50285BI");
    assert_eq!(ft.speed_label, "12 Mb/s Full");
    assert_eq!(ft.max_power_ma, 90);
    assert_eq!(ft.nodes, vec![node("/dev/ttyUSB0", "serial", "serial", None)]);
    // can0 belongs to the CAN adapter, not to the hub above it.
    assert!(by("1-1.3").nodes.is_empty());
    assert_eq!(by("1-1.3.1").nodes, vec![node("can0", "can", "can", None)]);
    assert_eq!(by("2-1").nodes, vec![node("/dev/sda", "disk", "drives", Some("2 partitions"))]);
    assert_eq!(by("2-1").interfaces[0].cls_name, "Mass storage (UAS)");
    assert_eq!((by("1-1.3.4").interfaces[0].cls_name.as_str(), by("1-1.3.4").interfaces[0].driver.clone()), ("Vendor-specific", None));
    assert_eq!(by("1-1.4").hint, Some(Hint { panel: "probes".into(), label: "ST-LINK".into() }));
    // usbip state
    assert_eq!(by("1-1.4").usbip, Some("in use"));
    assert_eq!(by("1-1.2").usbip, None);
}

#[test]
fn busy_per_port_hub_and_protection() {
    let (raw, udev) = busy();
    let inv = build_inventory(&raw, &udev);
    let mut hubs = parse_uhubctl(fx!("uhubctl-desky-pi4.txt"));
    hubs.extend(parse_uhubctl(fx!("uhubctl-examples.txt")));
    let topo = power_topology(&inv, &hubs, PI4);
    let mut protect = IndexMap::new();
    protect.insert("1-1.3.1".to_string(), s(&["is the management network adapter"]));
    protect.insert("2-1".to_string(), s(&["holds a served drive (/dev/sda1)"]));
    let ok = plan_power(&inv, &hubs, &topo, &protect, &req("1-1.3", Some(4), "off")).unwrap();
    assert_eq!(ok.needs, "none");
    assert_eq!(ok.ports, ["1-1.3:4"]);
    assert_eq!(ok.devices, ["1-1.3.4"]);
    assert_eq!(ok.commands, vec![s(&["-l", "1-1.3", "-p", "4", "-a", "off"])]);
    let bad = plan_power(&inv, &hubs, &topo, &protect, &req("1-1.3", Some(1), "cycle")).unwrap();
    assert_eq!(bad.needs, "unsafe");
    assert!(bad.warnings.join(" ").contains("management network adapter"));
    assert_eq!(bad.commands, vec![s(&["-l", "1-1.3", "-p", "1", "-a", "off"]), s(&["-l", "1-1.3", "-p", "1", "-a", "on"])]);
    // Everything on the Pi 4 sockets is ganged, and the served drive on 2-1 makes it unsafe.
    let all = plan_power(&inv, &hubs, &topo, &protect, &req("1-1", Some(2), "off")).unwrap();
    assert_eq!(all.needs, "unsafe");
    let mut d = all.devices.clone();
    d.sort();
    assert_eq!(d, ["1-1.2", "1-1.3", "1-1.3.1", "1-1.3.4", "1-1.4", "2-1"]);
}

#[test]
fn busy_ganged_and_nops() {
    let (raw, udev) = busy();
    let inv = build_inventory(&raw, &udev);
    let hubs = parse_uhubctl(fx!("uhubctl-examples.txt"));
    let topo = power_topology(&inv, &hubs, PI4);
    assert_eq!(topo.groups.iter().find(|g| g.id == "ganged-3-2").unwrap().hubs, ["3-2"]);
    let p = plan_power(&inv, &hubs, &topo, &none(), &req("3-2", Some(2), "off")).unwrap();
    assert_eq!(p.needs, "ganged");
    assert_eq!(p.commands, vec![s(&["-l", "3-2", "-a", "off", "-f"])]);
    assert!(plan_power(&inv, &hubs, &topo, &none(), &req("3-3", None, "off")).unwrap_err().contains("no power switching"));
}

#[test]
fn uhubctl_port_flags() {
    let h = parse_uhubctl(fx!("uhubctl-examples.txt"));
    assert_eq!(h.iter().map(|x| x.loc.as_str()).collect::<Vec<_>>(), ["1-1.3", "2-1", "3-2", "3-3"]);
    assert!(!h[0].ports[1].power && !h[0].ports[1].connect);
    assert_eq!(h[0].ports[1].flags, ["off"]);
    assert!(h[0].ports[3].power && h[0].ports[3].over_current);
    assert_eq!(
        h[0].ports[0].attached,
        Some(Attached { vidpid: "1d50:606f".into(), desc: "bytewerk candleLight USB to CAN adapter 003A00385744".into() })
    );
    let p = &h[1].ports[0];
    assert_eq!((p.speed.as_str(), p.link.as_str(), p.enable, p.connect), ("5gbps", "U0", true, true));
    assert_eq!((h[1].ports[3].power, h[1].ports[3].link.as_str()), (false, "Rx.Detect"));
    assert_eq!(h[2].ports[1].speed, "lowspeed");
}

#[test]
fn uhubctl_action_new_status_wins() {
    let h = parse_uhubctl(fx!("uhubctl-action.txt"));
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].ports.len(), 1);
    assert_eq!((h[0].ports[0].port, h[0].ports[0].power), (2, false));
}

#[test]
fn uhubctl_no_descriptions() {
    let h = parse_uhubctl(fx!("uhubctl-desky-pi4-nodesc.txt"));
    let l: Vec<String> = h.iter().map(|x| format!("{} {} \"{}\" {} {} {}", x.loc, x.vidpid, x.desc, x.usb, x.nports, x.mode)).collect();
    assert_eq!(l, ["2 1d6b:0003 \"\" 3.00 4 ppps", "1-1 2109:3431 \"\" 2.10 4 ppps", "1 1d6b:0002 \"\" 2.00 1 ppps"]);
    assert_eq!(h[2].ports[0].attached, Some(Attached { vidpid: "2109:3431".into(), desc: "USB 2.10, 4 ports, ppps".into() }));
}

#[test]
fn kernel_log_filter() {
    let lines: Vec<&str> = fx!("dmesg.txt").lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()).collect();
    let got: Vec<String> = lines.iter().filter_map(|l| filter_kernel_line(l)).map(|k| format!("{} {}", k.level, k.text)).collect();
    assert_eq!(
        got,
        [
            "info usb 1-1.2: new full-speed USB device number 3 using xhci_hcd",
            "info ftdi_sio 1-1.2:1.0: FTDI USB Serial Device converter detected",
            "info usb 1-1.2: FTDI USB Serial Device converter now attached to ttyUSB0",
            "info cdc_acm 1-1.3:1.0: ttyACM0: USB ACM device",
            "error usb 1-1.4: device descriptor read/64, error -71",
            "error usb usb1-port4: over-current condition",
            "warn usb 1-1.4: reset high-speed USB device number 5 using xhci_hcd",
            "error usb 1-1-port4: unable to enumerate USB device",
            "info usb 2-1: new SuperSpeed USB device number 2 using xhci_hcd",
            "info usb-storage 2-1:1.0: USB Mass Storage device detected",
            "info scsi host0: usb-storage 2-1:1.0",
            "info gs_usb 1-1.3.1:1.0: Configuring for 1 interfaces",
            "info usb 1-1.2: USB disconnect, device number 3",
            "info ftdi_sio ttyUSB0: FTDI USB Serial Device converter now disconnected from ttyUSB0",
            "info vhci_hcd vhci_hcd.0: new USB bus registered, assigned bus number 3",
            "info vhci_hcd vhci_hcd.0: USB bus 3 deregistered",
        ]
    );
    assert_eq!(filter_kernel_line(lines[0]).unwrap().ts, "2026-10-17T09:00:01,000001-07:00");
}

#[test]
fn ss_peers() {
    let out = "0      0      10.255.7.45:3240     10.255.3.20:51234\n0 0 [::ffff:10.255.7.45]:4000 [::ffff:10.255.3.21]:40000\n0 0 10.255.7.45:22 10.255.3.20:5555";
    assert_eq!(
        parse_ss_peers(out, &[3240, 4000]),
        vec![Peer { local: 3240, peer: "10.255.3.20:51234".into() }, Peer { local: 4000, peer: "10.255.3.21:40000".into() }]
    );
}

#[test]
fn labels() {
    assert_eq!(speed_label("1.5"), "1.5 Mb/s Low");
    assert_eq!(speed_label("10000"), "10 Gb/s SuperSpeed+");
    assert_eq!(speed_label("40000"), "40 Gb/s");
    assert_eq!(speed_label(""), "");
    assert_eq!(class_name(2, 2, 1), "CDC ACM (serial)");
    assert_eq!(class_name(0xe0, 1, 1), "Bluetooth");
    assert_eq!(class_name(0x42, 0, 0), "Class 0x42");
    assert_eq!(natural_cmp("/dev/ttyUSB10", "/dev/ttyUSB9"), std::cmp::Ordering::Greater);
    assert_eq!(parent_of("1-1.3.4"), Some(("1-1.3".into(), 4)));
    assert_eq!(parent_of("2-1"), Some(("usb2".into(), 1)));
    assert_eq!(with_upstream(&build_inventory(&busy().0, &busy().1).devices, "1-1.3.1"), ["1-1.3.1", "1-1.3", "1-1", "usb1"]);
}

/// The device objects keep usb.ts's key order (the UI and MCP clients see the same JSON).
#[test]
fn device_json_key_order() {
    let (raw, udev) = busy();
    let inv = build_inventory(&raw, &udev);
    let v = serde_json::to_value(inv.get("2-1").unwrap()).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "busid",
            "devpath",
            "bus",
            "devnum",
            "parent",
            "port",
            "depth",
            "root",
            "hub",
            "nports",
            "ports",
            "vid",
            "pid",
            "vidpid",
            "vendor",
            "product",
            "serial",
            "speed",
            "speedLabel",
            "version",
            "maxPowerMa",
            "config",
            "cls",
            "driver",
            "interfaces",
            "nodes",
            "hint",
            "idPath",
            "usbip",
            "controller"
        ]
    );
    assert_eq!(v["speed"], json!(5000));
    assert_eq!(v["nodes"], json!([{ "dev": "/dev/sda", "kind": "disk", "panel": "drives", "label": "2 partitions" }]));
    assert_eq!(serde_json::to_value(inv.get("1-1.2").unwrap()).unwrap()["speed"], json!(12));
    let c = serde_json::to_value(&inv.controllers[0]).unwrap();
    assert_eq!(c.as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "devpath", "name", "driver", "pciId", "buses"]);
}

/// The sysfs reader on a fake /sys laid out like desky's, with a shared device.
#[cfg(unix)]
#[test]
fn sysfs_reader() {
    let t = tempfile::tempdir().unwrap();
    let sys = fake_sys(t.path());
    let raw = read_usb_sysfs(&sys);
    assert_eq!(raw.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["1-1", "usb1"]);
    let r1 = raw.iter().find(|r| r.name == "usb1").unwrap();
    assert_eq!(r1.devpath, "/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0/usb1");
    assert_eq!(r1.driver, "usb");
    assert_eq!(
        r1.interfaces,
        vec![RawInterface { name: "1-0:1.0".into(), cls: 9, sub: 0, proto: 0, num: 0, alt: 0, driver: "hub".into() }]
    );
    assert_eq!(r1.ports, vec![RawPort { port: 1, connect_type: "hotplug".into(), over_current: 3 }]);
    assert_eq!(
        r1.controller,
        Some(RawController {
            devpath: "/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0".into(),
            name: "0000:01:00.0".into(),
            driver: "xhci_hcd".into(),
            pci_id: "1106:3483".into(),
        })
    );
    let r2 = raw.iter().find(|r| r.name == "1-1").unwrap();
    assert_eq!((r2.driver.as_str(), r2.usbip_status), ("usbip-host", Some(1)));
    let inv = build_inventory(&raw, &[]);
    let d = inv.get("1-1").unwrap();
    assert_eq!((d.vidpid.as_str(), d.usbip, d.hint.as_ref().map(|h| h.label.as_str())), ("0483:374b", Some("available"), Some("ST-LINK")));
    assert_eq!(inv.controllers[0].name, "VIA VL805/806 xHCI (USB 3.0)");
    assert_eq!(inv.devices[0].ports[0].child.as_deref(), Some("1-1"));
}

/// The driver's view and flattened list over the fake tree: status keys in usb.ts's order,
/// the shared device adopted as shared, and the flattened entries.
#[cfg(unix)]
#[tokio::test]
async fn driver_view_and_flat_list() {
    use std::sync::Arc;

    use benchbob_core::driver::Hub;
    use benchbob_core::settings::Settings;
    use benchbob_core::{Ctx, Driver};

    let t = tempfile::tempdir().unwrap();
    let sys = fake_sys(t.path());
    let usb = crate::Usb::with_sys(sys);
    let hub = Arc::new(Hub::new(Settings::load(t.path().join("peripherals.json"))));
    let _ = usb.inner.ctx.set(Ctx::new("usb", hub));
    assert_eq!(usb.inner.status(false), json!({ "active": false, "summary": "Scanning…" }));
    // What init does, minus the processes: the rescan sees the usbip-host binding.
    usb.inner.st.lock().unwrap().stopped = true;
    usb.inner.rescan().await;
    let st = usb.inner.status(false);
    let keys: Vec<&str> = st.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["active", "summary", "host", "controllers", "devices", "groups", "uhubctl", "usbip", "redir", "rules", "kernelLog"]);
    assert_eq!(st["summary"], "1 device · 1 shared · over-current seen");
    assert_eq!(st["usbip"]["shared"], 1);
    assert_eq!(st["uhubctl"], json!({ "available": false, "error": "", "at": 0 }));
    let d = &st["devices"][1];
    let extra: Vec<&str> = d.as_object().unwrap().keys().skip(30).map(String::as_str).collect();
    assert_eq!(extra, ["power", "upstream", "protect", "shareBlock", "redir", "rules"]);
    assert_eq!(d["usbip"], "available");
    assert_eq!(d["upstream"], json!({ "hub": "1", "port": 1 }));
    assert_eq!(st["devices"][0]["power"], json!({ "loc": "1", "switchable": false, "why": "uhubctl isn't installed (apt: uhubctl)" }));
    assert_eq!(st["devices"][0]["shareBlock"], "Root hubs can't be shared");
    let flat = usb.inner.flat_list();
    assert_eq!(
        flat,
        json!({
            "controllers": [{ "id": "0000:01:00.0", "name": "VIA VL805/806 xHCI (USB 3.0)", "driver": "xhci_hcd", "buses": [1] }],
            "devices": [
                { "busid": "usb1", "depth": 0, "parent": null, "port": null, "name": "1d6b:0002", "vidpid": "1d6b:0002", "speed": "480 Mb/s High", "usb": "", "maxPowerMa": 0, "hub": { "ports": 1, "power": "not switchable" }, "interfaces": ["1.0 Hub → hub"], "nodes": [] },
                { "busid": "1-1", "depth": 1, "parent": "usb1", "port": 1, "name": "0483:374b", "vidpid": "0483:374b", "speed": "12 Mb/s Full", "usb": "", "maxPowerMa": 0, "interfaces": ["1.0 Vendor-specific → no driver"], "nodes": [], "hint": "ST-LINK", "usbip": "available" }
            ]
        })
    );
    // Errors the UI shows.
    assert_eq!(
        usb.inner.must_dev("9-9").unwrap_err().message,
        "No USB device \"9-9\". Busids look like 1-1.2 (bus 1, port path 1.2); see GET /api/usb."
    );
    assert!(usb.inner.start_redir("usb1").is_err());
    // A service stop leaves the binding alone (no usbip unbind is attempted): the next start
    // adopts it.
    usb.stop(benchbob_core::StopReason::Shutdown).await;
    usb.inner.rescan().await;
    assert_eq!(usb.inner.status(false)["usbip"]["shared"], 1);
}

#[cfg(unix)]
fn fake_sys(t: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::symlink;
    let sys = t.canonicalize().unwrap().join("sys");
    let pci = sys.join("devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0");
    let w = |p: std::path::PathBuf, s: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{s}\n")).unwrap();
    };
    std::fs::create_dir_all(sys.join("bus/usb/devices")).unwrap();
    std::fs::create_dir_all(sys.join("bus/pci/drivers/xhci_hcd")).unwrap();
    std::fs::create_dir_all(sys.join("bus/usb/drivers/usb")).unwrap();
    std::fs::create_dir_all(sys.join("bus/usb/drivers/hub")).unwrap();
    std::fs::create_dir_all(sys.join("bus/usb/drivers/usbip-host")).unwrap();
    w(pci.join("vendor"), "0x1106");
    w(pci.join("device"), "0x3483");
    symlink(sys.join("bus/pci/drivers/xhci_hcd"), pci.join("driver")).unwrap();
    let usb1 = pci.join("usb1");
    for (k, v) in [
        ("idVendor", "1d6b"),
        ("idProduct", "0002"),
        ("speed", "480"),
        ("maxchild", "1"),
        ("busnum", "1"),
        ("devnum", "1"),
        ("bDeviceClass", "09"),
    ] {
        w(usb1.join(k), v);
    }
    symlink(sys.join("bus/usb/drivers/usb"), usb1.join("driver")).unwrap();
    let if0 = usb1.join("1-0:1.0");
    for (k, v) in [("bInterfaceClass", "09"), ("bInterfaceNumber", "00"), ("bAlternateSetting", " 0")] {
        w(if0.join(k), v);
    }
    symlink(sys.join("bus/usb/drivers/hub"), if0.join("driver")).unwrap();
    w(if0.join("usb1-port1/connect_type"), "hotplug");
    w(if0.join("usb1-port1/over_current_count"), "3");
    let d = usb1.join("1-1");
    for (k, v) in [("idVendor", "0483"), ("idProduct", "374B"), ("speed", "12"), ("busnum", "1"), ("devnum", "4"), ("usbip_status", "1")] {
        w(d.join(k), v);
    }
    symlink(sys.join("bus/usb/drivers/usbip-host"), d.join("driver")).unwrap();
    let dif = d.join("1-1:1.0");
    for (k, v) in [("bInterfaceClass", "ff"), ("bInterfaceNumber", "00")] {
        w(dif.join(k), v);
    }
    for (n, p) in [("usb1", &usb1), ("1-0:1.0", &if0), ("1-1", &d), ("1-1:1.0", &dif)] {
        symlink(p, sys.join("bus/usb/devices").join(n)).unwrap();
    }
    sys
}

#[test]
fn rules_validate() {
    use crate::routes::validate_rule;
    assert_eq!(validate_rule(&json!({ "match": "vidpid", "value": "0403:6001" })).unwrap().value, "0403:6001");
    assert_eq!(validate_rule(&json!({ "match": "vidpid", "value": "0403:ABCD" })).unwrap().value, "0403:abcd");
    assert_eq!(validate_rule(&json!({ "match": "port", "value": "1-1.2" })).unwrap().kind, "port");
    assert_eq!(validate_rule(&json!({ "match": "port", "value": "usb1" })).unwrap_err().status, 400);
    assert!(validate_rule(&json!({})).is_err());
}
