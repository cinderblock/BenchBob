# BenchBob's server

BenchBob's backend, in Rust: one statically linked binary per architecture, which is all
the package runs (no Bun, no Node). It replaced the Bun/TypeScript server panel by panel
and kept its API (paths, JSON shapes and key order, SSE and WebSocket messages, MCP tool
names) and its settings files, so an upgrade from a Bun-based release (0.4.x and earlier)
keeps everything.

Every panel is here: drives (with flashing, rpiboot, the file browser, OS detection and the
catalog), GPIO, serial, CAN, I²C, 1-Wire, USB, wired network, Wi-Fi, BLE, camera, debug
probes, RTL-SDR, GPS and UPS.

What it serves:

- Dashboard: `web/dist` at `/` (a page saying how to build it when there's none), build-version stamping and reload
- `GET /api` docs (examples rewritten to the caller's origin), `/api/status`, `/api/events` (SSE, per-client gzip)
- `/api/update*` (release check, settings, install), `/mcp` (stateless Streamable HTTP), `/api/periph/driver`, `/api/periph/timeline`
- `GET /api/boards`: the board files loaded, which one matched, files that failed (new)
- `POST /api/onboard-leds`, `onboardLeds` in the status, MCP `onboard_led`: light the board's own LEDs (`/sys/class/leds`, a Pi's ACT and PWR) while the dashboard's hold button is held, then restore their kernel trigger (new; same API as the nodes')
- GPIO panel: `/api/gpio*`, `/ws/gpio`, `gpio_*` MCP tools, plus `POST /api/gpio/line` (new)
- Drives: the drive fields of `/api/status`, eject, scan, probe, deep scan, erase, flash (with cloud-init / firstrun.sh settings and Install BenchBob), `/api/rpiboot/*`, `/api/blcard*`, `/api/files`, `/api/download`, `/api/osinfo`, `/api/images`, `/api/catalog`, `/api/favorites`, `/api/quicklist`, `/api/defaults`
- Serial panel: `/api/serial*`, `/ws/serial/:id`, the raw TCP and RFC 2217 bridges, `serial_*` MCP tools, esptool/avrdude flashing
- CAN panel: `/api/can*`, `/ws/can/:iface`, `can_*` MCP tools, the socketcand server and its UDP beacon
- I²C panel: `/api/i2c*`, `i2c_*` MCP tools
- 1-Wire panel: `/api/onewire*`, `onewire_*` MCP tools
- LED strips panel (new): `/api/leds*`, `leds_*` MCP tools, the nodes' API and string model (`firmware/proto`, `firmware/pixels`). WS2812-family strings over spidev (a Pi's SPI0/SPI1, SPI3–6 on a Pi 4) or a Pi 1–4's PWM + DMA (GPIO12/18, 13/19; `benchbob led-pwm-helper`, this binary run as root through `sudo -n`, owns /dev/mem and /dev/vcio), APA102/WS2801 over spidev or bit-banged on any free header GPIO; the pins show as taken in the GPIO panel; configs persist in peripherals.json. `BENCHBOB_SPIDEV_DIR` looks for spidev devices elsewhere (tests)
- USB panel: `/api/usb*`, `usb_*` MCP tools (device tree, hotplug and kernel timeline, uhubctl port power, USB/IP and usbredir sharing)
- Wired network panel: `/api/netdiag*`, `net_*` MCP tools (ethtool link, receive-only LLDP, iperf3, ping/DNS/HTTP/MTU checks)
- Wi-Fi panel: `/api/wifi*` (radios, regulatory domain, scanner with watch leases and history, client tests, profiles), `wifi_*` MCP tools
- BLE panel: `/api/ble*`, `/ws/ble`, `ble_*` MCP tools
- Camera panel: `/api/camera*` (the MJPEG stream proxied, snapshots), `camera_*` MCP tools, on-demand ustreamer per camera
- Debug probes panel: `/api/probes*`, `/ws/probes/:id`, `probes_*` MCP tools, OpenOCD GDB servers and one-shot sessions, UF2 and DFU flashing
- RTL-SDR panel: `/api/sdr*`, `sdr_*` MCP tools, rtl_tcp and rtl_433
- GPS panel: `/api/gps*`, `gps_*` MCP tools, a temporary gpsd
- UPS panel: `/api/ups*`, `ups_status`, a private NUT

