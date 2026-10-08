#!/usr/bin/env python3
"""Bridge Noctalia notification history → niri window urgency.

New unseen Noctalia notifications arm matching open windows with
``set-window-urgent`` (niri-rail buzzes those icons).

The niri fork only clears urgency on a click (window or rail), so this bridge:
- re-applies urgency to **unfocused** windows that still have pending notifs
- stops re-arming once a matching window gets focus (the urgency itself stays
  until the user clicks)

Startup ignores existing history so old Cursor "Done" entries do not spin.
"""
from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from typing import Any

HISTORY = os.path.expanduser("~/.local/state/noctalia/notification_history.json")
LOCK = os.path.expanduser("~/.cache/waybar-niri/noctalia-urgent.lock")

ALIASES: dict[str, tuple[str, ...]] = {
    "cursor": ("cursor",),
    "firefox": ("firefox",),
    "telegram": ("telegram",),
    "org.telegram.desktop": ("telegram",),
    "discord": ("discord",),
    "slack": ("slack",),
    "spotify": ("spotify",),
    "dolphin": ("dolphin",),
    "org.kde.dolphin": ("dolphin",),
    "kitty": ("kitty",),
    "thorium": ("thorium",),
    "thorium-browser": ("thorium",),
}


def _claim_lock() -> int:
    os.makedirs(os.path.dirname(LOCK), exist_ok=True)
    fd = os.open(LOCK, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        import fcntl

        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        os.lseek(fd, 0, os.SEEK_SET)
        try:
            old = int(os.read(fd, 32).decode() or "0")
        except ValueError:
            old = 0
        if old and old != os.getpid():
            try:
                os.kill(old, signal.SIGTERM)
            except OSError:
                pass
            time.sleep(0.15)
        import fcntl

        fcntl.flock(fd, fcntl.LOCK_EX)
    os.ftruncate(fd, 0)
    os.lseek(fd, 0, os.SEEK_SET)
    os.write(fd, str(os.getpid()).encode())
    return fd


def _norm(s: str | None) -> str:
    return (s or "").strip().lower()


def _load_history() -> dict[str, Any]:
    try:
        with open(HISTORY, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def _niri_windows() -> list[dict[str, Any]]:
    try:
        out = subprocess.check_output(
            ["niri", "msg", "--json", "windows"], text=True, timeout=3
        )
        data = json.loads(out)
        return data if isinstance(data, list) else []
    except (
        OSError,
        subprocess.CalledProcessError,
        json.JSONDecodeError,
        subprocess.TimeoutExpired,
    ):
        return []


def _match_keys(app_name: str, desktop_entry: str) -> set[str]:
    keys: set[str] = set()
    for raw in (app_name, desktop_entry):
        n = _norm(raw)
        if not n:
            continue
        keys.add(n)
        keys.add(n.split(".")[-1])
        for alias, needles in ALIASES.items():
            if n == alias or n in needles or n.endswith(alias):
                keys.update(needles)
                keys.add(alias)
    return {k for k in keys if k}


def _window_matches(app_id: str | None, keys: set[str]) -> bool:
    aid = _norm(app_id)
    if not aid or not keys:
        return False
    tail = aid.split(".")[-1]
    for k in keys:
        if k == aid or k == tail:
            return True
        if len(k) >= 3 and (k in aid or aid in k):
            return True
    return False


def _set_urgent(wid: int, urgent: bool) -> None:
    action = "set-window-urgent" if urgent else "unset-window-urgent"
    try:
        subprocess.Popen(
            ["niri", "msg", "action", action, "--id", str(wid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError:
        pass


def _entry_marker(entry: dict[str, Any]) -> int:
    try:
        serial = int(entry.get("event_serial") or 0)
    except (TypeError, ValueError):
        serial = 0
    notif = entry.get("notification") or {}
    try:
        nid = int(notif.get("id") or 0)
    except (TypeError, ValueError):
        nid = 0
    return max(serial, nid)


def _notif_id(entry: dict[str, Any]) -> int | None:
    notif = entry.get("notification") or {}
    try:
        return int(notif.get("id"))
    except (TypeError, ValueError):
        return None


def _unseen_ids(entries: list[dict[str, Any]]) -> set[int]:
    out: set[int] = set()
    for entry in entries:
        if entry.get("seen"):
            continue
        nid = _notif_id(entry)
        if nid is not None:
            out.add(nid)
    return out


def _pending_keys(pending: dict[str, set[int]]) -> set[str]:
    return set(pending.keys())


def _reconcile(pending: dict[str, set[int]], windows: list[dict[str, Any]]) -> None:
    """Ack on focus; keep urgency on unfocused windows with pending notifs."""
    keys = _pending_keys(pending)
    if not keys:
        return

    # Focusing a matching window = user saw it → drop pending for those keys.
    for win in windows:
        if not win.get("is_focused"):
            continue
        for key in list(pending):
            if _window_matches(win.get("app_id"), {key}):
                pending.pop(key, None)

    keys = _pending_keys(pending)
    if not keys:
        return

    for win in windows:
        if not _window_matches(win.get("app_id"), keys):
            continue
        if win.get("is_focused"):
            continue
        if win.get("is_urgent"):
            continue
        try:
            _set_urgent(int(win["id"]), True)
        except (TypeError, ValueError):
            pass


def main() -> int:
    _claim_lock()
    data = _load_history()
    entries = list(data.get("entries") or [])
    last_marker = max((_entry_marker(e) for e in entries), default=0)
    pending: dict[str, set[int]] = {}
    last_mtime = 0.0
    try:
        last_mtime = os.path.getmtime(HISTORY)
    except OSError:
        pass
    last_reconcile = 0.0

    while True:
        now = time.monotonic()
        changed = False
        try:
            mtime = os.path.getmtime(HISTORY)
        except OSError:
            mtime = last_mtime

        if mtime != last_mtime:
            time.sleep(0.05)
            try:
                last_mtime = os.path.getmtime(HISTORY)
            except OSError:
                pass
            changed = True

        if changed:
            data = _load_history()
            entries = list(data.get("entries") or [])
            unseen = _unseen_ids(entries)
            new_entries = [e for e in entries if _entry_marker(e) > last_marker]
            last_marker = max(
                last_marker, max((_entry_marker(e) for e in entries), default=0)
            )
            windows = _niri_windows()

            for entry in new_entries:
                if entry.get("seen"):
                    continue
                nid = _notif_id(entry)
                if nid is None:
                    continue
                notif = entry.get("notification") or {}
                keys = _match_keys(
                    str(notif.get("app_name") or ""),
                    str(notif.get("desktop_entry") or ""),
                )
                if not keys:
                    continue
                for key in keys:
                    pending.setdefault(key, set()).add(nid)
                # Arm now (no-op if focused; reconcile will retry when unfocused).
                for win in windows:
                    if win.get("is_focused"):
                        continue
                    if _window_matches(win.get("app_id"), keys):
                        try:
                            _set_urgent(int(win["id"]), True)
                        except (TypeError, ValueError):
                            pass

            for key, ids in list(pending.items()):
                ids.intersection_update(unseen)
                if not ids:
                    del pending[key]

            _reconcile(pending, windows)
            last_reconcile = now
        elif now - last_reconcile >= 0.25:
            windows = _niri_windows()
            # Drop pending ids that became seen without a file-event edge case.
            data = _load_history()
            unseen = _unseen_ids(list(data.get("entries") or []))
            for key, ids in list(pending.items()):
                ids.intersection_update(unseen)
                if not ids:
                    del pending[key]
            _reconcile(pending, windows)
            last_reconcile = now

        time.sleep(0.1)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(0)
