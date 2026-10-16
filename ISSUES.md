# Issues

## NTFS partitions unmount immediately after auto-mount; vfat sometimes never mounts — fixed in `c260345`, installed on the Pi 2026-10-11

Seen 2026-10-11 on `deskypi` with a Windows NVMe (`sda`: SYSTEM vfat, Windows ntfs,
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
download). `doFlash` downloads the compressed image to `/var/tmp/deskypi-flash` on that
filesystem, so a lite image (~550 MB) barely fits and a full desktop image (~3 GB) will
fail mid-download. A failed or cancelled flash also leaves its partial download behind
(`rm -f dlPath` only runs on success), eating more of the space. Worth finding what fills
the card, and possibly streaming the download straight into the decompressor.
