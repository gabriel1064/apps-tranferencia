#!/usr/bin/env python3
"""Toggle Uniwill/TongFang touchpad via HID selective-reporting (firmware LED).

Same protocol as tuxedo-touchpad-switch (Microsoft Touchpad Configuration Collection).
Needs write access to the UNIW0001 hidraw (udev MODE=0622).

Usage:
  uniwill-touchpad-toggle.py          # toggle
  uniwill-touchpad-toggle.py on|off   # force
  uniwill-touchpad-toggle.py status
"""
from __future__ import annotations

import array
import fcntl
import glob
import os
import struct
import sys

STATE_PATH = os.path.expanduser("~/.cache/waybar-niri/touchpad-enabled")
# linux/hidraw.h
_HIDIOCGRDESCSIZE = 0x80044801
_HIDIOCGRDESC = 0x90044802
# HIDIOCSFEATURE(2) = _IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, 2)
_HIDIOCSFEATURE_2 = 0xC0024806

# Usage Page Digitizers / Surface Switch collection marker
_SURFACE_BTN = bytes([0x05, 0x0D, 0x09, 0x22, 0xA1, 0x00, 0x09, 0x57, 0x09, 0x58])


def _hidraw_nodes() -> list[str]:
    nodes: list[str] = []
    for path in glob.glob("/sys/class/hidraw/hidraw*/device"):
        try:
            real = os.path.realpath(path)
        except OSError:
            continue
        if "i2c-UNIW0001:00" not in real:
            continue
        name = os.path.basename(os.path.dirname(path))
        node = f"/dev/{name}"
        if os.path.exists(node):
            nodes.append(node)
    return nodes


def _report_id(fd: int) -> int | None:
    size = array.array("I", [0])
    try:
        fcntl.ioctl(fd, _HIDIOCGRDESCSIZE, size, True)
    except OSError as exc:
        print(f"HIDIOCGRDESCSIZE: {exc}", file=sys.stderr)
        return None
    # struct hidraw_report_descriptor { __u32 size; __u8 value[4096]; }
    desc = bytearray(4 + 4096)
    struct.pack_into("I", desc, 0, size[0])
    try:
        fcntl.ioctl(fd, _HIDIOCGRDESC, desc, True)
    except OSError as exc:
        print(f"HIDIOCGRDESC: {exc}", file=sys.stderr)
        return None
    n = struct.unpack_from("I", desc, 0)[0]
    blob = bytes(desc[4 : 4 + n])
    idx = blob.find(_SURFACE_BTN)
    if idx < 0:
        return None
    for i in range(idx, min(idx + 64, len(blob) - 1)):
        if blob[i] == 0x85:
            return blob[i + 1]
    return None


def _set_state(enabled: bool) -> bool:
    nodes = _hidraw_nodes()
    if not nodes:
        print("No Uniwill touchpad hidraw found.", file=sys.stderr)
        return False
    ok = False
    for node in nodes:
        try:
            # udev MODE=0622: others have write-only (tuxedo-touchpad-switch uses O_WRONLY)
            fd = os.open(node, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as exc:
            print(f"open {node}: {exc}", file=sys.stderr)
            continue
        try:
            rid = _report_id(fd)
            if rid is None:
                print(f"{node}: selective-reporting report id not found", file=sys.stderr)
                continue
            # 0x03 = LED off + pad on + clicks on; 0x00 = LED on + pad off
            buf = bytearray([rid, 0x03 if enabled else 0x00])
            try:
                fcntl.ioctl(fd, _HIDIOCSFEATURE_2, buf, True)
            except OSError as exc:
                print(f"HIDIOCSFEATURE {node}: {exc}", file=sys.stderr)
                continue
            ok = True
        finally:
            os.close(fd)
    if ok:
        os.makedirs(os.path.dirname(STATE_PATH), exist_ok=True)
        with open(STATE_PATH, "w", encoding="utf-8") as fh:
            fh.write("1" if enabled else "0")
    return ok


def _read_state() -> bool:
    try:
        with open(STATE_PATH, encoding="utf-8") as fh:
            return fh.read().strip() != "0"
    except OSError:
        return True


def main() -> int:
    arg = (sys.argv[1] if len(sys.argv) > 1 else "toggle").lower()
    if arg in ("status", "state"):
        print("on" if _read_state() else "off")
        return 0
    if arg in ("on", "1", "enable"):
        enabled = True
    elif arg in ("off", "0", "disable"):
        enabled = False
    else:
        enabled = not _read_state()
    if not _set_state(enabled):
        return 1
    print("Touchpad", "ON" if enabled else "OFF (LED)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
