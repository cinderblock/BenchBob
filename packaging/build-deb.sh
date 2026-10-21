#!/bin/bash
# Build the BenchBob .deb for one architecture:
#
#   packaging/build-deb.sh amd64|arm64|armhf [version]
#
# The package is the server (one static Rust binary), the built web UI, the board files
# and the system glue (unit, udev rules, sudoers, setup scripts). No runtime, nothing
# fetched at install time. The version defaults to one derived from the newest v* tag
# (`git describe`). Needs bash, od and dpkg-deb (CI, any Debian box, or WSL), plus:
#
# - the server for ARCH: RUST_BIN=<file>, else backend's cargo output for the arch's
#   target (in $CARGO_TARGET_DIR or backend/target), built here first when cargo is on
#   PATH (`rustup target add <target>` once);
# - the web UI: WEB_DIST=<dir>, else web/ built here with Bun (a build tool only: nothing
#   in the package runs it).
set -euo pipefail

ARCH=${1:?usage: build-deb.sh amd64|arm64|armhf [version]}
# ELF e_machine: 0x3e x86-64, 0xb7 AArch64, 0x28 ARM.
case "$ARCH" in
  amd64) TARGET=x86_64-unknown-linux-musl ELF_MACHINE=3e ;;
  arm64) TARGET=aarch64-unknown-linux-musl ELF_MACHINE=b7 ;;
  # ARMv6 hard-float: Debian's armhf baseline is ARMv7, and this also runs on the ARMv6
  # Pi Zero W and Pi 1 (Raspberry Pi OS's armhf).
  armhf) TARGET=arm-unknown-linux-musleabihf ELF_MACHINE=28 ;;
  *) echo "unknown architecture $ARCH (amd64, arm64 or armhf)" >&2; exit 2 ;;
esac

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PKG=$ROOT/packaging
DIST=$ROOT/dist

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

# ---------- Server ----------
if [ -z "${RUST_BIN:-}" ]; then
  RUST_BIN=${CARGO_TARGET_DIR:-$ROOT/backend/target}/$TARGET/release/benchbob
  if command -v cargo > /dev/null; then
    (cd "$ROOT/backend" && cargo build --release --locked --target "$TARGET" -p benchbob) >&2
  elif [ -f "$RUST_BIN" ]; then
    echo "warning: no cargo on PATH; packaging $RUST_BIN as it is" >&2
  else
    echo "No server binary for $ARCH. Install Rust (https://rustup.rs), then" >&2
    echo "  rustup target add $TARGET" >&2
    echo "or pass a prebuilt one as RUST_BIN=<file>." >&2
    exit 1
  fi
fi
[ -f "$RUST_BIN" ] || { echo "RUST_BIN=$RUST_BIN: no such file" >&2; exit 1; }
magic=$(od -An -c -N4 "$RUST_BIN" | tr -d ' ')
machine=$(od -An -tx1 -j18 -N1 "$RUST_BIN" | tr -d ' ')
if [ "$magic" != '177ELF' ] || [ "$machine" != "$ELF_MACHINE" ]; then
  echo "$RUST_BIN is not an $ARCH ($TARGET) executable" >&2
  exit 1
fi

# ---------- Web UI ----------
# The dashboard served at / is web/'s Vite build. CI builds it once and passes it in with
# WEB_DIST=<dir>. Build output goes to stderr: stdout is the .deb's path.
if [ -n "${WEB_DIST:-}" ]; then
  [ -f "$WEB_DIST/index.html" ] || { echo "WEB_DIST=$WEB_DIST has no index.html" >&2; exit 1; }
else
  command -v bun > /dev/null || { echo "Bun is needed to build web/ (https://bun.sh), or pass a prebuilt one as WEB_DIST=<dir>" >&2; exit 1; }
  (cd "$ROOT/web" && bun install --frozen-lockfile && bun run build:full) >&2
  WEB_DIST=$ROOT/web/dist
fi

# ---------- Tree ----------
# Staged in a temp dir: dpkg-deb insists on real permissions, which a Windows drive
# mounted in WSL (/mnt/c, all 0777) can't give it.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
chmod 0755 "$STAGE"
APP=$STAGE/usr/lib/benchbob
mkdir -p "$APP/bin" "$APP/system" "$APP/drives" "$APP/web/dist" "$STAGE/DEBIAN"

install -m 0755 "$RUST_BIN" "$APP/bin/benchbob"
echo "server: $("$APP/bin/benchbob" --version 2>/dev/null || echo "$RUST_BIN ($TARGET)")" >&2
echo "$VERSION" > "$APP/VERSION"

cp -R "$WEB_DIST"/. "$APP/web/dist/"
find "$APP/web" -type d -exec chmod 0755 {} + && find "$APP/web" -type f -exec chmod 0644 {} +

# Board definitions: the shipped ones, and a directory for the user's own (a file there
# with the same id overrides a shipped one).
install -d -m 0755 "$STAGE/usr/share/benchbob/boards" "$STAGE/etc/benchbob/boards.d"
install -m 0644 "$ROOT"/backend/boards/*.toml "$ROOT/backend/boards/README.md" "$STAGE/usr/share/benchbob/boards/"

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
for s in preinst postinst prerm postrm; do install -m 0755 "$PKG/debian/$s" "$STAGE/DEBIAN/$s"; done
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

mkdir -p "$DIST"
OUT=$DIST/benchbob_${VERSION}_$ARCH.deb
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" > /dev/null
echo "$OUT"
