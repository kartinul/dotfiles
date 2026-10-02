#!/usr/bin/env python3
"""Minimal rumble for VID 0x2563 / PID 0x0575. Usage: python3 rumble.py [a] [b] [seconds]"""
import sys, time, hid

VID, PID = 0x2563, 0x0575

def rumble(dev, a=255, b=255):
    pkt = [0x02] + [0] * 7
    pkt[2] = a
    pkt[3] = b
    dev.write(bytes(pkt))

if __name__ == "__main__":
    a = int(sys.argv[1]) if len(sys.argv) > 1 else 255
    b = int(sys.argv[2]) if len(sys.argv) > 2 else 255
    secs = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0

    dev = hid.device()
    dev.open(VID, PID)
    try:
        rumble(dev, a, b)
        time.sleep(secs)
    finally:
        rumble(dev, 0, 0)
        dev.close()

