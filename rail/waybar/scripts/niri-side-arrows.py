#!/usr/bin/env python3
"""Edge fillets: hover left/right screen edge to reveal a full-height clickable strip."""
from __future__ import annotations

import fcntl
import json
import math
import os
import signal
import socket
import subprocess
import sys
import threading
import time

_LAYER_SO = "/usr/lib/libgtk4-layer-shell.so"
if _LAYER_SO not in os.environ.get("LD_PRELOAD", ""):
    env = os.environ.copy()
    prev = env.get("LD_PRELOAD")
    env["LD_PRELOAD"] = _LAYER_SO if not prev else f"{_LAYER_SO}:{prev}"
    os.execvpe(sys.executable, [sys.executable, *sys.argv], env)

import cairo
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkWayland", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")

from gi.repository import Gdk, GdkWayland, GLib, Gtk, Gtk4LayerShell

LOCK_PATH = os.path.expanduser("~/.cache/waybar-niri/side-arrows.lock")

# Overlay covers the side gap (struts/gaps) so the hotzone can sit just
# outside the tiling window, not over its content.
WIN_W = 160
HOT_W = 8.0
PEEK_PX = 36.0

# Same as niri-rail.py `_parse_color("ghost")` (inactive strip segments).
_GHOST = (160 / 255, 168 / 255, 180 / 255, 0.28)
_GHOST_HOVER = (160 / 255, 168 / 255, 180 / 255, 0.40)
# Window area above the bottom rail strut.
BOTTOM_RESERVE = 72


def _edge_tab(
    cr: cairo.Context, x: float, y: float, w: float, h: float, r: float, side: str
) -> None:
    """Flush to the screen edge; rounded only on the inward side."""
    r = max(0.0, min(r, w * 0.5, h * 0.5))
    cr.new_sub_path()
    if side == "left":
        cr.move_to(x, y)
        cr.line_to(x + w - r, y)
        cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
        cr.line_to(x + w, y + h - r)
        cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
        cr.line_to(x, y + h)
        cr.close_path()
    else:
        cr.move_to(x + w, y)
        cr.line_to(x + r, y)
        cr.arc_negative(x + r, y + r, r, -math.pi / 2, -math.pi)
        cr.line_to(x, y + h - r)
        cr.arc_negative(x + r, y + h - r, r, math.pi, math.pi / 2)
        cr.line_to(x + w, y + h)
        cr.close_path()


def _edge_tab_stroke(
    cr: cairo.Context, x: float, y: float, w: float, h: float, r: float, side: str
) -> None:
    """Stroke top + inward + bottom — never the screen edge."""
    r = max(0.0, min(r, w * 0.5, h * 0.5))
    cr.new_path()
    if side == "left":
        cr.move_to(x, y)
        cr.line_to(x + w - r, y)
        cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
        cr.line_to(x + w, y + h - r)
        cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
        cr.line_to(x, y + h)
    else:
        cr.move_to(x + w, y)
        cr.line_to(x + r, y)
        cr.arc_negative(x + r, y + r, r, -math.pi / 2, -math.pi)
        cr.line_to(x, y + h - r)
        cr.arc_negative(x + r, y + h - r, r, math.pi, math.pi / 2)
        cr.line_to(x + w, y + h)
    cr.stroke()


def _layer_shadow(
    cr: cairo.Context,
    x: float,
    y: float,
    w: float,
    h: float,
    r: float,
    *,
    side: str,
) -> None:
    """Inward-biased shadow so the glued strip still reads as a raised layer."""
    if w <= 0.5 or h <= 0.5:
        return
    cr.save()
    layers = 8
    spread = 11.0
    for i in range(layers, 0, -1):
        t = i / layers
        grow = spread * t
        alpha = 0.13 * math.exp(-((t * 1.4) ** 2))
        if side == "left":
            sx, sw = x, w + grow
        else:
            sx, sw = x - grow, w + grow
        _edge_tab(
            cr,
            sx,
            y - grow * 0.2,
            sw,
            h + grow * 0.4,
            r + grow * 0.25,
            side,
        )
        cr.set_source_rgba(0, 0, 0, alpha)
        cr.fill()
    cr.restore()


