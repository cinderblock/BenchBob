#!/usr/bin/env python3
"""A stand-in for `openocd` in the probes driver's tests.

Reads the ports from the -c arguments the driver passes, prints what OpenOCD 0.12 prints on
stderr (the BENCHBOB-READY echo, "Listening on port … for tcl connections"), and answers
the TCL RPC (0x1a-terminated, the driver's catch/capture wrapper) for the commands the
driver sends. Exits on `shutdown` or SIGTERM. FAKE_OPENOCD_FAIL=1 makes init fail the way
a missing probe does.
"""
import os
import re
import socket
import sys

args = sys.argv[1:]
if args == ["--version"]:
    sys.stderr.write("Open On-Chip Debugger 0.12.0+dev-snapshot (2026-02-16-16:07)\nLicensed under GNU GPL v2\n")
    sys.exit(0)
cmds = [args[i + 1] for i in range(len(args) - 1) if args[i] == "-c"]
port = lambda name: next((c.split()[-1] for c in cmds if c.startswith(name + " port ")), "disabled")
tcl = int(port("tcl"))


def err(line):
    sys.stderr.write(line + "\n")
    sys.stderr.flush()


err("Open On-Chip Debugger 0.12.0+dev-snapshot (2026-02-16-16:07)")
if os.environ.get("FAKE_OPENOCD_FAIL"):
    err("Error: unable to find a matching CMSIS-DAP device")
    sys.exit(1)
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", tcl))
srv.listen(4)
err("Info : Target voltage: 3.301")
err("Info : [chip.cpu] Cortex-M4 r0p1 processor detected")
err("Info : flash size = 64 KiB")
if port("gdb") != "disabled":
    err("Info : Listening on port %s for gdb connections" % port("gdb"))
print("BENCHBOB-READY", flush=True)
err("Info : Listening on port %d for tcl connections" % tcl)

WRAP = re.compile(r"catch \{(capture \{)?(.*?)\}?\} __r", re.S)


def answer(inner):
    if inner == "targets":
        return "@@OK@@    TargetName         Type       Endian TapName            State\n--  ------------------ ---------- ------ ------------------ ------------\n 0* chip.cpu           cortex_m   little chip.cpu           halted"
    if inner.startswith("read_memory"):
        n = int(inner.split()[-1])
        return "@@OK@@" + " ".join("0x%x" % ((0x41 + i) % 256) for i in range(n))
    if inner.startswith("program "):
        return "@@OK@@** Programming Started **\n** Programming Finished **\n** Verified OK **\n** Resetting Target **"
    if inner == "flash list":
        return "@@OK@@{name stm32f1x base 134217728 size 65536 bus_width 0 chip_width 0}"
    if inner in ("stm32f1x mass_erase 0", "reset halt", "reset run", "reset init", "halt", "resume"):
        return "@@OK@@"
    return '@@ERR@@invalid command name "%s"' % inner.split()[0]


while True:
    c, _ = srv.accept()
    buf = b""
    while True:
        d = c.recv(4096)
        if not d:
            break
        buf += d
        while b"\x1a" in buf:
            msg, buf = buf.split(b"\x1a", 1)
            text = msg.decode()
            m = WRAP.search(text)
            inner = m.group(2) if m else text
            if inner == "shutdown":
                c.sendall(b"@@OK@@shutdown command invoked\x1a")
                err("shutdown command invoked")
                sys.exit(0)
            c.sendall(answer(inner).encode() + b"\x1a")
