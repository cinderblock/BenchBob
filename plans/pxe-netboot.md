# PXE / netboot server on steamboat, and a diskless DeskyPi

Status: **design — nothing built or changed yet.** Written 2026-10-23; revised 2026-10-23
9:30 AM PDT (16:30Z) after the user's first answers (no DHCP responders, no Bootimus test,
fallback-menu and controller-size questions).
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
3. Bootimus's "no router change" works by running proxyDHCP, and FOG wants to own DHCP.
   Both are ruled out here (see "DHCP policy").
4. It matches BenchBob: same language, same axum/tokio stack, same UI conventions and
   theme, and BenchBob's EEPROM builder already exists for the Pi side.

Don't reinvent the commodity parts: ship stock **iPXE** (built with an embedded script
that phones home), **wimboot**, distro kernels/installers, `partclone`, `nbd`/NFS where
sensible. Borrow ideas from Bootimus (ISO auto-detection, autoinstall templates) and FOG
(imaging flow).

## DHCP policy: nothing on the LAN but UniFi ever answers DHCP

The user guards DHCP in the switch config and won't have any other server sending DHCP
replies. That rules out proxyDHCP: it *is* a DHCP responder (UDP 67/4011), and the switch
guard would drop it anyway. The boot server **never transmits DHCP**. Instead:

