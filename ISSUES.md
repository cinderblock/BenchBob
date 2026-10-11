# Issues

## NTFS partitions unmount immediately after auto-mount; vfat sometimes never mounts

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