def _claim_lock() -> int:
    os.makedirs(os.path.dirname(LOCK_PATH), exist_ok=True)
    fd = os.open(LOCK_PATH, os.O_CREAT | os.O_RDWR, 0o644)
    try:
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
        held = False
        for _ in range(30):
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                held = True
                break
            except OSError:
                time.sleep(0.05)
        if not held and old:
            try:
                os.kill(old, signal.SIGKILL)
            except OSError:
                pass
            time.sleep(0.05)
            fcntl.flock(fd, fcntl.LOCK_EX)
    os.ftruncate(fd, 0)
    os.lseek(fd, 0, os.SEEK_SET)
    os.write(fd, str(os.getpid()).encode())
    return fd


NIRI_BIN = os.path.expanduser("~/.local/bin/niri-live-reflow")
_VIEW_W_TTL = 2.0
# Skip neighbor poll while niri's horizontal-view-movement spring is in flight.
_PEEK_HOLD = 0.45

_view_w_cache: tuple[float, float] | None = None  # (width, monotonic)
_ipc_sock: socket.socket | None = None


def _niri_socket_path() -> str | None:
    path = os.environ.get("NIRI_SOCKET")
    if path and os.path.exists(path):
        return path
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    try:
        cands = sorted(
            (
                os.path.join(runtime, name)
                for name in os.listdir(runtime)
                if name.startswith("niri.") and name.endswith(".sock")
            ),
            key=lambda p: os.path.getmtime(p),
            reverse=True,
        )
    except OSError:
        return None
    return cands[0] if cands else None


def _ipc_connect() -> socket.socket | None:
    path = _niri_socket_path()
    if not path:
        return None
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(0.08)
    try:
        s.connect(path)
    except OSError:
        s.close()
        return None
    return s


def _scroll_view(delta: float) -> bool:
    """Apply one ScrollViewBy on this thread. `_applied` stays truthful."""
    global _ipc_sock
    if abs(delta) < 0.05:
        return True
    req = {"Action": {"ScrollViewBy": {"delta": float(delta)}}}
    payload = (json.dumps(req, separators=(",", ":")) + "\n").encode()
    for _ in range(2):
        try:
            if _ipc_sock is None:
                _ipc_sock = _ipc_connect()
                if _ipc_sock is None:
                    break
            _ipc_sock.sendall(payload)
            buf = b""
            while b"\n" not in buf:
                chunk = _ipc_sock.recv(4096)
                if not chunk:
                    raise ConnectionError("niri ipc closed")
                buf += chunk
            return True
        except OSError:
            if _ipc_sock is not None:
                try:
                    _ipc_sock.close()
                except OSError:
                    pass
                _ipc_sock = None
    return False


def _niri(*args: str) -> None:
    bin_ = NIRI_BIN if os.path.isfile(NIRI_BIN) else "niri"
    subprocess.Popen(
        [bin_, "msg", *args],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def _run_action(action: str) -> None:
    _niri("action", action)


def _view_width() -> float:
    global _view_w_cache
    now = time.monotonic()
    if _view_w_cache is not None and now - _view_w_cache[1] < _VIEW_W_TTL:
        return _view_w_cache[0]
    width = 1920.0
    try:
        raw = subprocess.check_output(
            ["niri", "msg", "--json", "outputs"],
            stderr=subprocess.DEVNULL,
            timeout=0.4,
        )
        outs = json.loads(raw)
        for out in outs.values():
            logical = out.get("logical") or {}
            if logical.get("width"):
                width = float(logical["width"])
                break
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, ValueError):
        pass
    _view_w_cache = (width, now)
    return width


def _windows() -> list[dict] | None:
    try:
        raw = subprocess.check_output(
            ["niri", "msg", "--json", "windows"],
            stderr=subprocess.DEVNULL,
            timeout=0.4,
        )
        data = json.loads(raw)
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, ValueError):
        return None
    return data if isinstance(data, list) else None


