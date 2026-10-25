#!/bin/bash
# nfs-drive-handler.sh must leave the disk a machine runs from alone, and eMMC, while
# treating a card in the SD slot of a network- or USB-booted machine as a drive. Runs the
# handler's `check` action with fake findmnt/lsblk and a fake /sys, no root needed.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/sys/block/mmcblk0/device" "$TMP/sys/block/mmcblk1/device"
echo SD > "$TMP/sys/block/mmcblk0/device/type"
echo MMC > "$TMP/sys/block/mmcblk1/device/type"
# findmnt -no SOURCE /  → $ROOT_SRC;  lsblk -no PKNAME /dev/X → X without its pN suffix.
cat > "$TMP/bin/findmnt" <<'EOF'
#!/bin/sh
echo "$ROOT_SRC"
EOF
cat > "$TMP/bin/lsblk" <<'EOF'
#!/bin/sh
dev=$(basename "$3")
parent=$(echo "$dev" | sed -E 's/p[0-9]+$//; s/^(sd[a-z]+)[0-9]+$/\1/')
[ "$parent" != "$dev" ] && echo "$parent"
EOF
chmod +x "$TMP/bin/"*

fail=0
check() { # expected root-source device
  local want=$1 root=$2 dev=$3 got
  got=$(PATH="$TMP/bin:$PATH" SYS="$TMP/sys" ROOT_SRC="$root" bash "$HERE/nfs-drive-handler.sh" check "$dev" x)
  if [ "$got" = "$want" ]; then
    echo "ok   $dev (root $root) → $got"
  else
    echo "FAIL $dev (root $root) → $got, wanted $want"
    fail=1
  fi
}

# A Pi booting from its card: the card and its partitions are the system.
check system /dev/mmcblk0p2 /dev/mmcblk0
check system /dev/mmcblk0p2 /dev/mmcblk0p1
check system /dev/mmcblk0p2 /dev/mmcblk0p2
# The same Pi booted from the network (desky) or a USB disk: the card is a drive.
check drive /dev/nbd0p2 /dev/mmcblk0
check drive /dev/nbd0p2 /dev/mmcblk0p2
check drive /dev/sda2 /dev/mmcblk0p1
# eMMC never is, nor its boot/rpmb areas; nothing is read off an NFS root either.
check system /dev/nbd0p2 /dev/mmcblk1
check system /dev/nbd0p2 /dev/mmcblk1p1
check system /dev/nbd0p2 /dev/mmcblk1boot0
check drive 10.0.0.5:/roots/pi /dev/mmcblk0p1
# USB drives are untouched by the card logic (udev only sends them when they're USB).
check drive /dev/mmcblk0p2 /dev/sda1
check drive /dev/nbd0p2 /dev/sdb

exit $fail
