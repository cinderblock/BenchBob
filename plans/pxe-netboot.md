# PXE / netboot server on steamboat, and a diskless DeskyPi

Status: **exploration / design — nothing built or changed yet.** Written 2026-10-23.
Working name for the new server: **BootBob** (placeholder, see open question 1).

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
| steamboat | Intel NUC, Ubuntu 25.04, kernel 6.14, 8 cores, 30 GiB RAM, 468 GB NVMe (ext4 root, ~360 GB free). `cameron@steamboat` |
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
  netboot with *no* DHCP changes and *no* proxyDHCP at all. Good fallback for desky.
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

## Decision recommendation: build our own (Rust), on standard components

Build it. Reasons, in order:

1. The default-image **controller with web remote control** is the centre of the design
   and no existing project has anything like it. Bolting it onto Bootimus means forking a
   Go codebase and fighting its model.
2. Native Pi netboot with a **dynamic TFTP server** (files generated per device, per boot)
   is what makes "assign an image to this Pi" and "boot into X once" trivial. Existing
   servers serve static trees.
3. "Trivial setup" is achievable ourselves: one binary (or one container) with proxyDHCP
   built in, so the router is never touched.
4. It matches BenchBob: same language, same axum/tokio stack, same UI conventions and
   theme, and BenchBob's EEPROM builder already exists for the Pi side.

Don't reinvent the commodity parts: ship stock **iPXE** (built with an embedded script
that phones home), **wimboot**, distro kernels/installers, `partclone`, `nbd`/NFS where
sensible. Steal freely from Bootimus (ISO auto-detection, autoinstall templates) and FOG
(imaging flow).

**Cheap insurance first (Phase 0):** run Bootimus in Docker on a test VM for an hour to
validate the proxyDHCP + iPXE + wimboot flow on our LAN and note UX ideas, before writing
code. It costs little and de-risks the PC half.

## Architecture

### Server (one Rust binary on steamboat, `network_mode: host`)

| Piece | Notes |
| --- | --- |
| **proxyDHCP** (UDP 67 + 4011) | Only answers PXE clients (vendor class `PXEClient…`). Hands out boot filename by arch (option 93): BIOS → `undionly.kpxe`, x64 UEFI → `snponly.efi`, arm64 UEFI → arm64 iPXE, Pi → option 43 `Raspberry Pi Boot`. UniFi keeps handing out addresses. Bound to `eno1` only (not the VLAN/slot interfaces). Crate: `dhcproto` |
| **Dynamic TFTP** (UDP 69) | Every read goes through our resolver: `(client MAC/serial, path) → bytes`. Serves iPXE binaries to PCs, and for Pis builds the boot tree on the fly from the assigned image (from a directory, or straight out of a `.img`'s FAT partition à la nobodd), with generated `cmdline.txt`/`config.txt`. Must handle blksize/tsize/windowsize options for speed |
| **HTTP** (one port, e.g. 8067) | iPXE scripts (`/boot/ipxe?mac=${mac}&uuid=${uuid}&arch=${buildarch}…`), kernels/initrds/squashfs, ISO-extracted trees, the REST API, WebSockets for agents and the UI, the web UI itself (embedded). Caddy fronts it as `pxe.tsl` for humans only; machines hit the port directly |
| **Root storage** | Per-device writable roots cloned from golden images. See "Network roots" |
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
  3. *Mini Linux:* kernel + a purpose-built initramfs, **~30–80 MB** (most of it NIC/Wi-Fi
     firmware and drivers). Full networking (WebSocket to the server, even Wi-Fi), every
     disk, DRM/KMS graphics, touch, USB input, serial console. **Recommended.**
- **How fast?** iPXE pulls 50 MB over HTTP on gigabit in well under a second; kernel to
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
- **Remote control:** the agent holds a WebSocket to the server. The web UI shows each
  booted machine: inventory (CPU, RAM, disks, SMART, NICs), its screen (agent streams
  its own framebuffer, trivial since it renders it), a web terminal (pty over the
  socket), and actions: boot X, back up disk Y, restore, wipe, reboot, power off.
- **Builds:** x86-64 and generic arm64 use a distro kernel (Debian). Pis use the Raspberry
  Pi kernel + DTBs, so the Pi controller has working Pi peripherals. One agent codebase,
  three kernel flavours.

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

### Network roots (full systems)

- **NBD (recommended default):** each device gets an ext4/btrfs disk image file, exported
  over NBD; the kernel mounts it via an initramfs with `nbd-client`
  (`nbdroot=… root=/dev/nbd0p2`). Behaves like a local disk: apt, Docker/overlayfs,
  xattrs and locking all just work, and small-file I/O is faster than NFS. Offline edits
  by loop-mounting on the server. Cheap clones need a reflink/snapshot filesystem: a
  dedicated **btrfs** (or XFS-reflink) volume on steamboat, as a partition/LV or, worst
  case, a loop file. steamboat's root is ext4, so this is a real decision (question 4).
- **NFS root:** no initramfs needed with Pi kernels, server can see files live, but
  overlayfs/Docker don't work on NFS root, and apt/dpkg is slower. Supported, not default.
- Later: our own NBD server inside BootBob (the protocol is small) for copy-on-write
  overlays over a shared golden image, live I/O stats, and snapshot-before-upgrade.

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

Steps once the server serves Pi boots (Phase 2):

1. Make a golden Pi OS trixie image from desky's current SD (BenchBob can already read
   cards). Add `nbd-client` + initramfs support, fix `fstab` (no `mmcblk0p*` mounts),
   point `/boot/firmware` at a server-side copy so `apt` kernel updates land in the tree
   TFTP serves (NBD: the boot partition lives inside the image and the TFTP server reads
   it from there, nobodd-style, which solves this neatly).