def _focused_column(wins: list[dict]) -> tuple[int, object] | None:
    for w in wins:
        if not w.get("is_focused"):
            continue
        pos = (w.get("layout") or {}).get("pos_in_scrolling_layout")
        if not pos:
            return None
        return int(pos[0]), w.get("workspace_id")
    return None


def _nearest_side_column(
    wins: list[dict], side: str, view_w: float
) -> list[dict] | None:
    found = _focused_column(wins)
    if found is None:
        return None
    focused_col, ws = found
    best_col: int | None = None
    for w in wins:
        if w.get("workspace_id") != ws:
            continue
        pos = (w.get("layout") or {}).get("pos_in_scrolling_layout")
        if not pos:
            continue
        col = int(pos[0])
        if side == "right" and col <= focused_col:
            continue
        if side == "left" and col >= focused_col:
            continue
        if best_col is None or abs(col - focused_col) < abs(best_col - focused_col):
            span = _tile_span(w)
            if span is not None:
                x, right = span
                if side == "right" and right <= view_w - 2.0:
                    continue
                if side == "left" and x >= 2.0:
                    continue
            best_col = col
    if best_col is None:
        return None
    tiles = []
    for w in wins:
        if w.get("workspace_id") != ws:
            continue
        pos = (w.get("layout") or {}).get("pos_in_scrolling_layout")
        if pos and int(pos[0]) == best_col:
            tiles.append(w)
    return tiles or None


def _column_fully_offscreen(tiles: list[dict], side: str, view_w: float) -> bool:
    """True only when the column is wholly outside the view, not cut at the edge."""
    saw = False
    for w in tiles:
        layout = w.get("layout") or {}
        pos = layout.get("tile_pos_in_workspace_view")
        size = layout.get("tile_size")
        if not pos or not size:
            continue
        saw = True
        x = float(pos[0])
        width = float(size[0])
        right = x + width
        if side == "right":
            # Any overlap with the view means a broken sliver is already clickable.
            if x < view_w - 1.0:
                return False
        else:
            if right > 1.0:
                return False
    return saw


def _neighbor_window_id(side: str) -> int | None:
    """Id of the nearest tiled window on the neighboring column."""
    wins = _windows()
    if not wins:
        return None
    tiles = _nearest_side_column(wins, side, _view_width())
    if not tiles:
        return None
    return int(tiles[0]["id"])


def _tile_span(w: dict) -> tuple[float, float] | None:
    layout = w.get("layout") or {}
    pos = layout.get("tile_pos_in_workspace_view")
    size = layout.get("tile_size")
    if not pos or not size:
        return None
    x = float(pos[0])
    return x, x + float(size[0])


def _edge_has_broken(wins: list[dict], side: str, ws: object, view_w: float) -> bool:
    """A column already cut by this screen edge."""
    for w in wins:
        if w.get("workspace_id") != ws:
            continue
        span = _tile_span(w)
        if span is None:
            continue
        x, right = span
        if side == "right" and x < view_w - 2.0 and right > view_w + 2.0:
            return True
        if side == "left" and x < -2.0 and right > 2.0:
            return True
    return False


def _has_neighbor(side: str) -> bool:
    """True when a column exists past this edge (off-screen or only a sliver)."""
    wins = _windows()
    if not wins:
        return False
    tiles = _nearest_side_column(wins, side, _view_width())
    return bool(tiles)


def _neighbor_needs_scroll(side: str) -> bool:
    """Peek-scroll only when the next column is wholly outside the view."""
    wins = _windows()
    if not wins:
        return False
    view_w = _view_width()
    tiles = _nearest_side_column(wins, side, view_w)
    if not tiles:
        return False
    return _column_fully_offscreen(tiles, side, view_w)


