//! The driver with the simulated devices (BENCHBOB_PROBES_SIM) and, where python3 exists, a
//! stand-in OpenOCD (fixtures/fake-openocd.py) that prints OpenOCD's lines and answers the
//! TCL RPC, so the one-shot and GDB-server paths run end to end.

use std::os::unix::fs::PermissionsExt;

use axum::http::Method;
use benchbob_core::Reply;
use benchbob_core::driver::{WsIn, WsOut};
use benchbob_core::util::{base64_encode, call_route, call_tool, test_ctx};
use tokio::sync::mpsc;

use super::*;
use crate::logic::tests::make_uf2;

const STLINK: &str = "STM32_STLink_066DFF383133524157185615";
const PICO: &str = "Debugprobe_on_Pico__CMSIS-DAP__E6614C311B597D32";
const BMP: &str = "Black_Magic_Probe_v1.10.0_7BB180B4";
const DUMMY: &str = "BenchBob_dummy_probe_SIM0001";
const RP2: &str = "RP2_Boot_E0C9125B0D9B";
const DFU: &str = "STM32_BOOTLOADER_205D35A3424B";

fn have_python() -> bool {
    std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
}

struct T {
    d: Probes,
    ctx: Ctx,
    dir: tempfile::TempDir,
}

/// `openocd`: None = not installed, Some(env) = the fake with extra environment.
async fn start(openocd: Option<&str>) -> T {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let bin = p.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    if let Some(env) = openocd {
        let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake-openocd.py");
        let s = bin.join("openocd");
        // The driver runs it with PATH set to the config's (only `bin` here), so an absolute python.
        let py = benchbob_core::run::which("python3").map(|p| p.display().to_string()).unwrap_or_else(|| "python3".into());
        std::fs::write(&s, format!("#!/bin/sh\n{env}\nexec {py} -u '{}' \"$@\"\n", fake.display())).unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for t in ["target/stm32f1x.cfg", "target/nordic/nrf52.cfg", "target/zzz.cfg"] {
        let f = p.join("scripts").join(t);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "").unwrap();
    }
    std::fs::create_dir_all(p.join("sim")).unwrap();
    std::fs::write(p.join("mounts"), "").unwrap();
    let cfg = Config {
        port_offset: 0,
        sim: p.join("sim").display().to_string(),
        web_port: 8097,
        tmp: p.join("tmp"),
        path: bin.display().to_string(),
        scripts: p.join("scripts"),
        sys: p.join("sys"),
        mounts: p.join("mounts"),
    };
    let d = Probes::with_config(cfg);
    let ctx = test_ctx("probes", p);
    d.init(ctx.clone()).await.unwrap();
    T { d, ctx, dir }
}

async fn call(t: &T, method: Method, path: &str, query: &[(&str, &str)], body: Value) -> Result<Value, HttpError> {
    match call_route(&t.d.routes(), method, path, query, body).await? {
        Reply::Json(v) => Ok(v),
        Reply::Response(_) => panic!("not JSON"),
    }
}

fn free() -> u32 {
    free_port().unwrap()
}

