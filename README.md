# BenchBob

A bench server with a web dashboard, for a Raspberry Pi or any other Linux box. Plug
things into it and use them from anywhere on the LAN, from a browser, a script or an AI
agent.

> **No login.** Anyone who can reach BenchBob can use the dashboard, the API and MCP to
> erase and flash drives, change the boot config, reboot, and drive GPIO and USB power.
> Run it only on a network you trust.

- **Drives**: USB disks are auto-mounted and shared read-only over NFS, NBD and SMB. The
  dashboard can browse and download files, identify what's installed (Windows, Linux,
  macOS, Pi OS, EFI loaders), erase, deep-scan, flash Raspberry Pi OS images with
  preconfigured settings (optionally one that installs BenchBob on its first boot, so a
  fresh card becomes another BenchBob), and write bootloader-update SD cards. On a
  machine that boots from the network (desky: BootBob, root over NBD), the SD slot is a
  drive bay too; the disk the machine runs from and built-in eMMC are never offered.
- **Compute Modules / USB boot**: rpiboot mass-storage mode, EEPROM checks and rewrites.
- **Peripherals**: serial, CAN, I²C, 1-Wire, GPIO, Wi-Fi, BLE, cameras, debug probes,
  RTL-SDR, GPS, UPS, USB power and sharing, and wired-network diagnostics (below).

Everything the dashboard does is also a REST endpoint (`GET /api` lists them), and the
peripheral operations are MCP tools at `POST /mcp`.

To find which board on the bench is which, hold the **LED button** in the dashboard's
header: the board's own LEDs (a Pi's ACT and PWR, a node's status LED) stay lit while
you hold it and go back to normal when you let go.

## Peripherals

Each device class is a panel. A panel appears on the dashboard when it has something to
show (a device plugged in, a capture running). Idle panels fold into the **Tools** strip,
where you can open them. **Peripherals…** turns whole panels off (GPIO, say), which also
removes them from the API and MCP. Turning the USB panel off also stops sharing its devices;
restarting BenchBob doesn't, so remote USB/IP clients stay attached across a deploy.

| Panel | What it does |
| --- | --- |
| USB | Device tree with speed, power and bound drivers; hotplug + kernel timeline; per-port power switching (uhubctl); share any device over the network with USB/IP or usbredir |
| Serial | Terminal (ANSI, hex, raw keys); any baud/framing/flow; auto-baud; the Pi 4's extra onboard UARTs (UART2–5) once enabled; DTR/RTS resets; network bridges (raw TCP, RFC 2217, WebSocket); esptool/avrdude flashing |
| CAN bus | SocketCAN and SLCAN adapters, vcan; bitrate/FD config and listen-only auto-bitrate; per-ID table, frame log, send and periodic jobs, candump recordings; socketcand server for python-can and SavvyCAN |
| I²C | Bus scan using i2cdetect's safe defaults; ~40 chips identified and decoded into readings; raw register access (writes need confirmation); per-bus clock speed, with the bus speed and extra buses settable from the panel |
| 1-Wire | DS18B20 and every other common family, iButton touch log, DS2482 bridges; move or turn off the GPIO master from the panel |
| GPIO | Raspberry Pi: live 40-pin header (function, level, pull, kernel owner); optional output control; staged `config.txt` changes with a diff, backup and reboot: I²C1 and its speed, SPI0 (plus the fixed core clock and larger spidev transfers that LED strips on SPI need), UART, 1-Wire, and on a Pi 4 extra I²C3–6, UART2–5 and a software i2c-gpio bus. Other boards: the header mapped onto the kernel's GPIO lines (name, owner, direction, alternate functions) where the board has a map, every gpiochip's lines (USB adapters included), levels for lines you choose to watch, and staged boot-overlay changes (armbianEnv.txt, orangepiEnv.txt, extlinux.conf). A change that puts two interfaces on one pin is refused |
| LED strips | Addressable LED strings: WS2812B, SK6812 (RGBW too), WS2813, WS2811 and custom timing on a Pi's SPI MOSI pins, or on a Pi 1–4's PWM pins (GPIO12/18, 13/19, PWM + DMA through a small root helper); APA102/SK9822 and WS2801 on SPI or any free GPIO. Any colour order, brightness, gamma, power budget; patterns (rainbow, chase, twinkle, order test, ruler…), pixels set through the API, or a show controller over sACN, Art-Net or DDP (xLights, WLED, QLC+…), which takes over while it sends and hands back to the pattern when it stops. The same API and panel as the BenchBob nodes; lit only while GPIO output control is on |
| Wi-Fi | Scanner across every band the radio supports, with channel charts and an hour of per-network signal history (scans every 30 s while the panel is open); client tests (associate, DHCP, ping/DNS/HTTP/iperf3) run in an isolated network namespace |
| Bluetooth LE | Advertiser scanner with decoded beacons (iBeacon, Eddystone, BTHome, Ruuvi, Xiaomi, Govee, Apple, Microsoft) |
| Camera | UVC webcams and HDMI capture sticks; controls; on-demand MJPEG stream and snapshots |
| Debug probes | ST-Link, J-Link, CMSIS-DAP, ESP-Prog and more: OpenOCD GDB server per probe, flash/erase/reset/read memory; UF2 flashing for Pico BOOTSEL |
| RTL-SDR | rtl_tcp server, or rtl_433 with a decoded-sensor table |
| GPS | Temporary gpsd: fix, sky plot, constellations, clock offset, PPS |
| UPS | USB UPSes via a temporary NUT: charge, runtime, load, power events |
| Wired network | Link details, which switch and port the Pi is on (LLDP/CDP), iperf3 server and client with quick-pick hosts and one-click profiles; ping buttons per target; DNS/HTTP/MTU checks. Host lists are editable ("Edit hosts…"), and "Logins…" holds the username, password and public key for servers that require a login (iperf3 `--authorized-users-path`). The far end needs `iperf3 -s` on 5201 |

