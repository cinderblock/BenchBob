# Board files

A board file tells BenchBob how to recognise a machine, what pin headers it has, how GPIO
levels are read, where its boot-time bus configuration lives and which health readings
it has. One TOML file per board. **Adding a board is adding a file; nothing is rebuilt.**

| Where | What |
| --- | --- |
| `/usr/share/benchbob/boards/*.toml` | Shipped with the package (this directory in the repo). Copies are also built into the binary, used when the directory doesn't exist. |
| `/etc/benchbob/boards.d/*.toml` | Yours. Read after the shipped ones: a file with the same `id` replaces the shipped board, any other id adds one. |

`BENCHBOB_BOARDS_DIR` and `BENCHBOB_BOARDS_D` point at other directories (tests, a side
instance). Files are read in name order; one that doesn't parse is skipped and reported
in the log and in `GET /api/boards`, which also lists every board loaded, where it came
from, and which one this machine matched. `BENCHBOB_BOARD=<id>` forces a board instead of
detecting one, to try its header view on a machine that isn't one (its pins then show no
live lines, since the chips don't match).

Pinouts come from vendor documentation or the kernel device tree, and every file cites
its sources in `sources` and in its header comment. A board nobody has run BenchBob on
says `tested = false`; the API and dashboard label it.

## Format

```toml
# Header comment: what the board is, and where every table below came from.

id = "acme-x1"                 # letters, digits, - and _; unique
name = "Acme X1"               # shown when the device tree / DMI gives no name
vendor = "Acme"
soc = "Rockchip RK3566"        # optional
tested = false                 # has BenchBob run on one?
health = "sysfs"               # "sysfs" | "vcgencmd" | { script = "/path" } (below)
sources = ["https://…"]        # pinout and config sources, shown in the API
notes = ["…"]                  # caveats shown with the board (optional)

[detect]                       # how the board is recognised
priority = 50                  # score when it matches; the highest score wins
compatible = ['^acme,x1$']     # regexes against each /proc/device-tree/compatible string
# model = ['…']                # regexes against /proc/device-tree/model
# arch = ["x86_64", "x86"]     # the architecture BenchBob was built for
# dmi = { sys_vendor = ['…'], product_name = ['…'], board_vendor = ['…'], board_name = ['…'] }
# wsl = true
# always = true                # match every machine (the generic fallback only)

[[refine]]                     # optional, any number: per-model tested flag, name, SoC
when = { compatible = ['^acme,x1-v2$'] }
tested = true
name = "{model} (v2)"          # {model}: device-tree model; {dmi}: DMI vendor + product
soc = "Rockchip RK3566T"

[gpio]
panel = "gpiochip"             # "gpiochip" (every board) | "rpi" (the Raspberry Pi path)
reader = "gpiochip"            # passive level reader (below); or a list of
                               # { when = {…}, use = "…" } tried in order

[chips]                        # header chip keys → gpiochip uAPI label or sysfs device
gpio0 = ["gpio0", "fdd60000.gpio"]
gpio3 = ["gpio3", "fe760000.gpio"]

[[header]]                     # any number of headers
id = "j1"
name = "10-pin header"
pins = [
  { pin = 1, kind = "3v3" },   # kind: 3v3 | 5v | gnd | gpio | other | nc
  { pin = 2, kind = "gnd" },
  { pin = 3, soc = "GPIO3_C6", chip = "gpio3", line = 22, funcs = ["UART7_TX"] },
  { pin = 4, kind = "other", label = "SARADC_IN0" },
  # …
]

[boot]                         # optional: where boot-time overlays live
files = [                      # tried in order; the first that exists is used
  { kind = "armbianEnv", path = "/boot/armbianEnv.txt" },
  { kind = "orangepiEnv", path = "/boot/orangepiEnv.txt" },
]
overlay_dirs = ["/boot/dtb/rockchip/overlay"]   # installed .dtbo files
# overlay_ref = "/overlays/"   # extlinux/uEnv: how a .dtbo is written in the file
overlays = [                   # documented header overlays, for pin-conflict checks
  { name = "uart7-m2", what = "UART7 (TX 3)", pins = [3] },
]
```

A pin with no `kind` is a GPIO. A Raspberry Pi pin gives `gpio = N` (its BCM number);
every other board gives `chip` (a key of `[chips]`) and `line` (the offset on that
gpiochip). Chips are matched by their uAPI label or the name of their parent device in
sysfs, because `gpiochipN` numbers depend on probe order.