| Client | How it finds the server | Router change? |
| --- | --- | --- |
| **Raspberry Pi 4/5** | `TFTP_IP=10.255.0.5` in its EEPROM (BenchBob's EEPROM builder can make this image). It gets an ordinary UniFi lease for its address | **None** |
| **PCs, PXE from firmware** | UniFi's per-network **Network Boot** setting (next-server `10.255.0.5` plus one boot filename) on the main LAN. The filename is an iPXE build with an embedded script, so iPXE's own DHCP pass doesn't loop back to the firmware filename | **Yes, one setting.** Staged in the ops repo for per-change approval in Phase 1. `ops/unifi` only models reservations and DNS today, so it needs a small code addition (or a documented manual setting) |
| **PCs, UEFI HTTP boot** | Many UEFI setups let you type a boot URI by hand (`http://10.255.0.5:8067/ipxe.efi`). No DHCP options needed | None (a firmware setting on each machine) |
| **Anything, last resort** | A USB stick or ISO with our iPXE build | None |

One boot filename for every PC firmware type is a problem: BIOS needs `undionly.kpxe`,
UEFI x64 needs `snponly.efi`, and arm64 needs its own build. Options, best first:

1. **Passive DHCP observer** (recommended; needs the user's OK, open question 5). The
   server opens a receive-only raw socket on `eno1` and records PXE `DISCOVER`s, which
   are broadcast and so reach steamboat: MAC, client arch (option 93), vendor class. It
   never sends anything. When the TFTP request for the boot filename arrives, the server
   maps client IP → MAC (kernel neighbour table, same L2) → arch, and serves the right
   binary under the one name. Bonus: the UI sees every machine that *tries* to netboot,
   even before it reaches iPXE. Check in Phase 1 that the switch's DHCP guard still floods
   client broadcasts to steamboat's port (`tcpdump -ni eno1 port 67`, receive-only).
2. Without that: UEFI x64 only by default, and legacy-BIOS boxes use the USB stick.

## Architecture

### Server (one Rust binary on steamboat, `network_mode: host`)

| Piece | Notes |
| --- | --- |
| **Passive DHCP observer** (optional) | Receive-only raw socket on `eno1`. Records PXE `DISCOVER`s (MAC, option 93 arch, vendor class). **Never transmits.** Feeds arch selection and "tried to netboot" discovery. Parsing: `dhcproto` |
| **Dynamic TFTP** (UDP 69) | Every read goes through our resolver: `(client MAC/serial, path) → bytes`. Serves the right iPXE binary to each PC under the one UniFi filename, and for Pis builds the boot tree on the fly from the assigned image (from a directory, or straight out of a `.img`'s FAT partition à la nobodd), with generated `cmdline.txt`/`config.txt`. Must handle blksize/tsize/windowsize options for speed |
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
To verify in Phase 1: whether a keypress cancels `choose`'s timeout in current iPXE. That
would be the right behaviour, since someone at the keyboard should stop the polling.

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

- **Phase 0, decide** *(current step: awaiting the remaining open questions)*
  - Build vs adopt: decided, build. No Bootimus test.
  - Answer the open questions. Pick a name and repo.
- **Phase 1, server skeleton:**
  - Rust workspace (axum, tokio, SQLite).
  - Dynamic TFTP, HTTP, and the passive DHCP observer if approved.
  - Device discovery and a boot log.
  - Minimal web UI with theme.
  - PC path: our iPXE build → the server-generated **fallback menu**, with reboot, power
    off and refresh polling, and assign-from-UI for hosts waiting at it.
  - Develop against a VM on an isolated virtual network first, so the LAN isn't touched
    until it works. Only there may a throwaway dnsmasq hand out boot options.
  - Docker image built in the app repo's CI; ops stack
    `servers/steamboat/stacks/bootbob/` pinned by digest.
  - **Each of these needs per-change approval:** the ops stack, the Caddy `pxe.tsl` site,
    the UniFi DNS name, and the UniFi Network Boot setting.
- **Phase 2, Pi-native boot + diskless desky:** per-device TFTP trees from images, NBD
  export of per-device roots, golden-image tooling, the EEPROM with `TFTP_IP`, then the
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
- **Build our own.** No Bootimus or other trial run first. (2026-10-23)
- **No DHCP responders on the LAN, ever:** no proxyDHCP, no dnsmasq in DHCP mode. The user
  guards DHCP in the switch config. Pis use `TFTP_IP`. PCs use UniFi's Network Boot
  setting (approved per change), a hand-entered UEFI HTTP-boot URI, or a USB iPXE stick.
  (2026-10-23)
- The fallback iPXE menu has **reboot** (and power off). Hosts waiting at it show up in the
  web UI and can be re-pointed live. (2026-10-23)
- The controller is driven by **API calls from the master server**, with the agent dialling
  out. A local mini web UI on the booted machine is an optional, off-by-default feature.
  Keep the controller as small as is practical. (2026-10-23)

## Open questions for the user

1. **Name and home.** A new repo (recommend, e.g. `BootBob`, matching BenchBob), or a
   crate inside this repo? A new repo seems right: it's a server product with its own
   release cycle, and BenchBob is a client of it.
2. **Network root default:** NBD (recommended) or NFS?
3. **Storage on steamboat:** OK to carve a btrfs volume for images (reflink clones,
   snapshots)? Options: a loop-file btrfs (simplest, slightly slower), or a second drive
   if the NUC has a free slot (best). How much space for images and backups?
4. **PCs and UniFi's Network Boot setting:** OK in principle to set next-server plus one
   boot filename on the main LAN? It would be staged as an ops change for your yes when
   Phase 1 is ready. This is UniFi's own DHCP adding two fields, not another server.
   Without it, PCs can only netboot via a hand-entered UEFI HTTP-boot URI or a USB iPXE
   stick.
5. **Passive DHCP observer:** OK for the server to *listen* for DHCP broadcasts on `eno1`
   (receive-only, never sends)? It lets the server learn each PC's firmware type and spot
   machines trying to netboot. Recommended. Without it, legacy-BIOS PCs need the USB stick.
6. **Legacy BIOS:** support it at all, or UEFI-only? This decides how much question 5
   matters.
7. **Secure Boot:** OK to require it off on machines in phases 1–6, and add the iPXE
   shim later?
8. **macOS:** OK to treat as deferred / old-Intel-rescue-only given the findings?
9. **desky EEPROM:** OK with network-only boot (`0xf2`) plus `TFTP_IP=10.255.0.5`, keeping
   the current SD as a rescue card?

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
- [ ] User answers to the remaining open questions.
- [ ] Phase 0 onward.

## Things not to do

- Don't touch UniFi DHCP options, Caddy, DNS or steamboat directly without a staged,
  approved ops change.
- Don't put SD back in desky's `BOOT_ORDER` once it's diskless.
- Don't make the Pi path depend on UEFI-on-Pi; BenchBob needs the native firmware.
- **Don't run anything that sends DHCP on the LAN**: no proxyDHCP, no dnsmasq in DHCP mode,
  not even briefly for a test. Test DHCP-dependent flows on an isolated VM network.
- Don't bind the TFTP/HTTP servers or the DHCP observer to all of steamboat's interfaces
  (VLANs, `slot*` tester bridges). Use `eno1` / 10.255.0.5 only.
- Don't rely on Pi HTTP boot without our own signing key in the EEPROM: unsigned
  `boot.img` from a custom host is rejected.
