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
  TLV and versioned) so a node boots straight into its pattern or sACN listening and works
  as a standalone pixel controller. Needs the config.uf2 tool to stay byte-exact.

### API / MCP / UI (same shapes everywhere)

- `GET /api/leds`: capabilities (which pins can do which kind, how many strings are left),
  configured strings, what each is showing.
- `POST /api/leds/strings` / `PUT …/:id` / `DELETE …/:id`: configure.
- `POST /api/leds/:id/pattern`, `POST /api/leds/:id/pixels` (raw RGB array or hex),
  `POST /api/leds/:id/off`.
- `POST /api/leds/receiver`: sACN / Art-Net / DDP on or off, universes.
- MCP: `leds_list`, `leds_configure`, `leds_pattern`, `leds_set_pixels`, `leds_off`.
- Web UI: a "LED strips" panel: string list, a pin picker that greys out unusable pins
  with the reason, the timing preset dropdown with a "custom" disclosure, a live pixel
  preview, pattern picker, receiver status (packets/s per universe, active source).
- Gated by GPIO output control, like every other write.

## Decisions already made (don't re-ask)

- Both one-wire (WS2812-style) and clocked (APA102-style) strings.
- Color order, timing (simplified: presets + 4-number custom), and length are per string.
- Firmware nodes: any pin.
- sACN receiver is wanted ("heck, even").

## Plan / steps

1. **Answer the open questions below** ← current step
2. Shared `pixels` crate: config, timing presets, encoders, patterns, E1.31/Art-Net/DDP
   parsers, with host tests (golden bit streams per preset).
3. Pico 2 W driver (the easiest any-pin target) + node API/MCP + node UI section.
4. ESP32-C6 driver (RMT + SPI), status LED.
5. Linux `leds` crate: spidev driver, GPIO-uAPI APA102, staging of SPI overlays.
5b. BCM283x/2711 PWM/PCM + DMA driver (GPIO 12/13/18/19/21), once the privilege model is
   decided.
6. Pi 5 `/dev/pio0` driver.
7. Network receiver on both sides.
8. Persistence on nodes, README updates (root, firmware, backend), hardware bring-up.

## Progress log

- [x] Hardware research for each platform (this doc)
- [ ] Open questions answered
- [ ] Everything in "Plan / steps" 2–8

## Open questions for the user

1. **Order of work.** Recommend: shared crate → Pico 2 W → ESP32-C6 → Linux spidev → Pi 5
   PIO → sACN/Art-Net/DDP, because the nodes are the any-pin targets.
2. **Pi 4: SPI only, or also rpi_ws281x-style PWM/PCM DMA?** First draft said SPI only: the DMA
   method needs `/dev/mem`, mailbox memory and conflicts with onboard audio, for only
   GPIO 12/13/18/19/21. **Revised after pushback:** PWM+DMA is the standard Pi method
   (rpi_ws281x's default, GPIO18), not an exotic one. Recommend both: spidev first
   (works on every Linux board), then a PWM/PCM+DMA driver for BCM283x/2711 so GPIO
   12/13/18/19/21 work without a reboot. Open: privilege model for `/dev/mem`.
3. **What did "RCA" mean?** Composite video out (an RCA jack), RC servo / PWM output, or
   something else?
4. **Which of the other GPIO features (below) go on the list**, and do they share this
   plan or get their own?
5. **Network protocols:** sACN only, or also Art-Net and DDP? Recommend all three: the
   parsers are tiny and DDP is what WLED/xLights use.
6. **Persist strings on nodes** so they boot into their pattern/receiver? Recommend yes.
7. **Own panel ("LED strips") or a section of GPIO?** Recommend its own panel.

## Other GPIO features

Proposed, roughly by value on a bench:

- **IR receive and transmit.** Linux: `gpio-ir` (rc-core decoding, any GPIO) and
  `gpio-ir-tx` / `pwm-ir-tx` overlays, used through `/dev/lirc*` (raw pulse/space capture
  and send) by ioctl, no LIRC daemon. Nodes: ESP32-C6 RMT was built for this (RX on
  ch2–3); Pico PIO. Features: capture raw timings, decode NEC/RC5/RC6/Sony/Samsung,
  replay, send by protocol + code. Good for testing remotes and driving TVs/projectors.
- **Logic analyzer / pulse capture** on the Pico (PIO at up to sys clock, 8–16 channels,
  export to sigrok/PulseView `.sr` or VCD). Probably the single most useful bench feature
  here. Linux: low-rate edge timestamps from the GPIO uAPI's event stream.
- **PWM / servo / frequency generator**: duty + frequency, servo pulse widths. Pi hardware
  PWM (GPIO 12/13/18/19), Pi 5 `pwm-pio` on any pin, nodes on any pin.
- **Frequency counter / pulse-width measurement / quadrature encoder** (embassy-rp has a
  rotary encoder PIO program; ESP32-C6 PCNT).
- **DMX512 output** over a UART at 250 kbaud with break: pairs with the sACN receiver into
  an sACN → DMX bridge (needs an RS-485 transceiver).
- **Timing-critical sensors**: DHT11/22, HX711 load cells.
- **Clock output** (Pi GPCLK0 on GPIO4, node PWM): a quick known-frequency source.
- **Composite video** (if that's what "RCA" meant): Pi 4 has it on the AV jack, Pi 5 on
  TV pads, and the RP2350 can generate it with PIO/HSTX. Low value on a bench; not
  recommended unless there's a specific use.

## Things not to do

- Don't bit-bang WS2812 through the GPIO uAPI on Linux: the timing can't be met.
- Don't depend on rpi_ws281x or piolib (C libraries): the backend has no C dependencies.
- Don't put two drivers on one pin or one PIO SM: claim through the GPIO crate.
