#!/bin/bash
# Upgrade test: the Bun-based 0.4.x release from GitHub, then the given package, the three
# ways a box gets there:
#
#   sudo packaging/test-upgrade.sh <new benchbob .deb> [apt|rust-mode|update ...]
#
#   apt        `apt install ./new.deb` over a plain 0.4.1
#   rust-mode  the same over a 0.4.1 in Rust mode (`benchbob-setup rust on`: a Rust server
#              in benchbob-rust.service on port 80, Bun moved to 8081 by a drop-in). The
#              released 0.4.1 has no Rust server, so a stand-in is packaged into it.
#   update     the dashboard's Update button: 0.4.1's own POST /api/update/install, which
#              runs 0.4.1's benchbob-update against an apt source holding the new package
#
# Each starts from a purged box, seeds settings through 0.4.1's API, upgrades, and checks
# the service, the settings, the API and that no Bun is left. Default: all three. Run as
# root on a systemd machine with network access (CI runners, a throwaway VM); it installs
# and purges benchbob, and replaces /etc/apt/sources.list.d/benchbob.sources. Needs curl,
# jq, ss and dpkg-deb.
set -euo pipefail

NEW=$(realpath "${1:?usage: test-upgrade.sh <new .deb> [apt|rust-mode|update ...]}")
shift
SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -gt 0 ] || SCENARIOS=(apt rust-mode update)
OLD_VERSION=${OLD_VERSION:-0.4.1}
[ "$EUID" -eq 0 ] || { echo "run as root" >&2; exit 1; }

ARCH=$(dpkg --print-architecture)
[ "$(dpkg-deb -f "$NEW" Architecture)" = "$ARCH" ] || { echo "$NEW is not for $ARCH" >&2; exit 1; }
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
# What the server reports (/usr/lib/benchbob/VERSION): the package version it was built as.
NEW_VERSION=$(dpkg-deb --fsys-tarfile "$NEW" | tar -xO ./usr/lib/benchbob/VERSION)
# A build between releases can sort below the old release (master is 0.4.0+git…, while
# 0.4.1 was tagged on release/0.4). apt wouldn't call that an upgrade, so give the package
# under test a version above the old one; only the control file changes.
if dpkg --compare-versions "$(dpkg-deb -f "$NEW" Version)" le "$OLD_VERSION"; then
  bumped=$OLD_VERSION+upgradetest
  echo "$(dpkg-deb -f "$NEW" Version) sorts below $OLD_VERSION: testing it as $bumped"
  dpkg-deb -R "$NEW" "$WORK/new"
  sed -i "s/^Version: .*/Version: $bumped/" "$WORK/new/DEBIAN/control"
  dpkg-deb --root-owner-group -Zxz -b "$WORK/new" "$WORK/benchbob_${bumped}_$ARCH.deb" > /dev/null
  rm -rf "$WORK/new"
  NEW=$WORK/benchbob_${bumped}_$ARCH.deb
fi
export DEBIAN_FRONTEND=noninteractive
APT=(apt-get install -y -qq --no-install-recommends -o Dpkg::Use-Pty=0)
DROPIN=/etc/systemd/system/benchbob.service.d/50-rust-mode.conf
SOURCES=/etc/apt/sources.list.d/benchbob.sources