#[tokio::test]
async fn simulated_devices_are_listed() {
    let t = start(None).await;
    let v = call(&t, Method::GET, "/api/probes", &[], Value::Null).await.unwrap();
    assert_eq!(v["openocd"], json!({ "path": "", "version": "", "scripts": t.dir.path().join("scripts").display().to_string() }));
    let ids: Vec<&str> = v["probes"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [STLINK, PICO, BMP, "USB_JTAG_serial_debug_unit_F4_12_FA_5C_3B_A8", DUMMY]);
    let st = &v["probes"][0];
    let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "kind",
            "name",
            "vendor",
            "model",
            "vidpid",
            "serial",
            "usb",
            "note",
            "iface",
            "transports",
            "gdbBuiltIn",
            "espSerial",
            "ttys",
            "sim",
            "config",
            "server",
            "ocd",
            "job",
            "lastJob"
        ]
    );
    assert_eq!(st["name"], "ST-LINK/V2-1");
    assert_eq!(st["model"], "STM32 STLink");
    assert_eq!(st["usb"], "9-1.1");
    assert_eq!(
        st["config"],
        json!({ "target": "generic-cortex-m", "transport": "swd", "speed": 4000, "ports": { "gdb": 3333, "telnet": 4444, "tcl": 6666 }, "effectivePorts": { "gdb": 3333, "telnet": 4444, "tcl": 6666 } })
    );
    // The second probe gets the next slot; the Pi Debug Probe defaults to the RP2040.
    assert_eq!(v["probes"][1]["config"]["target"], "target/rp2040.cfg");
    assert_eq!(v["probes"][1]["config"]["ports"]["gdb"], 3343);
    let bmp = &v["probes"][2];
    assert_eq!((bmp["config"].clone(), bmp["gdbBuiltIn"].clone()), (Value::Null, json!(true)));
    assert_eq!(bmp["ttys"], json!([{ "dev": "/dev/ttyACM90", "iface": "00" }, { "dev": "/dev/ttyACM91", "iface": "02" }]));
    assert_eq!(v["probes"][3]["config"]["speed"], 40000);
    assert_eq!(v["probes"][4]["config"]["target"], "none");
    let boots = v["bootloaders"].as_array().unwrap();
    assert_eq!(boots[0]["id"], RP2);
    assert_eq!(boots[0]["mount"], format!("{}/RPI-RP2", t.dir.path().join("sim").display()));
    assert_eq!(
        boots[0]["info"],
        json!({ "bootloader": "UF2 Bootloader v3.0", "fields": { "Model": "Raspberry Pi RP2", "Board-ID": "RPI-RP2" } })
    );
    assert_eq!(boots[1]["id"], DFU);
    assert_eq!(boots[1]["tool"], json!({ "name": "dfu-util", "pkg": "dfu-util", "path": "" }));
    let s = t.d.status().unwrap();
    assert_eq!(s["summary"], "5 probes · RP2040 BOOTSEL · STM32 DFU bootloader");
    assert_eq!(s.as_object().unwrap().keys().take(3).collect::<Vec<_>>(), ["active", "summary", "openocd"]);

    let tg = call(&t, Method::GET, "/api/probes/targets", &[], Value::Null).await.unwrap();
    assert_eq!(
        tg["presets"],
        json!([
            { "cfg": "generic-cortex-m", "label": "Any Cortex-M (detect)" }, { "cfg": "target/stm32f1x.cfg", "label": "STM32F1" },
            { "cfg": "target/nordic/nrf52.cfg", "label": "nRF52" }, { "cfg": "none", "label": "No target (telnet setup)" },
        ])
    );
    assert_eq!(tg["all"], json!(["target/nordic/nrf52.cfg", "target/stm32f1x.cfg", "target/zzz.cfg"]));
}

