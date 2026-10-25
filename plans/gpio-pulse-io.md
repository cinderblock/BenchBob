# Pulse I/O: PWM, RC servo, clock out, frequency counter, encoders

## Goal

Generate and measure simple pulse signals on GPIO from any BenchBob and node:

- **Out:** PWM (frequency + duty), RC servo (500–2500 µs pulses at 50 Hz, or an angle),
  a clock output (a known frequency source).
- **In:** frequency counter, pulse width / duty measurement, quadrature encoder count.

Asked for by the user on 2026-10-25 ("good idea" to RC servo PWM as a reading of "RCA",
alongside composite video, which has its own plan).

## Platform findings

- **Pi 1–4:** hardware PWM0 on GPIO 12/18, PWM1 on 13/19 (`pwm` / `pwm-2chan` overlays,
  `/sys/class/pwm`). Conflicts with onboard analog audio and with the LED strips' PWM+DMA
  driver: claim through the GPIO crate. GPCLK0/1/2 on GPIO 4/5/6 for clock out.
- **Pi 5:** RP1 PWM on a few pins, plus `pwm-pio` (any GPIO 0–27, up to 4, sharing PIO
  with LED strips).
- **Other Linux boards:** `/sys/class/pwm` wherever the board file documents PWM overlays.
- **Linux input side:** GPIO uAPI edge events with timestamps (low rates only).
- **Pico 2 W:** 12 PWM slices (24 channels); slices can also count edges (frequency in)
  and gate (pulse width). embassy-rp ships PIO programs for PWM, rotary encoder, clock
  divider and stepper.
- **ESP32-C6:** LEDC (6 channels) and MCPWM out; PCNT for counting and quadrature; RMT RX
  for pulse widths. Any pin.

## Design sketch

- One "Pulse I/O" panel with channels of a kind: pwm, servo, clock, counter, pulseWidth,
  encoder. Servo channels have min/max limits, so a servo can't be driven into its end
  stops.
- Live readouts on the stream; MCP tools `pulse_set`, `pulse_read`.
- Stepper step/dir pulse trains are a natural extension (embassy-rp's stepper program).

## Shared with the other GPIO feature plans

- Pins are claimed through the GPIO crate and shown as exclusions in the GPIO panel
  (rule from `plans/gpio-led-strips.md`); the shared pin picker selects pins.
- Outputs are gated by GPIO output control, off at start like every other output.
- Same REST shapes and MCP tool names on the Linux server and the firmware nodes.
- Order: after LED strips steps 2–5, so pin claims, the picker and node plumbing exist.

## Open questions for the user

1. Include stepper step/dir generation now or later? Recommend later.

## Progress log

- [ ] Open questions answered
- [ ] Implementation