Daemons that some panels use (gpsd, lldpd, NUT, iperf3, usbipd, OpenOCD, ustreamer) are
started on demand with private configs and stopped with BenchBob. Their system services
stay masked.

### Network endpoints

Apart from the dashboard and socketcand, nothing listens on the LAN until you turn it on
(gpsd and NUT run local-only until shared). Defaults, each changeable in its panel:

| Port | What |
| --- | --- |
| 80 | Dashboard, REST API, WebSockets, MCP |
| 4000 + n | Serial raw TCP bridge (n fixed per serial device) |
| 4100 + n | Serial RFC 2217 bridge (`rfc2217://benchbob.local:4100`) |
| 4200 + n | usbredir |
| 3240 | USB/IP (`usbip list -r benchbob.local`) |
| 29536 | socketcand (on by default); UDP 42000 discovery beacon (optional) |
| 3333 / 4444 / 6666 | OpenOCD GDB / telnet / TCL, +10 per additional probe |
| 1234 | rtl_tcp |
| 2947 | gpsd (local-only unless shared) |
| 3493 | NUT upsd (local-only unless shared) |
| 5201 | iperf3 server |
| 8080 | Camera direct stream (when shared directly) |

There is no authentication: BenchBob assumes a trusted LAN.

The examples here use `benchbob.local`: substitute your machine's name or address.

### For agents (MCP)

Point an MCP client at `http://benchbob.local/mcp` (Streamable HTTP, stateless). Every panel
contributes tools, e.g. `serial_write` (with a wait-for-regex), `can_capture`,
`i2c_scan`, `onewire_list`, `wifi_scan`, `camera_snapshot` (returns the image),
`probes_flash`, `usb_power`. With Claude Code:

```sh
claude mcp add --transport http benchbob http://benchbob.local/mcp
```

## Layout

- `backend/`: the server, in Rust: one crate per panel plus the core (driver registry, udev
  hotplug, settings, MCP), drives/flashing/rpiboot, and the HTTP server that ties them
  together; `backend/boards/` holds the board files (TOML). Its own
  [README](backend/README.md)
- `web/`: the dashboard (Vite + React/TypeScript on preact/compat), built to `web/dist` and
  served at `/`
- `packaging/`: the `.deb` build, the apt repository and SD image builds, the systemd unit,
  udev rules, drive handler, `benchbob-setup`, `benchbob-update`, and the upgrade test
- `firmware/`: node firmware for microcontroller boards (Rust + Embassy; its own README)

The server is one statically linked binary per architecture (musl, no C dependencies) that
talks to the kernel directly: termios, SocketCAN, I²C and GPIO ioctls, sysfs, udev. **Nothing
runs Bun or Node on the box**: Bun is only the web UI's build tool.

## Hardware

