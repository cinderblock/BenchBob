# Issues

## NTFS partitions unmount immediately after auto-mount; vfat sometimes never mounts — fixed in `c260345`, installed on the Pi 2026-10-11

Seen 2026-10-11 on `benchbob` with a Windows NVMe (`sda`: SYSTEM vfat, Windows ntfs,
WinRE_DRV ntfs). `/srv/nfs/drives/{SYSTEM,Windows,WinRE_DRV}` exist but nothing is mounted.

- NTFS: `/var/log/nfs-drive-handler.log` says `MOUNTED /dev/sda3 at /srv/nfs/drives/Windows`,
  but the journal shows ntfs-3g `Unmounting /dev/sda3 (Windows)` in the same second, as
  `nfs-drive-mount-sda3.service` (the `systemd-run` oneshot from `nfs-drive-handler.sh`)
  finishes. ntfs-3g is a FUSE daemon living in that unit's cgroup.
- vfat `sda1`: `Failed to start transient service unit: Unit nfs-drive-mount-sda1.service
  was already loaded or has a fragment file.` — a transient unit name left over from an
  earlier plug-in blocks the mount on re-plug.

Why it matters: nothing on such a drive is reachable over NFS, SMB or the dashboard's file
browser. A fix also has to be installed to `/usr/local/bin` on the Pi.

## Remove handler's `umount` always fails ("must be superuser"); log noise only

On unplug, `nfs-drive-handler.sh remove` logs `umount: /srv/nfs/drives/<LABEL>: must be
superuser to unmount.` for every mounted partition (seen 2026-10-11, and in the log back to
May). It runs inside udev's sandbox, which can't unmount — the same reason mounting is
deferred to `systemd-run`. Harmless since `c260345`: the mounts are `.mount` units bound to
the device, and systemd unmounts them itself (journal: `Unmounted srv-nfs-drives-*.mount`).
The `umount -l` / `rmdir` loop in the remove branch is now dead weight; dropping it (or
replacing it with `systemctl stop` of the mount unit) would clean up the log. Needs an
install on the Pi.

## nbd-server fails to restart when a drive's last export is removed

On unplug of the only USB drive (2026-10-11 15:50:11), the handler's `systemctl restart
nbd-server` after removing `sda.conf` failed: `Control process exited, code=exited,
status=1/FAILURE`. Probably nbd-server refusing to start with no exports in
`/etc/nbd-server/conf.d/`. It recovers on the next plug-in, so NBD works whenever a drive is
present — but the unit sits failed in between, which shows up as a failed service.

## desky's root filesystem is ~95–99% full; flash downloads live on it

Seen 2026-10-16: `/dev/mmcblk0p2 15G 14G 247M 99% /` (772M free after removing a test
download). `doFlash` downloads the compressed image to `/var/tmp/benchbob-flash` on that
filesystem, so a lite image (~550 MB) barely fits and a full desktop image (~3 GB) will
fail mid-download. A failed or cancelled flash also leaves its partial download behind
(`rm -f dlPath` only runs on success), eating more of the space. Worth finding what fills
the card, and possibly streaming the download straight into the decompressor.

2026-10-17: the culprit was Bun's package cache, `~/.bun/install/cache` (4.8 GB). It was
cleared, and the root filesystem is at 69% (4.4 GB free). The cache grows back with every
`bun install` on the Pi, so the download-to-SD design is still fragile.

2026-10-19: more likely it isn't `bun install` at all (there are no npm dependencies) but
Bun's runtime transpiler cache, which also lives in `~/.bun/install/cache` (`@t@/*.pile`):
one file per version of every source file, never pruned, so every deploy adds a copy of
the app. The package (`fa911fe`) keeps it in `/var/cache/benchbob` and empties it on
upgrade; a git-checkout deploy like desky's still grows it until desky moves to the
package. The flash download-to-root-filesystem problem is unchanged.

## lldpd loses LLDP multicast membership when another lldpd on the same NIC exits

Seen 2026-10-17: a side instance on desky started and stopped its own receive-only lldpd
on eth0 while the live one ran. Afterwards `ip maddr show dev eth0` no longer listed
`01:80:c2:00:00:0e` and the live lldpd reported "no neighbor" until netdiag was restarted
(disable/enable in Peripherals…). Only happens with two BenchBob instances on one Pi, but
netdiag could notice "listening for minutes, link up, no LLDP multicast address" and
restart lldpd itself. The Rust server's netdiag now does (`benchbob-netdiag`, `lldp_heal`:
no `0180c200000e` for a linked port in /proc/net/dev_mcast after two minutes); the Bun
server still doesn't.

## Full status refresh spawns ~12 processes every 2 s while a dashboard is open

`getStatus()` in server.ts runs lsusb, lsblk, findmnt, ip (×3), iw, vcgencmd and three `ls`
calls each time; with a dashboard open that is ~3.5% of a Pi 4 core in children alone
(measured 2026-10-17). Most of it could come from sysfs/procfs reads or be cached
(lsusb, nbd conf.d, root storage change rarely).

## GPIO stream pushes ~5 messages/s from SoC-internal pins

The live GPIO stream sends whenever any of GPIO 0–57 changes, and GPIO37 (SD card) toggles
constantly, so an open GPIO panel gets ~5 updates/s even when the header is idle. Could
send header-pin changes immediately and internal pins at most once a second.

## NetworkManager's TwillTech connection isn't bound to wlan0

The netplan-generated `netplan-wlan0-TwillTech` profile has no interface name, so NM may
bring it up on any new Wi-Fi radio (e.g. a USB dongle plugged in for the Wi-Fi panel). The
Wi-Fi panel sets a dongle unmanaged while it uses it, but between plug-in and first use NM
can grab it.

## Bun server's Wi-Fi panel finds no OUI vendors (CRLF oui.txt)

`parseOui` in `peripherals/lib/wifi-parse.ts` matches lines with `(.+)$`, and JavaScript's
`.` excludes ``, so the CRLF lines of IEEE's oui.txt (Debian's ieee-data ships it that
way) never match. desky's journal: `[wifi] OUI vendors: 0 from /usr/share/ieee-data/oui.txt`,
and no BSS shows a vendor. The Rust port strips the `` (`191178a`); the Bun server still
has the bug (left alone: the TS server is only changed for Rust-mode plumbing during the
port, and goes away after it).

## Bun server: drive names and image URLs reach a shell unchecked (command injection) — fixed in `e0ec561` (not yet released: releases are on hold until the Rust-only package)

Found 2026-10-20 while porting server.ts to Rust (the Rust drives crate validates both).
No login, so anyone on the LAN can do this against port 80:

- `POST /api/probe {"device": "x; touch /tmp/pwned"}`: `readBlocks` builds a `sudo sh -c`
  script with `/dev/${devName}`, and the device is only checked with `isSysDev`.
- `POST /api/flash`: `doFlash` runs `bash -o pipefail -c` with `/dev/${op.device}` and the
  download path, whose file name is the image URL's last segment (`"$(...)"` inside the
  double quotes runs). `eject`, `erase` and `deepscan` pass the name to argv only, but
  still unchecked.

Why it matters: root command execution (the commands run under sudo). `/api/blcard` and
`/api/osinfo` already check `^[a-z][a-z0-9]*$`; the other routes need the same, and the
flash file name a whitelist.