step() { echo; echo "=== $*"; }
fail() {
  echo "FAIL: $*" >&2
  journalctl -u benchbob -u benchbob-rust -u benchbob-update --no-pager -n 80 >&2 || true
  exit 1
}
# curl http://localhost<path> [curl args...]
api() {
  local p=$1
  shift
  curl -fsS --max-time 20 "$@" "http://localhost$p"
}
post() { api "$1" -X "${3:-POST}" -H 'Content-Type: application/json' --data "$2"; }
# Wait until /api/status reports this version.
wait_version() {
  local v=
  for _ in $(seq 90); do
    v=$(api /api/status 2> /dev/null | jq -r .version 2> /dev/null) || true
    [ "$v" = "$1" ] && return 0
    sleep 2
  done
  fail "timed out waiting for version $1 on port 80 (last seen: ${v:-nothing})"
}
purge() {
  if dpkg -s benchbob &> /dev/null; then apt-get purge -y -qq benchbob > /dev/null; fi
  rm -rf /var/lib/benchbob /var/cache/benchbob "$DROPIN" /etc/systemd/system/*.wants/benchbob-rust.service
  rmdir "$(dirname "$DROPIN")" 2> /dev/null || true
  systemctl daemon-reload
}

apt-get update -qq
OLD_DEB=$WORK/benchbob_${OLD_VERSION}_$ARCH.deb
step "download $OLD_VERSION ($ARCH)"
curl -fsSL -o "$OLD_DEB" "https://github.com/cinderblock/BenchBob/releases/download/v$OLD_VERSION/benchbob_${OLD_VERSION}_$ARCH.deb"
dpkg-deb -f "$OLD_DEB" Version

install_old() {
  step "install $OLD_VERSION"
  purge
  "${APT[@]}" "$1" > /dev/null
  wait_version "$OLD_VERSION"
  test -x /usr/lib/benchbob/bin/bun || fail "$OLD_VERSION should run on Bun"
}

# Settings a user would have made, through the old server's API.
seed() {
  step "seed settings through $OLD_VERSION's API"
  post /api/update/settings '{"check":false}' PUT > /dev/null
  post /api/favorites '[{"key":"rpios/current/64/lite","label":"Kept across the upgrade"}]' > /dev/null
  post /api/quicklist '[{"key":"rpios/current/32/lite","label":"Quick, kept"}]' > /dev/null
  post /api/periph/driver '{"id":"sdr","disabled":true}' > /dev/null
  # Some are written in the background.
  for f in favorites.json quicklist.json update.json peripherals.json; do
    for _ in $(seq 20); do [ -s "/var/lib/benchbob/$f" ] && break; sleep 0.5; done
    test -s "/var/lib/benchbob/$f" || fail "$OLD_VERSION didn't write $f"
  done
  ls -l /var/lib/benchbob
  sha256sum /var/lib/benchbob/favorites.json /var/lib/benchbob/quicklist.json > "$WORK/settings.sha256"
  # Bun's transpiler cache, which the upgrade must clear (a marker, should Bun have written
  # nothing yet).
  mkdir -p /var/cache/benchbob/transpiler
  touch /var/cache/benchbob/transpiler/marker
}

verify_new() {
  step "verify $NEW_VERSION"
  wait_version "$NEW_VERSION"
  systemctl is-active --quiet benchbob || fail "benchbob.service isn't active"
  systemctl is-enabled --quiet benchbob || fail "benchbob.service isn't enabled"
  local pid exe
  pid=$(systemctl show -p MainPID --value benchbob)
  exe=$(readlink "/proc/$pid/exe")
  [ "$exe" = /usr/lib/benchbob/bin/benchbob ] || fail "benchbob.service runs $exe"
  ss -Hltnp 'sport = :80' | grep -q "pid=$pid," || fail "port 80 isn't the Rust server's: $(ss -Hltnp 'sport = :80')"
  # No Bun anywhere: not running, not installed.
  if pgrep -x bun > /dev/null; then fail "bun is still running: $(pgrep -ax bun)"; fi
  test ! -e /usr/lib/benchbob/bin/bun || fail "bun is still installed"
  local left
  left=$(find /usr/lib/benchbob -name '*.ts')
  for f in ui peripherals index.html; do
    if [ -e "/usr/lib/benchbob/$f" ]; then left="$left /usr/lib/benchbob/$f"; fi
  done
  [ -z "$left" ] || fail "Bun-era files left: $left"
  test -x /usr/lib/benchbob/bin/benchbob && test -f /usr/lib/benchbob/web/dist/index.html || fail "package files missing"
  # Rust mode is gone.
  test ! -e "$DROPIN" || fail "the Rust-mode drop-in is still there"
  test ! -e /usr/lib/systemd/system/benchbob-rust.service || fail "benchbob-rust.service is still installed"
  if systemctl is-active --quiet benchbob-rust; then fail "benchbob-rust.service still runs"; fi
  if ls /etc/systemd/system/*.wants/benchbob-rust.service &> /dev/null; then fail "benchbob-rust.service is still enabled"; fi
  # The cache was emptied.
  [ -z "$(ls -A /var/cache/benchbob 2> /dev/null)" ] || fail "/var/cache/benchbob wasn't cleared: $(ls -A /var/cache/benchbob)"
  # The same settings, in the same files.
  sha256sum -c --quiet "$WORK/settings.sha256" || fail "settings files changed"
  api /api/favorites | jq -e '. == [{"key":"rpios/current/64/lite","label":"Kept across the upgrade"}]' > /dev/null || fail "favorites lost: $(api /api/favorites)"
  api /api/quicklist | jq -e '.[0].label == "Quick, kept"' > /dev/null || fail "quick list lost: $(api /api/quicklist)"
  api /api/update | jq -e '.check == false' > /dev/null || fail "update setting lost: $(api /api/update | jq -c '{check}')"
  api /api/status | jq -e '.peripherals.list | map(select(.id == "sdr"))[0].disabled == true' > /dev/null ||
    fail "the disabled SDR panel is enabled again: $(api /api/status | jq -c '.peripherals.list | map(select(.id == "sdr"))')"
  # The API and the dashboard.
  api /api | jq -e '.name == "BenchBob API"' > /dev/null || fail "GET /api"
  api /api/status | jq -e --arg v "$NEW_VERSION" '.version == $v and .system.cores > 0' > /dev/null || fail "GET /api/status"
  api / > "$WORK/index.html"
  grep -q 'name="benchbob-build"' "$WORK/index.html" || fail "/ isn't the web UI"
  if grep -q '__BUILD_VERSION__' "$WORK/index.html"; then fail "/ isn't stamped"; fi
  sudo -u benchbob sudo -n true || fail "the benchbob user lost sudo"
  echo "ok: $OLD_VERSION → $NEW_VERSION"
}

for s in "${SCENARIOS[@]}"; do
  case "$s" in
    apt)
      install_old "$OLD_DEB"
      seed
      step "apt install ./$(basename "$NEW")"
      "${APT[@]}" "$NEW" > /dev/null
      verify_new
      ;;
    rust-mode)
      # 0.4.1 as `RUST_BIN=… build-deb.sh` would have built it: its benchbob-rust.service,
      # and a stand-in Rust server (holds port 80, prints a driver list for the drop-in).
      step "build $OLD_VERSION with a stand-in Rust server"
      rm -rf "$WORK/old"
      dpkg-deb -R "$OLD_DEB" "$WORK/old"
      curl -fsSL -o "$WORK/old/usr/lib/systemd/system/benchbob-rust.service" \
        "https://raw.githubusercontent.com/cinderblock/BenchBob/v$OLD_VERSION/packaging/benchbob-rust.service"
      cat > "$WORK/old/usr/lib/benchbob/bin/benchbob" << 'EOF'
#!/bin/sh
# Test stand-in for 0.4.x's opt-in Rust server.
[ "$1" = --native-drivers ] && { echo gpio; exit 0; }
exec /usr/lib/benchbob/bin/bun -e 'Bun.serve({ port: Number(process.env.PORT || 80), fetch: () => new Response("rust-mode stand-in") })'
EOF
      chmod 0755 "$WORK/old/usr/lib/benchbob/bin/benchbob"
      dpkg-deb --root-owner-group -b "$WORK/old" "$WORK/old-rust.deb" > /dev/null
      install_old "$WORK/old-rust.deb"
      seed
      step "rust mode on ($OLD_VERSION's benchbob-setup)"
      benchbob-setup rust on
      for _ in $(seq 30); do [ "$(api / 2> /dev/null)" = "rust-mode stand-in" ] && break; sleep 1; done
      [ "$(api /)" = "rust-mode stand-in" ] || fail "the stand-in doesn't hold port 80"
      test -f "$DROPIN" && systemctl is-active --quiet benchbob-rust || fail "Rust mode isn't on"
      step "apt install ./$(basename "$NEW")"
      "${APT[@]}" "$NEW" > /dev/null
      verify_new
      ;;
    update)
      install_old "$OLD_DEB"
      seed
      step "the Update button ($OLD_VERSION's POST /api/update/install)"
      # An apt source with the new package in place of the release repository.
      mkdir -p "$WORK/repo"
      cp "$NEW" "$WORK/repo/"
      (
        cd "$WORK/repo"
        f=$(basename "$NEW")
        {
          dpkg-deb -f "$f"
          echo "Filename: ./$f"
          echo "Size: $(stat -c %s "$f")"
          echo "SHA256: $(sha256sum "$f" | cut -d' ' -f1)"
          echo
        } > Packages
      )
      chmod -R a+rX "$WORK"
      printf 'Types: deb\nURIs: file:%s\nSuites: ./\nTrusted: yes\n' "$WORK/repo" > "$SOURCES"
      : > /var/log/benchbob-update.log
      post /api/update/install '{"confirm":true}' | jq -e '.ok == true' > /dev/null || fail "the update didn't start"
      wait_version "$NEW_VERSION"
      # The new server answers as soon as postinst restarts it, but the updater writes its
      # "done" line only once apt itself has exited: wait for its unit to finish.
      for _ in $(seq 120); do systemctl is-active --quiet benchbob-update.service || break; sleep 1; done
      grep -q "done: $NEW_VERSION" /var/log/benchbob-update.log || {
        cat /var/log/benchbob-update.log
        fail "benchbob-update didn't report success"
      }
      verify_new
      ;;
    *) echo "unknown scenario $s (apt, rust-mode or update)" >&2; exit 2 ;;
  esac
done

step "purge"
apt-get purge -y -qq benchbob > /dev/null
if systemctl is-active --quiet benchbob; then fail "still running after purge"; fi
test ! -e /var/lib/benchbob || fail "/var/lib/benchbob survived the purge"
echo
echo "PASS: ${SCENARIOS[*]}"
