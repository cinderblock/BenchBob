# PXE / netboot server on steamboat, and a diskless DeskyPi

Status: **Phase 1 in progress.** BootBob repo created (public,
<https://github.com/cinderblock/BootBob>, local `~/git/Personal Projects/BootBob`) with the
server, web UI, iPXE build and a QEMU end-to-end test. Nothing deployed; steamboat, UniFi
and desky untouched. Written 2026-10-23; revised 9:30 AM PDT (16:30Z) after round 1, 3:40 PM
PDT (22:40Z) after round 2, and 4:30 PM PDT (23:30Z) after round 3 (storage, mDNS, repo).
Name: **BootBob**, in its own new repo (decided round 2).

## Goal

1. Run a network boot server on **steamboat** that any machine on the TSL LAN can boot
   from: PCs (legacy BIOS, UEFI x86-64, UEFI arm64) and Raspberry Pis (native Pi
   firmware netboot).
2. Boot **desky** (DeskyPi / BenchBob, Pi 4B) from it with **no onboard storage**, so
   the Pi's microSD slot is free to be one of BenchBob's I/O ports (read, flash, image
   cards like any other drive).
3. Decide whether to adopt an existing PXE project or build our own, against the
   feature list the user wants:
   - trivial setup
   - LAN-only web UI, no auth except an optional simple password (passkeys later)
   - assign a boot image to a device, or to a class of devices
   - a default image: a small, fast, skinnable boot menu matching the web UI theme,
     choosing from a large menu
   - web control of machines booted into that default image (for headless machines)
   - full systems with network-mounted root
   - installers: Linux distros, recent Windows, macOS if possible
   - rescue, backup and restore images

## Environment / context

| Thing | Value |
| --- | --- |
| steamboat | Intel NUC8i5BEH, Ubuntu 25.04, kernel 6.14, 8 cores, 30 GiB RAM, 477 GB NVMe (ext4 root, ~360 GB free). Plus a **spare 112 GB SATA SSD** (`sda`, ADATA SU655): `sda1` 512 MB vfat + `sda2` 111 GB ext4, **not mounted**, contents unknown (looks like an old OS install; not inspected). avahi-daemon inactive. `cameron@steamboat` |
| uberfall | Lenovo TS430 at TSL, Ubuntu 24.04, 4×11 TB, company storage. Candidate remote store for big images and backups |
| steamboat LAN | `eno1` untagged: **10.255.0.5/20** (TSL main LAN). Also VLANs 3 (10.55.0.5/16), 4, 99, and `slot1..6` (wifi-tsl tester). No DHCP/TFTP listeners today. Port 53 only on 10.55.0.5 and loopback |
| steamboat services | Caddy in Docker (`network_mode: host`, owns :80/:443), app stacks under `ops/servers/steamboat/stacks/` deployed by CI from pinned image digests (see ops `CLAUDE.md`, `add-app` skill) |
| TSL DHCP/DNS | UniFi gateway (UXG-Pro), managed by `ops/unifi` IaC. DHCP options for netboot are not modelled there today |
| desky | Raspberry Pi 4 Model B **Rev 1.1**, 4 GB, Debian 13 trixie (Pi OS), kernel 6.18.50+rpt-rpi-v8. Root today: `/dev/mmcblk0p2` (ext4). `eth0` 10.255.7.45/20 (DHCP), MAC `dc:a6:32:03:fc:36`. Hostname `deskypi`, ssh `pi@desky` |
| desky bootloader | EEPROM 2026-09-23 (release). Config: `BOOT_UART=0 WAKE_ON_GPIO=1 POWER_OFF_ON_HALT=0`, no `BOOT_ORDER` (so the default `0xf41`: SD, then USB, repeat) |
| desky SD slot | Device tree has **`broken-cd`** on `emmc2` → no card-detect line; the kernel polls the slot (~1 s). Hot-plug works, just polled |
| BenchBob | Rust backend (axum 0.8, tokio) + Vite web UI in this repo. Already builds Pi EEPROM images (`backend/crates/drives/src/rpiboot.rs`, `plans/bootloader-settings-ui.md`) |

## Research findings

### Raspberry Pi 4/5 network boot (from the official docs source, `raspberrypi/documentation` master)

- Netboot lives in the **EEPROM bootloader**, boot mode `2` in `BOOT_ORDER`. It sends a
  DHCP request with vendor class `PXEClient:Arch:00000:UNDI:002001` and expects option 43
  containing `Raspberry Pi Boot` (dnsmasq: `pxe-service=0,"Raspberry Pi Boot"`), either
  from the main DHCP server or a **proxyDHCP** reply.
- **`TFTP_IP`** in the EEPROM overrides the TFTP server address from DHCP, so a Pi can
  netboot with *no* DHCP changes and *no* proxyDHCP at all. The docs pitch it for exactly
  this case (home networks where the broadband router is the DHCP server). **This is now
  the primary Pi path** (see "DHCP policy"). Verify on desky in Phase 2 that the
  bootloader accepts an ordinary UniFi lease with no option 43 when `TFTP_IP` is set.
- Files are requested under a per-device prefix: `TFTP_PREFIX` = serial (default), MAC,
  or fixed string. The prefix is dropped if `start4.elf` isn't found under it.
- Then it is the normal Pi boot: `start4.elf`, `fixup4.dat`, `config.txt`,
  `cmdline.txt`, kernel, DTBs, overlays, optional initramfs, all over TFTP.
- **HTTP boot** (boot mode `7`, `HTTP_HOST`/`HTTP_PATH`/`HTTP_PORT`) downloads a single
  `boot.img` ramdisk + `boot.sig`. **All HTTP downloads must be signed**: with a custom
  `HTTP_HOST` we must sign `boot.img` with our own key and embed our public key in the
  EEPROM. A public EEPROM update wipes that key (BenchBob's EEPROM builder could re-add
  it). Plain HTTP only, unless on BCM2712 (Pi 5) with a custom CA cert. Wired only.
  → TFTP is the right first path. HTTP boot is a later speed-up.
- `recovery.bin` can't be loaded over TFTP on 2711/2712 (irrelevant here, but rules out
  "EEPROM update over the network").

