#!/usr/bin/env python3
"""Synthetic multi-constellation GNSS receiver for testing the gps driver.

  nmea-feeder.py --log 20 > gps-multi.nmea   static log (fixed 2026-10-17 date) for gpsfake
  nmea-feeder.py --pty [--seconds 300]       create a pty, print its path, emit live 1 Hz NMEA

Emits per epoch: GNRMC, GNGGA, GNGSA x4 (GPS/GLONASS/Galileo/BeiDou, NMEA 4.10 system ids),
and GSV groups for each constellation. The position drifts slightly so TPVs change.
"""
import argparse, datetime, math, os, sys, time

SATS = {  # talker: [(prn, elevation, azimuth, snr, used)]
    "GP": [(2, 45, 120, 38, 1), (5, 30, 200, 35, 1), (12, 62, 45, 42, 1), (13, 15, 310, 22, 0),
           (15, 50, 280, 40, 1), (18, 8, 160, 0, 0), (20, 25, 75, 31, 1), (25, 70, 10, 44, 1), (29, 35, 240, 33, 1)],
    "GL": [(65, 40, 100, 34, 1), (66, 22, 150, 28, 1), (72, 55, 330, 37, 1), (81, 12, 210, 19, 0)],
    "GA": [(3, 48, 60, 36, 1), (8, 33, 260, 32, 1), (13, 18, 120, 24, 0)],
    "GB": [(19, 52, 190, 35, 1), (20, 27, 300, 29, 1), (35, 10, 20, 0, 0)],
}
SYSID = {"GP": 1, "GL": 2, "GA": 3, "GB": 4}


def ck(body):
    c = 0
    for ch in body:
        c ^= ord(ch)
    return "$%s*%02X\r\n" % (body, c)


def latlon(v, pos, neg, width):
    a = abs(v)
    d = int(a)
    m = (a - d) * 60
    return ("%0*d%07.4f" % (width, d, m)), (pos if v >= 0 else neg)


def epoch(t, i):
    hms = t.strftime("%H%M%S") + ".00"
    dmy = t.strftime("%d%m%y")
    lat, ns = latlon(37.3861 + 0.00001 * math.sin(i / 10), "N", "S", 2)
    lon, ew = latlon(-122.0839 + 0.00001 * math.cos(i / 10), "E", "W", 3)
    used = sum(1 for s in SATS.values() for x in s if x[4])
    out = [
        ck("GNRMC,%s,A,%s,%s,%s,%s,0.12,87.5,%s,,,A,V" % (hms, lat, ns, lon, ew, dmy)),
        ck("GNGGA,%s,%s,%s,%s,%s,1,%02d,0.9,32.5,M,-25.7,M,," % (hms, lat, ns, lon, ew, used)),
    ]
    for talker, sats in SATS.items():
        ids = [("%02d" % x[0]) for x in sats if x[4]][:12]
        ids += [""] * (12 - len(ids))
        out.append(ck("GNGSA,A,3,%s,1.6,0.9,1.3,%d" % (",".join(ids), SYSID[talker])))
    for talker, sats in SATS.items():
        n = (len(sats) + 3) // 4
        for k in range(n):
            chunk = sats[k * 4:(k + 1) * 4]
            f = ",".join("%02d,%02d,%03d,%s" % (p, e, a, ("%02d" % s) if s else "") for p, e, a, s, _ in chunk)
            out.append(ck("%sGSV,%d,%d,%02d,%s" % (talker, n, k + 1, len(sats), f)))
    return "".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=int)
    ap.add_argument("--pty", action="store_true")
    ap.add_argument("--seconds", type=int, default=300)
    a = ap.parse_args()
    if a.log:
        t0 = datetime.datetime(2026, 10, 17, 8, 30, 0)
        for i in range(a.log):
            sys.stdout.write(epoch(t0 + datetime.timedelta(seconds=i), i))
        return
    if a.pty:
        import tty
        master, slave = os.openpty()
        tty.setraw(slave)
        os.set_blocking(master, False)  # nobody reading the slave yet must not stall us
        print(os.ttyname(slave), flush=True)
        # Close our copy of the slave so the device looks free, as a real receiver's tty is
        # before anything opens it (the gps driver refuses ttys another process holds).
        os.close(slave)
        end = time.time() + a.seconds
        i = 0
        while time.time() < end:
            # Emit just after the top of each second, as a real receiver does.
            time.sleep(1 - (time.time() % 1) + 0.05)
            try:
                os.write(master, epoch(datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None), i).encode())
            except OSError:
                pass
            i += 1


if __name__ == "__main__":
    main()