class EdgeFillet(Gtk.DrawingArea):
    def __init__(self, side: str, win: Gtk.Window) -> None:
        super().__init__()
        self.side = side  # "left" | "right"
        self.win = win
        self.hover = False
        self.neighbor = False
        self.need_scroll = False
        self.peeked = False
        self._navigated = False
        # Last commanded peek offset (+PEEK_PX / 0 / −PEEK_PX). Niri animates the rest.
        self._cmd = 0.0
        self._hold_until = 0.0
        self._leave_id = 0
        self._watch_id = 0
        self._last_input: tuple[int, int, int, int] | None = None
        # Frozen gap while peeked so the input region does not chase the animation.
        self._open_inset = 0.0
        self._inset_cache: tuple[float, float] = (0.0, 0.0)  # (px, monotonic)
        self.set_content_width(WIN_W)
        self.set_hexpand(True)
        self.set_vexpand(True)
        self.set_draw_func(self._draw, None)

        click = Gtk.GestureClick()
        click.set_button(1)
        click.connect("pressed", self._on_click)
        self.add_controller(click)

        motion = Gtk.EventControllerMotion()
        motion.connect("enter", self._on_enter)
        motion.connect("leave", self._on_leave)
        motion.connect("motion", self._on_motion)
        self.add_controller(motion)

    def _measure_inset(self) -> float:
        """Gap from this screen edge to the nearest visible tile (0 = flush)."""
        wins = _windows()
        if not wins:
            return 0.0
        found = _focused_column(wins)
        if found is None:
            return 0.0
        _col, ws = found
        view_w = _view_width()
        best: float | None = None
        for w in wins:
            if w.get("workspace_id") != ws:
                continue
            span = _tile_span(w)
            if span is None:
                continue
            x, right = span
            if right <= 1.0 or x >= view_w - 1.0:
                continue
            if self.side == "left":
                gap = max(0.0, x)
            else:
                gap = max(0.0, view_w - right)
            best = gap if best is None else min(best, gap)
        return 0.0 if best is None else best

    def _content_inset(self) -> float:
        # While open, never re-query IPC: the peek animation would move the
        # input region under the cursor and GTK fires leave → close → reopen.
        if self._region_open():
            return max(self._open_inset, PEEK_PX)
        now = time.monotonic()
        val, ts = self._inset_cache
        if now - ts < 0.15:
            return val
        val = self._measure_inset()
        self._inset_cache = (val, now)
        return val

    def _hit_span(self) -> float:
        # Idle: thin strip just outside the window.
        # Open: whole gap (window edge → screen edge), so returning through
        # the space before the next column still counts as inside.
        if self._region_open():
            return max(PEEK_PX, self._open_inset)
        return HOT_W

    def _input_rect(self, width: float, height: float) -> tuple[float, float, float, float]:
        """Input region in surface coords — never over the tiling window."""
        span = self._hit_span()
        open_ = self._region_open()
        if open_:
            # Full vão: screen edge → window. Close only when the cursor
            # leaves this gap onto the window (not while travelling it).
            w = min(float(width), max(span, PEEK_PX))
            if self.side == "left":
                return 0.0, 0.0, max(1.0, w), float(height)
            x = max(0.0, float(width) - w)
            return x, 0.0, max(1.0, float(width) - x), float(height)
        inset = self._content_inset()
        if self.side == "left":
            # Idle: strip against the window (end of gap).
            if inset < 0.5:
                x = 0.0
            else:
                x = max(0.0, inset - span)
            w = min(span, max(inset, span) - x) if inset >= 0.5 else span
            w = max(1.0, min(w, float(width) - x))
            return x, 0.0, w, float(height)
        # Right surface: local 0 = screen_w - width.
        if inset < 0.5:
            x = max(0.0, float(width) - span)
        else:
            x = max(0.0, float(width) - inset)
        w = min(span, float(width) - x)
        return x, 0.0, max(1.0, w), float(height)

    def _set_input(self, x: float, y: float, w: float, h: float) -> None:
        surface = self.win.get_surface()
        if surface is None:
            return
        if w <= 0.5 or h <= 0.5:
            key = (0, 0, 0, 0)
            region = cairo.Region()
        else:
            ix, iy = int(x), int(y)
            iw, ih = max(1, int(round(w))), max(1, int(h))
            key = (ix, iy, iw, ih)
            region = cairo.Region(cairo.RectangleInt(ix, iy, iw, ih))
        if key == self._last_input:
            return
        self._last_input = key
        GdkWayland.WaylandSurface.set_input_region(surface, region)

    def _region_open(self) -> bool:
        return self.peeked or self.hover or abs(self._cmd) > 0.5

    def _pointer_inside(self) -> bool:
        surface = self.win.get_surface()
        display = Gdk.Display.get_default()
        if surface is None or display is None:
            return False
        seat = display.get_default_seat()
        pointer = seat.get_pointer() if seat is not None else None
        if pointer is None:
            return False
        inside, x, y, _mask = surface.get_device_position(pointer)
        if not inside:
            return False
        width = float(surface.get_width())
        height = float(surface.get_height())
        rx, ry, rw, rh = self._input_rect(width, height)
        return rx - 1 <= x <= rx + rw + 1 and ry - 1 <= y <= ry + rh + 1

    def _peek_delta(self) -> float:
        return PEEK_PX if self.side == "right" else -PEEK_PX

    def _set_cmd(self, dest: float) -> None:
        """Send dest − last command. Niri springs; we never correct mid-flight."""
        delta = dest - self._cmd
        if abs(delta) < 0.5:
            return
        _scroll_view(delta)
        self._cmd = dest
        self.peeked = abs(dest) > 0.5
        self._hold_until = time.monotonic() + _PEEK_HOLD
        self.queue_draw()

    def _begin_peek(self) -> None:
        if not self.neighbor:
            return
        self._navigated = False
        self.hover = True
        if not self.need_scroll:
            return
        dest = self._peek_delta()
        if abs(self._cmd - dest) < 0.5:
            return
        if abs(self._open_inset) < 0.5:
            # Cover the gap that will exist after the peek (current inset + slide),
            # not just PEEK_PX — otherwise leave fires in the vão and the peek
            # snaps back before the cursor reaches the window.
            self._open_inset = min(WIN_W - 2.0, self._measure_inset() + PEEK_PX)
        self._set_cmd(dest)
        self._ensure_watch()

    def _end_peek(self) -> None:
        if self._navigated:
            return
        self.hover = False
        self._set_cmd(0.0)
        self._open_inset = 0.0
        self._inset_cache = (0.0, 0.0)
        self.queue_draw()

    def _consume_navigation(self) -> None:
        """Click committed: do not reverse the peek scroll."""
        self._navigated = True
        self.peeked = False
        self.hover = False
        self._cmd = 0.0
        self._hold_until = 0.0
        self._open_inset = 0.0
        if self._leave_id:
            GLib.source_remove(self._leave_id)
            self._leave_id = 0
        if self._watch_id:
            GLib.source_remove(self._watch_id)
            self._watch_id = 0
        self.queue_draw()

    def _ensure_watch(self) -> None:
        if self._watch_id:
            return
        self._watch_id = GLib.timeout_add(80, self._watch_pointer)

    def _watch_pointer(self) -> bool:
        if self._navigated:
            self._watch_id = 0
            return False
        if not self.peeked and abs(self._cmd) < 0.5:
            self._watch_id = 0
            return False
        if self._pointer_inside():
            return True
        self._end_peek()
        self._watch_id = 0
        return False

    def _on_enter(self, *_args) -> None:
        if not self.neighbor:
            return
        if self._leave_id:
            GLib.source_remove(self._leave_id)
            self._leave_id = 0
        self.set_cursor_from_name("pointer")
        self._begin_peek()

    def _on_motion(self, *_args) -> None:
        if not self.neighbor:
            return
        if self._leave_id:
            GLib.source_remove(self._leave_id)
            self._leave_id = 0
        if not self.hover:
            self._on_enter()

    def _on_leave(self, *_args) -> None:
        if self._navigated:
            self._navigated = False
            return
        if self._leave_id:
            GLib.source_remove(self._leave_id)
        self._leave_id = GLib.timeout_add(50, self._commit_leave)

    def _commit_leave(self) -> bool:
        self._leave_id = 0
        if self._navigated:
            return False
        if self._pointer_inside():
            return False
        self._end_peek()
        return False

    def _on_click(self, *_args) -> None:
        if not self.neighbor:
            return
        side = self.side
        wid = _neighbor_window_id(side)
        self._consume_navigation()
        if wid is None:
            return
        # This niri's focus-column-right keeps the camera still (view_pos
        # unchanged). focus-window pulls the next column onto the screen.
        bin_ = NIRI_BIN if os.path.isfile(NIRI_BIN) else "niri"

        def _go() -> None:
            subprocess.run(
                [bin_, "msg", "action", "focus-window", "--id", str(wid)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            if side != "right":
                subprocess.run(
                    [bin_, "msg", "action", "center-column"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )

        threading.Thread(target=_go, daemon=True).start()

    def _draw(self, _area, cr: cairo.Context, width: int, height: int, _data) -> None:
        cr.set_operator(cairo.OPERATOR_SOURCE)
        cr.set_source_rgba(0, 0, 0, 0)
        cr.paint()
        if height < 1 or not self.neighbor:
            self._set_input(0, 0, 0, 0)
            return
        span_x, span_y, span_w, span_h = self._input_rect(float(width), float(height))
        self._set_input(span_x, span_y, span_w, span_h)


class SideArrows(Gtk.Application):
    def __init__(self) -> None:
        super().__init__(application_id="dev.gabriel.niri-side-arrows")
        self._lock_fd = _claim_lock()
        self.wins: list[Gtk.Window] = []

    def do_activate(self) -> None:  # noqa: N802
        if self.wins:
            for win, _pad in self.wins:
                win.present()
            return

        css = Gtk.CssProvider()
        css.load_from_data(
            b"window.niri-side-arrow { background-color: transparent; }"
        )
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(),
            css,
            Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION,
        )

        for side, edge in (
            ("left", Gtk4LayerShell.Edge.LEFT),
            ("right", Gtk4LayerShell.Edge.RIGHT),
        ):
            win = Gtk.Window(application=self)
            win.set_decorated(False)
            win.set_default_size(WIN_W, 100)
            win.add_css_class("niri-side-arrow")
            pad = EdgeFillet(side, win)
            win.set_child(pad)

            Gtk4LayerShell.init_for_window(win)
            Gtk4LayerShell.set_layer(win, Gtk4LayerShell.Layer.OVERLAY)
            Gtk4LayerShell.set_namespace(win, f"niri-side-fillet-{side}")
            Gtk4LayerShell.set_keyboard_mode(win, Gtk4LayerShell.KeyboardMode.NONE)
            Gtk4LayerShell.set_exclusive_zone(win, 0)
            # Usable window height only (above the bottom rail strut).
            Gtk4LayerShell.set_anchor(win, edge, True)
            Gtk4LayerShell.set_margin(win, edge, 0)
            Gtk4LayerShell.set_anchor(win, Gtk4LayerShell.Edge.TOP, True)
            Gtk4LayerShell.set_anchor(win, Gtk4LayerShell.Edge.BOTTOM, True)
            Gtk4LayerShell.set_margin(win, Gtk4LayerShell.Edge.TOP, 0)
            Gtk4LayerShell.set_margin(win, Gtk4LayerShell.Edge.BOTTOM, BOTTOM_RESERVE)

            win.present()
            self.wins.append((win, pad))
            GLib.idle_add(pad.queue_draw)

        GLib.timeout_add(400, self._poll_neighbors)

    def _poll_neighbors(self) -> bool:
        for _win, pad in self.wins:
            # While hovering/peeking, don't re-evaluate — peek itself makes the
            # neighbor partially visible, which would false-negative and undo.
            if (
                pad.peeked
                or pad.hover
                or pad._navigated
                or abs(pad._cmd) > 0.5
                or time.monotonic() < pad._hold_until
            ):
                continue
            pad.neighbor = _has_neighbor(pad.side)
            pad.need_scroll = pad.neighbor and _neighbor_needs_scroll(pad.side)
            pad.queue_draw()
        return True


def main() -> int:
    app = SideArrows()
    return app.run(None)


if __name__ == "__main__":
    raise SystemExit(main())
