# BenchBob's Rust backend

The server that replaces the Bun/TypeScript one, panel by panel. One statically linked
binary per architecture, the same API (paths, JSON shapes and key order, SSE and
WebSocket messages, MCP tool names), the same settings files, so either the old dashboard
or the new web UI works against either server.

> **Status:** the foundation, the board layer and the GPIO and CAN panels are native. Everything
> else is proxied to the Bun server (see [Proxy mode](#proxy-mode)), so this server can
> own port 80 before it has every panel. The released `.deb` still runs Bun only.

| What | Native here |
| --- | --- |
| Dashboard: `web/dist` at `/` when it exists (the old page at `/legacy`), else `index.html` with `ui/*.js` inlined; build-version stamping and reload | yes |
| `GET /api` docs (examples rewritten to the caller's origin), `/api/status`, `/api/events` (SSE, per-client gzip) | yes |
| `/api/update*` (release check, settings, install), `/mcp` (stateless Streamable HTTP), `/api/periph/driver`, `/api/periph/timeline` | yes |
| `GET /api/boards`: the board files loaded, which one matched, files that failed (new) | yes |
| GPIO panel: `/api/gpio*`, `/ws/gpio`, `gpio_*` MCP tools, plus `POST /api/gpio/line` (new) | yes |
| CAN panel: `/api/can*`, `/ws/can/:iface`, `can_*` MCP tools, the socketcand server and its UDP beacon | yes |
| Drives, flashing, rpiboot, files, catalog, and every other panel | proxied |

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
| `crates/gpio` (`benchbob-gpio`) | The GPIO driver: gpiochip v2 uAPI (with output requests), passive level readers (BCM2711 gpiomem, RP1), config.txt and boot-overlay editors, board scripts |
| `crates/can` (`benchbob-can`) | The CAN driver: SocketCAN raw sockets (libc), `ip link` config, auto-bitrate, vcan, SLCAN, per-ID table, captures, periodic jobs, candump recordings, the socketcand server |
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
| `BENCHBOB_CONFIG_TXT`, `BENCHBOB_CMDLINE_TXT`, `BENCHBOB_BOOT_ENV`, `BENCHBOB_OVERLAY_DIR` | point the boot-config editors at test copies (reboot is then refused) |
| `RUST_LOG` | log filter (default `info`) |

The old `DESKYPI_*` names still work. `benchbob --native-drivers` prints the ids of the
drivers this build runs itself.

## Proxy mode

With `BENCHBOB_LEGACY_PORT`, any route not served here (HTTP, and WebSocket upgrades) is
streamed to the Bun server. The documents that span every panel merge both servers'
parts: `/api/status` (the drive and rpiboot fields come from Bun's status; `peripherals`
lists both servers' drivers in dashboard order), `/api/events` (it follows Bun's stream
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
