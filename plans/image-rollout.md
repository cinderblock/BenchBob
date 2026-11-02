# Rolling a new golden image out to the machines on it

Status: **merged to BootBob master (`7091b0f`), CI green, waiting for Renovate to deploy it.** Then
`time_zone` and the real Pi test, both approved. Parent plan: `plans/pxe-netboot.md`.

## Goal

From the host, on a schedule, move the machines that share a golden image onto a new version of
it, and be able to move them back. Approved by the user 2026-11-01 as a separate thread from TRIM.

Only for "cattle": machines whose state lives outside the root (a data disk, NFS). Plain disks
such as `desky.img` are pets and stay out; desky updates itself (`pxe-netboot.md`, "Updates on
desky").

## Environment / context

| Thing | Value |
| --- | --- |
| BootBob | `~/git/Personal Projects/BootBob`, public, so nothing private goes in it. Work is in the worktree `~/git/Personal Projects/BootBob-rollout`, branch `rollout` |
| Deploys | A master push publishes to GHCR; Renovate bumps both pins (steamboat, uberfall) and they restart. **So master is a deploy.** Never edit pins |
| Live on 2026-11-01 (read from the API) | Images `desky` (plain), `debian-trixie-rpi`, `debian-trixie-amd64` (both copy-on-write, on uberfall). Machines: desky on `desky`, two others only "seen". **Nothing shares a base today** |
| Test host | sentinel: Docker, 8+ cores, `~/bootbob-vmtest`. Throwaway privileged containers change nothing on the host |
| Local builds | `~/.srm/bin/srmctl run --slots 4 --low --label ... -- cargo ...` |

## How it works

**A track** is a named series of versions of one golden image. **A version** is an ordinary
BootBob image over its own immutable, dated disk file, e.g. `debian-trixie-rpi-arm64-20261108.img`.
A track points at one version as `current`.

**A machine follows a track** by opting in: its assignment gets `track: "<id>"`. The assignment's
`image` is still a plain version, which is the key simplification: `resolve`, TFTP, iPXE, NBD and
the store check need no change. They only ever see an ordinary assignment to an ordinary image.

**Moving a machine** means changing `assignment.image` to the new version and having the machine
reboot. It boots the new disk, and since overlays are named after their disk, it gets a new
overlay by itself. **The rollout never resets or deletes an overlay.** The old one stays exactly
as it was, so going back is the same operation in reverse, and the machine gets its state back.

**The host decides, the machine obeys.** A timer in the image asks `GET /boot/rollout?mac=…` every
2 minutes, saying its systemd state and whether it's busy (someone logged in, apt running, or an
optional `/etc/bootbob/busy` hook). The answer is the word `stay` or `reboot`. Any error means
stay. So the schedule, the ordering and the health check all live on the host.

**One machine at a time, stop at the first failure.** Every 30 s, for each track:

1. A machine in flight finishes first. It's done once it has booted the target and then reported
   `running`. A machine that reports busy before rebooting goes back on its old assignment and
   stays in the queue. One that never reboots (10 min) goes back and waits for the next window.
2. **A machine that boots but isn't healthy in 15 min halts the track.** Its assignment goes back,
   and if it's still answering, it's told to reboot onto its old version. If it isn't, it needs a
   person, and the log says so.