#[tokio::test]
async fn config_validation_and_clients() {
    let t = start(None).await;
    let put = |id: &str, b: Value| {
        let t = &t;
        let path = format!("/api/probes/{id}/config");
        async move { call(t, Method::PUT, &path, &[], b).await }
    };
    let e = put(STLINK, json!({ "target": "target/nope.cfg" })).await.unwrap_err();
    assert_eq!(e.message, "Unknown target script \"target/nope.cfg\" (see GET /api/probes/targets)");
    let e = put(PICO, json!({ "transport": "jtag" })).await.unwrap_err();
    assert_eq!(e.message, "Raspberry Pi Debug Probe supports swd only");
    let e = put(STLINK, json!({ "speed": "fast" })).await.unwrap_err();
    assert_eq!(e.message, "speed is in kHz, 1–100000");
    let e = put(STLINK, json!({ "ports": { "gdb": 80 } })).await.unwrap_err();
    assert_eq!(e.message, "gdb port must be 1024–65000");
    let e = put(STLINK, json!({ "ports": { "gdb": 4444 } })).await.unwrap_err();
    assert_eq!(e.message, "The three ports must differ");
    let e = put(DUMMY, json!({ "ports": { "gdb": 3333 } })).await.unwrap_err();
    assert_eq!(e.message, format!("Port clash with ST-LINK/V2-1 ({STLINK})"));
    // A target change brings its own transport and speed; by serial works too.
    let v = put("066DFF383133524157185615", json!({ "target": "target/stm32f1x.cfg", "speed": "0x3e8" })).await.unwrap();
    assert_eq!(v["config"]["target"], "target/stm32f1x.cfg");
    assert_eq!(v["config"]["speed"], 1000);
    assert_eq!(t.ctx.setting(&format!("probe:{STLINK}")).unwrap()["speed"], 1000);
    let e = put("nope", json!({})).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "No probe \"nope\""));

    let c = call(&t, Method::GET, &format!("/api/probes/{STLINK}/clients"), &[("host", "bob.local")], Value::Null).await.unwrap();
    assert_eq!(c["gdb"], "arm-none-eabi-gdb -ex \"target extended-remote bob.local:3333\" build/app.elf");
    assert!(c["platformio"].as_str().unwrap().contains(&format!("http://bob.local:8097/api/probes/{STLINK}/flash")));
    let c = call(&t, Method::GET, &format!("/api/probes/{BMP}/clients"), &[("host", "bob.local")], Value::Null).await.unwrap();
    assert_eq!(c["local"], "target extended-remote /dev/ttyACM90");
    let e = call(&t, Method::POST, &format!("/api/probes/{BMP}/examine"), &[], Value::Null).await.unwrap_err();
    assert_eq!(e.message, "Black Magic Probe is its own GDB server: connect GDB to /dev/ttyACM90");
    let e = call(&t, Method::POST, &format!("/api/probes/{STLINK}/start"), &[], json!({})).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (503, "openocd isn't installed (apt package: openocd)"));
    let e = call_tool(&t.d.tools(), "probes_examine", json!({})).await.unwrap_err();
    assert!(e.message.starts_with("Several probes are connected; give one of: STM32_STLink"));
    let e = call(&t, Method::POST, &format!("/api/probes/{STLINK}/reset"), &[], json!({ "mode": "sideways" })).await.unwrap_err();
    assert_eq!(e.message, "mode must be run, halt or init");
    let e = call(&t, Method::GET, &format!("/api/probes/{STLINK}/memory"), &[("address", "x")], Value::Null).await.unwrap_err();
    assert_eq!(e.message, "address must be a number (hex 0x… or decimal)");
    let e = call(&t, Method::POST, &format!("/api/probes/{STLINK}/command"), &[], json!({ "command": "echo {" })).await.unwrap_err();
    assert_eq!(e.message, "Unbalanced braces in command");
    let e = call(&t, Method::GET, "/api/probes/nobody/log", &[], Value::Null).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "No bootloader device \"nobody\""));
}