`GET /api` has a `backend` block: the server, its version and its drivers. Every response
carries permissive CORS
(`Access-Control-Allow-Origin: *`, no credentials; preflights and Chrome's Private
Network Access are answered for every path), so a desktop app on another origin can talk
to any bob.

## Layout

| Crate | What |
| --- | --- |
| `crates/core` (`benchbob-core`) | The `Driver` trait (routes, WebSockets, MCP tools, status), the registry, `peripherals.json` settings (merged on write), udev hotplug, the event timeline, MCP, child processes, the state directory |
| `crates/boards` (`benchbob-boards`) | The board file format, loading (`/usr/share/benchbob/boards`, then `/etc/benchbob/boards.d`) and detection |
| `crates/drives` (`benchbob-drives`) | Drives and the Pi boot tools: block devices, the operation queue, erase, flash and flash customisation, the raw probe and deep scan, rpiboot and bootloader cards, the image catalog and saved lists, the file browser, OS detection (a read-only registry hive reader for BCD/SOFTWARE/SYSTEM). An unlisted driver: no `peripherals` row, can't be disabled, adds top-level status fields |
| `crates/gpio` (`benchbob-gpio`) | The GPIO driver: gpiochip v2 uAPI (with output requests), passive level readers (BCM2711 gpiomem, RP1), config.txt and boot-overlay editors, board scripts |
| `crates/serial` (`benchbob-serial`) | The serial driver: termios2 ports, the ring buffer and fan-out, TCP / RFC 2217 / WebSocket bridges, auto-baud, DTR/RTS resets, esptool/avrdude jobs |
| `crates/can` (`benchbob-can`) | The CAN driver: SocketCAN raw sockets (libc), `ip link` config, auto-bitrate, vcan, SLCAN, per-ID table, captures, periodic jobs, candump recordings, the socketcand server |
| `crates/i2c-chips` (`benchbob-i2c-chips`) | I²C chip identification and decoders (~40 chips), the `I2cIo` bus trait and a FakeBus simulator; `no_std` + `alloc`, so the firmware nodes can use it too |
| `crates/i2c` (`benchbob-i2c`) | The I²C driver: /dev/i2c-N ioctls, i2cdetect-style scans, polling, raw register tools |
| `crates/onewire` (`benchbob-onewire`) | The 1-Wire driver: w1 sysfs masters and slaves, family decoders, DS2482 bridges, the iButton touch log, a fixture tree |
| `crates/usb` (`benchbob-usb`) | The USB driver: the sysfs device tree with udev nodes and panel hints, hotplug and `dmesg -W` events, uhubctl power with the ganged/internal-link/protection planner, USB/IP (usbipd) and usbredir sharing, auto-share rules |
| `crates/netdiag` (`benchbob-netdiag`) | The wired-network driver: ethtool link/EEE/error counters, a private receive-only lldpd, the iperf3 server and client with presets and logins, ping, a small DNS client for per-resolver timing, HTTP (curl) and path-MTU checks |
| `crates/wifi` (`benchbob-wifi`) | The Wi-Fi driver: `iw`/`nmcli` parsers, radios and regulatory channels, the scanner and per-BSS history, the OUI lookup, client tests in a network namespace (wpa_supplicant, udhcpc, ping/DNS/HTTP/iperf3) and their crash recovery |
| `crates/ble` (`benchbob-ble`) | The BLE driver: btmon's btsnoop stream, HCI advertising reports, AD structures, beacon and sensor decoders, the advertiser table, the bluetoothctl discovery session |
| `crates/camera` (`benchbob-camera`) | The camera driver: UVC capture nodes, v4l2-ctl formats and controls, one ustreamer per camera on demand, the MJPEG proxy over its unix socket, libcamera's CSI list |
| `crates/probes` (`benchbob-probes`) | The debug probes driver: the probe/bootloader table, OpenOCD command lines, log parsing and hints, the TCL RPC client, GDB servers and one-shot sessions, UF2 and dfu-util flashing |
| `crates/sdr` (`benchbob-sdr`) | The RTL-SDR driver: stick table, DVB-driver check, rtl_tcp / rtl_433 state machine, rtl_433 sensor table, rtl_test/rtl_eeprom probing |
| `crates/gps` (`benchbob-gps`) | The GPS driver: candidate ttys and their holders, gpsd as a child, the JSON watch client and fix/sky/PPS fold |
| `crates/ups` (`benchbob-ups`) | The UPS driver: NUT's udev table, HID Power Device detection, private NUT driver + upsd, upsc polling, status decoding and power events |
| `crates/benchbob` | The binary: HTTP server, UI, status, SSE, update, docs, health |
| `boards/` | The shipped board files ([format and script contract](boards/README.md)) |

