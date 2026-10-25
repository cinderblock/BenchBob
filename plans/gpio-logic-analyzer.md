# Logic analyzer / pulse capture

## Goal

Turn a node (above all the Pico 2 W) into a bench logic analyzer driven from the
dashboard and the API/MCP: capture N channels at a chosen sample rate with a trigger,
view the waveform in the browser, decode common protocols, export for PulseView. Asked
for by the user on 2026-10-25.

## Platform findings

- **Pico 2 W:** PIO `in pins, N` + DMA into RAM. Rates up to the system clock (150 MHz on
  the RP2350) for a few channels; 520 KB SRAM, minus firmware and Wi-Fi buffers, bounds
  buffered capture length. Prior art: sigrok-pico, gusmanb's LogicAnalyzer. 3.3 V inputs
  only.
- **ESP32-C6:** PARL_IO RX (parallel input with DMA) or RMT RX for edge timing on a few
  channels; lower rates than the Pico.
- **Linux:** GPIO uAPI edge events with kernel timestamps (`GPIO_V2_LINE_FLAG_EDGE_*`):
  fine for slow signals (kHz), not a real analyzer. Pi 5 PIO via `/dev/pio0` could sample
  faster, but only the FIFOs are reachable (DMA): check the throughput.
- Wi-Fi throughput (a few Mbit/s realistic) limits streaming: buffered capture, then
  upload.

## Design sketch

- Capture config: channels (pins), rate, depth, trigger (edge/level/pattern,
  pre-trigger %). Results as run-length encoded edges, so idle lines cost nothing.
- Export: VCD and sigrok `.sr` (a zip of metadata + logic chunks).
- Browser waveform viewer; decoders for UART, I²C, SPI, 1-Wire, WS2812, IR (reusing the IR
  plan's decoders).
- MCP: `la_capture` returning decoded results, so an agent can debug a bus.

## Shared with the other GPIO feature plans

- Pins are claimed through the GPIO crate and shown as exclusions in the GPIO panel
  (rule from `plans/gpio-led-strips.md`); the shared pin picker selects channels.
- Same REST shapes and MCP tool names on the Linux server and the firmware nodes.
- Order: after LED strips steps 2–5, so pin claims, the picker and node plumbing exist.

## Open questions for the user

1. SUMP/OLS over USB CDC (PulseView/sigrok driving the Pico directly): not asked yet;
   taking the recommendation (yes, later) unless the user says otherwise.
2. Linux edge-timestamp mode: taking the recommendation (yes, labelled "slow signals").

## Progress log

- [ ] Open questions answered
- [ ] Implementation
