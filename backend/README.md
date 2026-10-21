# BenchBob's Rust backend

The server that replaces the Bun/TypeScript one, panel by panel. One statically linked
binary per architecture, the same API (paths, JSON shapes and key order, SSE and
WebSocket messages, MCP tool names), the same settings files, so either the old dashboard
or the new web UI works against either server.

> **Status:** the foundation, the board layer, the GPIO panel and everything server.ts did
> itself (drives, flashing, rpiboot, files, OS detection, the catalog) are native. The
> other panels are proxied to the Bun server (see [Proxy mode](#proxy-mode)), so this server can
> own port 80 before it has every panel. The released `.deb` still runs Bun only.

| What | Native here |
| --- | --- |
| Dashboard: `web/dist` at `/` when it exists (the old page at `/legacy`), else `index.html` with `ui/*.js` inlined; build-version stamping and reload | yes |
| `GET /api` docs (examples rewritten to the caller's origin), `/api/status`, `/api/events` (SSE, per-client gzip) | yes |
| `/api/update*` (release check, settings, install), `/mcp` (stateless Streamable HTTP), `/api/periph/driver`, `/api/periph/timeline` | yes |
| `GET /api/boards`: the board files loaded, which one matched, files that failed (new) | yes |
| GPIO panel: `/api/gpio*`, `/ws/gpio`, `gpio_*` MCP tools, plus `POST /api/gpio/line` (new) | yes |
| Drives: the drive fields of `/api/status`, eject, scan, probe, deep scan, erase, flash (with cloud-init / firstrun.sh settings and Install BenchBob), `/api/rpiboot/*`, `/api/blcard*`, `/api/files`, `/api/download`, `/api/osinfo`, `/api/images`, `/api/catalog`, `/api/favorites`, `/api/quicklist`, `/api/defaults` | yes |
| Every other panel (serial, CAN, I²C, Wi-Fi, BLE, camera…) | proxied |

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
| `BENCHBOB_PORT_OFFSET` | shift every peripheral listener port |
| `BENCHBOB_CONFIG_TXT`, `BENCHBOB_CMDLINE_TXT`, `BENCHBOB_BOOT_ENV`, `BENCHBOB_OVERLAY_DIR` | point the boot-config editors at test copies (reboot is then refused) |
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
