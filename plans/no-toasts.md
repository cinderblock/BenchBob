# Replace toasts with in-place status

## Goal

The user (2026-10-21): "status of events like turning off and on USB power should show up
in the panel, not as an independent toast", then "clean them all up". Every action's
progress, result and failure is shown where the action was taken (beside its button, in
its card, row or dialog), and the global toast stack goes away entirely.

## Environment / context

- Web UI: `web/` (Vite + React authored, built against preact/compat, Bun). Checks:
  `bun run typecheck`, `bun run test`, `bun run build` (includes the MCU size budget).
- Shared working tree on master; other threads may be active. Only stage own files.
- Legacy `index.html` + `ui/*.js` (old dashboard) are being retired by the Rust/React port;
  out of scope.

## Decisions already made (don't re-ask)

- All toasts go, including the page-level ones (user: "clean them all up"). `toast` is
  removed from `BobApi` so it can't creep back.
- `b.api` stops toasting errors; it just throws. Every caller shows failures in place.
- One shared primitive (`web/src/ui/status.tsx`): `useAction()` + `<StatusLine>`. Status
  shape: busy / ok / error, a message, a time. OK and error lines stay until dismissed (×)
  or replaced by the next action on the same thing; busy disables the triggering buttons.
- `AsyncButton` shows its own failure (and optional success text) inline beside itself.
- Validation messages ("Choose a file first", "Enter an ID") show inline as errors on the
  same status line as the action they block.
- Stream `error` messages (serial/CAN/BLE sockets) show as a status line in the panel.
- Reference implementation: USB port power (`effc87f`), refactored onto the primitive.

## Plan / steps

1. [x] USB port power inline status (`effc87f`).
2. [x] Primitive in `ui/status.tsx`, AsyncButton inline status, `b.api` stops
   toasting; USB panel moved onto the primitive (`a9e1204`).
3. [x] Convert every remaining toast / silent-catch site, by area (parallel
   forks in worktrees off `a9e1204`, cherry-picked onto master; no force-push):
   - A: CAN, serial, BLE
   - B: probes, 1-Wire, GPIO (+Stage, Interfaces), I²C, node sections
   - C: Wi-Fi, netdiag, camera, GPS, SDR, UPS, shell/Periph
   - D: sections: drives, file browser, flash, boot, bootloader dialogs, system copy,
     update banner
4. [x] Remove `toast` from `BobApi`, `<Bob>`, CSS; new-version reload notice becomes a
   banner. Tests updated (`2c54643`).
5. [x] README check (no mention); final typecheck/test/build pass (79 tests).

## Findings / gotchas

- `b.api` (Bob.tsx) toasted every error and rethrew; ~94 call sites relied on that with
  `catch {}`. Those are the silent-failure risk once it stops toasting: grep for
  `catch {}` / `.catch(() => {})` after conversion.
- The Bash tool mangled a long python heredoc (`unexpected EOF while looking for matching`);
  use the Edit tool for multi-hunk edits.

## Progress log

- [x] USB power inline (`effc87f`)
- [x] Primitive + USB panel (`a9e1204`)
- [x] A: CAN/serial/BLE (`3212c9c`)
- [x] C: Wi-Fi/netdiag/camera/GPS/SDR/UPS/Periph (`a91aa81`)
- [x] B: probes/1-Wire/GPIO/I²C/node (`6ab35e5`)
- [x] D: drives/flash/boot/bootloader/system/update banner (`cc4d754`)
- [x] Toasts removed (`2c54643`)
- Remaining deliberate empty catches: JSON parse in connection.ts, localStorage write,
  node-API probe in store.ts, Wi-Fi watch-lease renewal (retries every 20 s).
- Flaky cold-run test timeouts logged in ISSUES.md (`76aa97f`), not part of this task.

## Findings (cont.)

- `findBy*` hangs under `vi.useFakeTimers({ toFake: ["setTimeout"] })` (it polls with
  setTimeout); use `act()` + `getBy*` while timers are faked.

## Things not to do

- Don't leave `catch {}` that swallows an error without showing it somewhere.
- Don't touch legacy `ui/*.js` / `index.html`.