### UEFI / PC side

- PCs PXE via the firmware, which we chainload into **iPXE** (`undionly.kpxe` for BIOS,
  `snponly.efi` / `ipxe.efi` for x86-64 and arm64 UEFI). iPXE then fetches a script over
  HTTP from our server, which can decide per machine (MAC, UUID, serial, arch).
- **Secure Boot:** iPXE's own shim was **Microsoft-signed in Nov 2025** (`ipxe/shim`
  release `ipxe-16.1`); general availability of public signed binaries was planned for
  early 2026, not verified yet. A July 2026 report of 11 MS-signed bootloaders allowing a
  Secure Boot bypass may lead to revocations; check whether the iPXE shim is affected
  before relying on it. Until then: Secure Boot off on lab machines, or distro
  shim → GRUB chains for the few distros that publish signed netboot trees.
- A Pi 4 *can* run UEFI firmware (pftf/RPi4 EDK2) and then use generic arm64 iPXE. This
  is how Bootimus claims "Pi 4/5". It costs the Pi's native firmware features (GPIO
  overlays via `config.txt`, the Pi kernel, 3 GB RAM limit by default in ACPI mode), so
  it is the wrong default for BenchBob. Worth supporting later as a per-device option.

### Existing projects

| Project | Stack / licence | What fits | What's missing for us |
| --- | --- | --- | --- |
| **Bootimus** (bootimus.com) | Go single binary, SQLite/Postgres, Apache-2.0 (+ GPL iPXE), v1.0 | Closest match: embedded iPXE, built-in **proxyDHCP** (no router change), web UI + REST, MAC-based image assignment, auto-detects 50+ distro ISOs, wimboot for Windows, autoinstall templates (preseed/kickstart/cloud-init/autounattend), Docker in 30 s | Pi only via UEFI firmware, not native. No post-boot agent or remote control. No network-root full systems (docs don't mention NFS/NBD/iSCSI). No imaging beyond launching Clonezilla. No theming. Auth is JWT/LDAP-flavoured, heavier than we want |
| **netboot.xyz** | iPXE menus + Docker web app | Huge curated menu of installers/live/rescue pulled from upstream; local asset mirroring | Not per-device; no assignment, no control, no imaging, no Pi-native. Great as *one menu entry* (chainload it) |
| **FOG Project** | PHP/MySQL, GPL, v1.5.x (2.0 long in progress) | Mature imaging (partclone, multicast), inventory, Windows deployment, a buildroot client ("FOS") | Heavy LAMP install that wants to own DHCP/TFTP; dated UI; no Pi-native; no network-root systems. The imaging design is the bit worth copying |
| **iPXE Station** (loglux) | FastAPI + React + dnsmasq, MIT | Menu builder, asset manager, ISO extraction, WinPE via wimboot | 6 stars, early stage; no assignment model, no ARM/Pi, UEFI HTTP boot "unverified" |
| Also noted | | **nobodd** (Canonical): TFTP server that serves the Pi boot partition straight out of a `.img`, plus NBD root — exactly the Pi-native trick we want. **Matchbox** (CoreOS): per-MAC/label profile matching, i.e. the "device or class of device" model. **iVentoy**: popular but closed source | |

**What nobody has:** native Pi netboot with per-device network roots *and* a post-boot
controller you can drive from the web *and* one theme across web UI and boot menu. Those
are three of the user's headline features, so they decide the question.

## Decision: build our own (Rust), on standard components

Decided by the user 2026-10-23, with no Bootimus test. Reasons:

1. The default-image **controller with web remote control** is the centre of the design
   and no existing project has anything like it.
2. Native Pi netboot with a **dynamic TFTP server** (files generated per device, per boot)
   is what makes "assign an image to this Pi" and "boot into X once" trivial. Existing
   servers serve static trees.
3. Bootimus's "no router change" works by running proxyDHCP by default, and FOG wants to
   own DHCP. Here proxyDHCP is at most an opt-in, never the default or the main way to
   configure things (see "DHCP policy").
4. It matches BenchBob: same language, same axum/tokio stack, same UI conventions and
   theme, and BenchBob's EEPROM builder already exists for the Pi side.

Don't reinvent the commodity parts: ship stock **iPXE** (built with an embedded script
that phones home), **wimboot**, distro kernels/installers, `partclone`, `nbd`/NFS where
sensible. Borrow ideas from Bootimus (ISO auto-detection, autoinstall templates) and FOG
(imaging flow).

## DHCP policy: the network's own DHCP server is the normal path

The user guards DHCP in the switch config. The default and recommended way to configure
netboot is the **network's real DHCP server** (UniFi here) handing out the boot options.
BootBob **never answers DHCP unless the admin explicitly turns that on.**

| Client | How it finds the server | Router change? |
| --- | --- | --- |
| **PCs, PXE from firmware** (the main path) | UniFi's per-network **Network Boot** setting (next-server `10.255.0.5` plus one boot filename) on the main LAN. The filename is an iPXE build with an embedded script, so iPXE's own DHCP pass doesn't loop back to the firmware filename | **Yes, one setting.** Approved in principle (round 2); the exact change is staged in the ops repo for a per-change yes in Phase 1. `ops/unifi` only models reservations and DNS today, so it needs a small code addition |
| **Clients that can store their own boot server** (Pi EEPROM `TFTP_IP`, a UEFI HTTP-boot URI) | A setting on the client itself, so DHCP is irrelevant. BootBob helps manage these, see "Client-side boot settings" | None |
| **Anything, last resort** | A USB stick with our iPXE build and the server's URL embedded. BenchBob can burn it, see "BenchBob integration" | None |
| **Opt-in: proxyDHCP** | BootBob answers PXE requests alongside the real DHCP server (UDP 67/4011, PXE options only, never addresses). **Off by default**, not offered in setup, enabled only in an advanced setting with a warning that a DHCP guard will (rightly) drop it. For networks where nobody can change the router | None |