#[tokio::test]
async fn uf2_and_dfu_bootloaders() {
    let t = start(None).await;
    let route = |path: &str, q: Vec<(&'static str, &'static str)>, body: Vec<u8>| {
        let t = &t;
        let path = path.to_string();
        async move {
            let routes = t.d.routes();
            let r = routes.iter().find(|r| r.method == Method::POST && benchbob_core::http::match_path(&r.path, &path).is_some()).unwrap();
            let req = benchbob_core::Request {
                method: Method::POST,
                path: path.clone(),
                params: benchbob_core::http::match_path(&r.path, &path).unwrap(),
                query: q.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                headers: Default::default(),
                body: body.into(),
            };
            match (r.handler)(req).await {
                Ok(Reply::Json(v)) => Ok(v),
                Ok(_) => panic!(),
                Err(e) => Err(e),
            }
        }
    };
    let uf2 = make_uf2(4, Some(0xe48bff56), 0x10000000);
    let v = route(&format!("/api/probes/boot/{RP2}/uf2"), vec![("name", "blink build.uf2")], uf2.clone()).await.unwrap();
    let dst = format!("{}/RPI-RP2/blink_build.uf2", t.dir.path().join("sim").display());
    assert_eq!(v["path"], dst);
    assert_eq!(v["rebooted"], false);
    assert_eq!(v["families"], json!(["RP2040"]));
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["ok", "blocks", "familyIds", "families", "start", "end", "payload", "path", "rebooted", "message"]);
    assert_eq!(std::fs::read(&dst).unwrap(), uf2);
    let last = call(&t, Method::GET, &format!("/api/probes/boot/{RP2}/info"), &[], Value::Null).await.unwrap()["lastJob"].clone();
    assert_eq!(
        (last["kind"].clone(), last["stage"].clone(), last["ok"].clone(), last["message"].clone()),
        (json!("uf2"), json!("done"), json!(true), json!("Copied (simulated)"))
    );
    let lk: Vec<&String> = last.as_object().unwrap().keys().collect();
    assert_eq!(lk, ["kind", "stage", "started", "done", "ok", "message"]);
    let log = call(&t, Method::GET, &format!("/api/probes/{RP2}/log"), &[], Value::Null).await.unwrap();
    assert_eq!(log["lines"][0], "UF2: 4 blocks, 1024 bytes for 0x10000000–0x10000400, family RP2040");

    let e = route(&format!("/api/probes/boot/{RP2}/uf2"), vec![], make_uf2(1, Some(0xe48bff59), 0x10000000)).await.unwrap_err();
    assert_eq!(e.message, "This UF2 is for RP2350 ARM-S; a RP2040 would ignore it (pass force to copy anyway)");
    let v =
        route(&format!("/api/probes/boot/{RP2}/uf2"), vec![("force", "1"), ("name", "x.UF2")], make_uf2(1, Some(0xe48bff59), 0x10000000))
            .await
            .unwrap();
    assert!(v["path"].as_str().unwrap().ends_with("/x.uf2"));
    let e = route(&format!("/api/probes/boot/{RP2}/uf2"), vec![], vec![0u8; 100]).await.unwrap_err();
    assert!(e.message.contains("512-byte"));
    let e = route(&format!("/api/probes/boot/{DFU}/uf2"), vec![], uf2.clone()).await.unwrap_err();
    assert_eq!(e.message, "STM32 DFU bootloader doesn't take UF2 files");
    let e = route(&format!("/api/probes/boot/{DFU}/dfu"), vec![], vec![1, 2, 3]).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (503, "dfu-util isn't installed (apt package: dfu-util)"));

    // The MCP tool takes base64.
    let v = call_tool(&t.d.tools(), "probes_uf2_flash", json!({ "device": RP2, "data_base64": base64_encode(&uf2), "name": "mcp.uf2" }))
        .await
        .unwrap();
    assert!(v["path"].as_str().unwrap().ends_with("/mcp.uf2"));
    let e = call_tool(&t.d.tools(), "probes_uf2_flash", json!({ "device": RP2 })).await.unwrap_err();
    assert_eq!(e.message, "Give the firmware as data_base64 or url");
}

#[tokio::test]
async fn websocket_log_stream() {
    let t = start(None).await;
    let sockets = t.d.sockets();
    let open = |id: &str| {
        let (tx, rx) = mpsc::unbounded_channel::<WsOut>();
        let (itx, irx) = mpsc::unbounded_channel::<WsIn>();
        let s = WsSession {
            client: WsClient::new(tx),
            params: [("id".to_string(), id.to_string())].into_iter().collect(),
            query: vec![],
            incoming: irx,
        };
        (s, rx, itx)
    };
    let (s, mut rx, itx) = open("nope");
    (sockets[0].handler)(s).await;
    assert!(matches!(rx.recv().await, Some(WsOut::CloseWith(4004, r)) if r == "No such device"));
    drop(itx);

    let (s, mut rx, itx) = open(RP2);
    let h = tokio::spawn((sockets[0].handler)(s));
    let Some(WsOut::Text(first)) = rx.recv().await else { panic!() };
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap(), json!({ "type": "log", "lines": [], "job": null, "lastJob": null }));
    let v =
        call_tool(&t.d.tools(), "probes_uf2_flash", json!({ "device": RP2, "data_base64": base64_encode(&make_uf2(1, None, 0)) })).await;
    assert!(v.is_ok());
    let mut types = Vec::new();
    while let Ok(Some(WsOut::Text(m))) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        types.push(serde_json::from_str::<Value>(&m).unwrap()["type"].as_str().unwrap().to_string());
    }
    assert_eq!(types.first().map(String::as_str), Some("job"));
    assert!(types.contains(&"line".to_string()));
    drop(itx);
    h.await.unwrap();
    assert!(lock(&t.d.inner.st).boots[RP2].dev.sockets.is_empty());
}

