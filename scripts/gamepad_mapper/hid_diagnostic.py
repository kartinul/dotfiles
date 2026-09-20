#!/usr/bin/env python3
"""
Raw HID probe - bypasses pygame/SDL entirely, talks to the device directly.

Setup:
    pip3 install hidapi

If this also sees nothing, it's almost certainly the macOS Input Monitoring
permission blocking raw HID reads for whatever app runs this script
(Terminal/iTerm/VS Code - not Chrome, which is why the browser tester works).
"""

import hid
import os

env_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".env")
if os.path.exists(env_path):
    with open(env_path, "r") as env_file:
        for line in env_file:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            name, value = line.split("=", 1)
            os.environ.setdefault(name.strip(), value.strip().strip('"').strip("'"))

VID = int(os.environ.get("HID_VENDOR_ID", "0x2563"), 0)
PID = int(os.environ.get("HID_PRODUCT_ID", "0x0575"), 0)

print("All HID devices matching this vendor:")
found = False
for d in hid.enumerate(VID, PID):
    found = True
    print(d)

if not found:
    print("  (none - device not visible to hidapi at all)")
    print("Check Input Monitoring permission for this terminal app.")
    raise SystemExit(1)

print("\nOpening device...")
dev = hid.device()
try:
    dev.open(VID, PID)
except OSError as e:
    print(f"Failed to open: {e}")
    print("This is almost always Input Monitoring permission missing.")
    raise SystemExit(1)

dev.set_nonblocking(True)
print("Opened OK. Press buttons on the pad (Ctrl+C to stop)...\n")

try:
    while True:
        data = dev.read(64)
        if data:
            os.system("clear")
            print(data)
except KeyboardInterrupt:
    pass
finally:
    dev.close()
