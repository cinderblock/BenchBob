#!/bin/bash
# Build the BenchBob .deb for one architecture:
#
#   packaging/build-deb.sh arm64|amd64 [version]
#
# The package carries the sources, the built web UI and a pinned Bun runtime; nothing is
# fetched at install time. The version defaults to one derived from the newest v* tag (`git
# describe`). Needs bash, curl, unzip, sha256sum and dpkg-deb: CI, any Debian box, or WSL;
# plus Bun to build web/, unless WEB_DIST=<dir> passes in a build made elsewhere.
set -euo pipefail

BUN_VERSION=1.4.2

ARCH=${1:?usage: build-deb.sh arm64|amd64 [version]}
case "$ARCH" in
  arm64) BUN_ASSET=bun-linux-aarch64 ;;
  # baseline: runs on x86-64 CPUs without AVX2 (older Celerons, Atoms).
  amd64) BUN_ASSET=bun-linux-x64-baseline ;;
  *) echo "unknown architecture $ARCH (arm64 or amd64)" >&2; exit 2 ;;
esac

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/packaging
DIST=$ROOT/dist
CACHE=$DIST/cache

# v1.2.3 → 1.2.3; 4 commits past it → 1.2.3+git4.abc1234; no tag yet → 0.0.0~git<n>.<sha>,
# n = commits in HEAD. Each part grows with every commit, so apt always sees a newer
# build as an upgrade (a hash alone sorts at random).
version_from_git() {
  local d
  if d=$(git -C "$ROOT" describe --tags --match 'v[0-9]*' --long 2>/dev/null); then
    local tag=${d%-*-g*} rest=${d#"${d%-*-g*}"-}
    local n=${rest%-g*} sha=${rest#*-g}
    if [ "$n" = 0 ]; then echo "${tag#v}"; else echo "${tag#v}+git$n.$sha"; fi
  else
    echo "0.0.0~git$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 0).$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  fi
}
VERSION=${2:-$(version_from_git)}
VERSION=${VERSION#v}

# ---------- Bun ----------
mkdir -p "$CACHE"
BUN_ZIP=$CACHE/$BUN_ASSET-$BUN_VERSION.zip
SUMS=$CACHE/bun-$BUN_VERSION-SHASUMS256.txt
BASE=https://github.com/oven-sh/bun/releases/download/bun-v$BUN_VERSION
[ -f "$SUMS" ] || curl -fsSL -o "$SUMS" "$BASE/SHASUMS256.txt"
[ -f "$BUN_ZIP" ] || curl -fsSL -o "$BUN_ZIP" "$BASE/$BUN_ASSET.zip"
want=$(awk -v f="$BUN_ASSET.zip" '$2 == f { print $1 }' "$SUMS")
have=$(sha256sum "$BUN_ZIP" | cut -d' ' -f1)
if [ -z "$want" ] || [ "$want" != "$have" ]; then
  rm -f "$BUN_ZIP"
  echo "checksum mismatch for $BUN_ASSET.zip (want ${want:-nothing}, got $have)" >&2
  exit 1
fi

# ---------- Tree ----------
# Staged in a temp dir: dpkg-deb insists on real permissions, which a Windows drive
# mounted in WSL (/mnt/c, all 0777) can't give it.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
chmod 0755 "$STAGE"
APP=$STAGE/usr/lib/benchbob
mkdir -p "$APP/bin" "$APP/system" "$APP/drives" "$STAGE/DEBIAN"

# The app: everything server.ts loads, minus tests and captured fixtures.
install -m 0644 "$ROOT"/index.html "$APP/"
(cd "$ROOT" && find . -maxdepth 1 -name '*.ts' ! -name '*.test.ts' -printf '%P\n' && find peripherals ui -type f \( -name '*.ts' -o -name '*.js' \) ! -name '*.test.ts' ! -path '*-fixtures/*') |
  while read -r f; do install -D -m 0644 "$ROOT/$f" "$APP/$f"; done
echo "$VERSION" > "$APP/VERSION"

unzip -q -j -o "$BUN_ZIP" "$BUN_ASSET/bun" -d "$APP/bin"
chmod 0755 "$APP/bin/bun"

# ---------- Web UI ----------
# The dashboard served at / is web/'s Vite build. CI builds it once and passes it in with
# WEB_DIST=<dir>; otherwise it's built here with the Bun on PATH or, when this host can run
# it, the Bun being packaged. Build output goes to stderr: stdout is the .deb's path.
if [ -n "${WEB_DIST:-}" ]; then
  [ -f "$WEB_DIST/index.html" ] || { echo "WEB_DIST=$WEB_DIST has no index.html" >&2; exit 1; }
else
  BUN=$(command -v bun || true)
  if [ -z "$BUN" ] && "$APP/bin/bun" --version > /dev/null 2>&1; then BUN=$APP/bin/bun; fi
  [ -n "$BUN" ] || { echo "Bun is needed to build web/ (or pass a prebuilt one as WEB_DIST=<dir>)" >&2; exit 1; }
  (cd "$ROOT/web" && "$BUN" install --frozen-lockfile && "$BUN" run build:full) >&2
  WEB_DIST=$ROOT/web/dist
fi
mkdir -p "$APP/web/dist"
cp -R "$WEB_DIST"/. "$APP/web/dist/"
find "$APP/web" -type d -exec chmod 0755 {} + && find "$APP/web" -type f -exec chmod 0644 {} +

install -m 0755 "$PKG/benchbob-setup" "$PKG/benchbob-update" "$APP/system/"
install -m 0755 "$PKG/drives/nfs-drive-handler.sh" "$PKG/drives/nfs-drive-mount.sh" "$APP/drives/"
install -D -m 0644 "$PKG/drives/90-benchbob-drives.rules" "$STAGE/usr/lib/udev/rules.d/90-benchbob-drives.rules"
install -D -m 0644 "$PKG/60-benchbob-gpio.rules" "$STAGE/usr/lib/udev/rules.d/60-benchbob-gpio.rules"
install -D -m 0644 "$PKG/benchbob.service" "$STAGE/usr/lib/systemd/system/benchbob.service"
install -D -m 0644 "$PKG/modules-load.conf" "$STAGE/usr/lib/modules-load.d/benchbob.conf"
install -D -m 0644 "$PKG/modprobe-rtl-sdr.conf" "$STAGE/usr/lib/modprobe.d/benchbob-rtl-sdr.conf"
install -D -m 0440 "$PKG/benchbob.sudoers" "$STAGE/etc/sudoers.d/benchbob"
install -D -m 0644 "$PKG/benchbob.exports" "$STAGE/etc/exports.d/benchbob.exports"
install -D -m 0644 "$PKG/default" "$STAGE/etc/default/benchbob"
# Our apt repository, so `apt upgrade` and the dashboard's Update button see new releases.
install -D -m 0644 "$PKG/benchbob.sources" "$STAGE/etc/apt/sources.list.d/benchbob.sources"
install -D -m 0644 "$PKG/benchbob-archive-keyring.asc" "$STAGE/usr/share/keyrings/benchbob-archive-keyring.asc"
mkdir -p "$STAGE/usr/sbin"
ln -s ../lib/benchbob/system/benchbob-setup "$STAGE/usr/sbin/benchbob-setup"
install -D -m 0644 "$ROOT/LICENSE" "$STAGE/usr/share/doc/benchbob/copyright"
install -m 0644 "$ROOT/README.md" "$STAGE/usr/share/doc/benchbob/README.md"

# ---------- Control ----------
for s in postinst prerm postrm; do install -m 0755 "$PKG/debian/$s" "$STAGE/DEBIAN/$s"; done
# The sudoers rule is deliberately not a conffile: it must go when the package is
# removed, not linger until a purge.
printf '%s\n' /etc/exports.d/benchbob.exports /etc/default/benchbob /etc/apt/sources.list.d/benchbob.sources > "$STAGE/DEBIAN/conffiles"

# Depends: what the server and the drive sharing can't work without. Recommends (installed
# by default with apt): the tools behind individual panels, which check for them and say
# what's missing. rpiboot and rpi-eeprom only exist in Raspberry Pi OS's archive; apt
# skips Recommends it can't find.
DEPENDS="sudo, systemd, udev, util-linux, fdisk, parted, dosfstools, iproute2, kmod, procps, passwd, file, xz-utils, gzip, unzip, usbutils, openssl, curl, ca-certificates, nfs-kernel-server, nbd-server, samba"
RECOMMENDS="apfsprogs, ntfs-3g, exfatprogs, udisks2, rpiboot, rpi-eeprom, iw, wpasupplicant, udhcpc, i2c-tools, gpiod, can-utils, usbip, usbredirect, usbredirserver, uhubctl, ustreamer, v4l-utils, lldpd, iperf3, ethtool, iputils-ping, bluez, rtl-sdr, rtl-433, gpsd, nut-server, nut-client, openocd, dfu-util, picotool, gdb-multiarch, esptool, avrdude, python3-serial, ieee-data, dkms, git"

SIZE=$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)
cat > "$STAGE/DEBIAN/control" << EOF
Package: benchbob
Version: $VERSION
Architecture: $ARCH
Maintainer: Cameron Tacklind <cameron@tacklind.com>
Installed-Size: $SIZE
Depends: $DEPENDS
Recommends: $RECOMMENDS
Section: misc
Priority: optional
Homepage: https://github.com/cinderblock/BenchBob
Description: bench server for the hardware plugged into it
 BenchBob turns a Raspberry Pi or other Linux box into a bench server. Plug things
 into it and use them from anywhere on the LAN, from a browser, a script or an AI
 agent: USB drives shared over NFS, NBD and SMB, OS image flashing, Compute Module
 boot tools, and panels for serial, CAN, I2C, 1-Wire, GPIO, Wi-Fi, BLE, cameras,
 debug probes, RTL-SDR, GPS, UPS and wired-network diagnostics. Everything is also
 a REST endpoint and an MCP tool.
 .
 There is no login: run it only on a network you trust.
EOF

OUT=$DIST/benchbob_${VERSION}_$ARCH.deb
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" > /dev/null
echo "$OUT"