#[tokio::test]
async fn one_shot_actions_and_the_gdb_server_with_a_stand_in_openocd() {
    if !have_python() {
        eprintln!("python3 missing: skipped");
        return;
    }
    let t = start(Some("")).await;
    assert_eq!(lock(&t.d.inner.st).openocd_version, "0.12.0+dev-snapshot (2026-02-16-16:07)");

    // One-shot: a throwaway OpenOCD on a loopback port for the read.
    let m = call(&t, Method::GET, &format!("/api/probes/{DUMMY}/memory"), &[("address", "0x20000000"), ("length", "4")], Value::Null)
        .await
        .unwrap();
    assert_eq!(
        m,
        json!({ "address": 536870912, "length": 4, "hex": "41424344", "dump": format!("20000000  41 42 43 44{}  ABCD", " ".repeat(36)) })
    );
    let p = call(&t, Method::GET, "/api/probes", &[], Value::Null).await.unwrap()["probes"][4].clone();
    assert_eq!(p["server"], Value::Null);
    assert_eq!((p["lastJob"]["kind"].clone(), p["lastJob"]["ok"].clone()), (json!("read memory"), json!(true)));
    assert_eq!(p["ocd"]["voltage"], 3.3);
    let log = call(&t, Method::GET, &format!("/api/probes/{DUMMY}/log"), &[], Value::Null).await.unwrap()["lines"].clone();
    let first = log[0].as_str().unwrap();
    assert!(first.starts_with("$ openocd -f interface/dummy.cfg -c 'transport select jtag' -c 'adapter speed 4000' -c 'bindto 127.0.0.1' -c 'gdb port disabled'"), "{first}");
    assert!(log.as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("-- openocd exited (")));

    // The GDB server.
    let (g, tn, tc) = (free(), free(), free());
    call(&t, Method::PUT, &format!("/api/probes/{DUMMY}/config"), &[], json!({ "ports": { "gdb": g, "telnet": tn, "tcl": tc } }))
        .await
        .unwrap();
    let v = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/start"), &[], json!({})).await.unwrap();
    assert_eq!(v["server"]["state"], "running");
    assert_eq!(v["server"]["ports"], json!({ "gdb": g, "telnet": tn, "tcl": tc }));
    assert_eq!(v["ocd"]["cpus"], json!(["chip.cpu: Cortex-M4 r0p1"]));
    let e = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/start"), &[], json!({})).await.unwrap_err();
    assert_eq!(e.message, "A GDB server is already running for this probe");
    assert_eq!(t.d.status().unwrap()["summary"], format!("5 probes · GDB :{g} Cortex-M4 r0p1 · RP2040 BOOTSEL · STM32 DFU bootloader"));

    // Actions go through the running server's TCL port.
    let x = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/examine"), &[], Value::Null).await.unwrap();
    assert!(x["targets"].as_str().unwrap().contains("chip.cpu"));
    assert_eq!(x["message"], "chip.cpu: Cortex-M4 r0p1");
    let r = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/reset"), &[], json!({ "mode": "halt" })).await.unwrap();
    assert_eq!(r, json!({ "ok": true, "output": "", "message": "reset halt" }));
    let e = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/command"), &[], json!({ "command": "bogus" })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (502, "OpenOCD: invalid command name \"bogus\""));
    let er = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/erase"), &[], Value::Null).await.unwrap();
    assert_eq!(
        er,
        json!({ "ok": true, "driver": "stm32f1x", "command": "stm32f1x mass_erase 0", "message": "Erased with \"stm32f1x mass_erase 0\"" })
    );
    let f = call_tool(
        &t.d.tools(),
        "probes_flash",
        json!({ "probe": DUMMY, "data_base64": base64_encode(&[0u8, 0x20, 0, 0x20]), "name": "app.bin", "address": "0x08000000", "verify": false }),
    )
    .await
    .unwrap();
    assert_eq!(f["kind"], "bin");
    assert_eq!(f["address"], 134217728);
    assert_eq!(f["message"], "Programmed 4 bytes");
    assert!(f["output"].as_str().unwrap().contains("Programming Finished"));
    let log = call(&t, Method::GET, &format!("/api/probes/{DUMMY}/log"), &[], Value::Null).await.unwrap()["lines"].clone();
    assert!(log.as_array().unwrap().iter().any(|l| {
        let l = l.as_str().unwrap();
        l.starts_with("> program {") && l.ends_with(".bin} 0x8000000 reset")
    }));
    // The upload is removed afterwards.
    assert_eq!(std::fs::read_dir(t.dir.path().join("tmp")).unwrap().count(), 0);
    let e = call_tool(&t.d.tools(), "probes_flash", json!({ "probe": DUMMY, "data_base64": base64_encode(&[1, 2]), "name": "a.bin" }))
        .await
        .unwrap_err();
    assert_eq!(e.message, "A .bin file needs a base address (e.g. 0x08000000 for STM32, 0x10000000 for RP2040)");

    let v = call(&t, Method::POST, &format!("/api/probes/{DUMMY}/stop"), &[], Value::Null).await.unwrap();
    assert_eq!((v["server"]["state"].clone(), v["server"]["stopped"].clone()), (json!("exited"), json!(true)));
    let texts: Vec<String> = t.ctx.hub.timeline.list().into_iter().map(|e| e.text).collect();
    assert_eq!(texts, [format!("GDB server for Dummy probe (simulated) on :{g}")]);
    t.d.stop(StopReason::Shutdown).await;
}

