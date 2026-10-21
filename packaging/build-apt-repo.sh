#!/bin/bash
# Build the signed apt repository served from GitHub Pages:
#
#   packaging/build-apt-repo.sh <dir of .debs> <output dir>
#
# The output is a complete repo (pool/, dists/stable/), plus the public key. CI runs it on
# every release with the newest few releases' packages, so the site is rebuilt from the
# releases each time instead of accumulating state. The signing key comes from
# $APT_SIGNING_KEY (an armored secret key); without it, the repo is built unsigned, which
# is only useful for testing with [trusted=yes]. Needs dpkg-dev, apt-utils and gpg.
set -euo pipefail

DEBS=${1:?usage: build-apt-repo.sh <debs dir> <out dir>}
OUT=${2:?usage: build-apt-repo.sh <debs dir> <out dir>}
ROOT=$(cd "$(dirname "$0")/.." && pwd)

rm -rf "$OUT"
mkdir -p "$OUT/pool/main"
OUT=$(cd "$OUT" && pwd) # absolute: the rest runs from inside it
cp "$DEBS"/benchbob_[0-9]*.deb "$OUT/pool/main/"
cd "$OUT"

for arch in amd64 arm64 armhf; do
  d=dists/stable/main/binary-$arch
  mkdir -p "$d"
  dpkg-scanpackages --multiversion --arch "$arch" pool > "$d/Packages"
  gzip -9nk "$d/Packages"
done

apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=BenchBob \
  -o APT::FTPArchive::Release::Label=BenchBob \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures="amd64 arm64 armhf" \
  -o APT::FTPArchive::Release::Components=main \
  -o APT::FTPArchive::Release::Description="BenchBob releases" \
  release dists/stable > dists/stable/Release

if [ -n "${APT_SIGNING_KEY:-}" ]; then
  export GNUPGHOME=$(mktemp -d)
  trap 'rm -rf "$GNUPGHOME"' EXIT
  echo "$APT_SIGNING_KEY" | gpg --batch --quiet --import
  gpg --batch --yes --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
  gpg --batch --yes --clearsign -o dists/stable/InRelease dists/stable/Release
else
  echo "APT_SIGNING_KEY not set: the repo is unsigned" >&2
fi

cp "$ROOT/packaging/benchbob-archive-keyring.asc" "$OUT/benchbob-archive-keyring.asc"
echo "$OUT"
