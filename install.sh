#!/bin/bash
# Build the BenchBob package from this checkout and install it: for unreleased changes.
# Releases install from GitHub instead (see README). Run on the box itself.
#
# Needs, besides dpkg-deb: Rust (https://rustup.rs) with the musl target for this box
# (packaging/build-deb.sh names it if it's missing), and Bun (https://bun.sh) to build the
# web UI. Neither is needed at runtime. Building on a small board is slow; building on a PC
# and copying the .deb over (packaging/build-deb.sh <arch>) works just as well.
set -euo pipefail

cd "$(dirname "$0")"
arch=$(dpkg --print-architecture)
case "$arch" in
  amd64) target=x86_64-unknown-linux-musl ;;
  arm64) target=aarch64-unknown-linux-musl ;;
  armhf) target=arm-unknown-linux-musleabihf ;;
  *) echo "BenchBob has no package for $arch (amd64, arm64 or armhf)" >&2; exit 1 ;;
esac
missing=
command -v cargo > /dev/null || missing="$missing Rust (https://rustup.rs, then: rustup target add $target);"
[ -n "${WEB_DIST:-}" ] || command -v bun > /dev/null || missing="$missing Bun (https://bun.sh);"
if [ -n "$missing" ]; then
  echo "Building the package needs:$missing" >&2
  exit 1
fi
if command -v rustup > /dev/null && ! rustup target list --installed | grep -qx "$target"; then
  rustup target add "$target"
fi
deb=$(packaging/build-deb.sh "$arch")
sudo apt-get install -y --reinstall "$deb"