**Why one boot filename is a snag (not a feature, just a constraint).** UniFi's Network Boot
setting has a single filename field for every machine. But BIOS PCs need `undionly.kpxe`,
UEFI x86-64 PCs need `snponly.efi`, and arm64 needs a third build, and a machine given the
wrong one simply fails to boot. Proper DHCP servers pick the file by the client's arch;
UniFi's UI can't. The fix is on our side, invisible to the user: the server **listens**
for DHCP broadcasts on `eno1` (approved round 2). A PXE `DISCOVER` names the client's arch
(option 93). When the TFTP request for the one filename arrives, the server maps client
IP → MAC (kernel neighbour table) → arch and serves the right binary under that name. The
same listener lets the UI show every machine that *tries* to netboot, even ones that never
get as far as iPXE (wanted, round 2). Check in Phase 1 that the switch's DHCP guard still
floods client broadcasts to steamboat's port (`tcpdump -ni eno1 port 67`, receive-only).

## Network check ("doctor")

BootBob tests that the network is set up right and suggests fixes (requested round 2).
Sniffing DHCP and pretending to be a DHCP client are approved (round 2).

- **Active probe as a client.** For each firmware arch, send a `DHCPDISCOVER` dressed as
  that firmware (vendor class `PXEClient:Arch:000NN:…`, option 93) from a locally
  administered MAC, read the `OFFER`, and **never send `REQUEST`**, so no lease is ever
  bound. Also try `DHCPINFORM` from steamboat's own address, which asks for options without
  allocating anything. Compare next-server / option 66 / filename / option 67 with what
  BootBob expects.
- **Reachability.** TFTP-read and HTTP-fetch the boot files at the address the offer
  names. The best vantage point is another machine (BenchBob, or a booted controller),
  since the server testing itself proves little.
- **Passive evidence.** A PXE `DISCOVER` followed by no TFTP request from that MAC means
  "this machine tried to netboot but its offer didn't send it to us". Shown per device.
- **Conflict check.** Any `OFFER` carrying PXE options that didn't come from UniFi (or from
  BootBob's own opt-in proxyDHCP) is flagged as a rogue boot server. It also confirms the
  switch guard is doing its job.
- **Fixes.** Each failed check comes with the exact fix: the UniFi values to enter, the
  ops-repo snippet for `ops/unifi`, or "this Pi's EEPROM lacks `TFTP_IP`; burn the setter
  image". Fixes are shown, never applied to the network by BootBob.

## Client-side boot settings

Clients that can store their own boot server bypass DHCP entirely. BootBob should help
manage them (round 2: "not sure how yet, maybe bootloader-setting setter images"). Ideas,
to firm up in Phase 2:

- **Pi EEPROM setter, SD card.** A tiny SD image of `recovery.bin` + `pieeprom.upd` +
  `pieeprom.sig` carrying `BOOT_ORDER`, `TFTP_IP`, `TFTP_PREFIX` for *this* server. Insert,
  power on, the green LED flashes, done. BenchBob already builds EEPROM images
  (`backend/crates/drives/src/rpiboot.rs`), so it can burn this directly. A Compute Module
  gets the same config over `rpiboot`.
- **Pi EEPROM setter, netboot.** A Pi already netbooting (or the Pi controller) runs a
  one-shot job that applies a new EEPROM config. `recovery.bin` can't load over TFTP, so
  this relies on `rpi-eeprom-update` writing the SPI flash from Linux via `flashrom`.
  **To verify** on Pi 4 (Pi 5 does this by default).
- **PC UEFI setter.** From the controller, add a UEFI boot entry with an HTTP-boot URI
  device path pointing at BootBob (`efibootmgr` or our own EFI-variable writer), so the
  machine netboots even on a network with no boot options. Firmware support varies;
  **to verify** on real hardware.
- The server keeps a record of which clients carry which settings, and the doctor flags
  stale ones (e.g. after a server IP change).

## BenchBob integration

Requested round 2: it should be easy to burn boot media on a LAN's BenchBob for that LAN's
BootBob server.

- BootBob advertises itself over **mDNS** on `eno1` only: `_bootbob._tcp` with TXT records
  for version, the HTTP URL, server IP, and capabilities (`ipxe`, `pi-tftp`, `bios`,
  `proxydhcp`). BootBob can be its own responder (e.g. the `mdns-sd` crate), so steamboat
  needs no avahi. Multicast DNS isn't DHCP, but it is a new listener, so it's an open
  question.