BenchBob picks a board file at startup from the device tree (ARM) or DMI (x86). `GET /api`
and the status stream say which (`board`: id, name, whether it's tested, what it supports),
and `BENCHBOB_BOARD=<id>` forces one.

- **Tested:** Raspberry Pi 4 (desky, Debian 13). Other Raspberry Pis (3, 5, 400/500,
  CM3/4/5, Zero 2 W on a 64-bit OS) run the same code but haven't been tried here; the Pi 5
  reads levels through `pinctrl` instead of `/dev/gpiomem`.
- **Any other Linux box** (an x86 mini PC, a VM, an unknown ARM board): everything
  but the pin header. Drives, USB, serial, CAN, I²C, 1-Wire, Wi-Fi, BLE, cameras, probes,
  SDR, GPS, UPS and network panels are plain Linux. GPIO lists the kernel's gpiochips and
  their lines, which also covers USB GPIO adapters (FT232H, CH341, MCP2221). Health comes from
  thermal zones, hwmon and power supplies. x86 machines are named from DMI.
- **Listed but untested** (pinouts from vendor documentation and the kernel device tree,
  cited in each board file; nobody here has run BenchBob on one): Orange Pi 5, 5B, 5 Plus,
  3B, Zero 3 and Zero 2W; Radxa ROCK 5B and 5B+; BeagleY-AI; Libre Computer Le Potato,
  Sweet Potato and Renegade. Recognised without a pin map: Orange Pi 5 Pro / Max / Ultra and
  BeagleBone AI-64. The dashboard marks these "untested board". Check pins against the
  vendor's pinout before wiring anything.
- **32-bit ARM** (the armhf package: Pi Zero W, Pi 1 and 2 on 32-bit Raspberry Pi OS,
  BeagleBone Black/Green, other ARMv7 boards): the same server built for ARMv6 hard-float,
  which runs on ARMv7 too. Built and run under emulation, not yet on hardware.

Off the Pi, GPIO is read through the kernel's character-device API, which can't read a level
without requesting the line. So levels are shown only for lines you pick to watch, requested
"as is" so the pin's direction and pull don't change, and released when you stop. Pins can't
be driven there yet. Boot-time buses are device-tree overlays: the GPIO panel edits
`overlays=` in Armbian's `armbianEnv.txt` or Orange Pi's `orangepiEnv.txt`, or `fdtoverlays`
in BeagleBoard.org's `extlinux.conf`, offering the overlays installed on the image. Boards
whose vendor tool owns that config (Radxa OS's rsetup, Libre's `ldto`) are left to it.

## Install

BenchBob runs on Debian-family Linux: Raspberry Pi OS, Debian or Ubuntu, on arm64, amd64 or
armhf. The Raspberry Pi gets the most (header, `config.txt`, bootloader tools); other
machines get everything that isn't Pi-specific. The package is the server binary, the web
UI and the system glue: no runtime to install.

```sh
curl -fLO https://github.com/cinderblock/BenchBob/releases/latest/download/benchbob_$(dpkg --print-architecture).deb
sudo apt install ./benchbob_*.deb
```

The dashboard is then on port 80. apt also installs the tools behind each panel
(Recommends); `--no-install-recommends` leaves them out, and the panels say what's
missing. Settings are kept in `/var/lib/benchbob`, and `/etc/default/benchbob` sets the
port. For APFS drives, build the module once with `sudo benchbob-setup apfs`.

Upgrading from 0.4.x, which ran the server on Bun, keeps the settings in
`/var/lib/benchbob`, empties Bun's cache in `/var/cache/benchbob` and removes the Bun
files; a box left in the old opt-in Rust mode (`benchbob-setup rust on`) has
`benchbob-rust.service` and its drop-in removed, and `benchbob.service` takes port 80 back.
Installing over a DeskyPi (`install.sh`) setup moves its settings over and retires
`deskypi-web.service`.

### Raspberry Pi SD card image

Each release also carries `benchbob-<version>-raspios-lite-arm64.img.xz`: the latest
Raspberry Pi OS Lite (64-bit) with BenchBob and the panels' tools already installed. Flash
it with Raspberry Pi Imager ("Use custom"); its OS customisation (user, Wi-Fi, SSH) still
applies. BenchBob is running when the Pi first boots. Or, to make a card from any Pi OS
image, flash it from a BenchBob dashboard with "Install BenchBob on first boot" ticked in
the flash settings.

### Updates

The package adds BenchBob's apt repository (`/etc/apt/sources.list.d/benchbob.sources`,
signed with key `6991 23FF 03D6 852B E732  BDA3 E963 5D71 0E30 8D93`), so new releases come
with `sudo apt upgrade`. The dashboard also checks GitHub for a newer release every 6 hours
and offers an **Update** button; `PUT /api/update/settings {"check": false}` turns the
check off. Without the apt source (if you delete it), the button installs the release
straight from GitHub. The repository alone, without the package, is described at
<https://cinderblock.github.io/BenchBob/>.

