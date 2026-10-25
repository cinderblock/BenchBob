# Timing-critical sensors: DHT11/22, HX711

## Goal

Read sensors whose protocols need microsecond timing that the I²C and 1-Wire panels don't
cover: DHT11 / DHT22 / AM2302 (single-wire temperature and humidity) and HX711 (24-bit
load-cell ADC, clocked). Asked for by the user on 2026-10-25.

## Platform findings

- **Linux:** DHT has a kernel driver (`dht11` overlay, IIO device under
  `/sys/bus/iio/devices`, any GPIO); reads fail now and then from interrupt latency, so
  retry. HX711 is clocked, so userspace bit-banging via the GPIO uAPI works, as long as
  SCK never stays high past 60 µs (the chip powers down); mainline also has an `hx711`
  IIO driver.
- **Nodes:** both are easy with PIO (Pico) or RMT / plain GPIO in a critical section (C6).

## Design sketch

- A configured sensor on a pin, periodic readings on the stream, MCP `sensor_read`.
- HX711: tare and calibration factor saved in settings.

## Shared with the other GPIO feature plans

- Pins are claimed through the GPIO crate and shown as exclusions in the GPIO panel
  (rule from `plans/gpio-led-strips.md`); the shared pin picker selects pins.
- Same REST shapes and MCP tool names on the Linux server and the firmware nodes.
- Order: after LED strips steps 2–5, so pin claims, the picker and node plumbing exist.

## Open questions for the user

1. Own panel, or fold into an existing one (1-Wire is the nearest)? Recommend a small
   "Sensors" panel that other pin-attached sensors can join later.

## Progress log

- [ ] Open questions answered
- [ ] Implementation