A new panel is a new crate implementing `benchbob_core::Driver`, added to `drivers()` in
`crates/benchbob/src/lib.rs`.

Every child process goes through `benchbob_core::child` (`Ctx::spawn`, `run::run`, or
`CommandExt::spawn_guarded`): when its handle is dropped while it runs (an aborted task, a
`run()` timeout), it gets SIGTERM, then SIGKILL after a three-second grace. Never a bare
SIGKILL or tokio's `kill_on_drop`: for `sudo -n prog` that kills sudo only and leaves `prog`
running as root.

## Build

No C dependencies: static musl binaries link with the `rust-lld` that ships with Rust,
against the musl CRT the targets' `rust-std` carries (`.cargo/config.toml`). Plain cargo
from any Linux host (CI uses Ubuntu); Windows and macOS hosts work too.

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
  armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf
cargo build --release --locked --target aarch64-unknown-linux-musl -p benchbob
# → target/aarch64-unknown-linux-musl/release/benchbob, about 4 MB (2 MB gzipped)
```

| Target | For |
| --- | --- |
| `x86_64-unknown-linux-musl` | PCs, VMs |
| `aarch64-unknown-linux-musl` | 64-bit ARM: Pi 3/4/5, Orange Pi, Radxa, BeagleY-AI… |
| `armv7-unknown-linux-musleabihf` | ARMv7 hard-float: BeagleBone Black/Green, Pi 2 |
| `arm-unknown-linux-musleabihf` | ARMv6 hard-float: Pi Zero W, Pi 1; also runs on every ARMv7, so an armhf package would ship this one |

The GitHub release check shells out to `curl` (a dependency of the package) rather than
carrying a TLS stack.

## Test

```sh
cargo test --workspace          # unit tests, with the TypeScript tests' fixtures
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

The drives crate's tests replay what the TypeScript server did: a harness ran server.ts,
rpiboot.ts and osinfo.ts behind mocks of every command, file, fetch and clock, and recorded
each command (with its stdin), stream read and file check alongside the operation's state
in `crates/drives/fixtures/*.expected.json`. The Rust tests run the same scenarios through
`sys::mock` and must produce the same log and outcome, and the same cloud-init and
firstrun.sh files byte for byte. The harness went with the TypeScript, so those files are
frozen: change one by hand, deliberately, when a change means to alter that behaviour.
The CAN tests that need a bus run on a vcan named by `BENCHBOB_CAN_TEST_VCAN` (create it once
as root: `ip link add dev vcanbbtest0 type vcan && ip link set vcanbbtest0 mtu 72 up`;
the tests themselves need no privileges) and are skipped without it.
The GPS driver's test against gpsd itself runs when `BENCHBOB_TEST_GPSD` names a gpsd
binary (or a wrapper script), with `crates/gps/fixtures/nmea-feeder.py` as the receiver on a
pty; the probes tests that drive the stand-in OpenOCD need python3.