#[tokio::test]
async fn openocd_failing_at_init_explains_why() {
    if !have_python() {
        eprintln!("python3 missing: skipped");
        return;
    }
    let t = start(Some("export FAKE_OPENOCD_FAIL=1")).await;
    let e = call(&t, Method::POST, &format!("/api/probes/{PICO}/start"), &[], json!({})).await.unwrap_err();
    assert_eq!(e.status, 502);
    assert_eq!(
        e.message,
        "Error: unable to find a matching CMSIS-DAP device — OpenOCD couldn't open the probe: it was unplugged, something else holds it, or it re-enumerated with a different serial."
    );
    let p = call(&t, Method::GET, "/api/probes", &[], Value::Null).await.unwrap()["probes"][1].clone();
    assert_eq!(p["server"]["state"], "exited");
    assert_eq!(p["server"]["exitCode"], 1);
    let e = call(&t, Method::POST, &format!("/api/probes/{PICO}/halt"), &[], Value::Null).await.unwrap_err();
    assert_eq!(e.status, 502);
    assert_eq!(call(&t, Method::GET, "/api/probes", &[], Value::Null).await.unwrap()["probes"][1]["lastJob"]["stage"], "failed");
}

#[test]
fn small_helpers() {
    assert_eq!(uf2_file_name("dir/sub\\My Blink.UF2"), "My_Blink.uf2");
    assert_eq!(uf2_file_name("blink.bin"), "blink.bin.uf2");
    assert_eq!(uf2_file_name(""), "firmware.uf2");
    assert_eq!(unescape_mount("/media/a\\040b"), "/media/a b");
    assert!(flag(None, true) && !flag(Some("off"), true) && flag(Some("1"), false) && flag(Some(""), true));
    assert_eq!(base36(35), "z");
}