- BenchBob browses for `_bootbob._tcp`. When it finds a server, its flash picker gains a
  **"Network boot: BootBob @ steamboat"** group:
  - USB iPXE stick for this server (PCs that can't PXE, or networks without boot options)
  - Pi EEPROM setter SD for this server
  - "Set this Pi's own EEPROM to netboot from this server" (for desky itself)
- BootBob builds these on request (iPXE with the server's URL embedded, EEPROM configs), so
  BenchBob only downloads and writes them. BenchBob can also run the doctor's checks from
  its own vantage point.
- The image catalogue (installers, rescue) that BootBob serves is also offered to BenchBob
  as burnable images, so the two share one catalogue.

## Architecture

### Server (one Rust binary on steamboat, `network_mode: host`)

| Piece | Notes |
| --- | --- |
| **DHCP listener** | Raw socket on `eno1`. Records PXE `DISCOVER`s (MAC, option 93 arch, vendor class). Feeds arch selection, "tried to netboot" discovery and the doctor. Parsing: `dhcproto` |
| **Doctor** | Client-style DHCP probes (`DISCOVER`/`INFORM`, never `REQUEST`), reachability checks, rogue-offer detection, suggested fixes. See "Network check" |
| **proxyDHCP** (opt-in, off by default) | UDP 67/4011, PXE options only. Advanced setting with a warning |
| **mDNS** | Advertises `_bootbob._tcp` on `eno1` for BenchBob |
| **Dynamic TFTP** (UDP 69) | Every read goes through our resolver: `(client MAC/serial, path) → bytes`. Serves the right iPXE binary to each PC under the one UniFi filename, and for Pis builds the boot tree on the fly from the assigned image (from a directory, or straight out of a `.img`'s FAT partition à la nobodd), with generated `cmdline.txt`/`config.txt`. Must handle blksize/tsize/windowsize options for speed |
| **HTTP** (one port, e.g. 8067) | iPXE scripts (`/boot/ipxe?mac=${mac}&uuid=${uuid}&arch=${buildarch}…`), kernels/initrds/squashfs, ISO-extracted trees, the REST API, WebSockets for agents and the UI, the web UI itself (embedded). Caddy fronts it as `pxe.tsl` for humans only; machines hit the port directly |
| **Stores** | One or more image stores, local or remote, with on-demand download and caching. Per-device writable roots. See "Storage" and "Network roots" |
| **NBD server** (built in) | Our own, serving copy-on-write per-device roots over a shared base image |
| **NFS** (managed) | BootBob writes its own exports file and reloads the store host's kernel NFS server |
| **State** | SQLite: devices (MAC, Pi serial, SMBIOS UUID, arch, last seen, inventory), classes/rules, images, assignments, one-shot "next boot", jobs (backup/restore/install), audit log |
| **Optional SMB** | For Windows installs (WinPE maps the extracted ISO). Samba in a sidecar container is fine |

### Assignment model

- **Device** = identified by MAC (+ Pi serial, + SMBIOS UUID when known). Auto-created on
  first PXE contact ("discovered").
- **Class** = a rule matching devices: arch, Pi model, MAC OUI, SMBIOS vendor/model,
  manual tag. Matchbox-style.
- **Resolution order:** one-shot next-boot → device assignment → class assignment →
  site default (the controller image). Every boot is logged with the reason it got what
  it got, shown in the UI.

### The iPXE fallback menu, and catching hosts that land on it

Every PC that reaches iPXE fetches `/boot/ipxe?mac=${mac}&uuid=${uuid}&serial=${serial}`
`&manufacturer=${manufacturer}&product=${product}&platform=${platform}&arch=${buildarch}`.
The server records the device on that request, creating it if new. So **every host that
gets the fallback shows up in the web UI immediately**, with the reason it got it ("no
assignment; site default = fallback menu").

Menu entries:

- **Boot the controller** (the mini Linux, once it exists).
- **Boot from local disk** (`exit`, or `sanboot --drive 0x80` on BIOS).
- **Installers / rescue / tools** submenus generated from the image catalogue, plus
  "chain to netboot.xyz".
- **iPXE shell**.
- **Reboot** and **Power off**: iPXE's `reboot` and `poweroff` commands. Our iPXE build
  enables `REBOOT_CMD` and `POWEROFF_CMD` in `config/general.h`.
- A header line with the device's short name or code (e.g. the last 3 MAC bytes) and its
  IP, so it's easy to find in the web UI.

**Re-pointing a waiting host from the web UI.** iPXE can't hold a connection open, so the
menu polls. It runs `choose --timeout <~10 s> --default refresh`, and the hidden `refresh`
entry re-fetches `/boot/ipxe`. The UI shows the host as **"waiting at fallback menu, last
seen 3 s ago"**. When the admin assigns an image (persistent, or one-shot "next boot"),
the next refresh returns a script that boots it. The host goes there within one poll
interval, **with nobody touching it and no reboot**. If the host is no longer waiting
(powered off, or someone picked an entry), the assignment applies on its next netboot.
**Verified** in iPXE's source (`hci/tui/menu_ui.c`): a keypress resets the menu timeout to
`--retimeout`, which defaults to 0 (never), so someone at the keyboard stops the polling.
The UI shows that as "at menu, idle". A failing `choose` (Esc, or an old iPXE that rejects
an option) pauses 3 s before refreshing, so a broken menu can't spin against the server.

Pis have no firmware menu, so an unassigned Pi gets the controller. Before Phase 3 it gets
a tiny "unassigned" image that shows its serial and MAC on HDMI and keeps re-checking. Its
TFTP requests are logged by serial/MAC in the same way, so Pis appear in the UI too.

### The default image: "the controller"

The user's questions, answered:

- **Bare metal vs Linux?** Three tiers are possible:
  1. *iPXE menu only:* bare metal, ~1 MB, boots instantly. Text menu with colours and a
     background PNG (`console --picture`), which the server can render from the theme.
     Networking: HTTP(S), iSCSI, AoE, NFS. No live link back to the server, no mouse,
     limited fonts. Fine as the *fallback* menu, not the "elegant" one.
  2. *Bare-metal Rust UEFI app (uefi-rs):* GOP framebuffer graphics, keyboard and
     sometimes mouse, but firmware network stacks are inconsistent (many lack usable
     TCP/HTTP protocols), no BIOS, no Pi-native. Lots of work for a fragile result. **No.**
  3. *Mini Linux:* our own kernel + a purpose-built initramfs. Full networking, every
     disk, a framebuffer UI, USB input, serial console. **Recommended.** Realistically
     **~8–14 MB total** (see "How small can the controller get?"). The first estimate of
     30–80 MB was for a generic distro kernel plus every firmware blob.
- **How fast?** iPXE pulls 20 MB over HTTP on gigabit in well under a second; kernel to
  UI ~3–5 s. Pi over TFTP is slower (est. 5–10 s for the transfer); Pi HTTP boot later.
- **Then boot something else?** Yes, two ways:
  - **Linux targets: `kexec`** straight into the chosen kernel (no firmware re-POST).
    Blocked by kernel lockdown under Secure Boot.
  - **Everything else** (Windows installer/WinPE, memtest, other EFI apps, local disk):
    the agent sets a **one-shot next-boot** on the server and reboots; iPXE (or the Pi's
    TFTP tree) gets the target on the next pass. Uniform, reliable, works for every
    firmware. Local disk: `efibootmgr --bootnext`, or iPXE `exit`/`sanboot`.
- **UI stack:** a Rust agent, statically linked (musl), as the main process of the
  initramfs:
  - Graphics: **Slint** with its LinuxKMS backend + software renderer (no GPU or
    compositor needed, a few MB, themeable via properties). Same theme tokens as the
    web UI.
  - Text fallback: **ratatui** on the serial console / plain VT for headless boxes.
  - Toolbox in the initramfs: busybox, `partclone`, `zstd`, `ntfs-3g`, `sgdisk`,
    `efibootmgr`, `kexec-tools`, `nvme-cli`, `smartctl`, `nwipe`.
- **Remote control (decided: API calls from the master server):** the agent *dials out*
  a WebSocket to the server and takes commands over it. By default it listens on **no**
  ports, so there's nothing to secure on the booted machine. The web UI on the master
  shows each booted machine:
  - inventory (CPU, RAM, disks, SMART, NICs)
  - its screen (the agent streams its own framebuffer, which is easy because it renders it)
  - a web terminal (a pty over the socket)
  - actions: boot X, back up disk Y, restore, wipe, reboot, power off
- **Optional local mini web UI:** a cargo feature (`local-ui`) plus an on/off switch per
  image or class, off by default. When on, the agent also serves a small embedded page
  and the same API on a local port, for when the master is down or the machine is on
  another network. It adds a few hundred KB.
- **Builds:** our own kernel configs, built in CI, for x86-64, generic arm64 and Pi. The
  Pi build uses the Raspberry Pi kernel tree and DTBs, so the Pi controller keeps working
  Pi peripherals. One agent codebase, three kernel flavours.

### How small can the controller get?

These are estimates, to be measured in Phase 3. Sizes are compressed, as transferred.

| Build | Kernel | initramfs | Total | Trade-off |
| --- | --- | --- | --- | --- |
| **Extreme**: `tinyconfig` + one NIC driver + framebuffer, agent as PID 1, no busybox | ~1–1.5 MB | ~1–2 MB | **~3 MB** | Only boots hardware it was configured for. Fine for VMs or a known fleet, useless as a "boot anything" default |
| **Recommended**: common wired NICs, NVMe/AHCI/USB storage, USB HID, `simpledrm`/`efifb`, ext4/vfat/exfat/ntfs3/btrfs/xfs and kexec built in; the agent + busybox as userland | ~5–8 MB | ~3–6 MB | **~8–14 MB** | Boots nearly any lab PC. The agent fetches the heavy tools only when a job needs them |
| **Everything**: modular distro-style kernel, all NIC/Wi-Fi/GPU firmware | ~10–12 MB | 30–70 MB | 40–80 MB | Covers Wi-Fi and exotic NICs. Not needed, since netbooting means wired |

What makes the difference:

- **No GPU drivers or firmware.** Draw into the framebuffer the firmware already set up
  (`simpledrm`/`efifb` on PCs, the Pi firmware's framebuffer). GPU firmware (i915,
  amdgpu) is the biggest single blob, and a menu doesn't need it.
- **Built-in drivers instead of modules**, and wired NICs only. A few NICs need firmware
  (some Realtek and Broadcom chips); those blobs are tens to hundreds of KB each.
- **Tools on demand.** Keep `partclone`, `ntfs-3g`, `nwipe`, `smartctl` and similar out of
  the base image. The agent downloads a tools squashfs over HTTP and mounts it when a
  backup, restore or wipe job starts. The base needs only the agent and busybox.
- **The agent itself:** Rust, statically linked against musl, built with
  `opt-level="z"`, LTO and `panic="abort"`, then stripped. Using plain WebSocket over
  HTTP on the LAN, with no TLS stack, keeps it around 1–2 MB.
  - The UI library is the main variable. Slint's software renderer adds a few MB. A
    hand-rolled renderer (`tiny-skia` or `embedded-graphics` plus a font rasteriser) is
    well under 1 MB.
  - Start with Slint for the shared theme, measure, and swap it out only if it turns out
    to be the problem.
- **Compression:** zstd for the kernel and initramfs. xz squeezes about 10% more but
  decompresses slower.

Where size actually matters:
- **Pi over TFTP.** TFTP is the slow link, so 10 MB vs 40 MB is seconds of boot time.
- **RAM on small machines**, because the initramfs is unpacked into RAM.

Over iPXE HTTP on gigabit, 14 MB vs 3 MB is about 0.1 s. Boot time there is dominated by
firmware POST and kernel driver probing, not image size.

### Theme ("skin")

One theme file (colour tokens, fonts, logo, background) is the source of truth, consumed
by: web UI (CSS variables), the controller (Slint properties), the iPXE fallback menu
(server-rendered background PNG + colour pairs), and generated GRUB themes for installers
that use GRUB. The Pi's own bootloader screen can't be skinned (it can be hidden with
`disable_splash`).

### Image types

| Kind | How |
| --- | --- |
| **Full system, network root** | Kernel via TFTP (Pi) or iPXE HTTP (PC); root on **NBD** (default) or **NFS**. Per-device writable clone of a golden image. See below |
| **Linux installers** | Upstream netboot kernels/initrds or ISO-extracted trees (netboot.xyz/Bootimus style), with generated preseed/autoinstall/kickstart/cloud-init per device. Pi: write a Pi OS image to local media from the controller (no installer concept on Pi) |
| **Windows (10/11, Server)** | iPXE → **wimboot** → WinPE `boot.wim` from the user's ISO → maps an SMB share of the extracted ISO → `setup.exe` with a generated `autounattend.xml`. Driver injection folder per class |
| **macOS** | Mostly not possible, see findings below. Out of scope except Intel-Mac BSDP → iPXE for rescue tools |
| **Rescue / tools** | The controller itself, plus memtest86+ (EFI and BIOS), SystemRescue, GParted Live, ShredOS, Clonezilla, and a "chain to netboot.xyz" entry for the long tail |
| **Backup** | From the controller: per-partition `partclone` (filesystem-aware, skips free space) → zstd → streamed over HTTP to steamboat, with partition table, EFI vars and inventory stored alongside. Scheduled or on demand from the web UI |
| **Restore** | Reverse of backup, from the controller or as a one-shot boot job ("restore image X to disk Y, then boot local"). Multicast later if ever needed |

### Network roots (full systems): both NBD and NFS, NBD preferred

Decided round 2: support both, prefer NBD, and BootBob manages the server side of each.

- **NBD (default):** the device's initramfs runs `nbd-client`
  (`nbdroot=… root=/dev/nbd0p2`). Behaves like a local disk: apt, Docker/overlayfs, xattrs
  and locking all just work, and small-file I/O is faster than NFS.
  - **Our own NBD server inside BootBob** (moved up from "later"; the protocol is small).
    Each device's root is a read-only **base image plus a sparse per-device overlay**
    file. Copy-on-write is then done by BootBob, not the filesystem. That's what lets any
    store, on any filesystem, hold per-device roots. It also gives live I/O stats per
    device, instant "reset to golden", and snapshot-before-upgrade (freeze the overlay,
    start a new one).
  - Offline edits: BootBob exposes a device's root to the server itself (loopback NBD)
    for mounting.
- **NFS:** no initramfs needed with Pi kernels and the server sees files live, but
  overlayfs/Docker don't work on an NFS root, and apt/dpkg is slower. BootBob manages it
  by unpacking the image into a directory on the store and writing **its own**
  `/etc/exports.d/bootbob.exports`, then running `exportfs -ra`. It never edits other
  exports. The store host needs `nfs-kernel-server` installed once, via an approved ops
  change.

### Storage: multiple stores, fetched on demand, cached

Requested round 2: support other hosts as NFS/NBD servers, multiple stores (small local,
big remote), and downloading images on the fly with a cache.

- **A store** is anywhere BootBob keeps images, roots and backups:
  - **local:** a directory on the BootBob host
  - **remote BootBob store:** the same binary in `store` mode on another host (NBD + NFS
    exports + HTTP blob API), registered with the main server
  - **read-only HTTP/NFS:** an existing server, for catalogue files only
- **Placement** is per image or per class, with sane defaults: small, hot things locally
  (iPXE, controller, Pi golden images, active device roots), big or cold things remotely
  (backups, Windows ISOs, big golden images).
- **Clients talk to the store directly.** The boot script or `cmdline.txt` names the
  store's host and port, so NBD traffic from a device goes straight to its store and never
  hairpins through steamboat.
- **On-demand download and cache.** A catalogue entry can be just an upstream URL plus a
  checksum (distro ISOs, netboot kernels, cloud images, Pi OS from BenchBob's existing
  catalogue). The first boot that needs it streams it: BootBob downloads and serves at the
  same time, verifies the checksum, and keeps it. A size-capped LRU cache with pinning
  manages each store. Remote-store content can be cached locally the same way (tiering).
- **btrfs is now optional.** Since the NBD server does copy-on-write itself, btrfs/XFS
  reflinks are a nice speed-up for NFS-style directory clones, not a requirement.

**Is there enough space?** Rough sizes: iPXE and the controller are tiny; a Pi OS golden
image is 4–8 GB; a per-device overlay is 1–5 GB; an installer ISO is 1–7 GB (Windows 11
~6 GB); a backup is tens to hundreds of GB each.

- **Local store (decided round 3):** a directory on steamboat's NVMe with a ~150 GB cache
  cap (of ~360 GB free). The spare 112 GB SATA SSD (`sda`) is **not** available: its
  contents are not disposable.
- **Remote store: uberfall** (4×11 TB at TSL) for backups and big images, approved in
  principle (round 3), once its link speed to the lab LAN is checked. The exact change (a
  `bootbob-store` stack) still needs a per-change yes.

### macOS findings (honest)

- **Apple Silicon Macs cannot netboot at all.** The closest thing is a DFU restore from
  another Mac (Apple Configurator), or Apple's own Internet Recovery.
- **Intel Macs with a T2 chip don't support NetBoot.**
- **Older Intel Macs** speak Apple's BSDP (hold N). Open-source BSDP servers exist and
  can hand them an EFI binary (iPXE → our controller works for rescue/backup). Apple's
  NetInstall images are dead in modern macOS, so *installing* current macOS over the
  network isn't realistic. Recommend: macOS = rescue/backup only on old Intel Macs,
  deferred.

## DeskyPi: going diskless

Decided round 2: desky goes diskless eventually, as dogfooding, once the server is solid.
The SD stays as a rescue card until then. Steps once the server serves Pi boots (Phase 2):

1. Make a golden Pi OS trixie image from desky's current SD (BenchBob can already read
   cards). Add `nbd-client` + initramfs support, fix `fstab` (no `mmcblk0p*` mounts),
   point `/boot/firmware` at a server-side copy so `apt` kernel updates land in the tree
   TFTP serves (NBD: the boot partition lives inside the image and the TFTP server reads
   it from there, nobodd-style, which solves this neatly).
2. **EEPROM:** `BOOT_ORDER=0xf2` (network only, retry forever). **Never leave SD in the
   boot order**: the slot will hold arbitrary cards being flashed, and SD-first would boot
   whatever is in it. `TFTP_PREFIX=1` (MAC) and `TFTP_IP=10.255.0.5`. `TFTP_IP` is
   required, not a fallback: it's how desky finds the server, since only UniFi answers
   DHCP. BenchBob's EEPROM editor can build this; apply it
   while the SD is still present, test, keep the SD as a rescue card.
3. **BenchBob code change:** `backend/crates/drives/src/devices.rs:14-16` treats every
   `mmcblk*` as a system device and never offers it. Change to: exclude only the device
   holding `/` (already detected via `findmnt /`) and eMMC (`/sys/block/mmcblk*/device/type`
   = `MMC`), so an SD card in desky's slot appears as a normal drive (browse, identify,
   flash, erase). Test hot-plug with `broken-cd` polling (insert/remove, uevents,
   lsblk refresh).
4. Header identity already reports NBD/NFS root (`findmnt /`), see
   `plans/header-identity.md`; verify it labels it "netboot".
5. Risks: desky's reboot depends on steamboat being up (it's in the rack, on the same
   switch fabric). Keep the SD rescue card. If steamboat's IP ever changes, desky's
   EEPROM has to change with it.

## Phased plan

- **Phase 0, decide:** done (rounds 1–3).
- **Phase 1, server skeleton** *(current step)*. In the BootBob repo:
  - [x] Rust workspace (axum, tokio). Storage is **redb**, not SQLite: pure Rust, so the
        binary stays static with no C toolchain, like BenchBob's.
  - [x] Dynamic TFTP (blksize, tsize, timeout, windowsize, rollover, loss recovery).
  - [x] HTTP: iPXE scripts, JSON API, SSE change feed, theme.css, web UI.
  - [x] DHCP listener (receive-only AF_PACKET + kernel BPF filter): arch selection under
        the one boot file name, "tried to netboot" detection, offers recorded.
  - [x] Device discovery, assignments (one-shot and persistent), event log.
  - [x] Fallback menu with reboot, power off, local disk, polling, choice reporting,
        failed-boot return; assign-from-UI for hosts waiting at it.
  - [x] Web UI (machines, images, settings with themed menu preview, log).
  - [x] iPXE build (pinned commit, embedded script), BIOS + UEFI x64 + arm64.
  - [x] QEMU end-to-end test in CI on a private bridge (SeaBIOS + OVMF): network check
        passes, both reach the menu with the right firmware, both power off when assigned
        from the API. **Green** (run 40039958136 and later).
  - [x] Dockerfile, CI (tests, clippy, fmt, web, iPXE, e2e, image smoke test).
  - [x] Doctor: client-style DHCP probes (both firmwares at once, 8 s window), offers
        matched by xid, iPXE/TFTP/listener checks, rogue boot servers, stuck machines,
        exact fixes. Network check tab in the UI.
  - [x] Menu gives up after 2 min untouched and boots the local disk (setting
        `menu_timeout_secs`), so PCs that try the network before their disk still start
        once UniFi hands out boot options.
  - [x] CI publishes `ghcr.io/cinderblock/bootbob` on green master pushes, with the
        revision label and the ops `pin.json` in the run summary (wifi-tester's pattern).
  - [ ] **Needs per-change approval:** the ops stack `servers/steamboat/stacks/bootbob/`,
        the Caddy `pxe.tsl` site, the UniFi DNS name, the UniFi Network Boot setting.
- **Phase 2, Pi-native boot, stores, BenchBob integration:**
  - Per-device TFTP trees from images.
  - Our NBD server with copy-on-write overlays; managed NFS exports.
  - Stores (local first, then remote `store` mode), on-demand download and cache.
  - Golden-image tooling.
  - Client-side boot settings: Pi EEPROM setter (SD and netboot).
  - mDNS advertisement; BenchBob's "Network boot" flash group (USB iPXE stick, EEPROM
    setter). BenchBob change: offer the SD slot as a drive (`devices.rs`).
  - Then dogfood: the DeskyPi steps above.
- **Phase 3, the controller:** mini-Linux build (x86-64, arm64, Pi), Rust agent with Slint
  UI + ratatui fallback, WebSocket control, kexec / one-shot next-boot, web screen +
  terminal.
- **Phase 4, assignment & classes:** class rules, per-device/class assignment UI,
  one-shot boots, audit trail.
- **Phase 5, installers:** ISO import + auto-detect, Linux netboot installers with
  templated answers, Windows via wimboot + SMB + autounattend, netboot.xyz chain.
- **Phase 6, backup & restore:** partclone streaming, image catalogue, scheduled backups,
  restore jobs.
- **Phase 7, polish:** optional password, passkeys, Secure Boot chain (iPXE shim),
  Pi HTTP boot with our signing key, UEFI-on-Pi option, multicast restore, macOS-BSDP,
  opt-in proxyDHCP, PC UEFI HTTP-boot setter.

## Decisions already made (don't re-ask)

- The user is happy to roll our own when it's what gets these features; "best way, not
  fastest way".
- Web UI is LAN-only; no auth beyond an optional simple password for now.
- Any steamboat / Caddy / UniFi / DNS change goes through the ops repo and needs explicit
  per-change approval. Approved ops changes go straight to master (no PRs).
- **Build our own.** No Bootimus or other trial run first. (2026-10-23)
- **DHCP:** the network's real DHCP server (UniFi) is the default and main way to configure
  netboot. BootBob never answers DHCP by default. **proxyDHCP is acceptable as an opt-in**,
  off by default, never the main configuration path. (Round 1 said "never"; corrected
  round 2.) The UniFi Network Boot setting is OK in principle; the exact change still
  needs a per-change yes. (2026-10-23)
- Sniffing DHCP and sending client-style DHCP probes are OK. (2026-10-23)
- The app should **test network settings and suggest fixes** (the doctor). (2026-10-23)
- Clients that store their own boot server (Pi EEPROM, UEFI URI) are managed by BootBob,
  e.g. via setter images. (2026-10-23)
- **BenchBob integration:** BootBob advertises `_bootbob._tcp` over mDNS on `eno1`
  (approved round 3); BenchBob offers burnable boot media for the LAN's server. (2026-10-23)
- **Storage:** local store is a capped directory on steamboat's NVMe; the spare `sda` SSD
  is off limits (not disposable). uberfall is OK in principle as the big remote store.
  (2026-10-23)
- **Repo:** `cinderblock/BootBob`, public. (2026-10-23)
- **Network roots:** both NBD and NFS, NBD preferred, server side managed by BootBob.
  (2026-10-23)
- **Storage:** multiple stores (local and remote hosts), on-demand download with caching.
  (2026-10-23)
- **Legacy BIOS:** supported. It costs one ~100 KB `undionly.kpxe` on the server and
  nothing in the controller (the same x86-64 kernel boots from BIOS and UEFI), so no
  build flag is needed. If something later turns out to inflate sizes, it goes behind an
  off-by-default build flag. (2026-10-23)
- Secure Boot off on lab machines until Phase 7. macOS deferred (old-Intel rescue only).
  (2026-10-23)
- Name **BootBob**, its own new repo. (2026-10-23)
- desky goes diskless eventually (dogfooding) with `0xf2` + `TFTP_IP`; SD kept as rescue.
  (2026-10-23)
- The fallback iPXE menu has **reboot** (and power off). Hosts waiting at it show up in the
  web UI and can be re-pointed live. (2026-10-23)
- The controller is driven by **API calls from the master server**, with the agent dialling
  out. A local mini web UI on the booted machine is an optional, off-by-default feature.
  Keep the controller as small as is practical. (2026-10-23)

## Open questions for the user

All design questions from rounds 1–3 are answered (see decisions). None open right now.
Next approvals will be the per-change infrastructure items at the end of Phase 1.

## Progress log

- [x] Surveyed steamboat (OS, NICs, free disk, listeners) and desky (board, EEPROM,
      root, SD card-detect) — read-only.
- [x] Read the Pi netboot/HTTP-boot docs source; researched Bootimus, iPXE Station,
      netboot.xyz, FOG, iPXE Secure Boot status.
- [x] Found the BenchBob `mmcblk*` exclusion that blocks the SD-slot-as-I/O goal.
- [x] Wrote this plan.
- [x] User round 1 (2026-10-23): build our own with no Bootimus test, no DHCP responders,
      a fallback menu with reboot and live re-pointing, and a small API-driven controller.
      Revised the plan: DHCP policy, fallback-menu design, controller size table.
- [x] User round 2 (2026-10-23): proxyDHCP opt-in only, the doctor, client-side boot
      settings, BenchBob integration over mDNS, NBD + NFS, multiple stores with caching,
      BIOS supported, all nine round-1 questions answered. Found steamboat's spare SSD.
- [x] User round 3 (2026-10-23): spare SSD not disposable, uberfall OK in principle, mDNS
      OK, repo public.
- [x] Phase 1 build (2026-10-23, BootBob `c25babd`..`6430fe3`): see the Phase 1 checklist.
      Created and pushed <https://github.com/cinderblock/BootBob>.
- [x] First green e2e, the doctor, menu timeout, GHCR publishing (BootBob up to
      `332dbc2`).
- [ ] Phase 1 remainder: the four infrastructure changes, each staged in the ops repo and
      applied only after the user's yes for that change (proposed 2026-10-23, see the
      reply that day): steamboat stack, `pxe.tsl` DNS, Caddy site, UniFi Network Boot.