Core's test that a dropped `sudo` child's program dies needs passwordless sudo, and is
skipped without it.

CI runs those on x86-64 and arm64 (where the sudo test runs), builds every target,
smoke-tests the x86-64 binary, packages amd64, arm64 and armhf, installs the first two and
tests their upgrade from 0.4.1 (`packaging/test-upgrade.sh`).

## Run

Installed, it is `benchbob.service`: `/usr/lib/benchbob/bin/benchbob` as the `benchbob` user
on port 80, settings in `/var/lib/benchbob`, environment from `/etc/default/benchbob`.
From a checkout (build the web UI first, `cd web && bun run build`, or `/` only says how):

```sh
cargo build --release -p benchbob
PORT=8091 BENCHBOB_PORT_OFFSET=1000 BENCHBOB_STATE_DIR=/tmp/bb target/release/benchbob
```

A side instance next to an installed one wants its own `PORT`, `BENCHBOB_PORT_OFFSET` and
`BENCHBOB_STATE_DIR`. It finds `web/dist` in the checkout above the binary; with
`CARGO_TARGET_DIR` elsewhere, set `BENCHBOB_ROOT` to the checkout.

| Variable | |
| --- | --- |
| `PORT` | listen port (default 80), IPv6 and IPv4 |
| `BENCHBOB_STATE_DIR` | settings (else `$STATE_DIRECTORY`, else the app root), the same files as the Bun server's |
| `BENCHBOB_ROOT` | where `web/dist` and `VERSION` are (default: the nearest directory above the executable with a `web/` directory: `/usr/lib/benchbob`, or the checkout) |
| `BENCHBOB_BOARD` | force a board id |
| `BENCHBOB_BOARDS_DIR`, `BENCHBOB_BOARDS_D` | other board directories (shipped / user) |
| `BENCHBOB_PORT_OFFSET` | shift every peripheral listener port (socketcand: 29536 + offset; usbipd 3240, usbredir 4200 + n and the default iperf3 server port 5201, each + offset) |
| `BENCHBOB_CAN_REC_DIR` | where CAN recordings go (default `/var/tmp/benchbob-can`, capped at 256 MB each) |
| `BENCHBOB_LEDS_ROOT` | onboard LEDs from a fixture directory instead of /sys/class/leds |
| `BENCHBOB_W1_ROOT` | read 1-Wire from a fixture tree instead of /sys/bus/w1/devices (`BENCHBOB_W1_FIXTURE_OUT=<dir> cargo test -p benchbob-onewire write_fixture_tree` writes one) |
| `BENCHBOB_CONFIG_TXT`, `BENCHBOB_CMDLINE_TXT`, `BENCHBOB_BOOT_ENV`, `BENCHBOB_OVERLAY_DIR` | point the boot-config editors at test copies (reboot is then refused) |
| `BENCHBOB_FLASH_DIR` | where the serial panel stages firmware files (default `/var/tmp/benchbob-serial`; a side instance can't write the packaged one's) |
| `BENCHBOB_CAMERA_SIMULATE` | add a simulated MJPEG camera "sim" on this device path (ustreamer serves its NO SIGNAL frame when it can't open it) |
| `BENCHBOB_PROBES_SIM` | add simulated probes and bootloaders (one is OpenOCD's dummy adapter; the RP2 BOOTSEL drive is a directory under this scratch path) |
| `BENCHBOB_UPS_SIMULATE` | run NUT's dummy-ups on this `.dev` file as a UPS (edit `ups.status:` in it to drive OL → OB → LB) |
| `RUST_LOG` | log filter (default `info`) |

The old `DESKYPI_*` names still work.

## CAN notes

- One raw socket per interface feeds the stats, the dashboard, captures and recordings. It
  waits for readiness instead of polling, then gathers for 5 ms while traffic flows.
  Kernel RX timestamps, own-frame echoes (`dir: "tx"`) and the drop counter come from
  recvmsg; timestamps use SO_TIMESTAMP_NEW, so 32-bit ARM builds read them right.
- `ip link` changes, vcan and slcand run through `sudo -n`, as the Bun server's do.
- On desky (Pi 4, 1k frames/s on a vcan) it used 3.0 % of a core idle and 3.9 % with a
  dashboard watching, against Bun's 4.2 % and 5.1 %.

## GPIO notes

- **Output on every board:** `POST /api/gpio/line` with `{chip, line}` or `{pin}` (a
  physical header pin) and `mode: high | low | input | release`, `pull: up | down | none`.
  It needs output control on (`POST /api/gpio/output-control {enabled: true, confirm:
  true}`, off by default, the same switch as the Pi's), requests the line as a v2 uAPI
  output and holds it, and refuses lines anyone else owns. Turning output control off (or
  the driver) releases every driven line; a service stop leaves them as they are. What a
  released pin does is up to the chip's driver: on desky's BCM2711 (Linux 6.18) it stays
  an output at its last value. On a Raspberry Pi the header bank is still driven through
  `pinctrl` (`POST /api/gpio/:gpio`), which writes the registers and holds nothing.
- **RP1 (Pi 5):** the `rp1` level reader follows the RP1 datasheet and Raspberry Pi's
  pinctrl, and has never run on hardware. `GET /api/gpio` reports it in `levelReader`
  (`tested: false`).
- Consumers come from the uAPI, not from spawning `gpioinfo`.

## Serial notes

- Same API and behaviour as `peripherals/drivers/serial.ts`; the tests run the driver
  against pseudo-terminals (`crates/serial/tests/pty.rs`).
- Reads and writes go through tokio's `AsyncFd` on the O_NONBLOCK tty, so a full
  transmit buffer waits for the tty to drain instead of polling every 5 ms.
- `wait` in `POST /api/serial/:id/write` and `serial_write` is compiled with the `regex`
  crate (flags i, m, s; `$` in multi-line mode also ends a line before `
`). Lookaround
  and backreferences aren't supported and answer 400 "Bad wait regex".
- Firmware given by URL is fetched with `curl`, like the update check.
- When the flash tool can't be started at all, the port is reopened (the TypeScript left it
  closed).

## Camera, probes, RTL-SDR, GPS and UPS notes

- Each starts its tool only when asked and stops it when done: ustreamer per camera (idle
  stop after a minute unless shared), OpenOCD per probe (GDB server, or a throwaway loopback
  one for a single action), rtl_tcp or rtl_433, gpsd, and NUT's driver + upsd as root via
  `sudo -n` with config in `/tmp/benchbob-nut-<PORT>`. Their ports shift with
  `BENCHBOB_PORT_OFFSET`: camera sharing 8080+, OpenOCD 3333/4444/6666 (+10 per probe), rtl_tcp
  1234, gpsd 2947, upsd 3493.
- Children are `benchbob_core::proc::Proc`: their output is read line by line, and an exit
  is handled only after the last line has been read, so "exited: why" messages are complete.
- ustreamer's HTTP API is spoken over its unix socket with hyper; a viewer leaving drops the
  upstream connection, which is what lets the idle stop see the camera unused.
- Native route bodies may be up to 64 MB (the probes' firmware upload limit); larger ones
  answer 413.
- The tests use stand-ins where the tool would need hardware: a script plus the test
  answering ustreamer's HTTP API, stand-in rtl_* and gpsd scripts with the captured
  fixtures, and `crates/probes/fixtures/fake-openocd.py` for OpenOCD's output and TCL RPC
  (skipped without python3).

## I²C and 1-Wire notes

- **I²C scans** use i2cdetect's auto mode (read-byte at 0x30–0x37 and 0x50–0x5F, quick-write
  elsewhere) and skip addresses a kernel driver owns. Identification sends only register
  pointers and read commands, in the chip table's order (pure reads first where a pointer
  would be a command). Auto-polling runs read-only decoders; Measure and auto-measure may
  write; raw writes and write transfers over REST need `confirm: true` (the MCP tools
  don't ask; their descriptions say they change device state).
- The chip table (`benchbob-i2c-chips`) builds without std; the firmware can implement
  `I2cIo` over its own I²C peripheral and reuse every probe and decoder.
- **1-Wire** root-only sysfs writes (search, bulk read, outputs, resolution, DS2482
  bind/unbind) fall back to `sudo -n tee`, as the Bun server's do.

## USB notes

- **Shares survive a restart:** a service stop leaves devices bound to usbip-host (the kernel
  keeps established attachments) and the next start adopts every binding and starts usbipd
  again. Only turning the USB panel off unshares. Power switching and sharing refuse, or ask
  for `confirm: "ganged" | "unsafe"`, exactly as the Bun server does.
- usbipd, usbredirect and the `dmesg -W` follower run through `sudo -n` and are stopped with
  SIGTERM, which sudo passes on (a SIGKILL would orphan them).

## Wired network notes

- lldpd runs receive-only with its control socket in `/tmp/benchbob-lldpd-<PORT>`, so a side
  instance never touches the live one's. Don't run two on one NIC anyway: lldpd joins the
  LLDP multicast group with SIOCADDMULTI, and when the second one exits it drops the group
  for both. This server notices (lldpd up two minutes, a linked port without
  `01:80:c2:00:00:0e` in `/proc/net/dev_mcast`) and restarts its lldpd, at most every ten
  minutes.
- The DNS check sends its own A queries to each resolver (UDP, TCP when truncated) with
  c-ares's error codes; the HTTP check uses `curl` and stops it once the headers arrive.
- Slow checks (ping, DNS, HTTP, MTU, `iperf` with `wait`) answer at once and send a space
  every 4 s until the JSON follows, as the Bun server's `slowJson` does.

## Wi-Fi notes

- Same API and behaviour as `peripherals/drivers/wifi.ts`. Scans never change an existing
  connection; a client test moves the whole phy into its own network namespace
  (`benchbob-wt-<n>`, with the owning process's pid in `/etc/netns/<ns>/benchbob-owner`),
  so its address, routes and DNS never reach the main namespace, and gives it back
  afterwards. A radio is only borrowed when it carries no default route, or a lower-metric
  default route via another interface that is up exists (the TypeScript's rule; on desky
  that means wlan0, the backup uplink, *can* be borrowed while eth0 is up).
- At start the driver returns phys from leftover `benchbob-wt-*` and `deskypi-wt-*`
  namespaces whose owner is gone, deletes them, removes stale `benchbob-wifi-*` /
  `deskypi-wifi-*` temp dirs (they hold a key), and tells NetworkManager again to leave
  dongles alone.
- The OUI list is read with CRLF line ends too (IEEE's and Debian's oui.txt have them; the
  Bun server found no vendors in it).
- Tested on desky against `mac80211_hwsim` (two simulated radios, a WPA2-PSK access point
  run by wpa_supplicant in AP mode with busybox udhcpd/httpd and iperf3 in its own
  namespace): pass, wrong key, cancel, a killed server and a legacy leftover namespace.
  `BENCHBOB_WIFI_CAPTURE=<dir> cargo test -p benchbob-wifi capture_dump -- --ignored` dumps
  the parsers' results for captured `iw` output, to diff against wifi-parse.ts.

## BLE notes

- Same API and behaviour as `peripherals/drivers/ble.ts`: `sudo btmon -w <fifo>` plus a
  `bluetoothctl` discovery session with `duplicate-data on`; the radio's rfkill block and
  power are put back afterwards. The FIFO is opened read-write, so the read ends when btmon
  exits.
- The decoder tests use the gitignored 10 s capture from desky
  (`crates/ble/fixtures/ble-scan.btsnoop`, or `BENCHBOB_BLE_CAPTURE`) when it is there. A truncated payload decodes to nothing rather than ending the scan (the TypeScript
  threw).