3. Inside the window (default 05:00 for 60 min in the configured time zone), or after "Run now", if
   nothing's in flight and the track isn't halted, it checks the target's boot files can be read
   (so no machine reboots into a dead store, or a download that isn't there yet) and starts the
   next idle machine. A machine that hasn't reported for 30 min isn't waited on: its assignment
   moves, and it gets the new version whenever it next boots.

**Publishing:** upload the dated disk, then `POST /api/tracks/{id}/versions {"source": …}`, which
copies the current version's definition onto the new disk. It's refused unless the disk's boot
files can be read. That makes it `current`; the next window does the rest.

**Rollback:** `POST /api/tracks/{id}/release {"image": <older version>, "now": true}`.

## Decisions already made (don't re-ask)

- Rollouts are for cattle only. A shared base can't change under an overlay (`35b9f4b`).
- Phase 3 (the controller) needs its own go-ahead, and isn't built here.
- `rpi-eeprom` stays out of automatic updates (user, 2026-11-01, overriding `51b22f2`).
- Decided here, smallest useful version:
  - **A track only takes copy-on-write versions with one disk source**, whose command line uses
    `{disk}` and `{bootbob}`. A plain disk can't get on one, so pets are excluded by construction.
  - **Opt-in per machine.** Following a track is what says "this root is disposable". Nothing else
    does: a machine merely assigned a version never moves.
  - **Nothing is deleted automatically.** Old versions and their overlays stay until a person
    deletes them, because deleting one is what loses state. A version can't be deleted while it's
    current, or any machine runs it, is assigned it, or may be sent back to it.
  - **One window, in settings**, not per track. One machine at a time needs no batch size.
  - **A person publishes.** CI builds dated images, and nothing adopts one by itself. Automatic
    adoption (BootBob watching a feed) is a good next step, but it would ship every CI build
    unreviewed, and the user hasn't asked for that.
  - **The machine reports its own MAC**, the same trust as `/boot/ipxe?mac=`: LAN only, no auth. A
    liar on the LAN can report busy or healthy for another machine, which could delay or wrongly
    pass a rollout. It can't reboot anything or reach another machine's disk.
  - **Shell + curl agent, not a Rust binary.** Under 40 lines, and systemd already gives a timer.
  - **`jiff` for "5 AM local"**: pure Rust with a bundled time zone database, so DST is right and the
    container needs no tzdata.

## Where this stops short of Phase 3

- **BootBob can't reboot anything.** A machine reboots only if its own timer is alive. A wedged
  machine stays wedged until someone power-cycles it (its assignment is already back on the old
  version, so that's all it needs).
- **Health means systemd's opinion of itself.** A service that fails marks the machine `degraded`,
  which is a failure here. Nothing looks inside the service.
- **No classes**, no power control, no screen, no way to see a machine that never reached userland
  beyond the boot log.

## Limits of the smallest version

- A new version is a fresh root: new SSH host keys and machine-id (`bootbob-first-boot`). That's what
  cattle means, but `known_hosts` will notice. Stable identity across versions isn't built.
- The image has no credentials, so keys are added to a copy before upload, as today
  (`mcopy` of `authorized_keys`). That's a reason publishing stays a person's job.
- Returning to a version resumes its old overlay rather than starting fresh. Reset it by hand first
  if that's not wanted. The rollout won't.

## Plan / steps

1. [x] Backend: `track` on assignments, `Track` table, `{disk}`/`{bootbob}` for Pis, rollout engine,
   `/boot/rollout`, tracks API, settings. Unit tests for every branch above.
2. [x] Machine side: `tools/machine/`, baked into both golden images. `curl` added.
3. [x] `rpi-eeprom-update` masked in the Pi golden image, checked by `test-pi-golden.sh`.
4. [x] Web UI: Rollouts tab, follow a track from a machine. Typecheck and build pass; **not looked at
   in a browser**.
5. [x] CI: dated images, weekly, PC image built too (`pc-golden.yml`).
6. [x] `tools/test-rollout.sh`: real VMs on a private bridge, real agent, real reboots, real
   rollback. Passed on sentinel in a throwaway container.
7. [x] README, BootBob ISSUES.
8. [x] Branch CI on `89eefbf`: success on every job, conclusions read from the runs. CI 41357709779
   (`publish` skipped, as it should be off master), PC golden 41357708246, which ran a real rollout on
   GitHub's runner, Pi golden 41357712636, which checked the EEPROM mask in an arm64 VM.
9. [x] `rollout` taken out of `pc-golden.yml`'s push branches again (the only change since).
10. [x] Merged: master `f80a805`, a fast-forward (user yes, 2026-11-01), then `7091b0f`, a test-only
    fix. CI on `7091b0f` success on every job, `publish` included (41365972494), and PC golden success,
    a real rollout on GitHub's runner (41365972479). Pi golden success on `f80a805` (41362729264).
    desky was the only NBD root, running `bootbob-nbd-client`.
11. [ ] **Current step:** wait for Renovate to bump both pins. Don't touch them. At 10:46 AM PST
    (18:46Z) on 2026-11-02 both pins were still `c1513d9`, and `/api/tracks` answered the web page's
    HTML with a 200, so the old build is running. `7091b0f` is published to GHCR as `latest`. Today's
    scheduled Renovate run hasn't started: the cron says 12:00Z, but GitHub has started it between
    15:06Z and 17:43Z for the last 8 days, so it's already later than usual. Next check 12:30 PM PST
    (20:30Z).
12. [ ] Approved: set `time_zone` to `America/Los_Angeles` in BootBob's settings once deployed.
13. [ ] Approved: a real Pi run on desky, once deployed, only when idle, back on image `desky` after.
    Steps below.

## Findings / gotchas

- A systemd timer's default `AccuracySec` is 1 minute, so the test's 15 s timer could run up to a
  minute late. That made the login check flaky: PC golden failed on master (run 41362729127, `FAIL: a
  login wasn't reported`) after passing on the branch and twice on sentinel. It's the test, not the
  rollout: every earlier step in that run passed. The test's drop-in now sets `AccuracySec=1s`.
- `A Pi that reboots quickly ignores a new assignment` (BootBob ISSUES, from another thread): a
  rollout tells a Pi to reboot well over 60 s after its last TFTP read. If it ever did hit, the move
  times out and the machine is put back.

- `make-pi-golden.sh` has `rpi-eeprom-update` enabled since `51b22f2`. A rollout would run it on
  every Pi on every version, against the user's rule, so it goes off in step 3.
- `debian-trixie-rpi`'s command line names its export by hand. A copy onto a new disk would boot
  the old disk's root under the new disk's kernel. `{disk}` for Pis fixes that, and a track refuses
  a version without it.

- GHCR refuses a pull from sentinel (`error from registry: denied`). The VM test takes iPXE builds
  and the NBD client from BootBob's own `/files/` over HTTP instead (read-only).
- An empty `OnUnitActiveSec=` in a drop-in clears **every** timer in the section, `OnBootSec` too.
  The timer then sits `active (elapsed)`, `Trigger: n/a`, and never fires. That was the test's
  speed-up drop-in, not the shipped timer.
- The PC image had no logind (no `dbus`, no `libpam-systemd`), so "someone's logged in" could never
  fire there. Both added to `make-pc-golden.sh`, and the VM test checks a real session is reported.
- `tail` in a pipeline hides the exit code: one run looked fine with the VM step never started.
- A snapshot of an overlay taken while its machine runs changes before the machine leaves: a running
  system writes to its root up to shutdown. The check starts once it has left.
- A bare `wait` in `test-rollout.sh` waits for QEMU and BootBob too, so it hung. Wait for the pid.
- Killing `ssh sentinel` also ended the container running there, so a timed-out test left nothing.
- `~/bootbob-rollout` on sentinel (source, `golden.img`) and Docker volumes `bootbob-rollout-cargo`
  and `bootbob-rollout-target` are this thread's. Remove them when it's over.

## Progress log

- [x] Read the plan, BootBob's code, live state (read-only). Design written.
- [x] Engine, API, UI, machine timer, golden image changes written. Unit tests: all pass on Linux in
      `rust:1` on sentinel (95 in `bootbob`). Clippy and Windows still to run.
- [x] VM test passed on sentinel, with an image built without keys, as CI's are.
- [x] All nine mutations caught, after adding one test. Windows and clippy clean.
- [x] Committed on branch `rollout` (`89eefbf`), CI success on all three workflows, merged to master.

## What's proven (2026-11-01)

`tools/test-rollout.sh`, about 12 minutes, on sentinel with KVM in a throwaway `debian:trixie`
container, private bridge, real `bootbob` binary, firmware netboot (SeaBIOS), a real
`make-pc-golden.sh` image with no key in it, real check-in timer (sped up to 15 s). Run in the
afternoon of 2026-11-01 PST:

| Check | Result |
| --- | --- |
| Published, window closed, 35 s | Nothing moved |
| "Run now" | One machine at a time; the second went after the first said healthy |
| The file machine 0 wrote on version 1 | Gone on version 2's fresh root |
| Machine 0's overlay of version 1, after it left | Byte for byte the same through machine 1's move |
| Rollback | Both moved back, and **machine 0's file was there again** |
| `/etc/bootbob/busy` printing | Reported; machine 1 went ahead; machine 0 took its turn once it stopped |
| An open SSH session | Reported as `login` |
| A version with a failing service | `degraded`, track halted after 3 min, machine took itself back, other machine never left |
| SHA-256 of every version's disk | Unchanged |

Unit tests: 96 in `bootbob`, all passing on Linux and Windows.
fmt and clippy clean. **Mutation-checked** on a private copy, all nine caught: busy machines
rebooting, two at once, a halted track moving, any report counting as healthy, a report from before
the reboot counting, a plain disk on a track, ignoring the window, no check that the disk is
readable, deleting a version still running. That last one wasn't caught at first, which is why
`the_version_a_machine_is_leaving_stays_until_it_has_left` exists.

**Not proven:**

- A real Pi. Unit tests cover `{disk}`/`{bootbob}` over TFTP; the Pi image's timer is checked in an
  arm64 VM with no `bootbob.url`, so it has never actually been told to reboot. The 2-minute shipped
  timer has run only as the sped-up copy.
- A machine on a **store**: the VMs' disks were local to BootBob. Nothing in the rollout is
  store-specific beyond the readable-disk check, which uses the same path as boot files.
- The daily window firing by itself on real machines. "Run now" drove the VM test; the window is
  unit-tested, including the morning the clocks changed.
- A catalogue-fetched version. `ready` goes through the same hold as a boot, tested only that far.
- The web UI in a browser.
- The hour-long failure paths on real machines: a machine that never reboots (unit-tested only).

## Steps for desky, once deployed (approved 2026-11-01)

The artifact is `debian-trixie-rpi-arm64-20261101` from run 41362729264, expires 2026-12-01. This
thread is set to resume itself at 12:30 PM PST (20:30Z) on 2026-11-02.

Only if idle: `who`, load, and no other thread `running` on it (`pxe-netboot.md`: a quiet machine
isn't proof). The golden image needs the new build, with keys added, uploaded twice under two names.
Follow a test track, boot it, publish the second, Run now, expect it to reboot itself. Then
`PUT` `{"image":"desky","once":false}` and reboot it from root's login, and confirm image `desky`,
root rw. `desky.img` isn't touched by any of this: it's a plain disk, and only its assignment changes.

## LAN-level settings (user, 2026-11-01: "we should have a LAN level setting")

What the network hands out today, read from real leases on sentinel and steamboat:

- **NTP: yes.** DHCP gives `NTP=10.255.0.9`, the Home Assistant Yellow, whose `chrony` add-on serves
  it (stratum 3, running). steamboat syncs from it. The UXG (`10.255.0.1`) and steamboat don't answer
  NTP. This is a live UniFi setting that **ops doesn't model**.
- **Time zone: not as far as anything shows.** The leases have no `TIMEZONE=`, but that's weak
  evidence: networkd only asks for option 101 with `UseTimezone=yes`, and a server sends it only when
  asked. The strong evidence is that `rest/dhcpoption` is empty on all six sites and no network has a
  custom option field.
- **The user recalls 101 being enabled once. Ops didn't remove it.** No commit in `unifi/` has ever
  mentioned `dhcpoption` or option 101, nothing calls that endpoint, and the only network write sends
  the whole live object back with just the boot fields changed (`netbootBody`), so it can't drop a
  field. Whether it ever existed can't be told: `stat/event` and `stat/admin` answer 404 here.
- Golden Pi images use networkd, which takes DHCP's NTP server, so they should use it with no change
  (check on desky's test). desky itself runs NetworkManager on an externally configured interface,
  so it ignores the option and uses `2.debian.pool.ntp.org`.

**What the gateway's APIs offer (read-only GETs, 2026-11-01 evening PST; UXG-Pro 5.1.26.33914,
Network 10.6.109, site `default`).** The ops API key works on both APIs:

| API | Time zone | NTP | Custom DHCP options |
| --- | --- | --- | --- |
| Integration v1, `/proxy/network/integration/v1/`, 44 paths, spec at `/proxy/network/api-docs/integration.json` | None | Per network, `dhcpConfiguration.ntpServerIpAddresses` | None. Only `option43Value`, `pxeConfiguration` |
| Older `/proxy/network/api/s/default/` | **`rest/setting` key `locale`: `timezone = America/Los_Angeles`**, also `stat/sysinfo` | `rest/networkconf` `dhcpd_ntp_enabled`, `dhcpd_ntp_1`. The gateway's own in `rest/setting` key `ntp` | `rest/dhcpoption` answers 200 with 0 items, so the object exists. **How a network attaches one is unknown**: no network has such a field, and the field can't be seen until one is set |

So the site already has a LAN-level time zone, it just isn't handed to clients. `/v2/.../dhcp-options`,
`/v2/.../dhcp/options` and `rest/dhcp_option` don't exist (404, 404, 400).

Ways to get it to BootBob, best first:

1. **DHCP option 101.** Standard, works for anything on the LAN, needs no UniFi knowledge in BootBob.
   Unproven: needs one custom option added on a quiet network (UI or API, needs a yes) and the JSON
   read back. Web sources say UniFi takes custom values through an option code, and one reports hex
   byte values, so string encoding needs checking. Clients only get 101 if they ask, so the doctor's
   probe and networkd (`UseTimezone=yes`) must request it.
2. **Ops-managed config.** A `time_zone` in `bootbob.toml`, with the value in ops. No new DHCP, but
   it's a second copy of the zone rather than one LAN setting.
3. **BootBob reads UniFi's API.** Rejected: it ties BootBob, a public project, to one router.

**Done (user yes for `TSL`, 2026-11-01 evening PST):** option 101 is live on `TSL`.

- `POST rest/dhcpoption` `{"name":"tzdb_timezone","code":"101","type":"text","signed":false}` made the
  site-level definition, `_id` `6ae8030eb6bda5fd0d234084`. That changes no network by itself.
- `PUT rest/networkconf/59e1a496e4b0f74756791b5f`: the live object plus
  `dhcpd_user_option_6ae8030eb6bda5fd0d234084 = America/Los_Angeles`. A diff against a snapshot showed
  that one field and nothing else. The field is named for the definition's **`_id`**:
  `dhcpd_user_option_tzdb_timezone` and `dhcpd_user_option_101` were refused with
  `api.err.InvalidDhcpUserOption`, which changed nothing. Found from the Network UI's own JavaScript
  (`/proxy/network/manage/react/js/`, prefix `dhcpd_user_option_`, codes 7-254, a few disallowed).
- Proved with a DISCOVER from sentinel's `enp12s0` (made-up MAC, never REQUEST) asking for 42, 100 and
  101: the offer from `10.255.0.1` carried `101: America/Los_Angeles` and `42: 10.255.0.9`. It sends
  no option 100, and nothing was defined for it.
- sentinel, steamboat, desky (root rw) and BootBob all answered afterwards.
- To undo: `PUT` the network without that field, then `DELETE rest/dhcpoption/<_id>`.

**Staged, not pushed:** ops `0a1b27b` on branch `unifi-lan-dhcp`, worktree `~/git/Personal Projects/ops-lan-dhcp`.
A `lan:` block (`time_zone`, `ntp`) in `unifi/config/tomsawyerlabs.yaml`, with `derive-lan.ts`
following `netboot:`. 164 tests pass. A real dry run says `LAN settings: in sync` and all sites clean,
so applying it changes nothing on the gateway. It re-reads the network right before writing, because
`netboot:` writes the same object in the same run.

**Not built:** BootBob reading the zone from the network. Its doctor probe has to ask for 101, then
`time_zone` can default to it. Golden images would need `UseTimezone=yes` in their networkd config.
Until BootBob reads it, `time_zone` is set by hand.

## Open questions for the user

1. Push ops branch `unifi-lan-dhcp` to master, so `lan:` is managed there? It changes nothing on the
   gateway. Recommended: yes.
2. Build BootBob reading the time zone from DHCP 101? Recommended: yes, as its own small change,
   after the Pi test.

## Things not to do

- Don't reset or delete an overlay from the rollout, for any reason.
- Don't edit pins, and don't merge to master without a yes: it deploys.
- Don't put a plain-disk image on a track.
- Don't enable `rpi-eeprom-update` in a golden image.
- Don't use desky for tests unless idle (`who`, load, recent thread activity), and leave it on image
  `desky`. `desky.img` is the only live copy of its root.
- Don't run VMs on the user's screen. Everything runs in a container on sentinel, or in CI.