## Findings / gotchas

- **dhcproto pulls in hickory-proto and rand**; BootBob has its own small DHCPv4 codec
  instead (reading options and building two probe packets is all it needs).
- **WSL Ubuntu on the dev PC has no make, QEMU or cross compiler**, and installing needs
  a sudo password. iPXE builds and the QEMU test therefore run in GitHub Actions.
- **The e2e test's DHCP server** is dnsmasq on a bridge with no uplink inside the CI VM,
  with `--dhcp-boot=bootbob,,<server>`, i.e. exactly UniFi's next-server + one file name.
  `default_firmware` is set deliberately wrong, so reaching the menu proves the listener's
  arch detection picked the right iPXE build.
- **Packets BootBob's own host sends show as PACKET_OUTGOING** on the listener and are
  ignored. So the e2e test runs dnsmasq in its own network namespace (veth into the
  bridge), making its replies arrive from outside, as UniFi's do.
- **A machine's DHCP offer is only visible to the listener when it's broadcast** (client
  set the broadcast flag). Unicast offers go straight to the client's MAC, and a switch
  never floods them to steamboat. So "what DHCP told this machine" on a device is
  best-effort. The doctor's probes set the broadcast flag, so its checks don't depend on
  this.
- **iPXE's `:type` suffix re-formats raw bytes.** `${mac:uristring}` percent-encodes the
  MAC's six binary bytes, not its text, so BootBob couldn't identify the first e2e VM. Use
  `${netX/mac:hexhyp}` and plain `${uuid}`; `:uristring` only on text settings.
