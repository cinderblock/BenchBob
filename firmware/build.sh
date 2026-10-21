#!/usr/bin/env bash
# Build the release images into firmware/dist/:
#   firmware/build.sh            both boards
#   firmware/build.sh pico2w     benchbob-node-pico2w.uf2   (needs Bun for elf2uf2.ts)
#   firmware/build.sh esp32c6    benchbob-node-esp32c6.bin  (needs espflash 4)
# Works in Git Bash on Windows too.
#
# Both images embed the web UI's MCU build. This builds it (web/dist-mcu, with Bun) unless
# BENCHBOB_WEB_DIST already points at one (CI passes the web job's tested build).
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p dist

if [ -z "${BENCHBOB_WEB_DIST:-}" ]; then
	(cd ../web && bun install --frozen-lockfile && bun run build:mcu)
fi

build_pico2w() {
	(cd pico2w && cargo build --release --locked)
	bun tools/elf2uf2.ts pico2w/target/thumbv8m.main-none-eabihf/release/benchbob-node-pico2w dist/benchbob-node-pico2w.uf2
}

build_esp32c6() {
	(cd esp32c6 && cargo build --release --locked)
	# --skip-padding: the image ends with the app, so it never covers the bbconfig partition.
	# --partition-table-offset: see esp32c6/partitions.csv.
	espflash save-image --chip esp32c6 --merge --skip-padding --flash-size 4mb \
		--partition-table esp32c6/partitions.csv --partition-table-offset 0x8000 --skip-update-check \
		esp32c6/target/riscv32imac-unknown-none-elf/release/benchbob-node-esp32c6 dist/benchbob-node-esp32c6.bin
}

boards=("$@")
[ ${#boards[@]} -gt 0 ] || boards=(pico2w esp32c6)
for b in "${boards[@]}"; do
	case "$b" in
	pico2w) build_pico2w ;;
	esp32c6) build_esp32c6 ;;
	*) echo "unknown board: $b (pico2w or esp32c6)" >&2; exit 2 ;;
	esac
done
ls -l dist