2. **EEPROM:** `BOOT_ORDER=0xf2` (network only, retry forever). **Never leave SD in the
   boot order**: the slot will hold arbitrary cards being flashed, and SD-first would boot
   whatever is in it. `TFTP_PREFIX=1` (MAC) and optionally `TFTP_IP=10.255.0.5` so desky
   boots even if proxyDHCP is down. BenchBob's EEPROM editor can build this; apply it
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
   switch fabric). Keep the SD rescue card; keep `TFTP_IP` set.

## Phased plan

- **Phase 0, decide & de-risk** *(current step: awaiting user decisions below)*
  - Answer the open questions. Pick a name and repo.
  - Bootimus bake-off on a test VM/spare PC (no LAN DHCP changes; proxyDHCP only).
  - Confirm nothing else on the TSL LAN answers PXE (tcpdump a test PXE boot).
- **Phase 1, server skeleton:** Rust workspace (axum, tokio, SQLite), proxyDHCP, dynamic
  TFTP, HTTP, device discovery + boot log, minimal web UI with theme. PC path: iPXE
  chainload → server-generated iPXE menu (the fallback menu). Docker image built in the
  app repo's CI; ops stack `servers/steamboat/stacks/bootbob/` pinned by digest
  (**needs per-change approval**, as do the Caddy `pxe.tsl` site and the UniFi DNS name).
- **Phase 2, Pi-native boot + diskless desky:** Pi branch of proxyDHCP, per-device TFTP
  trees from images, NBD export of per-device roots, golden-image tooling, then the
  DeskyPi steps above.
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
  Pi HTTP boot with our signing key, UEFI-on-Pi option, multicast restore, macOS-BSDP.

## Decisions already made (don't re-ask)

- The user is happy to roll our own when it's what gets these features; "best way, not
  fastest way".
- Web UI is LAN-only; no auth beyond an optional simple password for now.
- Any steamboat / Caddy / UniFi / DNS change goes through the ops repo and needs explicit
  per-change approval. Approved ops changes go straight to master (no PRs).

## Open questions for the user

1. **Name and home.** A new repo (recommend, e.g. `BootBob`, matching BenchBob), or a
   crate inside this repo? A new repo seems right: it's a server product with its own
   release cycle, and BenchBob is a client of it.
2. **Build vs adopt:** agree with "build our own, after a one-hour Bootimus bake-off"?
3. **Network root default:** NBD (recommended) or NFS?
4. **Storage on steamboat:** OK to carve a btrfs volume for images (reflink clones,
   snapshots)? Options: shrink nothing and use a loop-file btrfs (simplest, slightly
   slower), or add a second drive if the NUC has a free slot (best). How much space for
   images and backups?
5. **proxyDHCP vs UniFi options:** recommend proxyDHCP (no router change, "trivial
   setup"). Fine to bind it to `eno1` (10.255.0.0/20) only, leaving VLANs alone?
6. **Secure Boot:** OK to require it off on machines in phase 1–6, and add the iPXE
   shim later?
7. **macOS:** OK to treat as deferred / old-Intel-rescue-only given the findings?
8. **desky EEPROM:** OK with network-only boot (`0xf2`) plus `TFTP_IP` fallback, keeping
   the current SD as a rescue card?

## Progress log

- [x] Surveyed steamboat (OS, NICs, free disk, listeners) and desky (board, EEPROM,
      root, SD card-detect) — read-only.
- [x] Read the Pi netboot/HTTP-boot docs source; researched Bootimus, iPXE Station,
      netboot.xyz, FOG, iPXE Secure Boot status.
- [x] Found the BenchBob `mmcblk*` exclusion that blocks the SD-slot-as-I/O goal.
- [x] Wrote this plan.
- [ ] User answers to the open questions.
- [ ] Phase 0 onward.

## Things not to do

- Don't touch UniFi DHCP options, Caddy, DNS or steamboat directly without a staged,
  approved ops change.
- Don't put SD back in desky's `BOOT_ORDER` once it's diskless.
- Don't make the Pi path depend on UEFI-on-Pi; BenchBob needs the native firmware.
- Don't bind proxyDHCP to all interfaces on steamboat (VLANs, `slot*` tester bridges).
- Don't rely on Pi HTTP boot without our own signing key in the EEPROM: unsigned
  `boot.img` from a custom host is rejected.
