# BenchBob node firmware

A **node** is a small Wi-Fi microcontroller board that speaks a subset of BenchBob's API:
the same REST shapes and MCP tool names as the Linux server, for the GPIO and I²C it has.
Scripts and AI agents that work against a BenchBob work against a node too. Like every
bob it stands alone and serves its own dashboard: open `http://<hostname>.local/` for the
web UI's node build (status, GPIO, I²C, LED strings), embedded in the firmware.

> **Status: v0.1, untested on hardware.** Everything here compiles, and the shared logic
> (API, MCP, JSON, Improv framing, config record, UF2, mDNS) has host tests, but nobody
> has run either image on a real board yet. Expect first bring-up to find bugs.

> **No login**, like BenchBob itself: anyone on the network can read pins and buses, and
> drive pins once output control is on. Keep nodes on a trusted network.

## Boards

| Board | Chip | Image | Status |
| --- | --- | --- | --- |
| Raspberry Pi Pico 2 W | RP2350 + CYW43439 | `benchbob-node-pico2w.uf2` | Compiles; untested |
| ESP32-C6 (DevKitC-1 pinout) | ESP32-C6 | `benchbob-node-esp32c6.bin` | Compiles; untested |

Images are on the [releases page](https://github.com/cinderblock/BenchBob/releases)
under `firmware-v*` tags, with SHA256SUMS.

## Flashing

**Pico 2 W:** hold BOOTSEL, plug it in, and copy `benchbob-node-pico2w.uf2` onto the
`RP2350` drive. (BenchBob's Debug probes panel can do the copy for a Pico plugged into it.)

**ESP32-C6:** flash the merged image at offset 0 with
[espflash](https://github.com/esp-rs/espflash) 4 or esptool:

```sh
espflash write-bin 0x0 benchbob-node-esp32c6.bin
# or: esptool.py --chip esp32c6 write_flash 0x0 benchbob-node-esp32c6.bin
```

Reflashing either board keeps its Wi-Fi settings (see [Flash layout](#flash-layout)).

## Provisioning (Wi-Fi)

Release images carry no credentials. Two ways in:

1. **Improv Wi-Fi over USB serial**, on both boards. Open
   [improv-wifi.com](https://www.improv-wifi.com/) in Chrome or Edge (Web Serial), click
   *Connect device to Wi-Fi*, pick the node's serial port, then the network. The node
   joins, saves the settings and replies with its URL. Any Improv serial client works
   (ESP Web Tools, Home Assistant's). On the ESP32-C6 use the **native USB** port
   (USB-Serial-JTAG; the DevKitC-1's "USB" connector); its "UART" connector carries the
   logs. The hostname can be changed the same way (Improv command 0x05).
2. **`config.uf2`, Pico 2 W only.** Open [`tools/config.html`](tools/config.html) (a
   static page that works from disk; nothing leaves the browser), enter the SSID,
   password and an optional hostname, and copy the downloaded `config.uf2` onto the
   BOOTSEL drive *after* the firmware. Or from a shell:
   ```sh
   bun firmware/tools/config-uf2.ts --ssid "Bench Lab" --password "…" --hostname bench-1
   ```
   The file holds the password in plain text.

An access point with a captive portal, as a third fallback, is **not implemented**.

The default hostname is `benchbob-` plus the last three bytes of the MAC
(`benchbob-0a1b2c.local`). The Pico 2 W's LED blinks slowly with no Wi-Fi configured,
fast while connecting, and stays on when connected. The ESP32-C6 has no status LED yet.

## API

`http://<hostname>.local/` on port 80, shapes as on the Linux server
(`GET /api` lists them):

| Endpoint | What |
| --- | --- |
| `GET /` | The web UI (see [Web UI](#web-ui)) |
| `GET /api` | Docs: `name: "BenchBob node API"`, `appVersion`, board, endpoints, MCP tools |
| `GET /api/status` | Hostname, uptime, version, `build`, board, Wi-Fi (state, SSID, RSSI, IP, MAC), heap (ESP32-C6) |
| `GET /api/gpio` | Every GPIO: function, level, pull, header pin, and why it can't be driven |
| `POST /api/gpio/output-control` | `{enabled, confirm: true}`: output control is **off at every boot**; enabling needs `confirm: true` |
| `POST /api/gpio/:gpio` | `{mode: high\|low\|input, pull: up\|down\|none}`, once output control is on |
| `GET /api/i2c` | Buses (pins, clock) and the devices from the last scan |
| `POST /api/i2c/:bus/scan` | i2cdetect's range (0x08–0x77) |
| `POST /api/i2c/:bus/:addr/raw/read` | `{register, size: byte\|word\|block\|raw, length}` |
| `GET /api/leds` | Addressable LED strings and what the board can drive (Pico 2 W only so far; see [LED strings](#led-strings)) |
| `POST /api/leds` · `POST`/`DELETE /api/leds/:id` | Create, change, remove a string |
| `POST /api/leds/:id/pattern` · `…/pixels` | Show a built-in pattern, or set pixels directly |
| `POST /mcp` | MCP (Streamable HTTP, stateless JSON): `gpio_read`, `gpio_write`, `i2c_buses`, `i2c_scan`, `i2c_read`, and on boards with LED support `leds_list`, `leds_configure`, `leds_pattern`, `leds_set_pixels` |

```sh
claude mcp add --transport http bench-node http://benchbob-0a1b2c.local/mcp
```

Every response carries permissive CORS (`Access-Control-Allow-Origin: *`, no
credentials) and `OPTIONS` answers preflights, as on the Linux server, so a page or app
served from elsewhere can use a node's API.

Differences from the Linux server: no chip identification or decoded readings on I²C
(addresses only), no I²C writes, no WebSockets, no settings persisted besides Wi-Fi and
hostname. Both boards' I²C controllers can't send an address-only quick write, so scans
read one byte at every address (as the Pico SDK's `bus_scan` does). Pins the firmware uses
(I²C, USB, the log UART) are listed with what uses them and can't be driven.

| | Pico 2 W | ESP32-C6 |
| --- | --- | --- |
| GPIO | GP0–22, GP26–28 (header pins) | 0–11, 15–23 |
| I²C0 | GP4 SDA, GP5 SCL (pins 6/7) | GPIO6 SDA, GPIO7 SCL |
| Improv | USB CDC (VID 2E8A, PID 000A) | USB-Serial-JTAG |
| Logs | defmt over RTT (debug probe) | UART0 (GPIO16/17) at 115200 |

### LED strings

WS2812-family strings (`chip`: `ws2812b`, `sk6812`, `ws2813`, `ws2811-400k`, or `custom`
with your own `timing` of T0H, T1H, bit period and reset) and clocked ones (`apa102`
for APA102/SK9822/HD107S, `ws2801`; these take a `clockPin` and `clockHz`), on **any
free GPIO**. Per string: pixel count, colour `order` (any RGB permutation, plus RGBW
variants such as `GRBW`), brightness, gamma, output inversion (for an inverting level
shifter) and an optional power budget that scales the frame down to fit. Patterns:
solid, gradient, rainbow, chase, breathe, twinkle, plus `order-test` (pixels 0–3 red,
green, blue, white) and `ruler` (every 10th pixel white, every 50th red, the last
green) for checking a new string. Strings can be set up at any time but only light up
while GPIO output control is on; turning it off blanks them.

| | Pico 2 W | ESP32-C6 |
| --- | --- | --- |
| Driver | PIO1, one state machine + DMA channel per string | not yet |
| Strings | 4, up to 1024 pixels each | |
| Status | Compiles; untested on hardware | |

A string's pins show in `GET /api/gpio` as used by it and can't be driven. Settings
don't survive a reset yet. 3.3 V data into a 5 V strip is marginal; use a level shifter
(74AHCT125) if the first pixel misbehaves. The colour, timing and pattern logic is
[`pixels/`](pixels), shared with the Linux server.

**mDNS:** `<hostname>.local`, plus `_benchbob._tcp` (TXT `board=`, `version=`,
`api=/api`, `mcp=/mcp`) and `_http._tcp`, so `dns-sd -B _benchbob._tcp` or
`avahi-browse -r _benchbob._tcp` lists nodes.

## Web UI

The firmware embeds `web/dist-mcu`, the web UI built with only the node sections
(`cd web && bun run build:mcu`; one JS and one CSS file, about 25 KB gzipped). At build
time `node/build.rs` stores each file gzipped and raw (raw for clients that don't send
`Accept-Encoding: gzip`), and stamps the page with the **build id**: the firmware version
plus a hash of the UI (`0.1.0+49592bd9`), also reported as `build` in `/api/status`. An
open page whose node reports another build (after a reflash) reloads itself.

`index.html` is sent with `Cache-Control: no-cache` and an ETag (a revisit costs a 304);
the files under `/assets/` have content hashes in their names and are cached for a year.
Files go out straight from flash, so their size isn't limited by the response buffer.

The HTTP server runs four connections at a time and closes each after one response;
a connection arriving while all four are busy is refused (embassy-net answers it with a
reset). That's why the node build is one JS file rather than lazy chunks.

**Without hardware**, `cargo run --example mock_node` in `proto/` runs this crate's API,
MCP and UI handling over std networking against a pretend board, on
`http://127.0.0.1:8099/` (arguments: address, then a `dist-mcu` directory). It serves
the UI uncompressed, and its build id changes on every start, so open pages reload.

## Flash layout

| Board | Firmware | Config region |
| --- | --- | --- |
| Pico 2 W (4 MiB) | `0x10000000`–`0x103EFFFF` (`memory.x`; 56 KiB after it reserved) | `0x103FE000`–`0x103FFFFF` (8 KiB, last two sectors) |
| ESP32-C6 (4 MiB+) | bootloader `0x0`, partition table `0x8000`, app `0x10000` (up to 3.9 MiB) | `bbconfig` partition, `0x3FE000`–`0x3FFFFF` |

The config region is two 4 KiB slots holding a small record (magic `BBNC`, sequence
number, CRC-32, TLV fields for SSID, password and hostname); the format is documented in
[`proto/src/config.rs`](proto/src/config.rs). The firmware writes the slot not holding
the current record, so a power cut mid-write keeps the old one. A `config.uf2` writes both
slots (the record in slot 0, slot 1 erased). A firmware UF2 only rewrites its own
sectors, and the ESP32-C6 image is built with `--skip-padding` so it ends with the app.

## Layout

- `proto/`: `no_std`, dependency-free apart from `pixels/`, host-tested: the API router
  and MCP, LED string config, JSON, HTTP framing and CORS, serving the web UI, Improv
  packets, the config record, UF2 blocks, mDNS. `examples/mock_node.rs` is the host-side
  pretend node (it pretends to drive LED strings too).
- `pixels/`: `no_std`, dependency-free, host-tested, and shared with the Linux server:
  addressable LED strings (colour order, gamma, power budget, WS2812-family timing
  presets and fitting them to SPI or PIO, WS2812-over-SPI / APA102 / WS2801 encodings,
  patterns, sACN / Art-Net / DDP parsing and universe mapping). The Pico 2 W drives
  strings with it (`pico2w/src/leds.rs`); the rest is in `plans/gpio-led-strips.md`.
- `node/`: Embassy glue both boards share: HTTP server, mDNS task, Improv session,
  config store (embedded-storage `NorFlash`), the Wi-Fi request channels, and the
  embedded web UI (`build.rs`).
- `pico2w/`, `esp32c6/`: the board crates (pins, I²C, flash, USB, Wi-Fi task; the Pico's
  PIO LED driver).
- `tools/`: `elf2uf2.ts` (RP2350 UF2s), `config-uf2.ts` and `config.html`, tests.

## Building

Stock stable Rust (no Xtensa toolchain), [Bun](https://bun.sh) for the web UI and the
tools, and espflash 4 for the ESP32-C6 image. `build.sh` builds the web UI's node build
first; a plain `cargo build` in a board crate needs `web/dist-mcu` to exist (or
`BENCHBOB_WEB_DIST` pointing at one) and says so if it doesn't:

```sh
rustup target add thumbv8m.main-none-eabihf riscv32imac-unknown-none-elf
cargo install espflash --locked        # or a prebuilt release
firmware/build.sh                      # both images into firmware/dist/
firmware/build.sh pico2w               # just one
(cd firmware/proto && cargo test)      # host tests
bun test firmware/tools/               # UF2/config tool tests
```

With a debug probe, `cargo run --release` in `pico2w/` flashes over SWD and shows the
defmt log (probe-rs); in `esp32c6/` it flashes with espflash and opens the monitor. Note
the ESP32-C6 needs `--partition-table-offset 0x8000` whenever `partitions.csv` is
passed to espflash (the cargo runner and `build.sh` do this).

Pinned crates (each board has its own `Cargo.lock`): embassy-executor 0.10,
embassy-net 0.9.1, embassy-time 0.5.1, embassy-sync 0.8, embassy-usb 0.6, embassy-rp
0.10, cyw43 0.7, cyw43-pio 0.10; esp-hal 1.1.2, esp-radio 0.18, esp-rtos 0.3,
esp-bootloader-esp-idf 0.5, esp-storage 0.9, esp-alloc 0.10. The esp set is the newest
that agrees on a stable esp-hal (esp-rtos 0.4 wants esp-hal 1.2, whose esp-radio is
still a 1.0.0 beta).

Releases: push a `firmware-v<version>` tag matching `proto/Cargo.toml`; CI builds both
images and publishes them with SHA256SUMS.

## Verified vs untested

Verified by building and tests only:

- Both images build in release mode; the Pico UF2's pages reproduce
  `objcopy -O binary` byte for byte, and BenchBob's own UF2 parser accepts both the
  firmware and config UF2s for an RP2350. The ESP32-C6 image was read back: partition
  table at 0x8000 with `bbconfig` at 0x3FE000, image ending at 0xD3B60 (with the web
  UI; well clear of `bbconfig`). Both images carry the stamped page.
- The API, MCP, JSON, HTTP head parsing, CORS, UI caching (304, gzip or raw), Improv
  framing, config record (incl. torn writes), UF2 and mDNS packets, against a mock board
  on the host.
- The web UI's node build in a real browser against `mock_node`: page and assets load,
  status/GPIO/I²C poll, a scan and output control work, and the page reloads when the
  node's build changes.
- `config.uf2` from the Rust encoder, `config-uf2.ts` and `config.html` are
  byte-identical (golden fixture).

Untested on hardware: everything that touches a radio, a pin, a bus, flash or USB.

## Known gaps and follow-ups

- AP + captive-portal provisioning; BLE Improv on the ESP32-C6.
- ESP32-C6 status LED (WS2812 on GPIO8 needs RMT); CAN (TWAI), 1-Wire, serial bridge.
- A BenchBob USB PID for the Pico (it borrows Raspberry Pi's CDC PID).
- No mDNS probing/conflict resolution; pick unique hostnames (the default is unique).
- Changing the hostname over Improv updates mDNS at once; DHCP sees it after a reset.
- API requests over 4 KiB or API responses over 8 KiB are refused (UI files aren't
  limited); `Expect: 100-continue`
  isn't answered (fine for curl's small JSON bodies).
