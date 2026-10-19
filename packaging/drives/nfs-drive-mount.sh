#!/bin/bash
# Mount a drive for NFS export — called via systemd-run from udev handler
DEVNAME="$1"
MOUNTPOINT="$2"
LOGFILE="/var/log/benchbob-drives.log"

sleep 1

# systemd-mount, not mount: the mount gets its own .mount unit. A FUSE filesystem
# (ntfs-3g, apfs-fuse) runs as a daemon, and a plain `mount` would leave that daemon in
# this short-lived oneshot's cgroup, where systemd kills it — and the mount with it — the
# moment this script exits. A .mount unit for a block device is also bound to the device,
# so systemd unmounts it by itself when the drive is pulled.
#
# --fsck=no: systemd-mount fscks block devices by default, and fsck -a writes to the
# filesystem. These are other people's drives, mounted read-only on purpose.
# --collect: a mount that fails must not leave a failed unit squatting on the name.
if systemd-mount --no-ask-password --fsck=no --collect -o ro "$DEVNAME" "$MOUNTPOINT" 2>>"$LOGFILE"; then
  echo "$(date -Is) MOUNTED $DEVNAME at $MOUNTPOINT" >> "$LOGFILE"
  exportfs -ra 2>>"$LOGFILE"
else
  echo "$(date -Is) FAILED to mount $DEVNAME at $MOUNTPOINT" >> "$LOGFILE"
  rmdir "$MOUNTPOINT" 2>/dev/null
fi
