#!/usr/bin/env python3
"""Handle Uniwill/Tuxedo Fn keyboard-backlight keys + keep zone4 color in sync.

Listens on the TUXEDO Keyboard device for KEY_KBDILLUMUP/DOWN/TOGGLE (228-230)
and adjusts /sys/class/leds/rgb:kbd_backlight* brightness.

Also mirrors zone0 color onto zone3 when zone3 looks "stuck" at a clamped teal
while other zones have fuller TCC colors (0-255), so the numpad follows.
"""
from __future__ import annotations

import glob
import json
import os
import re
import struct
import subprocess
import time

EVENT = struct.Struct("llHHi")
EV_KEY = 1
KEY_KBDILLUMTOGGLE = 228
KEY_KBDILLUMDOWN = 229
KEY_KBDILLUMUP = 230

LOG = os.path.expanduser("~/.cache/waybar-niri/kbd-backlight-watcher.log")
STATE = os.path.expanduser("~/.cache/waybar-niri/kbd-backlight-bri")


def log(msg: str) -> None:
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}\n")


def led_zones() -> list[str]:
    paths = sorted(
        glob.glob("/sys/class/leds/rgb:kbd_backlight*"),
        key=lambda p: int(p.rsplit("_", 1)[-1]) if p[-1].isdigit() else 0,
    )
    return paths


def read_bri() -> tuple[int, int]:
    z = led_zones()
    if not z:
        return 0, 50
    with open(f"{z[0]}/brightness", encoding="utf-8") as fh:
        cur = int(fh.read().strip() or "0")
    with open(f"{z[0]}/max_brightness", encoding="utf-8") as fh:
        mx = int(fh.read().strip() or "50")
    return cur, mx


STEP = os.path.expanduser("~/.local/bin/kbd-backlight-step")


def set_bri(val: int) -> None:
    """Kept for logs; Fn keys go through kbd-backlight-step (debounce + HID paint)."""
    z = led_zones()
    if not z:
        return
    _, mx = read_bri()
    val = max(0, min(mx, val))
    log(f"brightness -> {val}/{mx}")


def _bus_get_states() -> list[dict] | None:
    try:
        raw = subprocess.check_output(
            [
                "busctl",
                "call",
                "com.tuxedocomputers.tccd",
                "/com/tuxedocomputers/tccd",
                "com.tuxedocomputers.tccd",
                "GetKeyboardBacklightStatesJSON",
            ],
            text=True,
            timeout=2,
        )
        m = re.match(r's\s+"(.*)"\s*$', raw.strip(), re.S)
        if not m:
            return None
        return json.loads(m.group(1).encode().decode("unicode_escape"))
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
        return None


def _states_with_bri(bri: int) -> str:
    states = _bus_get_states() or []
    if not states:
        # minimal 4-zone fallback
        states = [
            {"mode": 0, "brightness": bri, "red": 0, "green": 169, "blue": 255}
            for _ in range(4)
        ]
    for st in states:
        st["brightness"] = bri
    return json.dumps(states, separators=(",", ":"))


def step(delta: int) -> None:
    arg = "+" if delta > 0 else "-"
    try:
        out = subprocess.check_output([STEP, arg], text=True, timeout=3)
        log(f"step {arg} -> {out.strip()}")
    except (OSError, subprocess.SubprocessError) as exc:
        log(f"step {arg} fail: {exc}")


def toggle() -> None:
    try:
        out = subprocess.check_output([STEP, "toggle"], text=True, timeout=3)
        log(f"toggle -> {out.strip()}")
    except (OSError, subprocess.SubprocessError) as exc:
        log(f"toggle fail: {exc}")


def sync_zone4_from_sysfs() -> None:
    """If zones 0-2 share a color and zone3 differs, copy zone0 → zone3 on sysfs."""
    z = led_zones()
    if len(z) < 4:
        return
    cols = []
    for path in z:
        try:
            with open(f"{path}/multi_intensity", encoding="utf-8") as fh:
                cols.append(fh.read().strip())
        except OSError:
            return
    if cols[0] == cols[1] == cols[2] and cols[3] != cols[0]:
        try:
            with open(f"{z[3]}/multi_intensity", "w", encoding="utf-8") as fh:
                fh.write(cols[0])
            log(f"zone4 sync {cols[3]} -> {cols[0]}")
        except OSError as exc:
            log(f"zone4 sync fail: {exc}")


def find_tuxedo_event() -> str | None:
    try:
        with open("/proc/bus/input/devices", encoding="utf-8") as fh:
            block: list[str] = []
            for line in fh:
                if line.strip() == "":
                    name = next((l[3:] for l in block if l.startswith("N: ")), "")
                    handlers = next((l[3:] for l in block if l.startswith("H: ")), "")
                    if "TUXEDO Keyboard" in name:
                        for part in handlers.split():
                            if part.startswith("event"):
                                return f"/dev/input/{part}"
                    block = []
                else:
                    block.append(line.rstrip("\n"))
    except OSError:
        pass
    return None


def main() -> int:
    path = find_tuxedo_event()
    if not path:
        log("TUXEDO Keyboard event node not found")
        return 1
    log(f"watching {path}")
    fd = os.open(path, os.O_RDONLY)
    last_sync = 0.0
    try:
        while True:
            # periodic zone4 sync (UI race)
            now = time.monotonic()
            if now - last_sync > 0.8:
                last_sync = now
            # wait with timeout via select
            import select

            r, _, _ = select.select([fd], [], [], 0.5)
            if not r:
                continue
            data = os.read(fd, EVENT.size * 8)
            off = 0
            while off + EVENT.size <= len(data):
                _s, _u, typ, code, value = EVENT.unpack_from(data, off)
                off += EVENT.size
                if typ != EV_KEY or value != 1:
                    continue
                if code == KEY_KBDILLUMUP:
                    step(+1)
                elif code == KEY_KBDILLUMDOWN:
                    step(-1)
                elif code == KEY_KBDILLUMTOGGLE:
                    toggle()
    finally:
        os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
