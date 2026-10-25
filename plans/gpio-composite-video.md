# Composite video ("RCA")

## Goal

Composite video output from BenchBob hardware. The user said "yes" to composite video on
2026-10-25 (their "RCA"), and also wants RC servo PWM (`plans/gpio-pulse-io.md`). **What
it is for isn't settled yet** (see open questions); so far this plan only records what
the hardware can do.

## Platform findings

- **Pi 4 / 3 / Zero:** composite on the 3.5 mm AV jack (TRRS) or pads, enabled with
  `dtoverlay=vc4-kms-v3d,composite` (plus `sdtv_mode` or
  `video=Composite-1:720x480@60ie`). On the Pi 4 it disables HDMI while active. Staged
  through config.txt + reboot.
- **Pi 5:** composite on the TV-out test pads, the same `,composite` overlay parameter.
- **Pico 2 W (RP2350):** composite from PIO plus a resistor-ladder DAC (RP2040 projects
  exist); monochrome is easy, colour NTSC/PAL harder. HSTX does DVI, not composite.
- **ESP32-C6:** no DAC; not a sensible target.

## Possible uses (to pick from)

- Test-pattern generator (colour bars, crosshatch, NTSC/PAL) for old monitors, capture
  cards, CRTs, camera inputs.
- A status display on a small composite screen.
- Showing something from the API (an agent puts an image or text on a screen).

## Decisions already made (don't re-ask)

- Uses: test patterns, a BenchBob status screen, and a known signal for capture devices. So both a Pi side (the Pi's composite out, rendering patterns / a status screen) and the Pico (PIO/DMA video) are in scope. (User, 2026-10-25.)

## Open questions for the user

1. Answered 2026-10-25: composite is for **test patterns, a status screen, and feeding a
   capture device** (not just a Pi config switch). Recorded in Decisions.
2. Composite input beyond the Camera panel's USB capture: not raised; assume no.

## Progress log

- [ ] Open questions answered
- [ ] Implementation
