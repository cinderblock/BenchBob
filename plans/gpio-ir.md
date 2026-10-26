# IR receive and transmit on GPIO

## Goal

Capture, decode, replay and send consumer infrared from any BenchBob and node: test a
remote, learn a code, drive a TV/projector/AC unit from the API or an AI agent. Asked for
by the user on 2026-10-25 (from the "other GPIO features" list in
`plans/gpio-led-strips.md`).

## Platform findings

- **Linux (Pi and others):** kernel overlays, no daemon. `gpio-ir` (any GPIO; params
  gpio_pin, gpio_pull, invert, rc-map-name; kernel rc-core decoding to an input device)
  and `gpio-ir-tx` (any GPIO, bit-banged carrier) or `pwm-ir-tx` (Pi PWM0 on GPIO 12/18,
  lower CPU). Both expose `/dev/lirc*`: raw pulse/space read and write by ioctl
  (LIRC_GET_FEATURES, LIRC_SET_SEND_CARRIER, LIRC_SET_SEND_DUTY_CYCLE, LIRC_SET_REC_MODE
  with LIRC_MODE_MODE2 or LIRC_MODE_SCANCODE). Overlay changes go through the existing
  config.txt / boot-overlay staging + reboot.
- **ESP32-C6:** RMT was designed for IR: RX on channels 2–3, TX on 0–1 with carrier
  modulation in hardware. Any pin. (TX channels are shared with LED strips.)
- **Pico 2 W:** PIO for RX (edge timing) and TX (carrier); any pin.
- Hardware: a demodulating receiver (TSOP38238-style, 38 kHz) and an IR LED with a
  transistor driver; the UI should say so.

## Design sketch

- Shared `no_std` decoder/encoder: NEC (+extended), RC5, RC6 (incl. MCE), Sony SIRC
  12/15/20, Samsung, JVC, Panasonic/Kaseikyo; raw pulse arrays as the fallback. Pronto
  hex import/export (the common interchange format).
- API/MCP: `ir_capture` (wait for the next frame: decoded + raw), a live capture stream,
  `ir_send` (protocol + address/command, or raw timings + carrier), saved codes.
- Own panel ("IR"), same pattern as LED strips.

## Shared with the other GPIO feature plans

- Pins are claimed through the GPIO crate, so a pin can't be in two features, and the
  GPIO panel lists it with this feature as its owner (the exclusions rule from
  `plans/gpio-led-strips.md`).
- The pin picker is the shared header-diagram component built for LED strips.
- Outputs are gated by GPIO output control, off at start like every other output.
  Settings persist like any other settings; running jobs don't outlive the process.
- Same REST shapes and MCP tool names on the Linux server and the firmware nodes.
- Order: after LED strips steps 2–5, so pin claims, the picker and node plumbing exist.

## Decisions already made (don't re-ask)

- Saved codes: a named list per device in settings. Receive is BenchBob-only (LIRC mode2); no rc-core keypresses. (User, 2026-10-25.)

## Open questions for the user

Answered 2026-10-25: a **named-codes library** (named buttons per device, in settings),
and receive stays **BenchBob-only** (raw LIRC mode2, no Linux input device). Recorded in
Decisions.

## Plan / steps

1. **Codec crate** `firmware/ir` (`benchbob-ir`, `no_std`, no deps, host-tested; shared
   by the Linux crate and the nodes). Timings are µs, alternating mark/space, starting
   with a mark. Protocols: NEC (8-bit and extended 16-bit address, plus its repeat
   code), Samsung32, Sony SIRC 12/15/20, RC5, RC6 mode 0, JVC; anything else is raw.
   Pronto hex import/export. Done (see Progress log).
2. ~~**Linux** `backend/crates/ir` (panel id `ir`)~~ done: `/dev/lirc*` (features ioctl, mode2
   receive, pulse write with carrier/duty), a receive thread that splits frames on long
   spaces and decodes them, a ring of recent captures, `capture` (wait for the next
   frame), send (gated by output control), the named-codes library in settings. Overlays
   (`gpio-ir`, `gpio-ir-tx`, `pwm-ir-tx`) staged through the GPIO panel's interfaces, with
   `rc-map-name=rc-empty` so nothing becomes keypresses. Pins claimed in the GPIO panel.
3. ~~**Web**~~ done: an IR panel: receivers/transmitters, live captures (decoded + raw), Learn
   (capture → name → save to a device), the library with send buttons, raw/Pronto send.
   MCP: `ir_list`, `ir_capture`, `ir_send`, `ir_codes`.
4. **Nodes**: Pico (PIO) and ESP32-C6 (RMT RX ch 2–3, TX with hardware carrier). ← next

## Progress log

- [x] Open questions answered (2026-10-25)
- [x] Step 1 (2026-10-25): `firmware/ir` (`benchbob-ir`): `decode`/`encode` for NEC,
  NEC extended, NEC repeat, Samsung32, Sony 12/15/20, RC5 (incl. RC5X), RC6 mode 0, JVC;
  `pronto::{parse, format}`. 7 host tests: every protocol round-trips, also with marks
  10 % long and spaces 10 % short (receiver skew); known frames; noise. Builds for
  thumbv8m and riscv32imac. In CI's firmware-test job.
- [x] Steps 2–3 (2026-10-25): `backend/crates/ir` (package `benchbob-ir-panel`, panel
  `ir`): `lirc.rs` (features ioctl, mode2 samples, `Framer` splitting at 15 ms spaces,
  pulse send with carrier + 33 % duty), captures ring (32), `capture` waits on a Notify,
  `plan()` turns a code / timings / Pronto / library button into frames (Sony ×3, NEC
  hold = repeat codes), library in settings (`fromCapture` saves a learned one). GPIO
  panel: `ir: {rx, tx, txPwm}` stages `gpio-ir,gpio_pin=N,rc-map-name=rc-empty`,
  `gpio-ir-tx` or `pwm-ir-tx` (GPIO18 func 2 / GPIO12 func 4), in the pin-conflict check.
  Web: `panels/ir/Ir.tsx` (Learn, captures with raw/Pronto, library buttons, send form) and
  an IR group in the Interfaces dialog. Tests: 4 Rust (IR crate) + 1 GPIO + 3 web.
  **On desky through the real kernel**, using `rc-loopback` (sends come back as
  receives): output control gated the first send; all nine protocols round-tripped
  (nec, necx, samsung, sony12/15/20, rc5, rc6, jvc) decoded with the right address and
  command; Learn → library → send by button name, and Pronto replay, both came back as
  the same Samsung code; the library persisted. Cleaned up afterwards (side instance
  stopped by PID, rc-loopback unloaded).
- [ ] Step 4 (nodes); real IR hardware (receiver/LED) not tried yet

## Findings

- An extended-NEC address whose two bytes are complements is indistinguishable from
  plain NEC on the wire; it decodes as `nec`.
- Pulse-distance decoders must check the frame length: JVC's 16 bits otherwise match the
  first half of any NEC frame (NEC's 9 ms leader is within JVC's 8.4 ms ± 30 %).
