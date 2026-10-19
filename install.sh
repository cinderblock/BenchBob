#!/bin/bash
# Build the BenchBob package from this checkout and install it: for unreleased changes.
# Releases install from GitHub instead (see README). Run on the box itself.
set -euo pipefail

cd "$(dirname "$0")"
deb=$(packaging/build-deb.sh "$(dpkg --print-architecture)")
sudo apt-get install -y --reinstall "$deb"
