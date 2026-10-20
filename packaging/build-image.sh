#!/bin/bash
# Build a Raspberry Pi SD card image with BenchBob preinstalled:
#
#   sudo packaging/build-image.sh <benchbob_*_arm64.deb> <output.img.xz>
#
# Starts from the latest official Raspberry Pi OS Lite (64-bit), grows it, installs the
# package and its Recommends in a chroot, and recompresses it. The chroot runs arm64
# binaries, so build on an arm64 machine (CI uses an ubuntu-24.04-arm runner; a Pi works
# too). Flash it like any Pi OS image; Raspberry Pi Imager's OS customisation (user,
# Wi-Fi, SSH) still applies.
set -euo pipefail

DEB=$(realpath "${1:?usage: build-image.sh <benchbob arm64 .deb> <output .img.xz>}")
OUT=$(realpath -m "${2:?usage: build-image.sh <benchbob arm64 .deb> <output .img.xz>}")
BASE_URL=https://downloads.raspberrypi.com/raspios_lite_arm64_latest
GROW_MB=1536 # BenchBob, Bun and the panels' tools, with room to spare

[ "$EUID" -eq 0 ] || { echo "run as root" >&2; exit 1; }
[ "$(uname -m)" = aarch64 ] || { echo "build on arm64: the chroot runs the image's binaries" >&2; exit 1; }

WORK=$(mktemp -d)
MNT=$WORK/root
LOOP=
cleanup() {
  set +e
  for m in boot/firmware dev/pts dev proc sys run; do mountpoint -q "$MNT/$m" && umount "$MNT/$m"; done
  mountpoint -q "$MNT" && umount "$MNT"
  [ -n "$LOOP" ] && losetup -d "$LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "--- base image"
curl -fsSL -o "$WORK/base.img.xz" "$BASE_URL"
xz -dc "$WORK/base.img.xz" > "$WORK/image.img"
rm "$WORK/base.img.xz"

echo "--- grow the root partition by $GROW_MB MB"
truncate -s +"$GROW_MB"M "$WORK/image.img"
parted -s "$WORK/image.img" resizepart 2 100%
LOOP=$(losetup -f --show -P "$WORK/image.img")
e2fsck -fy "${LOOP}p2" > /dev/null || [ $? -le 1 ]
resize2fs "${LOOP}p2" > /dev/null

mkdir -p "$MNT"
mount "${LOOP}p2" "$MNT"
mount "${LOOP}p1" "$MNT/boot/firmware"
for m in dev dev/pts proc sys; do mount --bind "/$m" "$MNT/$m"; done
# An empty /run, not the host's: with the host's, postinsts would find a running systemd
# (/run/systemd/system) and start or restart services on the build machine.
mount -t tmpfs tmpfs "$MNT/run"
BASE=$(cat "$MNT/etc/rpi-issue" 2>/dev/null | head -1 || true)
echo "base: ${BASE:-unknown}"

echo "--- install BenchBob"
# No daemons starting inside the chroot. DNS from the host for the duration; the image's
# own resolv.conf (often a symlink into /run) goes back afterwards.
printf '#!/bin/sh\nexit 101\n' > "$MNT/usr/sbin/policy-rc.d"
chmod +x "$MNT/usr/sbin/policy-rc.d"
mv "$MNT/etc/resolv.conf" "$WORK/resolv.conf.image" 2>/dev/null || true
cp -L /etc/resolv.conf "$MNT/etc/resolv.conf"
cp "$DEB" "$MNT/tmp/benchbob.deb"
chroot "$MNT" /bin/bash -euc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq -o Dpkg::Use-Pty=0 /tmp/benchbob.deb
  apt-get clean
  rm -f /tmp/benchbob.deb
'
rm -f "$MNT/etc/resolv.conf" "$MNT/usr/sbin/policy-rc.d"
[ -e "$WORK/resolv.conf.image" ] || [ -L "$WORK/resolv.conf.image" ] && mv "$WORK/resolv.conf.image" "$MNT/etc/resolv.conf"

echo "--- checks"
chroot "$MNT" /bin/bash -euc '
  echo "benchbob $(cat /usr/lib/benchbob/VERSION)"
  systemctl is-enabled benchbob.service
  for u in lldpd gpsd; do [ -z "$(systemctl list-unit-files --no-legend $u.service)" ] || echo "$u: $(systemctl is-enabled $u.service || true)"; done
  test -f /etc/apt/sources.list.d/benchbob.sources
  id benchbob
'
# What the image ships: a record next to the code.
echo "${BASE:-unknown base}" > "$MNT/usr/lib/benchbob/IMAGE_BASE"

sync
umount "$MNT/boot/firmware"
for m in run sys proc dev/pts dev; do umount "$MNT/$m"; done
umount "$MNT"
losetup -d "$LOOP"
LOOP=

echo "--- compress"
xz -T0 -6 -c "$WORK/image.img" > "$OUT"
ls -la "$OUT"
