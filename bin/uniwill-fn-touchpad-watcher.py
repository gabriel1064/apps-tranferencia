#!/usr/bin/env python3
"""Watch AT keyboard for Uniwill Fn+F5 (LeftMeta+LeftCtrl+F24) → touchpad toggle."""
from __future__ import annotations
import os, struct, subprocess, time

EVENT = struct.Struct("llHHi")
EV_KEY = 1
KEY_LEFTCTRL, KEY_LEFTMETA, KEY_F24 = 29, 125, 194
TOGGLE = os.path.expanduser("~/.local/bin/uniwill-touchpad-toggle.py")
LOG = os.path.expanduser("~/.cache/waybar-niri/touchpad-fn-watcher.log")

def log(msg: str) -> None:
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}\n")

def find_keyboard() -> str:
    try:
        with open("/proc/bus/input/devices", encoding="utf-8") as fh:
            block = []
            for line in fh:
                if line.strip() == "":
                    name = next((l[3:] for l in block if l.startswith("N: ")), "")
                    handlers = next((l[3:] for l in block if l.startswith("H: ")), "")
                    if "AT Translated Set 2 keyboard" in name:
                        for part in handlers.split():
                            if part.startswith("event"):
                                return f"/dev/input/{part}"
                    block = []
                else:
                    block.append(line.rstrip("\n"))
    except OSError:
        pass
    return "/dev/input/event4"

def main() -> int:
    path = find_keyboard()
    log(f"watching {path}")
    fd = os.open(path, os.O_RDONLY)
    down: set[int] = set()
    last = 0.0
    buf = b""
    try:
        while True:
            chunk = os.read(fd, EVENT.size * 8)
            if not chunk:
                time.sleep(0.05)
                continue
            buf += chunk
            while len(buf) >= EVENT.size:
                data, buf = buf[: EVENT.size], buf[EVENT.size :]
                _s, _u, typ, code, value = EVENT.unpack(data)
                if typ != EV_KEY:
                    continue
                if value == 1:
                    down.add(code)
                    if code in (KEY_F24, KEY_LEFTMETA, KEY_LEFTCTRL):
                        log(f"down {code} set={sorted(down)}")
                    if (
                        code == KEY_F24
                        and KEY_LEFTMETA in down
                        and KEY_LEFTCTRL in down
                    ):
                        now = time.monotonic()
                        if now - last < 0.35:
                            continue
                        last = now
                        log("chord → toggle")
                        subprocess.Popen(
                            ["/usr/bin/python3", TOGGLE],
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            start_new_session=True,
                        )
                elif value == 0:
                    down.discard(code)
                # value==2 key repeat: ignore
    finally:
        os.close(fd)

if __name__ == "__main__":
    raise SystemExit(main())