- **dnsmasq pings an address for ~3 s before offering it**; the doctor's first 3 s window
  closed just before the offer arrived. Real gateways may do conflict checks too.
- **UEFI firmware fetches each TFTP file twice**: once with `tsize` only, aborting with
  error 8, then for real. That's a cancelled transfer, not a failure.
- **steamboat (read-only check 2026-10-23):** 8067/tcp and 69/udp free; **5353/udp is
  already taken** (an existing mDNS responder), which Phase 2's mDNS must coexist with.
  `eno1` also carries 10.0.100.5/24.
- **Git Bash heredocs collapse `\\`** before Python sees them; edit Rust strings with the
  Edit tool, not inline Python, when they contain escapes.

## Things not to do

- Don't touch UniFi DHCP options, Caddy, DNS or steamboat directly without a staged,
  approved ops change.
- Don't put SD back in desky's `BOOT_ORDER` once it's diskless.
- Don't make the Pi path depend on UEFI-on-Pi; BenchBob needs the native firmware.
- **Don't send DHCP replies on the LAN** unless the admin has explicitly enabled the
  opt-in proxyDHCP. Never dnsmasq in DHCP mode. Test DHCP-serving flows on an isolated VM
  network. The doctor's client probes send `DISCOVER`/`INFORM` only, **never `REQUEST`**,
  so they never bind a lease.
- Don't make proxyDHCP the default, or part of first-run setup.
- Don't use steamboat's spare SSD (`sda`) at all: its contents are not disposable.
- Don't bind the TFTP/HTTP servers, the DHCP listener, mDNS or proxyDHCP to all of steamboat's interfaces
  (VLANs, `slot*` tester bridges). Use `eno1` / 10.255.0.5 only.
- Don't rely on Pi HTTP boot without our own signing key in the EEPROM: unsigned
  `boot.img` from a custom host is rejected.
