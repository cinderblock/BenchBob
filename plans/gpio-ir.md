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

## Open questions for the user

1. A saved-codes library on the box (named buttons per device), or capture/send only?
   Recommend a simple named list in settings.
2. Should receive also inject keypresses as a Linux input device (rc-core's default), or
   stay BenchBob-only? Recommend BenchBob-only (raw LIRC_MODE_MODE2).

## Progress log

- [ ] Open questions answered
- [ ] Implementation
