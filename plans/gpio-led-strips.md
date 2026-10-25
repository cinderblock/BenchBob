# Addressable LED strips (WS2812 / APA102) and sACN on GPIO

## Goal

Let any BenchBob (the Linux server and the firmware nodes) drive addressable LED strings
from GPIO pins: the one-wire timed kind (WS2812/WS2811/SK6812/WS2815…) and the clocked SPI
kind (APA102/SK9822). Per string: pin(s), length, color order, timing preset (with simple
overrides), brightness. Content comes from built-in patterns, the API/MCP (set pixels), or
a network lighting receiver (sACN / E1.31, and probably Art-Net and DDP too).

Requirement from the user: **firmware nodes can use any pin**. Linux boards use whatever
their hardware can do, and say plainly which pins those are.

Second half of the request: propose other low-level GPIO features worth adding alongside
(IR receive/transmit, "RCA", others). See [Other GPIO features](#other-gpio-features).

## Environment / context

- Repo: `C:\Users\camer\git\Personal Projects\DeskyPi` (the project is BenchBob).
- Linux backend: `backend/` (Rust, one crate per panel; GPIO is `backend/crates/gpio`,
  board files in `backend/boards/*.toml`). Static musl binary, no C dependencies: talks to
  the kernel by ioctl (`gpio/src/uapi.rs` is the pattern to follow).
- Firmware nodes: `firmware/` (Embassy). `proto/` is the `no_std`, dependency-free,
  host-tested shared logic; `node/` the shared embassy glue; `pico2w/`, `esp32c6/` the
  boards. Same REST shapes and MCP tool names as the Linux server.
- Existing safety gate: GPIO output control (off at every boot, `confirm: true` to enable).
- Existing config.txt / boot-overlay staging with diff + token + reboot
  (`gpio/src/config_txt.rs`, `overlays.rs`), which refuses two interfaces on one pin.
- Firmware crate versions: embassy-rp 0.10, esp-hal 1.1.x (see `plans/benchbob-firmware.md`).

## Hardware findings (what each platform can do)

| Platform | One-wire (WS2812) | Clocked (APA102) | Pins |
| --- | --- | --- | --- |
| Pico 2 W (RP2350) | PIO; embassy-rp ships `pio_programs::ws2812` | PIO (`pio_programs::spi`) or hardware SPI | **Any GPIO.** 3 PIO blocks × 4 state machines = 12, minus the one cyw43 uses → up to ~11 strings |
| ESP32-C6 | RMT: 4 channels, **only ch0–1 can TX** (esp-metadata `for_each_rmt_channel`: (0,0),(1,1) TX; (2,0),(3,1) RX). Also GP-SPI2 + DMA with SPI-bit encoding, and PARL_IO (parallel TX, DMA) for many strings at once | GP-SPI2 + DMA | **Any GPIO** via the GPIO matrix. 2 strings on RMT + 1 on SPI without trickery; more via PARL_IO later |
| Pi 5 (RP1) | RP1 has **one PIO block (4 SMs)** reachable from Linux through `/dev/pio0` (rp1-pio driver; ioctls wrapped by piolib in raspberrypi/utils, not documented, but open source). Also the `ws2812-pio` overlay (`/dev/leds%d`, params gpio, num_leds, rgbw, brightness; fixed at boot). SPI MOSI encoding also works | RP1 SPI controllers (spidev), or PIO | **Any of GPIO 0–27** via PIO, up to 4 strings (shared with anything else using PIO, e.g. `pwm-pio`) |
| Pi 4 / 3 / Zero 2 (BCM283x/2711) | The classic method (rpi_ws281x's default): **PWM + DMA**. PWM0 on GPIO12/18, PWM1 on GPIO13/19, so two strings at once; PCM_DOUT on GPIO21 as a third. Needs `/dev/mem` (PWM, clock manager and DMA registers), uncached memory from the VideoCore mailbox (`/dev/vcio`), a free DMA channel (rpi_ws281x defaults to 10; 5 corrupts the filesystem on a Pi 3), and PWM conflicts with onboard analog audio (`snd_bcm2835` must be off). Also: SPI MOSI encoding through spidev | spidev | PWM/PCM: GPIO 12, 13, 18, 19, 21 (up to 3 strings: 2 PWM channels + PCM), no reboot. SPI: Pi 4 SPI0 GPIO10, SPI1 GPIO20, SPI3 GPIO2, SPI4 GPIO6, SPI5 GPIO14, SPI6 GPIO20 → 5 distinct pins, each a config.txt overlay + reboot. Pi 3: GPIO10, GPIO20 |
| Other Linux boards | SPI MOSI encoding via spidev, where the board's overlay enables it | spidev; or userspace bit-bang via the GPIO uAPI (APA102 is clocked, so jitter is harmless) on **any free line** | Whatever SPI overlays the board file documents |

The "one pin with a timer" the user remembered is PWM0 on GPIO18, rpi_ws281x's default
(the method most Pi WS2812 guides use). The first draft of this plan recommended SPI only
and called PWM+DMA "not planned"; the user pushed back, and that was the wrong framing: PWM
is the standard Pi 4 method and the only one with no reboot on pins other than GPIO10.
Its real costs:

- BenchBob runs as `User=benchbob` (`packaging/benchbob.service`), and `/dev/gpiomem`
  only maps the GPIO block. PWM/DMA needs `/dev/mem` (root or CAP_SYS_RAWIO) and
  `/dev/vcio`, so either the service gains a capability or a small privileged helper does
  the DMA setup (there is already `packaging/benchbob.sudoers`).
- Analog audio has to be off while a PWM string runs (PCM on GPIO21 keeps it).
- Reimplementing rpi_ws281x's register work in Rust (no C dependencies) with per-SoC
  peripheral bases and DMA channel choice.

### Gotchas already known (verify during implementation)

- **Pi 4 SPI clock follows the core clock**, which scales with load; WS2812 encoding over
  SPI needs a fixed core clock (`core_freq=500` / `core_freq_min=500` on Pi 4, per
  rpi_ws281x). Staging must add that to config.txt along with the SPI overlay.
- **spidev's default transfer limit is 4096 bytes** (`spidev.bufsiz`, kernel cmdline).
  At 3 SPI bits per WS2812 bit that is 9 bytes/LED → ~450 RGB LEDs per transfer. Either
  raise `spidev.bufsiz` in cmdline.txt (staged like config.txt) or chain transfers with
  `cs_change` in one `SPI_IOC_MESSAGE` (gaps between transfers may trip the latch).
- SPI MOSI must idle low between frames; some controllers idle high or glitch on the first
  bit. Use a leading zero byte.
- ESP32-C6 RMT channel RAM is 48 items per channel; a WS2812 LED is 24 items, so long
  strings need RMT ping-pong refills, which Wi-Fi interrupts can starve (visible
  glitches). SPI+DMA (or PARL_IO) has no refill problem: probably the primary C6 path for
  long strings, RMT for short ones.
- The ESP32-C6 DevKitC-1's onboard RGB LED is a WS2812 on GPIO8: this work gives the C6
  its missing status LED for free (`firmware/README.md` lists it as not done).
- Pi 5 PIO is shared: `pwm-pio`, `ws2812-pio` overlays and anything else using `/dev/pio0`
  compete for the 4 SMs and 32 instruction words. Claim/release must be clean.
- 3.3 V data into 5 V WS2812 strings is marginal; the UI should mention a level shifter
  (74AHCT125) or a sacrificial first pixel, not pretend it always works.

## Proposed design

### Shared core (one implementation, host-tested)

A new `no_std`, dependency-free crate, used by both `backend/` and `firmware/` (path dep),
e.g. `firmware/pixels` (or a top-level `shared/` dir: decide when building). It holds:

- **String config**: kind (one-wire / clocked), length, color order (any permutation of
  RGB, plus RGBW/WRGB variants for SK6812), brightness (global 0–255), gamma on/off,
  optional power budget (mA per channel at full, max mA total → brightness auto-scaled).
- **Timing presets** for one-wire chips: WS2812B (800 kHz), WS2811 (400 kHz), SK6812,
  WS2813/WS2815, TM1814 (inverted), plus "custom" with just four numbers: T0H, T1H, bit
  period, reset/latch time (ns/µs). That is the whole "raw timing" surface.
- **Encoders**: pixels → WS2812 bit stream for SPI (3 or 4 SPI bits per data bit, picking
  the SPI clock from the timing), pixels → APA102 frames (start frame, 5-bit global
  brightness, end frame of ⌈n/16⌉ bytes), pixels → RMT items / PIO words.
- **Patterns** as pure functions of (time, pixel index, params): solid, rainbow, chase,
  breathe/fade, twinkle, gradient, and a **color-order test** (pixel 0 red, 1 green, 2
  blue, 3 white, rest off) so the right color order is obvious from one glance.
- **Network receivers' parsers**: E1.31 (sACN, UDP 5568, multicast 239.255.hi.lo,
  priority + source handling, sequence numbers), Art-Net ArtDmx (UDP 6454), DDP (UDP 4048,
  what WLED/xLights speak natively and the only one with no 512-channel universe limit).
  Mapping: start universe + start channel per string; strings spanning several universes.

### Linux backend

New panel crate `backend/crates/leds` (a panel of its own, so "Peripherals…" can turn it
off), with pin claims going through the GPIO crate so conflicts with I²C/SPI/UART/1-Wire
are refused as they are today. Drivers, picked per board:

1. **spidev** (all Linux boards): WS2812-over-SPI and APA102. Uses the existing staging to
   enable an SPI overlay (+ `core_freq` on Pi 4, + `spidev.bufsiz` if needed).
2. **Pi 5 PIO** via `/dev/pio0` ioctls reimplemented in Rust (as `uapi.rs` does for GPIO):
   any of GPIO 0–27, runtime config, no reboot. Fallback when the ioctls prove awkward:
   stage the `ws2812-pio` overlay (fixed timing and length, reboot).
3. **GPIO uAPI bit-bang** for APA102 on any free line, on any board.

### Firmware nodes

- Pico 2 W: PIO programs for both kinds (embassy-rp's ws2812 program parameterised on the
  timing; a clocked program for APA102), any pin.
- ESP32-C6: RMT (2 strings), SPI+DMA (1 more, and the APA102 path), status LED on GPIO8.
- String configs persisted in the config record (new TLV fields; the format is already
  TLV and versioned), under the same persistence rule as everything else (see Decisions).
  Needs the config.uf2 tool to stay byte-exact.

### API / MCP / UI (same shapes everywhere)

Pinned 2026-10-25 while building the node side; the Linux `leds` crate serves the same
shapes. A string is one object:

```json
{ "id": 1, "name": "desk", "chip": "ws2812b", "kind": "one-wire", "pin": 6, "clockPin": null,
  "pixels": 60, "order": "GRB", "timing": { "t0hNs": 400, "t1hNs": 800, "periodNs": 1250, "resetUs": 300 },
  "clockHz": null, "brightness": 255, "gamma": true, "invert": false,
  "power": { "maPerChannel": 20, "idleMa": 1, "maxMa": 2000 },
  "source": "pattern", "pattern": { "name": "rainbow", "spread": 60, "periodMs": 5000 },
  "state": "running", "error": null, "estimateMa": 840 }
```

- **`chip` is the one choice that matters** ("simplify options"): `ws2812b`, `sk6812`,
  `ws2813`, `ws2811-400k`, `custom` (one-wire; `timing` editable, otherwise shown for
  reference), `apa102` (covers SK9822/HD107S), `ws2801` (clocked; need `clockPin`,
  `clockHz`). It sets `kind` and, on create, the default `order` (GRB for the WS281x/SK
  family, BGR for APA102, RGB for WS2801 and WS2811-400k).
- `state`: `running`, `off` (output control off: the string was blanked and isn't driven),
  `error` (with `error`: e.g. no PIO state machine left).
- Pattern parameters, all optional: `color`, `background`, `a`, `b` (hex colours),
  `spread`, `periodMs`, `width`, `speed` (px/s), `density` (0–255).

| Endpoint | What |
| --- | --- |
| `GET /api/leds` | `{outputEnabled, capabilities: {maxStrings, maxPixels, chips, orders, patterns, pins}, strings: [...]}` |
| `POST /api/leds` | Create a string (fields above; `chip`, `pin`, `pixels` required) |
| `POST /api/leds/:id` | Change any fields |
| `DELETE /api/leds/:id` | Remove it (blanks it, frees its pins) |
| `POST /api/leds/:id/pattern` | `{name, ...params}` |
| `POST /api/leds/:id/pixels` | `{pixels: "ff0000 00ff00 …" or ["#ff0000", …], start}` or `{fill: "#ff0000", start, count}`; switches the source to `pixels` |

MCP: `leds_list`, `leds_configure` (no `id`: create; `delete: true`: remove),
`leds_pattern`, `leds_set_pixels`. Writes (creating, changing, patterns, pixels) work
with output control off, but nothing is driven until it is on.

- Web UI: a "LED strips" panel of its own: string list, a pin picker (a header diagram)
  that greys out unusable pins with the reason, the timing preset dropdown with a "custom" disclosure, a live pixel
  preview, pattern picker, receiver status (packets/s per universe, active source).
- Gated by GPIO output control, like every other write.
- **The GPIO panel shows the exclusions**: pins a string holds appear there with
  "LED strip <name>" as their owner and can't be driven, exactly as pins held by I²C/SPI
  or a kernel driver are shown today. The pin picker is a reusable header-diagram
  component, so the GPIO panel and the other pin-based features below can share it.

## Decisions already made (don't re-ask)

User's answers of 2026-10-25:

- Both one-wire (WS2812-style) and clocked (APA102-style) strings.
- Color order, timing (simplified: presets + 4-number custom), and length are per string.
- Firmware nodes: any pin.
- Pi 1–4: SPI (every Linux board) **and** PWM/PCM + DMA (GPIO 12/13/18/19/21). PWM+DMA is
  the standard Pi method; SPI-only was a wrong first recommendation.
- Network receivers: **sACN, Art-Net and DDP**, all three.
- **Own panel ("LED strips")**, but the GPIO panel must show the pins it holds as
  exclusions, as for other peripheral features, and it gets a pin picker.
- **Persistence: no special rule.** "Things keep working until they are reconfigured",
  and across a restart or reboot only to the same extent anything else persists.
  Concretely, matching today's code: string configs are settings (`peripherals.json` on
  Linux, like serial bridges; the config record on nodes); animations and receivers are
  runtime and stop with the process (a strip physically holds its last frame); after a
  restart, output follows GPIO output control, which is off at start like every other
  output. If the user later wants "boot straight into the pattern", that's a change to
  the output-control rule for everything, not a special case here.
- Pi 1–4 PWM/DMA privileges: **a small privileged helper** owns `/dev/mem`, `/dev/vcio`
  and the DMA setup; the main `benchbob` process stays unprivileged (answer 2026-10-25).
- Order of work: as recommended (shared crate → Pico 2 W → ESP32-C6 → Linux spidev →
  Pi 1–4 PWM/DMA → Pi 5 PIO → receivers). No objection was raised.
- Other features: **all of them**, plus composite video ("RCA") and RC servo PWM.
  Each family gets its own plan (below); DMX512 out stays here as a lighting output.

## Plan / steps

1. ~~Answer the open questions~~ (all answered)
2. ~~Shared `pixels` crate~~: done, `firmware/pixels` (see Progress log).
3. Pico 2 W driver: node API/MCP and PIO driver done; node UI section ← current step (the easiest any-pin target) + node API/MCP + node UI section.
4. ESP32-C6 driver (RMT + SPI), status LED.
5. Linux `leds` crate: spidev driver, GPIO-uAPI APA102, staging of SPI overlays; GPIO
   panel exclusions; pin picker component.
5b. BCM283x/2711 PWM/PCM + DMA driver (GPIO 12/13/18/19/21), in a privileged helper.
6. Pi 5 `/dev/pio0` driver.
7. Network receivers (sACN, Art-Net, DDP) on both sides.
8. DMX512 output over a UART (sACN → DMX bridge).
9. Persistence per the rule above, README updates (root, firmware, backend), hardware
   bring-up.

## Progress log

- [x] Hardware research for each platform (this doc)
- [x] Open questions 1–7 answered (2026-10-25)
- [x] Question 8: privileged helper
- [x] Step 2: `firmware/pixels` (`benchbob-pixels`), 30 host tests, clippy-clean, builds for
  thumbv8m and riscv32imac; CI runs its tests in `firmware-test`. Modules: `color` (Rgbw,
  any RGB/RGBW order, gamma 2.6, brightness, `PowerBudget`), `timing` (presets ws2812b,
  sk6812, ws2813, ws2811-400k; `SpiFit::find` takes the controller's real clock function;
  `PioFit` for the SDK ws2812 program's three phases), `encode` (wire bytes, ws-over-SPI,
  APA102/SK9822, WS2801), `pattern` (off, solid, gradient, rainbow, chase, breathe,
  twinkle, order-test, ruler), `net` (sACN, Art-Net, DDP, `SacnArbiter`, `Patch`).
  Formatted with backend's rustfmt.toml (140 cols), since the backend will use it too.
- [x] Step 3a: node API (`firmware/proto/src/api/leds.rs`): string configs, validation,
  `/api/leds` routes, `leds_*` MCP tools (hidden on boards without LED support), pins
  claimed in the GPIO view and refused by gpio_write, `Leds::job/render/report` for the
  board task. 9 host tests (`proto/tests/leds.rs`); the mock node pretends to drive.
- [x] Step 3b: Pico 2 W driver (`firmware/pico2w/src/leds.rs`): PIO1, 4 strings, any free
  GPIO, per-string program (fitted ws2812 or clocked), DMA of bytes, 50 fps. Builds;
  untested on hardware.
- [x] Step 3c: web UI: `web/src/panels/leds/LedStrips.tsx` (shared body: string cards
  with state, quick order-test/ruler/off, pattern editor, add/edit dialog), the node
  section `sections/node/NodeLeds.tsx` (shown when the node lists `GET /api/leds`), and
  `ui/PinPicker.tsx` (header-ordered pin buttons, refusals explained on tap). 3 tests;
  MCU bundle 28.3 KB gz of 60. Checked end to end against the mock node: added a string
  through the dialog, and the GPIO section showed both strings' pins as taken.
- [ ] Follow-up: a live pixel preview in the UI (port the patterns to TS, or have the
  node report a downsampled frame). Not started.
- [ ] Steps 4–9

## Open questions for the user

None open.

## Other GPIO features (own plans)

- [`plans/gpio-ir.md`](gpio-ir.md): IR receive and transmit.
- [`plans/gpio-logic-analyzer.md`](gpio-logic-analyzer.md): logic analyzer / pulse capture.
- [`plans/gpio-pulse-io.md`](gpio-pulse-io.md): PWM, RC servo, clock output, frequency
  counter, pulse width, quadrature encoder.
- [`plans/gpio-timing-sensors.md`](gpio-timing-sensors.md): DHT11/22, HX711.
- [`plans/gpio-composite-video.md`](gpio-composite-video.md): composite video ("RCA").

## Findings from building the shared crate

- Output inversion (for an inverting level shifter) is a driver feature (PIO/RMT/GPIO
  invert), not an encoding: SPI MOSI's idle level can't be inverted, so SPI strings
  can't offer it.
- Art-Net controllers that discover nodes (QLC+, Resolume, MadMapper) expect an
  ArtPollReply to ArtPoll; `parse_artnet` returns `Other` for ArtPoll. A reply builder is
  still to write, in step 7. xLights and WLED-style senders don't need it.
- DDP: only render on `push` (WLED does the same); a sender that never sets push is
  rare, but render after a short idle (e.g. 50 ms) as a fallback.
- SPI fitting on a Pi 4 at core_freq=500 lands at 3 bits/2.38 MHz (40 ns worst error) for
  WS2812B, with spi-bcm2835's even-divider rounding.

## Findings from the node work

- `pio::Pin` can only be made from a concrete `PIN_n` peripheral (`PioPin` isn't
  implemented for `AnyPin`), so runtime pin choice is a `match` over `PIN_n::steal()`.
  The board's `Flex` for that pin must be **forgotten, not dropped**: `Flex`'s Drop sets
  the pin's FUNCSEL to NULL, which would cut the PIO off. On release a fresh
  `Flex::new(AnyPin::steal(n))` puts it back on SIO.
- PIO TX FIFO + 8-bit DMA writes: the byte is replicated across the word, so a left
  shift with an 8-bit autopull threshold sends exactly the frame's bytes, with no
  padding bits at the end.
- MCP tool results are embedded twice (text + structuredContent) in an 8 KiB response,
  so `leds_list` has to stay well under ~3.5 KiB: capabilities list chip names, pattern
  names and pin numbers only.
- `gen` is a reserved keyword in edition 2024.
- Web tests: `fireEvent.change` on a happy-dom `<select>` doesn't reach preact's handler;
  set `.value` and dispatch a real `change` event instead (LedStrips.test.tsx).

## Things not to do

- Don't bit-bang WS2812 through the GPIO uAPI on Linux: the timing can't be met.
- Don't depend on rpi_ws281x or piolib (C libraries): the backend has no C dependencies.
- Don't put two drivers on one pin or one PIO SM: claim through the GPIO crate.
- Don't invent a LED-specific "resume at boot" rule (see the persistence decision).
