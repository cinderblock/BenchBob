#!/bin/bash
# Auto-mount/unmount USB drives under NFS export tree + manage NBD exports
# Called by udev on block device add/remove

ACTION="$1"
DEVNAME="$2"
DEVTYPE="$3"  # "disk" or "partition"

EXPORT_BASE="/srv/nfs/drives"
NBD_CONF_DIR="/etc/nbd-server/conf.d"
LOGFILE="/var/log/benchbob-drives.log"

log() { echo "$(date -Is) $*" >> "$LOGFILE"; }

nbd_export_name() {
  basename "$1"
}

add_nbd_export() {
  local dev="$1"
  local name
  name=$(nbd_export_name "$dev")
  cat > "${NBD_CONF_DIR}/${name}.conf" <<EOF
[${name}]
  exportname = ${dev}
  readonly = true
EOF
  log "NBD export added: ${name} -> ${dev}"
}

remove_nbd_export() {
  local dev="$1"
  local name
  name=$(nbd_export_name "$dev")
  rm -f "${NBD_CONF_DIR}/${name}.conf"
  log "NBD export removed: ${name}"
}

reload_nbd() {
  systemctl restart nbd-server 2>>"$LOGFILE" && log "NBD server restarted"
}

# The whole-disk name behind a device: /dev/mmcblk0p2 → mmcblk0, /dev/sda → sda.
disk_of() {
  local parent
  parent=$(lsblk -no PKNAME "$1" 2>/dev/null | head -1)
  if [ -n "$parent" ]; then echo "$parent"; else basename "$1"; fi
}

# A card in the SD slot is a drive like any other, unless it is what this machine runs
# from (a Pi booting from its card) or built-in eMMC (which udev already keeps out, by
# MMC_TYPE). The disk the root filesystem is on is found the way the server finds it.
is_system_card() {
  case "$1" in
    /dev/mmcblk*) ;;
    *) return 1 ;;
  esac
  local disk root_src root_disk
  disk=$(disk_of "$1")
  case "$disk" in
    *boot*|*rpmb*) return 0 ;;
  esac
  root_src=$(findmnt -no SOURCE / 2>/dev/null)
  case "$root_src" in
    /dev/*)
      root_disk=$(disk_of "$root_src")
      [ "$disk" = "$root_disk" ] && return 0
      ;;
  esac
  [ "$(cat "${SYS:-/sys}/block/$disk/device/type" 2>/dev/null)" = "MMC" ] && return 0
  return 1
}

if is_system_card "$DEVNAME"; then
  [ "$ACTION" = "check" ] && echo "system"
  exit 0
fi

case "$ACTION" in
  # For the tests: say what an add/remove would treat this device as.
  check)
    echo "drive"
    ;;

  add)
    sleep 1

    # NBD: export the raw block device (whole disk or partition)
    add_nbd_export "$DEVNAME"

    # Only mount partitions with filesystems, not whole disks
    if [ "$DEVTYPE" = "partition" ]; then
      FSTYPE=$(lsblk -no FSTYPE "$DEVNAME" 2>/dev/null | head -1)
      if [ -n "$FSTYPE" ]; then
        LABEL=$(lsblk -no LABEL "$DEVNAME" 2>/dev/null | head -1 | tr -s ' ' '_' | tr -d '/')
        if [ -z "$LABEL" ]; then
          LABEL=$(basename "$DEVNAME")
        fi

        MOUNTPOINT="${EXPORT_BASE}/${LABEL}"

        if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
          log "SKIP $DEVNAME already mounted at $MOUNTPOINT"
        else
          mkdir -p "$MOUNTPOINT"
          UNIT="nfs-drive-mount-$(basename "$DEVNAME")"
          # A failed run from an earlier plug-in (before --collect) keeps the unit name
          # loaded, and systemd-run then refuses to start it again.
          systemctl reset-failed "$UNIT.service" 2>/dev/null
          # Defer mount to systemd to escape udev's restricted mount namespace.
          # --collect unloads the unit once it finishes, however it finishes.
          systemd-run --no-block --collect \
            --unit="$UNIT" \
            --property="Type=oneshot" \
            /usr/lib/benchbob/drives/nfs-drive-mount.sh "$DEVNAME" "$MOUNTPOINT" \
            2>>"$LOGFILE"
        fi
      fi
    fi

    reload_nbd
    ;;

  remove)
    # NBD: remove export
    remove_nbd_export "$DEVNAME"

    # NFS: unmount if it was a mounted partition
    grep "$DEVNAME" /proc/mounts 2>/dev/null | awk '{print $2}' | while read -r MP; do
      umount -l "$MP" 2>>"$LOGFILE" && log "UNMOUNTED $MP ($DEVNAME)"
      rmdir "$MP" 2>/dev/null
    done

    for dir in "${EXPORT_BASE}"/*/; do
      [ -d "$dir" ] || continue
      if ! mountpoint -q "$dir" 2>/dev/null; then
        rmdir "$dir" 2>/dev/null
      fi
    done

    exportfs -ra 2>>"$LOGFILE"
    reload_nbd
    ;;
esac
