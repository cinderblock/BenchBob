# BenchBob's Rust backend

The server that replaces the Bun/TypeScript one, panel by panel. One statically linked
binary per architecture, the same API (paths, JSON shapes and key order, SSE and
WebSocket messages, MCP tool names), the same settings files, so either the old dashboard
or the new web UI works against either server.

> **Status:** the foundation, the board layer and the drives (with flashing, rpiboot, the
> file browser, OS detection and the catalog), GPIO, serial, CAN, I²C, 1-Wire, Wi-Fi and
> BLE panels are native. USB, wired network, camera, debug probes, RTL-SDR, GPS and UPS are
> still proxied to the Bun server (see [Proxy mode](#proxy-mode)), which goes away once
> they're ported: the goal is this binary alone, with no Bun at runtime. Until then the
> released `.deb` runs Bun, and releases are on hold until the Rust-only package is ready.

| What | Native here |
| --- | --- |
| Dashboard: `web/dist` at `/` when it exists (the old page at `/legacy`), else `index.html` with `ui/*.js` inlined; build-version stamping and reload | yes |
| `GET /api` docs (examples rewritten to the caller's origin), `/api/status`, `/api/events` (SSE, per-client gzip) | yes |
| `/api/update*` (release check, settings, install), `/mcp` (stateless Streamable HTTP), `/api/periph/driver`, `/api/periph/timeline` | yes |
| `GET /api/boards`: the board files loaded, which one matched, files that failed (new) | yes |
| GPIO panel: `/api/gpio*`, `/ws/gpio`, `gpio_*` MCP tools, plus `POST /api/gpio/line` (new) | yes |
| Drives: the drive fields of `/api/status`, eject, scan, probe, deep scan, erase, flash (with cloud-init / firstrun.sh settings and Install BenchBob), `/api/rpiboot/*`, `/api/blcard*`, `/api/files`, `/api/download`, `/api/osinfo`, `/api/images`, `/api/catalog`, `/api/favorites`, `/api/quicklist`, `/api/defaults` | yes |
| Serial panel: `/api/serial*`, `/ws/serial/:id`, the raw TCP and RFC 2217 bridges, `serial_*` MCP tools, esptool/avrdude flashing | yes |
| CAN panel: `/api/can*`, `/ws/can/:iface`, `can_*` MCP tools, the socketcand server and its UDP beacon | yes |
| I²C panel: `/api/i2c*`, `i2c_*` MCP tools | yes |
| 1-Wire panel: `/api/onewire*`, `onewire_*` MCP tools | yes |
| Wi-Fi panel: `/api/wifi*` (radios, regulatory domain, scanner with watch leases and history, client tests, profiles), `wifi_*` MCP tools | yes |
| BLE panel: `/api/ble*`, `/ws/ble`, `ble_*` MCP tools | yes |
| USB, wired network, camera, probes, RTL-SDR, GPS, UPS | proxied (being ported) |

Responses say who answered (`X-BenchBob-Backend: native` or `legacy`), and `GET /api` has a
`backend` block listing the native routes. Every response carries permissive CORS
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
| `crates/wifi` (`benchbob-wifi`) | The Wi-Fi driver: `iw`/`nmcli` parsers, radios and regulatory channels, the scanner and per-BSS history, the OUI lookup, client tests in a network namespace (wpa_supplicant, udhcpc, ping/DNS/HTTP/iperf3) and their crash recovery |
| `crates/ble` (`benchbob-ble`) | The BLE driver: btmon's btsnoop stream, HCI advertising reports, AD structures, beacon and sensor decoders, the advertiser table, the bluetoothctl discovery session |
| `crates/benchbob` | The binary: HTTP server, UI, status, SSE, update, docs, health, the legacy proxy |
| `boards/` | The shipped board files ([format and script contract](boards/README.md)) |

A new panel is a new crate implementing `benchbob_core::Driver`, added to `drivers()` in
`crates/benchbob/src/lib.rs`. Its id then joins `benchbob --native-drivers`, which is what
Rust mode tells the Bun server to stop running.

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

The drives crate's tests replay the TypeScript server itself: `bun
backend/crates/drives/fixtures/harness.ts` (from the repository root) cuts the sections
under test out of server.ts, and loads rpiboot.ts and osinfo.ts, behind mocks of every
command, file, fetch and clock; it records each command (with its stdin), stream read
and file check alongside the operation's state, and writes `fixtures/*.expected.json`.
The Rust tests run the same scenarios through `sys::mock` and must produce the same log
and outcome, and the same cloud-init and firstrun.sh files byte for byte. Rerun the
harness after changing the TypeScript it covers.
The CAN tests that need a bus run on a vcan named by `BENCHBOB_CAN_TEST_VCAN` (create it once
as root: `ip link add dev vcanbbtest0 type vcan && ip link set vcanbbtest0 mtu 72 up`;
the tests themselves need no privileges) and are skipped without it.

CI runs those on x86-64 and arm64, builds every target, smoke-tests the x86-64 binary,
and installs a `.deb` built with it to switch Rust mode on and off.

## Run

```sh
PORT=8091 BENCHBOB_STATE_DIR=/tmp/bb BENCHBOB_ROOT=/path/to/checkout ./benchbob
```

| Variable | |
| --- | --- |
| `PORT` | listen port (default 80), IPv6 and IPv4 |
| `BENCHBOB_STATE_DIR` | settings (else `$STATE_DIRECTORY`, else the app root), the same files as the Bun server's |
| `BENCHBOB_ROOT` | where `index.html`, `ui/`, `web/dist` and `VERSION` are (default: the nearest directory above the executable with an `index.html`, e.g. `/usr/lib/benchbob`) |
| `BENCHBOB_LEGACY_PORT` | proxy every route this server doesn't have to the Bun server on `127.0.0.1:<port>` |
| `BENCHBOB_BOARD` | force a board id |
| `BENCHBOB_BOARDS_DIR`, `BENCHBOB_BOARDS_D` | other board directories (shipped / user) |
| `BENCHBOB_PORT_OFFSET` | shift every peripheral listener port (socketcand: 29536 + offset) |
| `BENCHBOB_CAN_REC_DIR` | where CAN recordings go (default `/var/tmp/benchbob-can`, capped at 256 MB each) |
| `BENCHBOB_PORT_OFFSET` | shift every peripheral listener port |
| `BENCHBOB_W1_ROOT` | read 1-Wire from a fixture tree instead of /sys/bus/w1/devices (`BENCHBOB_W1_FIXTURE_OUT=<dir> cargo test -p benchbob-onewire write_fixture_tree` writes one) |
| `BENCHBOB_CONFIG_TXT`, `BENCHBOB_CMDLINE_TXT`, `BENCHBOB_BOOT_ENV`, `BENCHBOB_OVERLAY_DIR` | point the boot-config editors at test copies (reboot is then refused) |
| `BENCHBOB_FLASH_DIR` | where the serial panel stages firmware files (default `/var/tmp/benchbob-serial`; a side instance can't write the packaged one's) |
| `RUST_LOG` | log filter (default `info`) |

The old `DESKYPI_*` names still work. `benchbob --native-drivers` prints the ids of the
drivers this build runs itself.

## Proxy mode

With `BENCHBOB_LEGACY_PORT`, any route not served here (HTTP, and WebSocket upgrades) is
streamed to the Bun server. The documents that span every panel merge both servers'
parts: `/api/status` (`peripherals` lists both servers' drivers in dashboard order), `/api/events` (it follows Bun's stream
for `periph` events), `/api`, MCP `tools/list` and `tools/call`, and the timeline. The
build version folds in Bun's, so a page reloads when either server changes.

The Bun server runs with `BENCHBOB_NATIVE_DRIVERS=<ids>` (from `benchbob
--native-drivers`), which makes it skip those drivers and keep their entries in
`peripherals.json` as this server wrote them; this server, in turn, only replaces its own
drivers' entries. Both use the same state directory.

In the package (built with `RUST_BIN=… packaging/build-deb.sh`), `sudo benchbob-setup rust
on` enables `benchbob-rust.service` on port 80 and moves `benchbob.service` to port 8081
with that variable (a drop-in, refreshed on every upgrade); `sudo benchbob-setup rust off`
undoes it. The Bun server still listens on every interface on 8081 (it has no
bind-address setting).

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
  (`peripherals/lib/radios-fixtures/ble-scan.btsnoop`, or `BENCHBOB_BLE_CAPTURE`) when it is
  there. A truncated payload decodes to nothing rather than ending the scan (the TypeScript
  threw).