Releases are cut by pushing a `v*` tag: CI builds the three packages, install-tests the
amd64 and arm64 ones and their upgrade from 0.4.1 (`packaging/test-upgrade.sh`), publishes
the GitHub Release, and rebuilds the apt repository from the newest releases.

### Privileges

The service runs as the `benchbob` user, in the device groups (dialout, gpio, i2c, video,
…), so the server itself isn't root. Erasing, flashing, mounting and
`config.txt` changes go through sudo, and `/etc/sudoers.d/benchbob` allows that without a
password. That includes `dd` and `sh`, so the account is root-equivalent: another reason
to keep BenchBob on a trusted network.

## Develop

You need [Rust](https://rustup.rs) for the server and [Bun](https://bun.sh) for the web UI.
The server is Linux-only (on Windows, use WSL).

```sh
cd backend
cargo test --workspace                 # the server's tests
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
cargo build --release -p benchbob
(cd ../web && bun install && bun run build)   # the dashboard, into web/dist
# A side instance from the checkout (it finds web/dist above the binary); settings go
# where BENCHBOB_STATE_DIR says, and BENCHBOB_PORT_OFFSET shifts the peripheral ports.
PORT=8090 BENCHBOB_PORT_OFFSET=1000 BENCHBOB_STATE_DIR=/tmp/bb target/release/benchbob
```

Packages:

```sh
./install.sh                    # on the box: build the .deb from this checkout and install it
packaging/build-deb.sh arm64    # just build one: amd64, arm64 or armhf (CI, any Debian box, WSL)
```

`build-deb.sh` builds the server with cargo for the package's architecture (static musl,
linked with the `rust-lld` that ships with Rust: `rustup target add
aarch64-unknown-linux-musl`, or `x86_64-…`, `arm-unknown-linux-musleabihf`) and the web UI
with Bun. `RUST_BIN=<binary>` and `WEB_DIST=<dir>` pass in ones built elsewhere, which is
what CI does. `sudo packaging/test-upgrade.sh <deb>` (on a throwaway machine) installs
0.4.1 and upgrades it to that package three ways.

Running from a checkout without `BENCHBOB_STATE_DIR`, settings are kept in the checkout.

### The web UI

`web/` is the dashboard: React components built against `preact/compat` with Vite, one
lazily loaded module per section and panel, so a bob only downloads the panels it reports.
It talks to the bob's API as it stands; request and response types sit next to each
panel's client (`api.ts`). Without a build, `/` says how to make one.

```sh
cd web && bun install
bun run dev                  # http://localhost:5173; append ?bob=http://deskypi.local to use a real bob
BOB_PROXY=http://deskypi.local bun run dev   # or proxy /api, /ws and /mcp to one (same-origin)
bun run test                 # Vitest component and unit tests
bun run typecheck
bun run build                # dist/ (full) and dist-mcu/ (node build), then the size budget
```

For the node sections and the MCU build, `cargo run --example mock_node` in
`firmware/proto` runs a pretend node on :8099 using the firmware's own request handling:
it serves `dist-mcu/` at `/`, or use `bun run dev` with `?bob=http://127.0.0.1:8099`.

Every request goes through a `BobConnection` (base URL), and a `<Bob>` component holds
one bob's whole dashboard, so a page can show several bobs; every bob answers with
permissive CORS for that. A page reloads itself when the bob that served it reports a new
build (server or UI); a page served from elsewhere shows a notice instead. The MCU build
holds only what a node serves (status, GPIO, I²C) and must stay under 60 KB gzipped.

## Board files

Boards are data: TOML files in `/usr/share/benchbob/boards` (shipped) and
`/etc/benchbob/boards.d` (yours; the same id overrides a shipped one), with a script
contract for boot config and health. Adding a board needs no rebuild; see
[backend/boards/README.md](backend/boards/README.md).

## Microcontroller nodes

`firmware/` holds Rust + Embassy firmware that turns a Raspberry Pi Pico 2 W or an
ESP32-C6 into a BenchBob **node**: a Wi-Fi board with the same REST shapes and MCP tool
names for the GPIO and I²C it has, provisioned over Improv Wi-Fi (USB) or, on the Pico, a
`config.uf2`, and discoverable over mDNS (`_benchbob._tcp`). Each node serves the web
UI's MCU build from its own flash at `http://<node>.local/`, so a browser is all it
takes. It is v0.1 and not yet tested on hardware; see [firmware/README.md](firmware/README.md).

## License

MIT