Detection: every condition given in `[detect]` must hold, any pattern in a list may
match. `refine` rules are checked in order; for each of `tested`, `name` and `soc` the
first rule whose `when` matches and that sets it wins. A name template whose placeholders
are all empty yields nothing, and the next rule is tried; failing all of them the name is
the device-tree model, then `name`.

### Level readers (`[gpio] reader`)

| Name | What it does |
| --- | --- |
| `gpiochip` | Generic: levels only for lines the user watches, read through as-is v2 uAPI line requests (nothing about the pin changes). |
| `bcm2711-gpiomem` | GPLEV0/1 through `/dev/gpiomem` on BCM2835/6/7 and BCM2711. No root needed (group `gpio`). |
| `rp1` | The Pi 5's RP1: SYS_RIO SYNC_IN through `/dev/gpiomem0`, unknown while a pad's input is disabled. **Untested: no Pi 5 here.** |
| `pinctrl` | Run `pinctrl get` every tick (the Pi fallback). |

A reader that can't be opened falls back to pinctrl (Pi) or watched-line reads.

### Boot-config backends (`[boot] files` kinds)

| Kind | File | Edited |
| --- | --- | --- |
| `config.txt` | Raspberry Pi firmware config | dtparam / dtoverlay lines in the `[all]` context |
| `armbianEnv` | Armbian `/boot/armbianEnv.txt` | the last `overlays=` line |
| `orangepiEnv` | Orange Pi `/boot/orangepiEnv.txt` | the same format |
| `uEnv` | BeagleBoard.org AM335x `/boot/uEnv.txt` | `uboot_overlay_addr0..7=` slots |
| `extlinux` | `extlinux.conf` | the default label's `fdtoverlays` line |
| `script` | an executable of yours | anything: see the contract below |

No `[boot]` table (or no file that exists) means none: the panel shows the overlays as
read-only, with the board's notes. Every built-in edit is staged as a unified diff,
written only after a confirm, backed up first (`<file>.bak-<UTC timestamp>`), and takes
effect after a reboot.

### Health (`health`)

`"sysfs"`: thermal zones, hwmon and power supplies, on any Linux box. `"vcgencmd"`: the
same plus the Raspberry Pi firmware's throttling flags and PMIC (`status.power`), and the
SoC temperature from thermal_zone0. `{ script = "/path" }`: a script (below).

## Script contract

