# DeskyPi

A Raspberry Pi bench server with a web dashboard. Plug things into it and use them from
anywhere on the LAN, from a browser, a script or an AI agent.

- **Drives**: USB disks are auto-mounted and shared read-only over NFS, NBD and SMB. The
  dashboard can browse and download files, identify what's installed (Windows, Linux,
  macOS, Pi OS, EFI loaders), erase, deep-scan, flash Raspberry Pi OS images with
  preconfigured settings, and write bootloader-update SD cards.
- **Compute Modules / USB boot**: rpiboot mass-storage mode, EEPROM checks and rewrites.
- **Peripherals**: serial, CAN, I²C, 1-Wire, GPIO, Wi-Fi, BLE, cameras, debug probes,
  RTL-SDR, GPS, UPS, USB power and sharing, and wired-network diagnostics (below).

Everything the dashboard does is also a REST endpoint (`GET /api` lists them), and the
peripheral operations are MCP tools at `POST /mcp`.

## Peripherals

Each device class is a panel. A panel appears on the dashboard when it has something to
show (a device plugged in, a capture running). Idle panels fold into the **Tools** strip,
where you can open them. **Peripherals…** turns whole panels off (GPIO, say), which also
removes them from the API and MCP. Turning the USB panel off also stops sharing its devices;
restarting DeskyPi doesn't, so remote USB/IP clients stay attached across a deploy.

| Panel | What it does |
| --- | --- |
| USB | Device tree with speed, power and bound drivers; hotplug + kernel timeline; per-port power switching (uhubctl); share any device over the network with USB/IP or usbredir |
| Serial | Terminal (ANSI, hex, raw keys); any baud/framing/flow; auto-baud; the Pi 4's extra onboard UARTs (UART2–5) once enabled; DTR/RTS resets; network bridges (raw TCP, RFC 2217, WebSocket); esptool/avrdude flashing |
| CAN bus | SocketCAN and SLCAN adapters, vcan; bitrate/FD config and listen-only auto-bitrate; per-ID table, frame log, send and periodic jobs, candump recordings; socketcand server for python-can and SavvyCAN |
| I²C | Bus scan using i2cdetect's safe defaults; ~40 chips identified and decoded into readings; raw register access (writes need confirmation); per-bus clock speed, with the bus speed and extra buses settable from the panel |
| 1-Wire | DS18B20 and every other common family, iButton touch log, DS2482 bridges; move or turn off the GPIO master from the panel |
| GPIO | Live 40-pin header (function, level, pull, kernel owner); optional output control; staged `config.txt` changes with a diff, backup and reboot: I²C1 and its speed, SPI0, UART, 1-Wire, and on a Pi 4 extra I²C3–6, UART2–5 and a software i2c-gpio bus. A change that puts two interfaces on one GPIO is refused |
| Wi-Fi | Scanner across every band the radio supports, with channel charts and an hour of per-network signal history (scans every 30 s while the panel is open); client tests (associate, DHCP, ping/DNS/HTTP/iperf3) run in an isolated network namespace |
| Bluetooth LE | Advertiser scanner with decoded beacons (iBeacon, Eddystone, BTHome, Ruuvi, Xiaomi, Govee, Apple, Microsoft) |
| Camera | UVC webcams and HDMI capture sticks; controls; on-demand MJPEG stream and snapshots |
| Debug probes | ST-Link, J-Link, CMSIS-DAP, ESP-Prog and more: OpenOCD GDB server per probe, flash/erase/reset/read memory; UF2 flashing for Pico BOOTSEL |
| RTL-SDR | rtl_tcp server, or rtl_433 with a decoded-sensor table |
| GPS | Temporary gpsd: fix, sky plot, constellations, clock offset, PPS |
| UPS | USB UPSes via a temporary NUT: charge, runtime, load, power events |
| Wired network | Link details, which switch and port the Pi is on (LLDP/CDP), iperf3 server and client with quick-pick hosts and one-click profiles; ping buttons per target; DNS/HTTP/MTU checks. Host lists are editable ("Edit hosts…"). The far end needs `iperf3 -s` on 5201 |

Daemons that some panels use (gpsd, lldpd, NUT, iperf3, usbipd, OpenOCD, ustreamer) are
started on demand with private configs and stopped with DeskyPi. Their system services
stay masked.

### Network endpoints

Apart from the dashboard and socketcand, nothing listens on the LAN until you turn it on
(gpsd and NUT run local-only until shared). Defaults, each changeable in its panel:

| Port | What |
| --- | --- |
| 80 | Dashboard, REST API, WebSockets, MCP |
| 4000 + n | Serial raw TCP bridge (n fixed per serial device) |
| 4100 + n | Serial RFC 2217 bridge (`rfc2217://deskypi.tsl:4100`) |
| 4200 + n | usbredir |
| 3240 | USB/IP (`usbip list -r deskypi.tsl`) |
| 29536 | socketcand (on by default); UDP 42000 discovery beacon (optional) |
| 3333 / 4444 / 6666 | OpenOCD GDB / telnet / TCL, +10 per additional probe |
| 1234 | rtl_tcp |
| 2947 | gpsd (local-only unless shared) |
| 3493 | NUT upsd (local-only unless shared) |
| 5201 | iperf3 server |
| 8080 | Camera direct stream (when shared directly) |

There is no authentication: DeskyPi assumes a trusted LAN.

### For agents (MCP)

Point an MCP client at `http://deskypi.tsl/mcp` (Streamable HTTP, stateless). Every panel
contributes tools, e.g. `serial_write` (with a wait-for-regex), `can_capture`,
`i2c_scan`, `onewire_list`, `wifi_scan`, `camera_snapshot` (returns the image),
`probes_flash`, `usb_power`. With Claude Code:

```sh
claude mcp add --transport http deskypi http://deskypi.tsl/mcp
```

## Layout

- `server.ts`: HTTP server, drives, flashing, rpiboot, status stream
- `peripherals/core.ts`: driver registry, udev hotplug, routes, WebSockets, MCP
- `peripherals/drivers/<id>.ts`: one file per panel (auto-discovered)
- `peripherals/lib/`: parsers, FFI layers (termios, SocketCAN, I²C, gpiomem), tests
- `ui/<id>.js`: each panel's dashboard script, inlined into `index.html` at serve time
- `install.sh`: packages, services, udev rules; `peripherals.json` holds panel settings

Serial, CAN, I²C and GPIO talk to the kernel through `bun:ffi`. There are no npm
dependencies: Bun runs the TypeScript directly.

## Install and develop

```sh
sudo ./install.sh          # on the Pi
bun test peripherals/      # unit tests (also run on Windows)
PORT=8090 bun run server.ts  # a side instance; DESKYPI_PORT_OFFSET shifts peripheral ports
```
