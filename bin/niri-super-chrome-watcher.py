#!/usr/bin/env python3
"""Watch Super (Left/Right Meta) without grabbing the device.

Holds do not steal the key from niri. Writes ~/.cache/niri-window-chrome/super
and notifies the window-chrome Quickshell instance via IPC.
"""
from __future__ import annotations

import fcntl
import glob
import os
import select
import struct
import subprocess
import time

EVENT = struct.Struct("llHHi")
EV_KEY = 1
KEY_LEFTMETA = 125
KEY_RIGHTMETA = 126
META = {KEY_LEFTMETA, KEY_RIGHTMETA}

CACHE = os.path.expanduser("~/.cache/niri-window-chrome")
STATE = os.path.join(CACHE, "super")
LOG = os.path.join(CACHE, "watcher.log")
QS_PATH = os.path.expanduser("~/.config/quickshell/window-chrome")
QS = os.path.expanduser("~/.local/bin/qs")
if not os.path.isfile(QS):
    QS = "/usr/bin/quickshell"


def _ioc_read(typ: str, nr: int, size: int) -> int:
    return (2 << 30) | (size << 16) | (ord(typ) << 8) | nr


def EVIOCGBIT(ev: int, length: int) -> int:
    return _ioc_read("E", 0x20 + ev, length)


def log(msg: str) -> None:
    os.makedirs(CACHE, exist_ok=True)
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}\n")


def device_has_meta(path: str) -> bool:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return False
    try:
        buf = bytearray(128)
        fcntl.ioctl(fd, EVIOCGBIT(EV_KEY, len(buf)), buf)
        def bit(k: int) -> bool:
            return bool(buf[k // 8] & (1 << (k % 8)))
        return bit(KEY_LEFTMETA) or bit(KEY_RIGHTMETA)
    except OSError:
        return False
    finally:
        os.close(fd)


def notify(pressed: bool) -> None:
    val = "1" if pressed else "0"
    os.makedirs(CACHE, exist_ok=True)
    tmp = STATE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(val + "\n")
    os.replace(tmp, STATE)
    for cmd in (
        [QS, "-p", QS_PATH, "ipc", "call", "chrome", "setSuper", val],
        [QS, "ipc", "-p", QS_PATH, "call", "chrome", "setSuper", val],
    ):
        try:
            subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            return
        except OSError:
            continue


def open_devices() -> dict[str, int]:
    out: dict[str, int] = {}
    for path in sorted(glob.glob("/dev/input/event*")):
        if not device_has_meta(path):
            continue
        try:
            out[path] = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        except OSError as exc:
            log(f"open {path}: {exc}")
    return out


def main() -> int:
    os.makedirs(CACHE, exist_ok=True)
    notify(False)
    fds = open_devices()
    if not fds:
        log("no Super-capable input devices")
        return 1
    log("watching " + ", ".join(fds))
    down: set[tuple[str, int]] = set()
    last = False
    try:
        while True:
            r, _, _ = select.select(list(fds.values()), [], [], 2.0)
            if not r:
                continue
            path_by_fd = {fd: p for p, fd in fds.items()}
            for fd in r:
                path = path_by_fd.get(fd, "")
                try:
                    data = os.read(fd, EVENT.size * 64)
                except BlockingIOError:
                    continue
                except OSError as exc:
                    log(f"read {path}: {exc}")
                    try:
                        os.close(fd)
                    except OSError:
                        pass
                    fds.pop(path, None)
                    continue
                off = 0
                while off + EVENT.size <= len(data):
                    _s, _u, typ, code, value = EVENT.unpack_from(data, off)
                    off += EVENT.size
                    if typ != EV_KEY or code not in META:
                        continue
                    key = (path, code)
                    if value == 0:
                        down.discard(key)
                    elif value == 1:
                        down.add(key)
                    pressed = bool(down)
                    if pressed != last:
                        last = pressed
                        notify(pressed)
                        log(f"super {int(pressed)}")
    finally:
        for fd in fds.values():
            try:
                os.close(fd)
            except OSError:
                pass
        notify(False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