Scripts are the escape hatch for what the built-ins don't cover: a vendor overlay tool,
a merged DTB, a board-specific sensor. They are run as the BenchBob user (use sudo rules
of your own if they need root), with JSON on stdout. A non-zero exit, or a JSON object
with an `"error"` string, is a refusal; its message (stderr, or `error`) is shown to the
user. Environment: `BENCHBOB_BOARD` (the board id), `BENCHBOB_OVERLAY_DIRS` (the board
file's `overlay_dirs`, `:`-separated), `BENCHBOB_STATE_DIR`.

### Boot-config script (`kind = "script"`)

| Call | Prints | Notes |
| --- | --- | --- |
| `<script> read` | `{"enabled": ["name", …], "path"?: "/boot/…", "label"?: "…", "prefix"?: "…", "user"?: [...]}` | the overlays in effect now; `path` is what the UI shows |
| `<script> list` | `{"installed": [{"name": "…", "file"?: "…", "what"?: "…", "pins"?: [3, 5]}]}` | overlays that can be enabled; `what`/`pins` default to the board file's `overlays` entry with that name |
| `<script> stage '<change>'` | `{"enabled": [after], "diff"?: "…", "summary"?: ["…"], "warnings"?: ["…"]}` | **must not change anything**; `enabled` is the list after the change |
| `<script> apply '<change>'` | `{"ok": true, "message"?: "…", "backup"?: "/path", "rebootRequired"?: true}` | make the change (and keep your own backup) |

`<change>` is one argument, `{"enable": ["…"], "disable": ["…"]}`. BenchBob refuses
overlays that `list` doesn't offer and changes that put two documented overlays on one
pin before calling `stage`, and before `apply` it calls `read` again and refuses if the
enabled list changed since `stage`. Time limits: 30 s, `apply` 120 s.

### Health script (`health = { script = "…" }`)

`<script> read` prints the sensors in the status's shape, every list optional, plus two
optional extras:

```json
{
  "temps": [{ "source": "soc", "label": "CPU", "c": 48.5 }],
  "volts": [{ "source": "pmic", "label": "VDD_CORE", "v": 0.9 }],
  "fans": [{ "source": "fan0", "label": "case", "rpm": 1200 }],
  "supplies": [{ "name": "battery", "type": "Battery", "online": true, "status": "Charging", "capacity": 80, "volts": 4.1 }],
  "cpuTemp": 48.5,
  "power": { "anything": "shown as status.power" }
}
```

It is called at most every 5 s with a 3 s limit; when it fails, BenchBob falls back to the
sysfs readings.

## Worked example

A board BenchBob doesn't ship: say an "Acme X1" whose vendor image manages overlays with
its own `acme-overlay` tool. Two files, no rebuild.

`/etc/benchbob/boards.d/acme-x1.toml`:

```toml
# Acme X1 (RK3566). Pinout: Acme X1 user manual v1.2, table 4-1 (https://example.com/x1.pdf);
# compatible string and GPIO bank labels from the vendor kernel's rk3566-acme-x1.dts.

id = "acme-x1"
name = "Acme X1"
vendor = "Acme"
soc = "Rockchip RK3566"
tested = false
health = "sysfs"
sources = ["https://example.com/x1.pdf"]

[detect]
priority = 50
compatible = ['^acme,x1$']

[gpio]
panel = "gpiochip"
reader = "gpiochip"

[chips]
gpio3 = ["gpio3", "fe760000.gpio"]

[[header]]
id = "j1"
name = "6-pin header"
pins = [
  { pin = 1, kind = "3v3" },
  { pin = 2, kind = "gnd" },
  { pin = 3, soc = "GPIO3_C6", chip = "gpio3", line = 22, funcs = ["UART7_TX_M1"] },
  { pin = 4, soc = "GPIO3_C7", chip = "gpio3", line = 23, funcs = ["UART7_RX_M1"] },
  { pin = 5, soc = "GPIO3_D0", chip = "gpio3", line = 24 },
  { pin = 6, kind = "gnd" },
]

[boot]
files = [{ kind = "script", path = "/usr/local/lib/benchbob/acme-x1-boot" }]
overlays = [{ name = "uart7-m1", what = "UART7 (TX 3, RX 4)", pins = [3, 4] }]
```

`/usr/local/lib/benchbob/acme-x1-boot` (executable), wrapping the vendor tool:

```sh
#!/bin/sh
# BenchBob boot-config script for the Acme X1 (see backend/boards/README.md).
set -e
json_list() { printf '['; sep=; for x in "$@"; do printf '%s"%s"' "$sep" "$x"; sep=,; done; printf ']'; }
names() { python3 -c 'import json,sys; print(" ".join(json.loads(sys.argv[1]).get(sys.argv[2], [])))' "$1" "$2"; }
case "$1" in
  read)  printf '{"enabled":%s,"path":"acme-overlay"}\n' "$(json_list $(acme-overlay --enabled))" ;;
  list)  printf '{"installed":[%s]}\n' "$(acme-overlay --available | sed 's/.*/{"name":"&"}/' | paste -sd,)" ;;
  stage) after=$(acme-overlay --enabled --dry-run --enable "$(names "$2" enable)" --disable "$(names "$2" disable)")
         printf '{"enabled":%s}\n' "$(json_list $after)" ;;
  apply) sudo -n acme-overlay --enable "$(names "$2" enable)" --disable "$(names "$2" disable)" >&2
         printf '{"ok":true,"rebootRequired":true}\n' ;;
  *)     echo "usage: $0 read|list|stage <json>|apply <json>" >&2; exit 2 ;;
esac
```

Restart BenchBob (`sudo systemctl restart benchbob`) and check:

```sh
curl -s http://benchbob.local/api/boards | jq '.current, .errors'   # "acme-x1", []
curl -s http://benchbob.local/api/gpio | jq '.headers[0].pins[2]'    # pin 3 with its live line
curl -s http://benchbob.local/api/gpio/overlays | jq '.kind, .installed'
```

To try a board file on a machine that isn't one, run a side instance with
`BENCHBOB_BOARDS_D=<dir with the file> BENCHBOB_BOARD=acme-x1`.

## The shipped boards

Converted from the TypeScript board layer (`peripherals/boards/*.ts`), with their
citations: `rpi` (every 40-pin Raspberry Pi; tested on a Pi 4), `orangepi-5`, `orangepi-5b`,
`orangepi-5-plus`, `orangepi-5-other`, `orangepi-3b`, `orangepi-zero3`, `orangepi-zero2w`,
`rock-5b`, `rock-5b-plus`, `beagley-ai`, `beaglebone-ai64`, `le-potato`, `roc-rk3328-cc`,
`x86` and `generic`. The tests in `crates/boards/tests/boards.rs` hold every file to the
same checks the TypeScript tests did (detection fixtures, pins 1..N, unique lines, overlay
pins on GPIO pins, sources cited, untested boards marked).
