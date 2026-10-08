#!/usr/bin/env python3
"""Viewport rail drawn on Niri's refresh clock.

WindowLayoutsChanged already carries the animated view position every frame
(the same number the compositor used to place the windows). This process
patches that into memory and repaints one surface. It does not ask `niri msg`
again, and it does not go through Waybar's per-widget class swap.
"""
from __future__ import annotations

import array
import fcntl
import glob
import importlib.util
import json
import math
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import time

# gtk4-layer-shell must be mapped before libwayland. Python loads wayland via
# GTK first, so re-exec once with the layer shell preloaded.
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
gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")
gi.require_version("GdkPixbuf", "2.0")

from gi.repository import Gdk, GdkPixbuf, GdkWayland, GLib, Gtk, Gtk4LayerShell, Pango, PangoCairo

# The fork's client: the stock `niri msg` drops fields it does not know (is_minimized, ...).
NIRI_BIN = shutil.which("niri-live-reflow") or os.path.expanduser("~/.local/bin/niri-live-reflow")

_SPEC = importlib.util.spec_from_file_location(
    "niri_layout",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "niri-layout.py"),
)
assert _SPEC and _SPEC.loader
layout = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(layout)

# Overall rail size (1.0 = original design).
SCALE = 0.82

# Rail px per niri viewport, relative to the waybar strip's ROW_PX.
RAIL_ZOOM = 0.75
ROW_PX = float(layout.ROW_PX) * SCALE * RAIL_ZOOM
MIN_SLOT_PX = float(layout.MIN_SLOT_PX) * SCALE
COMPACT_PX = float(layout.COMPACT_PX) * SCALE
COL_GAP_PX = float(layout.COL_GAP_PX) * SCALE
LOCK_PATH = os.path.expanduser("~/.cache/waybar-niri/rail.lock")

PAD_X = 8.0 * SCALE
# The rail is as wide as the niri workspace strip (1 viewport = ROW_PX),
# at least one viewport and at most this many. Apps start at the left;
# content only scrolls inside if it exceeds the cap.
RAIL_VIEWPORTS = 4.0
RAIL_MAX_W = RAIL_VIEWPORTS * ROW_PX
RAIL_MIN_W = ROW_PX
# Width follows the strip with an exponential ease (per second).
RAIL_W_RATE = 12.0
# Noctalia bar.default.thickness (and niri `struts { bottom }`).
HOST_H = 42
# Rail window gap above the screen's bottom edge (logical px).
RAIL_BOTTOM = 0
# Positive sinks content below the window (clips); negative lifts the rail.
RAIL_SINK = -2.0
PAD_T = 2.0 * SCALE
PAD_B = 0.6 * SCALE
ICON_H = 34.0 * SCALE
ICON_MAX = 30.0 * SCALE
GAP_Y = 3.0 * SCALE
STRIP_H = 7.0 * SCALE
BAR_H = 5.0 * SCALE
BAR_INSET = (STRIP_H - BAR_H) / 2.0
# View travel (rail px) per ratchet click.
TOOTH_PX = 5.0 * RAIL_ZOOM
PILL_H = PAD_T + ICON_H + GAP_Y + STRIP_H + PAD_B
# Logo colors as glass: keep hue, add sheen; slightly translucent.
GLASS_COLOR_ALPHA = 0.88
GLASS_SHEEN = 0.38
# Opened in the background, not clicked yet: one ease-in/out turn per period.
SPIN_PERIOD_S = 1.5
# niri flags a background-opened window urgent right at map; urgency that
# shows up this soon after the window first appears means "new", not a notif.
NEW_URGENT_GRACE_S = 1.5
# Notification (urgent) icons buzz in short bursts until clicked.
AWAIT_PERIOD_S = 1.1
AWAIT_BUZZ_S = 0.5
AWAIT_BUZZ_HZ = 11.0
AWAIT_SHAKE_PX = 2.2
AWAIT_SHAKE_RAD = 0.14

# Left cluster placement: right after Noctalia's launcher (logical px from the
# screen's left edge), and room kept free for Noctalia's end cluster.
RAIL_LEFT = 34
RIGHT_RESERVE = 560.0
# Workspace block: vertical stack of dashes, then the app pill.
WS_PAD_L = 8.0
DASH_W = 13.0
DASH_H = 2.6
WS_PAD_R = 6.0
WS_GAP = 6.0
# Bottom-strip viewport mark only (no tall fill behind tiling icons).
VIEWPORT_EDGE_RGBA = (0.93, 0.94, 0.96, 0.48)
VIEWPORT_EDGE_W = 1.25
# Tall clickable divider between tiled columns (gap handle).
GAP_LINE_RGBA = (1.0, 1.0, 1.0, 0.36)
GAP_HANDLE_RGBA = (0.96, 0.97, 0.98, 0.82)
GAP_HANDLE_R = 3.6
GAP_HIT_R = 8.0
GAP_HOVER_GROW = 0.32
# Separator between tiling cluster and floating chips.
TILE_FLOAT_SEP_RGBA = (1.0, 1.0, 1.0, 0.18)
TILE_FLOAT_SEP_W = 2.0
# Air on each side of the tiling↔floating divider (symmetric, modest).
TILE_FLOAT_PAD = 8.0 * SCALE
TILE_FLOAT_PAD_RIGHT = 8.0 * SCALE
# Vertical inset so the bar isn't flush with top/bottom of the visible rail.
TILE_FLOAT_SEP_VINSET = 5.0 * SCALE
# Minimized / floating chips after the pill.
CHIP_W = 34.0 * SCALE
CHIP_ICON = 28.0 * SCALE
CLUSTER_GAP = 10.0 * SCALE
MINIMIZED_ALPHA = 0.42
# niri's default workspace-switch spring (damping 1.0, stiffness 1000).
WS_SPRING_OMEGA = math.sqrt(1000.0)
WS_ANIM_S = 0.42

# Touchpad over the rail scrolls the view (the wheel is handled by niri).
WHEEL_STEP_PX = 120.0
TOUCHPAD_GAIN = 2.0
# Ratchet click per tooth passing a fixed point.
RATCHET_VOLUME = 0.35
RATCHET_MAX_HZ = 45.0
RATCHET_RATE = 48000


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


def _parse_color(value: str) -> tuple[float, float, float, float]:
    if not value or value == "transparent":
        return (0.0, 0.0, 0.0, 0.0)
    if value == "ghost":
        return (160 / 255, 168 / 255, 180 / 255, 0.28)
    text = value.strip()
    if text.startswith("#") and len(text) >= 7:
        r = int(text[1:3], 16) / 255
        g = int(text[3:5], 16) / 255
        b = int(text[5:7], 16) / 255
        return (r, g, b, 1.0)
    return (91 / 255, 141 / 255, 239 / 255, 1.0)


def _seg_palette(seg: dict) -> list[tuple[str, float]]:
    """[(#rrggbb, weight), ...] from state, or single color fallback."""
    raw = seg.get("palette") or []
    out: list[tuple[str, float]] = []
    for item in raw:
        if not isinstance(item, (list, tuple)) or len(item) < 2:
            continue
        color, weight = item[0], item[1]
        try:
            w = float(weight)
        except (TypeError, ValueError):
            continue
        if w <= 0 or not isinstance(color, str) or not color.startswith("#"):
            continue
        out.append((color, w))
    if out:
        total = sum(w for _, w in out) or 1.0
        return [(c, w / total) for c, w in out]
    color = str(seg.get("color") or "")
    if color.startswith("#"):
        return [(color, 1.0)]
    return []


def _fill_round_rect_palette(
    cr: cairo.Context,
    x: float,
    y: float,
    w: float,
    h: float,
    radius: float,
    palette: list[tuple[str, float]],
    alpha: float,
) -> None:
    """Paint logo colors as glassy gel (hue + vertical sheen), no outline."""
    if w <= 0 or h <= 0 or not palette:
        return
    stops = layout.palette_gradient_stops(palette)
    if not stops:
        return
    a = max(0.0, min(1.0, alpha)) * GLASS_COLOR_ALPHA
    cr.save()
    _round_rect(cr, x, y, w, h, radius)
    cr.clip()
    # Horizontal logo mix.
    grad = cairo.LinearGradient(x, y, x + w, y)
    for off, color in stops:
        r, g, b, _ = _parse_color(color)
        grad.add_color_stop_rgba(off, r, g, b, a)
    cr.set_source(grad)
    cr.paint()
    # Vertical glass sheen only — no stroke/rim (looks like a border).
    sheen = cairo.LinearGradient(x, y, x, y + h)
    sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, GLASS_SHEEN * a)
    sheen.add_color_stop_rgba(0.45, 1.0, 1.0, 1.0, 0.06 * a)
    sheen.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.12 * a)
    cr.set_source(sheen)
    cr.paint()
    cr.restore()


def _fill_glassy_solid(
    cr: cairo.Context,
    x: float,
    y: float,
    w: float,
    h: float,
    radius: float,
    color: str,
    alpha: float,
) -> None:
    """Single logo color with the same glass treatment."""
    _fill_round_rect_palette(cr, x, y, w, h, radius, [(color, 1.0)], alpha)


def _round_rect(cr: cairo.Context, x: float, y: float, w: float, h: float, r: float) -> None:
    if w <= 0 or h <= 0:
        return
    r = max(0.0, min(r, w / 2.0, h / 2.0))
    cr.new_sub_path()
    cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    cr.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    cr.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    cr.close_path()


def _desktop_icon_names() -> dict[str, str]:
    found: dict[str, str] = {}
    roots = (
        "/usr/share/applications",
        os.path.expanduser("~/.local/share/applications"),
    )
    for root in roots:
        for path in glob.glob(os.path.join(root, "*.desktop")):
            icon = None
            wm = None
            try:
                lines = open(path, encoding="utf-8", errors="replace")
            except OSError:
                continue
            with lines:
                for line in lines:
                    if line.startswith("Icon="):
                        icon = line.split("=", 1)[1].strip()
                    elif line.startswith("StartupWMClass="):
                        wm = line.split("=", 1)[1].strip()
            if not icon:
                continue
            stem = os.path.splitext(os.path.basename(path))[0]
            for key in (stem, stem.lower(), wm or "", (wm or "").lower()):
                if key:
                    found.setdefault(key, icon)
    return found


def _icon_file(app_id: str | None) -> str | None:
    if not app_id:
        return None
    cached = _ICON_FILES.get(app_id)
    if cached is not None:
        return cached or None
    names: list[str] = []
    desktop = _DESKTOP_ICONS.get(app_id) or _DESKTOP_ICONS.get(app_id.lower())
    if desktop:
        names.append(desktop)
    tail = app_id.split(".")[-1]
    names.extend((app_id, app_id.lower(), tail, tail.lower()))
    theme = Gtk.IconTheme.get_for_display(Gdk.Display.get_default())
    path = None
    seen: set[str] = set()
    for name in names:
        if not name or name in seen:
            continue
        seen.add(name)
        if os.path.isabs(name) and os.path.exists(name):
            path = name
            break
        if theme.has_icon(name):
            paintable = theme.lookup_icon(name, None, 128, 1, Gtk.TextDirection.NONE, 0)
            file = paintable.get_file()
            if file is not None and file.get_path():
                path = file.get_path()
                break
    _ICON_FILES[app_id] = path or ""
    return path


def _center_ink(src: cairo.ImageSurface) -> tuple[bytes, int, int]:
    """Scale the opaque pixels so the icon sits in the middle of a square."""
    src.flush()
    data = src.get_data()
    sw, sh = src.get_width(), src.get_height()
    stride = src.get_stride()
    minx, miny, maxx, maxy = sw, sh, -1, -1
    for y in range(sh):
        row = y * stride
        for x in range(sw):
            if data[row + x * 4 + 3] > 16:
                if x < minx:
                    minx = x
                if y < miny:
                    miny = y
                if x > maxx:
                    maxx = x
                if y > maxy:
                    maxy = y
    size = 160
    out = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    if maxx < minx:
        out.flush()
        return (bytes(out.get_data()), size, out.get_stride())
    bw = maxx - minx + 1
    bh = maxy - miny + 1
    inner = size * 0.78
    scale = min(inner / bw, inner / bh)
    dw, dh = bw * scale, bh * scale
    dx = (size - dw) / 2.0
    dy = (size - dh) / 2.0
    cr = cairo.Context(out)
    cr.translate(dx, dy)
    cr.scale(scale, scale)
    cr.translate(-minx, -miny)
    cr.set_source_surface(src, 0, 0)
    cr.paint()
    out.flush()
    return (bytes(out.get_data()), size, out.get_stride())


def _icon_tex(app_id: str | None, label: str) -> tuple[bytes, int, int]:
    """App icon in its own colors, ink-centered. Glyph fallback stays unpainted (white)."""
    key = (app_id or "", label)
    cached = _GLYPHS.get(key)
    if cached is not None:
        return cached
    path = _icon_file(app_id)
    src = cairo.ImageSurface(cairo.FORMAT_ARGB32, 160, 160)
    cr = cairo.Context(src)
    if path:
        try:
            pb = GdkPixbuf.Pixbuf.new_from_file_at_size(path, 128, 128)
            if not pb.get_has_alpha():
                pb = pb.add_alpha(False, 0, 0, 0)
            w, h = pb.get_width(), pb.get_height()
            row = pb.get_rowstride()
            pixels = pb.get_pixels()
            raw = cairo.ImageSurface(cairo.FORMAT_ARGB32, w, h)
            dst = raw.get_data()
            dstride = raw.get_stride()
            for y in range(h):
                for x in range(w):
                    i = y * row + x * 4
                    r, g, b, a = pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]
                    af = a / 255.0
                    o = y * dstride + x * 4
                    dst[o] = int(b * af)
                    dst[o + 1] = int(g * af)
                    dst[o + 2] = int(r * af)
                    dst[o + 3] = a
            raw.mark_dirty()
            cr.set_source_surface(raw, (160 - w) / 2.0, (160 - h) / 2.0)
            cr.paint()
        except Exception:
            path = None
    if not path:
        plo = PangoCairo.create_layout(cr)
        plo.set_font_description(Pango.FontDescription("CaskaydiaCove Nerd Font 72"))
        plo.set_text(label or "?", -1)
        tw, th = plo.get_pixel_size()
        cr.set_source_rgba(0.96, 0.97, 0.98, 1)
        cr.move_to((160 - tw) / 2.0, (160 - th) / 2.0)
        PangoCairo.show_layout(cr, plo)
    tex = _center_ink(src)
    _GLYPHS[key] = tex
    return tex


_GLYPHS: dict[tuple, tuple[bytes, int, int]] = {}
_ICON_SURF: dict[tuple, cairo.ImageSurface] = {}
_ICON_FILES: dict[str, str] = {}
_DESKTOP_ICONS = _desktop_icon_names()


def _icon_surface(app_id: str | None, label: str) -> cairo.ImageSurface:
    key = (app_id or "", label)
    hit = _ICON_SURF.get(key)
    if hit is not None:
        return hit
    data, size, stride = _icon_tex(app_id, label)
    buf = bytearray(data)
    src = cairo.ImageSurface.create_for_data(buf, cairo.FORMAT_ARGB32, size, size, stride)
    out = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    cr = cairo.Context(out)
    cr.set_source_surface(src, 0, 0)
    cr.paint()
    _ICON_SURF[key] = out
    return out


# Faint groove under the whole strip; only the current viewport is light.
TRACK_RGBA = (0.82, 0.84, 0.88, 0.05)
VIEWPORT_RGBA = (0.88, 0.90, 0.93, 0.34)


def _fill_neutral_bar(
    cr: cairo.Context,
    x: float,
    y: float,
    w: float,
    h: float,
    rgba: tuple[float, float, float, float],
) -> None:
    if w <= 0.5 or h <= 0.5:
        return
    cr.save()
    _round_rect(cr, x, y, w, h, h / 2.0)
    cr.set_source_rgba(*rgba)
    cr.fill()
    cr.restore()


def _spring_progress(t: float) -> float:
    """Critically damped spring from 0 to 1, same curve niri uses for workspace switches."""
    if t <= 0:
        return 0.0
    wt = WS_SPRING_OMEGA * t
    return min(1.0, 1.0 - (1.0 + wt) * math.exp(-wt))


def _ease_in_out_cubic(t: float) -> float:
    """Smoothstep-ish cubic: slow start, fast middle, slow end (t in 0..1)."""
    t = max(0.0, min(1.0, t))
    if t < 0.5:
        return 4.0 * t * t * t
    u = -2.0 * t + 2.0
    return 1.0 - (u * u * u) / 2.0


def _paint_icon(
    cr: cairo.Context,
    cx: float,
    cy: float,
    side: float,
    app_id: str | None,
    label: str,
    alpha: float,
    angle: float = 0.0,
) -> None:
    if side < 4 or not (label or app_id):
        return
    # Soft ground shadow under the icon.
    cr.save()
    sw = side * 0.7
    sh = side * 0.16
    cr.translate(cx, cy + side * 0.42)
    cr.scale(1.0, sh / max(1.0, sw * 0.5))
    cr.arc(0, 0, sw * 0.5, 0, 2 * math.pi)
    cr.set_source_rgba(0, 0, 0, 0.07 * alpha)
    cr.fill()
    cr.restore()
    surf = _icon_surface(app_id, label)
    cr.save()
    cr.translate(cx, cy)
    if abs(angle) > 1e-4:
        cr.rotate(angle)
    cr.translate(-side / 2.0, -side / 2.0)
    cr.scale(side / surf.get_width(), side / surf.get_height())
    cr.set_source_surface(surf, 0, 0)
    cr.paint_with_alpha(max(0.0, min(1.0, alpha)))
    cr.restore()


def _app_width(app: dict) -> float:
    if app.get("col_gap"):
        return COL_GAP_PX
    pct = float(app.get("percent") or 0)
    if app.get("floating") and pct <= 0:
        return COMPACT_PX
    if pct > 0:
        return max(MIN_SLOT_PX, pct / 100.0 * ROW_PX)
    if app.get("spacer") or app.get("compact") or app.get("offscreen"):
        return COMPACT_PX
    return 0.0


def _seg_width(seg: dict) -> float:
    if seg.get("col_tick"):
        return COL_GAP_PX
    if seg.get("spacer") or seg.get("hidden"):
        return 0.0
    pct = float(seg.get("percent") or 0)
    if pct > 0:
        return pct / 100.0 * ROW_PX
    return 0.0


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
            key=os.path.getmtime,
            reverse=True,
        )
    except OSError:
        return None
    return cands[0] if cands else None


class NiriIpc:
    """Persistent request socket (no `niri msg` process per scroll event)."""

    def __init__(self) -> None:
        self.sock: socket.socket | None = None

    def _connect(self) -> socket.socket | None:
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

    def action(self, action: dict) -> bool:
        payload = (json.dumps({"Action": action}, separators=(",", ":")) + "\n").encode()
        for _ in range(2):
            try:
                if self.sock is None:
                    self.sock = self._connect()
                    if self.sock is None:
                        return False
                self.sock.sendall(payload)
                buf = b""
                while b"\n" not in buf:
                    chunk = self.sock.recv(4096)
                    if not chunk:
                        raise ConnectionError("niri ipc closed")
                    buf += chunk
                return True
            except OSError:
                try:
                    if self.sock is not None:
                        self.sock.close()
                except OSError:
                    pass
                self.sock = None
        return False


def _synth_click(pitch: float, seed: int) -> array.array:
    """Short metallic pawl click: noise transient + two damped partials."""
    rng = random.Random(seed)
    n = int(RATCHET_RATE * 0.016)
    out = array.array("f", [0.0] * n)
    f1 = pitch
    f2 = pitch * 2.63
    ph = rng.random() * math.tau
    lp = 0.0
    for i in range(n):
        t = i / RATCHET_RATE
        noise = rng.uniform(-1.0, 1.0)
        lp += (noise - lp) * 0.55
        v = (
            (noise - lp) * 0.55 * math.exp(-t / 0.0011)
            + math.sin(math.tau * f1 * t + ph) * 0.55 * math.exp(-t / 0.0042)
            + math.sin(math.tau * f2 * t) * 0.25 * math.exp(-t / 0.0019)
        )
        if i < 12:
            v *= i / 12.0
        out[i] = v
    return out


class Ratchet:
    """Real-time ratchet audio fed to one long-lived pw-cat process.

    Each frame writes exactly the elapsed wall time worth of samples with the
    clicks spread across it, so latency stays flat and click rate follows the
    scroll speed.
    """

    PITCHES = (1700.0, 2000.0, 2350.0, 2750.0, 3200.0, 3700.0)
    VARIANTS = 3

    def __init__(self) -> None:
        self.proc: subprocess.Popen[bytes] | None = None
        self.bank = [
            [_synth_click(p, int(p) * 7 + v) for v in range(self.VARIANTS)]
            for p in self.PITCHES
        ]
        self.last_write = 0.0
        self.pending: array.array = array.array("f")
        self._dead_until = 0.0

    def _ensure(self) -> bool:
        if self.proc is not None and self.proc.poll() is None:
            return True
        if time.monotonic() < self._dead_until:
            return False
        try:
            self.proc = subprocess.Popen(
                [
                    "pw-cat",
                    "--playback",
                    "--raw",
                    "--format=s16",
                    f"--rate={RATCHET_RATE}",
                    "--channels=1",
                    "--latency=15ms",
                    "--media-role=Game",
                    "-P",
                    '{ node.name = "niri-rail-ratchet" node.description = "Rail ratchet" }',
                    "-",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except OSError:
            self.proc = None
            self._dead_until = time.monotonic() + 5.0
            return False
        assert self.proc.stdin is not None
        fd = self.proc.stdin.fileno()
        fl = fcntl.fcntl(fd, fcntl.F_GETFL)
        fcntl.fcntl(fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)
        self.last_write = 0.0
        return True

    def play(self, clicks: int, dt: float, speed: float) -> None:
        """`clicks` teeth passed during the last `dt` seconds at `speed` px/s."""
        if clicks <= 0 or RATCHET_VOLUME <= 0:
            return
        if not self._ensure():
            return
        now = time.monotonic()
        # After silence start fresh; otherwise cover exactly the elapsed time.
        if now - self.last_write > 0.12:
            dt = 0.03
        dt = max(0.008, min(0.06, dt))
        clicks = min(clicks, max(1, int(round(RATCHET_MAX_HZ * dt + 0.49))))
        n = int(RATCHET_RATE * dt)
        buf = array.array("f", [0.0] * n)
        if self.pending:
            k = min(len(self.pending), n)
            for i in range(k):
                buf[i] += self.pending[i]
            self.pending = self.pending[k:]

        s = max(0.0, min(1.0, speed / 1600.0))
        pitch_i = min(len(self.PITCHES) - 1, int(s * (len(self.PITCHES) - 0.01)))
        gain = RATCHET_VOLUME * (1.0 - 0.45 * s)
        step = n / clicks
        tail = array.array("f")
        for c in range(clicks):
            jitter = random.uniform(-0.15, 0.15) * step
            start = int(max(0, min(n - 1, c * step + step * 0.5 + jitter)))
            pi = max(0, min(len(self.PITCHES) - 1, pitch_i + random.choice((-1, 0, 0, 1))))
            click = random.choice(self.bank[pi])
            g = gain * random.uniform(0.8, 1.0)
            for j, v in enumerate(click):
                idx = start + j
                if idx < n:
                    buf[idx] += v * g
                else:
                    k = idx - n
                    while len(tail) <= k:
                        tail.append(0.0)
                    tail[k] += v * g
        if tail:
            if len(self.pending) < len(tail):
                self.pending.extend([0.0] * (len(tail) - len(self.pending)))
            for i, v in enumerate(tail):
                self.pending[i] += v

        pcm = array.array("h", (int(max(-1.0, min(1.0, v)) * 32000) for v in buf))
        try:
            assert self.proc is not None and self.proc.stdin is not None
            os.write(self.proc.stdin.fileno(), pcm.tobytes())
            self.last_write = now
        except BlockingIOError:
            pass
        except OSError:
            self.proc = None


class Rail(Gtk.Application):
    def __init__(self) -> None:
        super().__init__(application_id="dev.gabriel.niri-rail")
        self._lock_fd = _claim_lock()
        self.snap = layout.fresh_snapshot()
        self.state: dict | None = None
        # (x0, x1, y0, y1, window id, kind): kind is app | float | min.
        self.hits: list[tuple[float, float, float, float, int, str]] = []
        # (cx, cy, radius, scroll_delta): only the gap ball is clickable.
        self.gap_hits: list[tuple[float, float, float, float]] = []
        # Workspace switch slide: previous state, direction (+1 = to a lower one), start time.
        self.ws_anim: dict | None = None
        # (dash top y, workspace idx) of the last paint, for clicks on the block.
        self._dash_hits: list[tuple[float, int]] = []
        self._ws_ref: tuple[object, int] | None = None
        self.pointer_x = -1.0
        self.pointer_y = -1.0
        self.gap_hover = False
        self.gap_grow = 0.0
        self.pill = (0.0, 0.0, 0.0, 0.0)
        self.hover_id: int | None = None
        self.hover: dict[int, dict[str, float]] = {}
        self._hover_tick = False
        # Per-window breathe: only focused (+ at most one winding down).
        self.pulse: dict[int, dict[str, float | bool]] = {}
        # Appear 0→1 when a window first shows on the rail.
        self.appear: dict[int, float] = {}
        self._appear_ready = False
        # Opened in the background, not clicked yet: phase 0..1 (ease-in/out → angle).
        self.spin: dict[int, float] = {}
        # Notification urgency: buzz phase 0..1 per window.
        self.awaiting: dict[int, float] = {}
        self._await_ready = False
        # wid -> monotonic time it first appeared on the rail.
        self._first_seen: dict[int, float] = {}
        # Rail-clicked wids: ignore stale is_urgent until niri catches up.
        self._acked: dict[int, float] = {}
        self._tick_last = 0.0
        self._last_rail_offset = 0.0
        self.win: Gtk.Window | None = None
        self.area: Gtk.DrawingArea | None = None
        self._buf = bytearray()
        self._proc: subprocess.Popen[bytes] | None = None
        self._stream_reconnect_pending = False
        self._recompute_scheduled = False
        self._focus_gen = 0
        self._pending_center_wid: int | None = None
        self.ipc = NiriIpc()
        self.ratchet = Ratchet()
        # Gear phase in rail px (view position mapped onto the bar scale).
        self.gear_phase = 0.0
        self._gear_ref: tuple[object, float, float] | None = None
        self._gear_t = 0.0
        self._scroll_accum = 0.0
        self._ws_scroll = 0.0
        # Shown rail width (eases toward the strip width) and its last step time.
        self.rail_w: float | None = None
        self._drag_frozen: dict | None = None
        self._was_dragging = False
        self._rail_w_t = 0.0
        # Dash column x-range of the last paint, for clicks.
        self._dash_x = (0.0, 0.0)

    def do_activate(self) -> None:  # noqa: N802
        if self.win is not None:
            self.win.present()
            return

        win = Gtk.Window(application=self)
        self.win = win
        win.set_decorated(False)
        # Sit inside the Noctalia bottom bar band as the center component.
        # Noctalia keeps exclusive zone + chrome; rail is visual-only in the gap.
        # Full bar height so the viewport backdrop reaches the bar's top edge.
        rail_h = HOST_H - RAIL_BOTTOM
        win.set_default_size(1280, rail_h)
        win.set_size_request(-1, rail_h)
        win.add_css_class("niri-rail")

        css = Gtk.CssProvider()
        css.load_from_data(
            b"window.niri-rail { background-color: transparent; }"
        )
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(),
            css,
            Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION,
        )

        area = Gtk.DrawingArea()
        area.set_hexpand(True)
        area.set_vexpand(True)
        area.set_content_height(rail_h)
        area.set_draw_func(self._draw, None)
        self.area = area
        win.set_child(area)

        click = Gtk.GestureClick()
        click.set_button(0)
        click.connect("pressed", self._on_click)
        area.add_controller(click)

        motion = Gtk.EventControllerMotion()
        motion.connect("motion", self._on_motion)
        motion.connect("leave", self._on_leave)
        area.add_controller(motion)

        scroll = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.BOTH_AXES
        )
        scroll.connect("scroll", self._on_scroll)
        area.add_controller(scroll)

        Gtk4LayerShell.init_for_window(win)
        # Overlay so we paint on top of Noctalia's empty center (same band, not a second bar).
        Gtk4LayerShell.set_layer(win, Gtk4LayerShell.Layer.OVERLAY)
        Gtk4LayerShell.set_namespace(win, "niri-rail")
        Gtk4LayerShell.set_keyboard_mode(win, Gtk4LayerShell.KeyboardMode.NONE)
        Gtk4LayerShell.set_exclusive_zone(win, 0)
        # Sit low in the Noctalia band, like its icons.
        bottom_pad = RAIL_BOTTOM
        # Left cluster right after Noctalia's launcher; the window spans to the
        # right edge but the input region only covers what is drawn.
        for edge, margin in (
            (Gtk4LayerShell.Edge.BOTTOM, bottom_pad),
            (Gtk4LayerShell.Edge.LEFT, RAIL_LEFT),
            (Gtk4LayerShell.Edge.RIGHT, 0),
        ):
            Gtk4LayerShell.set_anchor(win, edge, True)
            Gtk4LayerShell.set_margin(win, edge, margin)
        Gtk4LayerShell.set_anchor(win, Gtk4LayerShell.Edge.TOP, False)

        def _on_realize(*_args) -> None:
            # Empty input until the first paint sets the pill-only hit region,
            # so Noctalia side widgets stay clickable.
            self._set_input(0, 0, 0, 0)

        win.connect("realize", _on_realize)
        win.present()
        self._recompute()
        self._start_stream()

    def _request_recompute(self) -> None:
        """Coalesce layout floods (scroll animations) to ~one paint per frame."""
        if self._recompute_scheduled:
            return
        self._recompute_scheduled = True
        GLib.timeout_add(8, self._flush_recompute)

    def _flush_recompute(self) -> bool:
        self._recompute_scheduled = False
        self._recompute()
        return False

    def _recompute(self) -> None:
        prev = self.state
        try:
            new_state = layout.compute_state(
                snapshot=self.snap, prev_state=self.state
            )
        except Exception as exc:  # noqa: BLE001 — keep the rail up if one frame is bad
            print(f"niri-rail compute: {exc}", file=sys.stderr)
            return
        # Edit (drag) mode: paint the workspace exactly as it was when the drag
        # began — width and contents — and only follow the real layout on drop.
        dragging = self._window_dragging()
        if dragging:
            if not self._was_dragging and prev is not None:
                self._drag_frozen = prev
            frozen = self._drag_frozen
            if frozen is not None and frozen.get("workspace_id") == new_state.get(
                "workspace_id"
            ):
                new_state = frozen
            else:
                self._drag_frozen = None
        else:
            self._drag_frozen = None
        self._was_dragging = dragging
        self.state = new_state
        switched = self._sync_ws_switch(prev)
        self._sync_appear(no_pop=switched)
        self._sync_spin()
        self._sync_awaiting()
        self._sync_gear()
        self._maybe_flush_pending_center()
        need_tick = (
            bool(self.pulse)
            or bool(self.spin)
            or bool(self.hover)
            or bool(self.awaiting)
            or self.ws_anim is not None
            or self._rail_w_moving()
            or any(v < 0.999 for v in self.appear.values())
            or self._focused_wid() is not None
        )
        if need_tick:
            self._ensure_hover_tick()
        elif self.area is not None:
            self.area.queue_draw()

    def _sync_ws_switch(self, prev: dict | None) -> bool:
        """Start the slide when the shown workspace changes. Returns True on a switch."""
        st = self.state or {}
        ref = (st.get("workspace_id"), int(st.get("workspace_idx") or 0))
        old = self._ws_ref
        self._ws_ref = ref
        if old is None or prev is None or old[0] == ref[0] or ref[0] is None:
            return False
        self.ws_anim = {
            "from": prev,
            "from_idx": old[1],
            "dir": 1.0 if ref[1] >= old[1] else -1.0,
            "t0": time.monotonic(),
        }
        return True

    def _ws_anim_progress(self) -> float | None:
        anim = self.ws_anim
        if anim is None:
            return None
        t = time.monotonic() - float(anim["t0"])
        if t >= WS_ANIM_S:
            self.ws_anim = None
            return None
        return _spring_progress(t)

    def _real_tiled_apps(self, state: dict) -> list[dict]:
        """Tiled windows that own a strip slot (not gaps/spacers/floating)."""
        out: list[dict] = []
        for a in state.get("apps") or []:
            if a.get("floating") or a.get("col_gap") or a.get("spacer"):
                continue
            if a.get("window_id") is None and float(a.get("percent") or 0) <= 0:
                continue
            out.append(a)
        return out

    def _map_geom(self, st: dict) -> dict | None:
        """Map niri on-screen column spans onto rail px (1 viewport = ROW_PX).

        ``column_spans`` / ``view_clip`` share output coordinates (tile_pos path).
        Including the full viewport in the content extent keeps empty sides of a
        centered window visible on the map — not packed to the left.
        """
        try:
            usable = float(st.get("usable_width") or 0)
        except (TypeError, ValueError):
            return None
        if usable <= 1:
            return None
        clip = st.get("view_clip") or []
        if len(clip) != 2:
            return None
        try:
            clip_l = float(clip[0])
            clip_r = float(clip[1])
        except (TypeError, ValueError, IndexError):
            return None
        if clip_r - clip_l <= 1:
            return None
        spans: dict[int, tuple[float, float]] = {}
        for key, span in (st.get("column_spans") or {}).items():
            try:
                col = int(key)
                left = float(span[0])
                width = float(span[1])
            except (TypeError, ValueError, IndexError):
                continue
            if width <= 0:
                continue
            spans[col] = (left, width)
        if not spans:
            return None
        content_l = clip_l
        content_r = clip_r
        for left, width in spans.values():
            content_l = min(content_l, left)
            content_r = max(content_r, left + width)
        scale = ROW_PX / usable
        return {
            "usable": usable,
            "scale": scale,
            "clip_l": clip_l,
            "clip_r": clip_r,
            "content_l": content_l,
            "content_r": content_r,
            "total": max(ROW_PX, (content_r - content_l) * scale),
            "spans": spans,
        }

    def _strip_width(self, state: dict) -> float:
        """Rail px the workspace strip needs: its columns, clamped to 1..4 viewports.

        Empty tiling: 0 — do not keep a 1-viewport hollow bar.
        Held at its current width while a window is being dragged; resizes on drop.
        """
        if self.rail_w is not None and self._window_dragging():
            return self.rail_w
        if not self._real_tiled_apps(state):
            return 0.0
        geom = self._map_geom(state)
        if geom is None:
            apps = [a for a in state.get("apps") or [] if not a.get("floating")]
            total = sum(self._slot_width(a) for a in apps)
            return min(RAIL_MAX_W, max(RAIL_MIN_W, total))
        return min(RAIL_MAX_W, max(RAIL_MIN_W, float(geom["total"])))

    def _window_dragging(self) -> bool:
        """niri reports an interactively moved window with no workspace."""
        for win in (self.snap or {}).get("windows") or []:
            if win.get("workspace_id") is None and not win.get("is_minimized"):
                return True
        return False

    def _step_rail_w(self, target: float) -> float:
        now = time.monotonic()
        if self.rail_w is None:
            self.rail_w = target
        else:
            dt = min(0.1, max(0.0, now - self._rail_w_t))
            self.rail_w += (target - self.rail_w) * (1.0 - math.exp(-RAIL_W_RATE * dt))
            if abs(target - self.rail_w) < 0.4:
                self.rail_w = target
        self._rail_w_t = now
        if self.rail_w != target:
            self._ensure_hover_tick()
        return self.rail_w

    def _rail_w_moving(self) -> bool:
        if self.rail_w is None:
            return False
        return abs(self._strip_width(self.state or {}) - self.rail_w) >= 0.4

    def _sync_appear(self, no_pop: bool = False) -> None:
        ids: set[int] = set()
        for app in self._all_chips(self.state):
            wid = app.get("window_id")
            if wid is not None and not app.get("col_gap"):
                ids.add(int(wid))
        if no_pop:
            # Workspace switch: the slide brings them in, no per-icon pop-in.
            for wid in ids:
                self.appear[wid] = 1.0
        if not self._appear_ready:
            # First snapshot: no pop-in for already-open windows.
            for wid in ids:
                self.appear[wid] = 1.0
            self._appear_ready = True
            return
        for wid in ids:
            if wid not in self.appear:
                # Start slightly above 0 so the first frame already draws something.
                self.appear[wid] = 0.04
        for wid in list(self.appear):
            if wid not in ids:
                del self.appear[wid]

    def _sync_spin(self) -> None:
        """Split niri ``is_urgent`` into new-window spin vs notification buzz.

        niri only clears urgency on a click (window or rail), never on hover
        focus, so both animations run until the user actually clicks.
        """
        now = time.monotonic()
        ids: set[int] = set()
        urgent: set[int] = set()
        for app in self._all_chips(self.state):
            wid = app.get("window_id")
            if wid is None or app.get("col_gap"):
                continue
            wid = int(wid)
            ids.add(wid)
            if app.get("is_urgent"):
                urgent.add(wid)

        if not self._await_ready:
            # Windows already open when the rail starts count as old.
            for wid in ids:
                self._first_seen[wid] = now - NEW_URGENT_GRACE_S - 1.0
            self._await_ready = True
        for wid in ids:
            self._first_seen.setdefault(wid, now)
        for wid in list(self._first_seen):
            if wid not in ids:
                del self._first_seen[wid]

        for wid, t in list(self._acked.items()):
            if now - t > 0.8 or wid not in urgent:
                del self._acked[wid]
        urgent -= self._acked.keys()

        for wid in urgent:
            if wid in self.spin or wid in self.awaiting:
                continue
            if now - self._first_seen[wid] <= NEW_URGENT_GRACE_S:
                self.spin[wid] = 0.0
            else:
                self.awaiting[wid] = 0.0
        for table in (self.spin, self.awaiting):
            for wid in list(table):
                if wid not in urgent:
                    del table[wid]

    def _sync_awaiting(self) -> None:
        """Kept for call-site compatibility; ``_sync_spin`` drives both states."""

    def _sync_gear(self) -> None:
        """Advance the gear teeth with the view and click once per tooth."""
        st = self.state or {}
        try:
            view = float(st.get("view_pos"))
            usable = float(st.get("usable_width") or 0)
        except (TypeError, ValueError):
            return
        if usable <= 1:
            return
        ws = st.get("workspace_id")
        now = time.monotonic()
        ref = self._gear_ref
        self._gear_ref = (ws, usable, view)
        prev_t = self._gear_t
        self._gear_t = now
        # Workspace/output change: re-anchor, no teeth jump or sound.
        if ref is None or ref[0] != ws or abs(ref[1] - usable) > 0.5:
            return
        delta = (view - ref[2]) * ROW_PX / usable
        if abs(delta) < 1e-3:
            return
        old = self.gear_phase
        self.gear_phase += delta
        crossed = abs(math.floor(self.gear_phase / TOOTH_PX) - math.floor(old / TOOTH_PX))
        if crossed and abs(delta) < ROW_PX * 1.5:
            dt = now - prev_t if prev_t > 0 else 0.016
            speed = abs(delta) / max(0.004, dt)
            self.ratchet.play(int(crossed), dt, speed)

    def _shake(self, wid: int | None) -> tuple[float, float]:
        """Phone-style buzz for notification icons: (x offset px, angle rad)."""
        if wid is None:
            return (0.0, 0.0)
        phase = self.awaiting.get(int(wid))
        if phase is None:
            return (0.0, 0.0)
        t = float(phase) * AWAIT_PERIOD_S
        if t >= AWAIT_BUZZ_S:
            return (0.0, 0.0)
        env = math.sin(math.pi * t / AWAIT_BUZZ_S)
        osc = 2.0 * math.pi * AWAIT_BUZZ_HZ * t
        return (
            AWAIT_SHAKE_PX * env * math.sin(osc),
            AWAIT_SHAKE_RAD * env * math.sin(osc * 1.3 + 0.7),
        )

    def _spin_angle(self, wid: int | None) -> float:
        if wid is None:
            return 0.0
        phase = self.spin.get(int(wid))
        if phase is None:
            return 0.0
        return _ease_in_out_cubic(float(phase)) * 2.0 * math.pi

    def _clear_urgent(self, wid: int) -> None:
        """Stop spin/buzz immediately and ask niri to drop urgency (rail click)."""
        self.spin.pop(wid, None)
        self.awaiting.pop(wid, None)
        self._acked[wid] = time.monotonic()
        subprocess.Popen(
            [NIRI_BIN, "msg", "action", "unset-window-urgent", "--id", str(wid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _appear_ease(self, wid: int | None) -> float:
        if wid is None:
            return 1.0
        t = float(self.appear.get(int(wid), 1.0))
        t = max(0.0, min(1.0, t))
        # Ease-out cubic: visible growth early, soft landing — no "blink then done".
        return 1.0 - (1.0 - t) ** 3

    def _slot_width(self, app: dict) -> float:
        w = _app_width(app)
        if app.get("col_gap") or w <= 0:
            return w
        return w * self._appear_ease(app.get("window_id"))

    def _seg_slot_width(self, seg: dict) -> float:
        w = _seg_width(seg)
        if seg.get("col_tick") or w <= 0:
            return w
        return w * self._appear_ease(seg.get("window_id"))

    def _reap_stream(self) -> None:
        proc = self._proc
        self._proc = None
        if proc is None:
            return
        try:
            if proc.stdout is not None:
                proc.stdout.close()
        except OSError:
            pass
        try:
            if proc.poll() is None:
                proc.kill()
        except OSError:
            pass
        try:
            proc.wait(timeout=0.5)
        except Exception:
            pass

    def _schedule_stream_reconnect(self) -> None:
        """Event-stream died (HUP/EOF) — rail must reconnect or it freezes on last frame."""
        if self._stream_reconnect_pending:
            return
        self._stream_reconnect_pending = True
        self._reap_stream()
        GLib.timeout_add(400, self._reconnect_stream)

    def _reconnect_stream(self) -> bool:
        self._stream_reconnect_pending = False
        try:
            self.snap = layout.fresh_snapshot()
            self._recompute()
            self._start_stream()
        except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as exc:
            print(f"niri-rail stream reconnect: {exc}", file=sys.stderr)
            self._schedule_stream_reconnect()
        return False

    def _start_stream(self) -> None:
        self._reap_stream()
        self._buf.clear()
        proc = subprocess.Popen(
            [NIRI_BIN, "msg", "--json", "event-stream"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        assert proc.stdout is not None
        self._proc = proc
        fd = proc.stdout.fileno()
        flags = fcntl.fcntl(fd, fcntl.F_GETFL)
        fcntl.fcntl(fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)
        GLib.io_add_watch(
            fd,
            GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR,
            self._on_stream,
        )

    def _on_stream(self, fd: int, cond: int) -> bool:
        if cond & (GLib.IO_HUP | GLib.IO_ERR) and not (cond & GLib.IO_IN):
            self._schedule_stream_reconnect()
            return False
        try:
            chunk = os.read(fd, 1 << 16)
        except BlockingIOError:
            return True
        if not chunk:
            self._schedule_stream_reconnect()
            return False
        self._buf.extend(chunk)
        dirty = False
        while True:
            nl = self._buf.find(b"\n")
            if nl < 0:
                break
            raw = bytes(self._buf[:nl])
            del self._buf[: nl + 1]
            if not raw:
                continue
            try:
                ev = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if not isinstance(ev, dict):
                continue
            kind = layout.apply_stream_event(self.snap, ev)
            if kind != "ignore":
                dirty = True
        if dirty:
            self._request_recompute()
        return True

    @staticmethod
    def _all_chips(state: dict | None) -> list[dict]:
        st = state or {}
        return (
            list(st.get("apps") or [])
            + list(st.get("minimized_tiled") or [])
            + list(st.get("floating_open") or [])
            + list(st.get("minimized_floating") or [])
        )

    def _hit_entry(self, x: float, y: float | None = None) -> tuple[int, str] | None:
        for x0, x1, y0, y1, wid, kind in self.hits:
            if not (x0 <= x < x1):
                continue
            if y is not None and not (y0 <= y < y1):
                continue
            return (wid, kind)
        return None

    def _hit(self, x: float, y: float | None = None) -> int | None:
        hit = self._hit_entry(x, y)
        return None if hit is None else hit[0]

    def _niri_action(self, action: str, wid: int) -> None:
        subprocess.Popen(
            [NIRI_BIN, "msg", "action", action, "--id", str(wid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _snap_focused_wid(self) -> int | None:
        for win in (self.snap or {}).get("windows") or []:
            if win.get("is_focused") and win.get("id") is not None:
                try:
                    return int(win["id"])
                except (TypeError, ValueError):
                    return None
        return None

    def _app_on_screen(self, wid: int) -> bool:
        """True when the rail shows this window as in-viewport (not offscreen stub)."""
        for app in (self.state or {}).get("apps") or []:
            if app.get("window_id") != wid:
                continue
            if app.get("col_gap") or app.get("spacer"):
                continue
            return not bool(app.get("offscreen"))
        # Unknown → treat as on-screen (focus then center).
        return True

    def _should_center_on_click(self, wid: int) -> bool:
        """Center unless the window is the first/last column of a capped strip.

        At the 4-viewport limit, those columns sit flush with the screen edge;
        pulling them to the viewport center would invent empty space past the cap.
        """
        apps = [
            a
            for a in (self.state or {}).get("apps") or []
            if a.get("window_id") is not None
            and not a.get("col_gap")
            and not a.get("floating")
            and not a.get("spacer")
        ]
        if len(apps) < 2:
            return True
        try:
            ids = [int(a["window_id"]) for a in apps]
        except (TypeError, ValueError):
            return True
        if wid not in ids:
            return True
        if wid != ids[0] and wid != ids[-1]:
            return True
        geom = self._map_geom(self.state or {})
        if geom is not None:
            return float(geom["total"]) + 1.0 < RAIL_MAX_W
        total = sum(self._slot_width(a) for a in (self.state or {}).get("apps") or [])
        return total + 1.0 < RAIL_MAX_W

    def _schedule_center(self, wid: int, gen: int, delay_ms: int) -> None:
        def _run(g: int = gen, w: int = wid) -> bool:
            if g != self._focus_gen:
                return False
            self._niri_action("center-window", w)
            return False

        GLib.timeout_add(delay_ms, _run)

    def _maybe_flush_pending_center(self) -> None:
        """When focus lands on the clicked window, center once (if allowed)."""
        wid = self._pending_center_wid
        if wid is None:
            return
        if self._snap_focused_wid() != wid:
            return
        self._pending_center_wid = None
        if self._should_center_on_click(wid):
            self._niri_action("center-window", wid)

    def _focus_and_center(self, wid: int) -> None:
        """Focus first, then center unless the column is already at the strip edge."""
        self._focus_gen += 1
        gen = self._focus_gen
        if not self._should_center_on_click(wid):
            self._pending_center_wid = None
            self._niri_action("focus-window", wid)
            return
        if self._snap_focused_wid() == wid:
            self._pending_center_wid = None
            self._niri_action("center-window", wid)
            return
        self._pending_center_wid = wid
        self._niri_action("focus-window", wid)

        def _fallback(g: int = gen, w: int = wid) -> bool:
            if g != self._focus_gen:
                return False
            if self._pending_center_wid != w:
                return False
            self._pending_center_wid = None
            if self._should_center_on_click(w):
                self._niri_action("center-window", w)
            return False

        GLib.timeout_add(90, _fallback)

    def _gap_at(self, x: float, y: float) -> float | None:
        for cx, cy, radius, delta in self.gap_hits:
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= radius * radius:
                return delta
        return None

    def _scroll_view_by(self, delta: float) -> None:
        if abs(delta) < 0.5:
            return
        self.ipc.action({"ScrollViewBy": {"delta": float(delta)}})

    def _on_click(self, gesture: Gtk.GestureClick, _n: int, x: float, y: float) -> None:
        dx0, dx1 = self._dash_x
        if self._dash_hits and dx0 <= x < dx1:
            # Workspace dashes: nearest dash by y.
            _, idx = min(self._dash_hits, key=lambda d: abs(d[0] + DASH_H / 2.0 - y))
            subprocess.Popen(
                [NIRI_BIN, "msg", "action", "focus-workspace", str(idx)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            return
        gap_delta = self._gap_at(x, y)
        if gap_delta is not None:
            self._scroll_view_by(gap_delta)
            if self.area is not None:
                self.area.queue_draw()
            return
        hit = self._hit_entry(x, y)
        if hit is None:
            return
        wid, kind = hit
        button = gesture.get_current_button()
        if kind != "app":
            self._clear_urgent(wid)
            if kind == "min":
                self._niri_action("unminimize-window", wid)
            elif button == 3 or button == 2:
                self._niri_action("minimize-window", wid)
            else:
                self._niri_action("focus-window", wid)
            if self.area is not None:
                self.area.queue_draw()
            return
        # Any rail click acknowledges urgency (same as activating the window).
        self._clear_urgent(wid)
        if button == 3:
            cmds = [
                [NIRI_BIN, "msg", "action", "focus-window", "--id", str(wid)],
                [NIRI_BIN, "msg", "action", "move-column-right"],
            ]
        elif button == 2:
            cmds = [
                [NIRI_BIN, "msg", "action", "focus-window", "--id", str(wid)],
                [NIRI_BIN, "msg", "action", "move-column-left"],
            ]
        else:
            self._focus_and_center(wid)
            if self.area is not None:
                self.area.queue_draw()
            return
        for cmd in cmds:
            subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if self.area is not None:
            self.area.queue_draw()

    def _on_motion(self, _ctrl: Gtk.EventControllerMotion, x: float, y: float) -> None:
        self.pointer_x = x
        self.pointer_y = y
        if self.area is None:
            return
        over_gap = self._gap_at(x, y) is not None
        if over_gap != self.gap_hover:
            self.gap_hover = over_gap
            self._ensure_hover_tick()
        wid = None if over_gap else self._hit(x, y)
        if over_gap or wid is not None:
            self.area.set_cursor_from_name("pointer")
        else:
            self.area.set_cursor_from_name("default")
        if wid != self.hover_id:
            if wid is not None:
                self.hover.setdefault(wid, {"grow": 0.0})
            self.hover_id = wid
            self._ensure_hover_tick()

    def _on_leave(self, _ctrl: Gtk.EventControllerMotion) -> None:
        self.pointer_x = -1.0
        self.pointer_y = -1.0
        self.hover_id = None
        self.gap_hover = False
        if self.area is not None:
            self.area.set_cursor_from_name("default")
        self._ensure_hover_tick()

    def _ensure_hover_tick(self) -> None:
        if self._hover_tick:
            return
        self._hover_tick = True
        GLib.timeout_add(16, self._tick_hover)

    @staticmethod
    def _breathe(phase: float) -> float:
        """phase 0/1 = small, 0.5 = large. Smooth peaks."""
        wave = 0.5 - 0.5 * math.cos(phase * 2.0 * math.pi)
        return wave * wave * (3.0 - 2.0 * wave)

    def _focused_wid(self) -> int | None:
        for app in self._all_chips(self.state):
            if app.get("is_focused") and app.get("window_id") is not None:
                return int(app["window_id"])
        return None

    def _tick_hover(self) -> bool:
        now = time.monotonic()
        dt = 0.016 if self._tick_last <= 0 else min(0.05, max(0.008, now - self._tick_last))
        self._tick_last = now
        period = 1.6
        alive = False
        focused_wid = self._focused_wid()

        # Appear: ~0.55s open for icon + slot + bar fill.
        for wid, t in list(self.appear.items()):
            if t < 1.0:
                nt = min(1.0, float(t) + dt / 0.55)
                self.appear[wid] = nt
                alive = True

        # Focused breathes; at most one other winds down (mix fades immediately).
        if focused_wid is not None:
            st = self.pulse.get(focused_wid)
            if st is None:
                self.pulse[focused_wid] = {"phase": 0.0, "mix": 0.0, "leaving": False}
            else:
                st["leaving"] = False
        leavers = [
            w for w, s in self.pulse.items() if focused_wid is None or w != focused_wid
        ]
        if leavers:
            leavers.sort(key=lambda w: float(self.pulse[w]["mix"]), reverse=True)
            keep = leavers[0]
            self.pulse[keep]["leaving"] = True
            for w in leavers[1:]:
                del self.pulse[w]

        for wid, st in list(self.pulse.items()):
            phase = float(st["phase"])
            mix = float(st["mix"])
            leaving = bool(st["leaving"])
            phase = (phase + dt / period) % 1.0
            if leaving:
                mix += (0.0 - mix) * min(1.0, dt * 8.0)
                if mix < 0.01:
                    del self.pulse[wid]
                    alive = True
                    continue
            else:
                mix += (1.0 - mix) * min(1.0, dt * 9.0)
                if abs(mix - 1.0) < 0.008:
                    mix = 1.0
            st["phase"] = phase
            st["mix"] = mix
            st["leaving"] = leaving
            alive = True

        for wid, st in list(self.hover.items()):
            target = 1.0 if wid == self.hover_id else 0.0
            st["grow"] += (target - st["grow"]) * min(1.0, dt * 14.0)
            if abs(st["grow"] - target) < 0.012:
                st["grow"] = target
            if abs(st["grow"] - target) > 0.001:
                alive = True
            elif wid != self.hover_id and st["grow"] <= 0.0:
                del self.hover[wid]

        gap_target = 1.0 if self.gap_hover else 0.0
        self.gap_grow += (gap_target - self.gap_grow) * min(1.0, dt * 14.0)
        if abs(self.gap_grow - gap_target) < 0.012:
            self.gap_grow = gap_target
        if abs(self.gap_grow - gap_target) > 0.001:
            alive = True

        # Urgency spin: phase advances; angle uses ease-in/out per revolution.
        if self.spin:
            alive = True
            for wid in list(self.spin):
                self.spin[wid] = (float(self.spin[wid]) + dt / SPIN_PERIOD_S) % 1.0
                alive = True

        for wid in list(self.awaiting):
            self.awaiting[wid] = (float(self.awaiting[wid]) + dt / AWAIT_PERIOD_S) % 1.0
            alive = True

        if self.ws_anim is not None or self._rail_w_moving():
            alive = True

        if self.area is not None and (
            alive
            or self.hover
            or self.pulse
            or self.spin
            or self.awaiting
            or self.gap_grow > 0.001
            or any(v < 1.0 for v in self.appear.values())
        ):
            self.area.queue_draw()
        if (
            not alive
            and not self.pulse
            and not self.hover
            and not self.spin
            and not self.awaiting
            and self.gap_grow <= 0.001
            and all(v >= 0.999 for v in self.appear.values())
        ):
            self._hover_tick = False
            self._tick_last = 0.0
            return False
        return True

    def _on_scroll(self, ctrl: Gtk.EventControllerScroll, dx: float, dy: float) -> bool:
        """Wheel/touchpad pans only over app icons or the gap ball — not empty rail."""
        amount = dy if abs(dy) >= abs(dx) else dx
        if amount == 0:
            return True
        px, py = self.pointer_x, self.pointer_y
        dx0, dx1 = self._dash_x
        if dx0 <= px < dx1:
            # Over the workspace dashes: one workspace per gesture step.
            self._ws_scroll += amount
            if abs(self._ws_scroll) >= 1.0:
                action = "focus-workspace-down" if self._ws_scroll > 0 else "focus-workspace-up"
                self._ws_scroll = 0.0
                subprocess.Popen(
                    [NIRI_BIN, "msg", "action", action],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            return True
        # Empty backdrop / strip: ignore — only icons and the gap ball move the view.
        if self._gap_at(px, py) is None and self._hit(px, py) is None:
            return True
        try:
            unit = ctrl.get_unit()
        except AttributeError:
            unit = Gdk.ScrollUnit.WHEEL
        if unit == Gdk.ScrollUnit.WHEEL:
            delta = amount * WHEEL_STEP_PX
        else:
            delta = amount * TOUCHPAD_GAIN
        # Touchpad sends many tiny deltas; batch them into ~1 request per frame.
        self._scroll_accum += delta
        if abs(self._scroll_accum) >= 1.0:
            self.ipc.action({"ScrollViewBy": {"delta": float(self._scroll_accum)}})
            self._scroll_accum = 0.0
        return True

    def _content_pad(self, total: float, view_w: float) -> float:
        """Legacy sequential layout pad (unused when span mapping is active)."""
        return 0.0

    def _viewport_slot(
        self, track_x: float, offset: float, pad: float, st: dict
    ) -> tuple[float, float]:
        """Current niri viewport on the rail (1 viewport = ROW_PX)."""
        geom = self._map_geom(st)
        if geom is None:
            base = track_x + pad - offset
            try:
                usable = float(st.get("usable_width") or 0)
                scroll_x = float(st.get("scroll_x") or 0)
            except (TypeError, ValueError):
                return base, ROW_PX
            if usable <= 1:
                return base, ROW_PX
            return base + (scroll_x / usable) * ROW_PX, ROW_PX
        scale = float(geom["scale"])
        content_l = float(geom["content_l"])
        base = track_x - offset
        vx = base + (float(geom["clip_l"]) - content_l) * scale
        return vx, float(geom["usable"]) * scale

    def _set_input(self, x: float, y: float, w: float, h: float) -> None:
        self._set_input_rects([(x, y, w, h)])

    def _set_input_rects(self, rects: list[tuple[float, float, float, float]]) -> None:
        if self.win is None:
            return
        surface = self.win.get_surface()
        if surface is None:
            return
        # Empty region = fully click-through (Noctalia sides / empty frames).
        region = cairo.Region()
        for x, y, w, h in rects:
            if w > 1 and h > 1:
                region.union(
                    cairo.RectangleInt(int(x), int(y), max(1, int(w)), max(1, int(h)))
                )
        GdkWayland.WaylandSurface.set_input_region(surface, region)

    def _draw(self, _area: Gtk.DrawingArea, cr: cairo.Context, width: int, height: int, _data) -> None:
        cr.set_operator(cairo.OPERATOR_SOURCE)
        cr.set_source_rgba(0, 0, 0, 0)
        cr.paint()
        cr.set_operator(cairo.OPERATOR_OVER)

        state = self.state or {}
        self.hits = []
        self.gap_hits = []
        has_tiled = bool(self._real_tiled_apps(state))
        view_w = self._step_rail_w(self._strip_width(state))
        # No hollow 1-viewport strip when tiling is empty (allow ease-out to 0).
        show_pill = has_tiled or view_w > 0.5
        pill_w = (view_w + PAD_X * 2) if show_pill else 0.0
        pill_h = min(PILL_H, float(height))
        y0 = max(0.0, height - pill_h) + RAIL_SINK
        ws_w = self._ws_block_width(state, cr)
        limit = float(width) - RIGHT_RESERVE
        icon_y0 = y0 + PAD_T
        icon_y1 = y0 + PAD_T + ICON_H

        float_groups = (
            ("float", state.get("floating_open") or [], False),
            ("min", state.get("minimized_floating") or [], True),
        )
        tiled_chips = ("min", state.get("minimized_tiled") or [], True)
        has_floating = any(chips for _k, chips, _m in float_groups)
        has_tiled_side = (show_pill and pill_w > 1.0) or bool(tiled_chips[1])

        # Order: dashes | floating | bar | tiling (+ minimized tiled).
        cx = ws_w + WS_GAP
        rects: list[tuple[float, float, float, float]] = []

        def _paint_chip_group(kind: str, chips: list, minimized: bool) -> None:
            nonlocal cx
            for chip in chips:
                if cx + CHIP_W > limit:
                    break
                self._paint_chip(cr, chip, cx, y0, minimized)
                wid = chip.get("window_id")
                if wid is not None:
                    self.hits.append((cx, cx + CHIP_W, icon_y0, icon_y1, int(wid), kind))
                cx += CHIP_W

        first_float = True
        for kind, chips, minimized in float_groups:
            if not chips:
                continue
            if not first_float:
                _round_rect(
                    cr,
                    cx - CLUSTER_GAP / 2.0 - 1.0,
                    y0 + pill_h * 0.3,
                    2.0,
                    pill_h * 0.4,
                    1.0,
                )
                cr.set_source_rgba(1, 1, 1, 0.16)
                cr.fill()
            first_float = False
            _paint_chip_group(kind, chips, minimized)
            cx += CLUSTER_GAP
        if has_floating and not first_float:
            # Undo trailing cluster gap; sep/pad owns the next spacing.
            cx -= CLUSTER_GAP

        if has_floating and has_tiled_side:
            # [floating] --pad_right-- | --pad-- [tiling]
            sep_x = cx + TILE_FLOAT_PAD_RIGHT
            self._paint_tile_float_sep(cr, sep_x, y0, pill_h, float(height))
            cx = sep_x + TILE_FLOAT_PAD
        elif has_tiled_side and has_floating is False:
            pass

        x0 = cx
        self.pill = (x0, y0, pill_w, pill_h)
        glue_w = x0 + pill_w

        progress = self._ws_anim_progress()
        if show_pill and pill_w > 1.0:
            cr.save()
            cr.rectangle(x0, 0.0, pill_w, float(height))
            cr.clip()
            if progress is None:
                self._paint_pill(cr, state, x0, y0, pill_w, 0.0, live=True)
            else:
                anim = self.ws_anim or {}
                direction = float(anim.get("dir") or 1.0)
                old = anim.get("from") or {}
                dy_old = -direction * pill_h * progress
                dy_new = direction * pill_h * (1.0 - progress)
                self._paint_pill(cr, old, x0, y0, pill_w, dy_old, live=False)
                self._paint_pill(cr, state, x0, y0, pill_w, dy_new, live=True)
            cr.restore()
            cx = glue_w

        # Minimized tiled stay with the tiling side (after the pill).
        if tiled_chips[1]:
            cx += CLUSTER_GAP
            _paint_chip_group(*tiled_chips)

        self._paint_dashes(cr, state, y0, pill_h, progress)

        # Input: workspace dashes, app icons, gap balls, chips.
        dx0, dx1 = self._dash_x
        rects.append((dx0, y0, max(1.0, dx1 - dx0), pill_h))
        for x0h, x1h, y0h, y1h, _wid, _kind in self.hits:
            rects.append((x0h, y0h, max(1.0, x1h - x0h), max(1.0, y1h - y0h)))
        for gcx, gcy, gr, _delta in self.gap_hits:
            rects.append((gcx - gr, gcy - gr, gr * 2.0, gr * 2.0))
        self._set_input_rects(rects)

    def _shown_workspaces(self, state: dict) -> list[dict]:
        wss = list(state.get("workspaces") or [])
        cur = int(state.get("workspace_idx") or 1)
        return wss or [
            {"idx": cur, "is_active": True, "has_windows": False}
        ]

    def _ws_block_width(self, state: dict, cr: cairo.Context) -> float:
        return WS_PAD_L + DASH_W + WS_PAD_R

    def _dash_ys(self, n: int, y0: float, pill_h: float) -> list[float]:
        avail = pill_h - 14.0
        gap = 4.0 if n <= 1 else max(1.6, min(5.0, (avail - n * DASH_H) / (n - 1)))
        total = n * DASH_H + (n - 1) * gap
        top = y0 + (pill_h - total) / 2.0
        return [top + i * (DASH_H + gap) for i in range(n)]

    def _paint_dashes(
        self, cr: cairo.Context, state: dict, y0: float, pill_h: float, progress: float | None
    ) -> None:
        shown = self._shown_workspaces(state)
        ys = self._dash_ys(len(shown), y0, pill_h)
        x = WS_PAD_L
        self._dash_x = (0.0, WS_PAD_L + DASH_W + WS_PAD_R + WS_GAP)
        cur = int(state.get("workspace_idx") or 1)
        cur_y = None
        for ws, y in zip(shown, ys):
            idx = int(ws.get("idx") or 0)
            if idx == cur:
                cur_y = y
            alpha = 0.42 if ws.get("has_windows") else 0.2
            w = DASH_W * 0.62
            _round_rect(cr, x + (DASH_W - w) / 2.0, y, w, DASH_H, DASH_H / 2.0)
            cr.set_source_rgba(1, 1, 1, alpha)
            cr.fill()
        self._dash_hits = [(y, int(ws.get("idx") or 0)) for ws, y in zip(shown, ys)]
        if cur_y is None:
            return
        if progress is not None and self.ws_anim is not None:
            from_idx = int(self.ws_anim.get("from_idx") or 0)
            from_y = next(
                (y for ws, y in zip(shown, ys) if int(ws.get("idx") or 0) == from_idx), cur_y
            )
            cur_y = from_y + (cur_y - from_y) * progress
        _round_rect(cr, x, cur_y - 0.3, DASH_W, DASH_H + 0.6, (DASH_H + 0.6) / 2.0)
        cr.set_source_rgba(0.96, 0.97, 0.98, 0.95)
        cr.fill()

    def _paint_chip(self, cr: cairo.Context, chip: dict, x: float, y0: float, minimized: bool) -> None:
        wid = chip.get("window_id")
        st = self.hover.get(int(wid)) if wid is not None else None
        grow = float(st["grow"]) if st else 0.0
        side = CHIP_ICON * (1.0 + 0.16 * grow)
        if wid is not None and not minimized:
            pst = self.pulse.get(int(wid))
            if pst is not None and float(pst["mix"]) > 0.001:
                side *= 1.0 + float(pst["mix"]) * 0.2 * self._breathe(float(pst["phase"]))
        appear = self._appear_ease(wid)
        side *= min(1.0, appear * 1.15)
        icon_cy = y0 + PAD_T + ICON_H / 2.0
        shake_dx, shake_angle = self._shake(wid)
        _paint_icon(
            cr,
            x + CHIP_W / 2.0 + shake_dx,
            icon_cy,
            side,
            chip.get("app_id"),
            chip.get("label") or "",
            (MINIMIZED_ALPHA if minimized else 1.0) * min(1.0, appear * 1.15),
            self._spin_angle(int(wid) if wid is not None else None) + shake_angle,
        )
        bar_y = y0 + PAD_T + ICON_H + GAP_Y + BAR_INSET
        if minimized:
            cr.arc(x + CHIP_W / 2.0, bar_y + BAR_H / 2.0, BAR_H * 0.42, 0, 2 * math.pi)
            cr.set_source_rgba(0.9, 0.92, 0.95, 0.55)
            cr.fill()
        else:
            w = CHIP_W * 0.56
            rgba = VIEWPORT_RGBA if not chip.get("is_focused") else (0.95, 0.96, 0.98, 0.7)
            _fill_neutral_bar(cr, x + (CHIP_W - w) / 2.0, bar_y, w, BAR_H, rgba)

    def _gap_scroll_delta(self, state: dict, left_col: int, right_col: int) -> float | None:
        """Scroll delta that centers the gap between two columns in the viewport."""
        spans = state.get("column_spans") or {}
        clip = state.get("view_clip") or []
        left = spans.get(str(left_col))
        right = spans.get(str(right_col))
        if not left or not right or len(clip) != 2:
            return None
        try:
            l0, lw = float(left[0]), float(left[1])
            r0 = float(right[0])
            clip_l, clip_r = float(clip[0]), float(clip[1])
        except (TypeError, ValueError, IndexError):
            return None
        gap_mid = (l0 + lw + r0) / 2.0
        center = (clip_l + clip_r) / 2.0
        return gap_mid - center

    def _paint_viewport_frame(
        self,
        cr: cairo.Context,
        x0: float,
        x1: float,
        y_top: float,
        y_bot: float,
    ) -> None:
        """Thin top edge only — no side strokes (those read as L-shaped accents)."""
        if x1 - x0 < 2.0:
            return
        cr.save()
        cr.set_line_width(VIEWPORT_EDGE_W)
        cr.set_source_rgba(*VIEWPORT_EDGE_RGBA)
        cr.move_to(x0, y_top + VIEWPORT_EDGE_W * 0.5)
        cr.line_to(x1, y_top + VIEWPORT_EDGE_W * 0.5)
        cr.stroke()
        cr.restore()

    def _paint_tile_float_sep(
        self, cr: cairo.Context, x: float, y0: float, pill_h: float, win_h: float
    ) -> None:
        """Vertical bar between floating and tiling — equal inset in the visible band."""
        inset = TILE_FLOAT_SEP_VINSET
        # Ignore any sunk/clipped part below the window so bottom inset is real.
        top = max(0.0, y0)
        bot = min(y0 + pill_h, win_h)
        avail = bot - top
        if avail < inset * 2.0 + 4.0:
            inset = max(1.0, avail * 0.18)
        sep_y = top + inset
        sep_h = max(4.0, avail - inset * 2.0)
        _round_rect(
            cr,
            x - TILE_FLOAT_SEP_W / 2.0,
            sep_y,
            TILE_FLOAT_SEP_W,
            sep_h,
            TILE_FLOAT_SEP_W / 2.0,
        )
        cr.set_source_rgba(*TILE_FLOAT_SEP_RGBA)
        cr.fill()

    def _paint_gap_divider(
        self,
        cr: cairo.Context,
        cx: float,
        y_ball: float,
        y_bot: float,
        grow: float = 0.0,
    ) -> None:
        """Short lower line with the ball sitting on its top tip (grows on hover)."""
        r = GAP_HANDLE_R * (1.0 + GAP_HOVER_GROW * max(0.0, min(1.0, grow)))
        line_top = y_ball + r * 0.85
        if y_bot - line_top < 2.0:
            return
        cr.save()
        line_w = 1.5
        cr.rectangle(cx - line_w / 2.0, line_top, line_w, y_bot - line_top)
        cr.set_source_rgba(*GAP_LINE_RGBA)
        cr.fill()
        cr.arc(cx, y_ball, r, 0.0, 2.0 * math.pi)
        cr.set_source_rgba(*GAP_HANDLE_RGBA)
        cr.fill()
        cr.arc(cx, y_ball, r * 0.38, 0.0, 2.0 * math.pi)
        cr.set_source_rgba(0.12, 0.12, 0.14, 0.55)
        cr.fill()
        cr.restore()

    def _paint_pill(
        self,
        cr: cairo.Context,
        state: dict,
        x0: float,
        y0: float,
        pill_w: float,
        dy: float,
        live: bool,
    ) -> None:
        """Workspace apps + strip (floating windows have their own chips after the pill)."""
        apps = [a for a in state.get("apps") or [] if not a.get("floating")]
        segs = [s for s in state.get("segments") or [] if not s.get("floating")]
        geom = self._map_geom(state)
        view_w = pill_w - PAD_X * 2
        y0 = y0 + dy

        clip_l = x0
        clip_r = x0 + pill_w
        cr.push_group()

        icon_y = y0 + PAD_T
        strip_y = y0 + PAD_T + ICON_H + GAP_Y
        bar_y = strip_y + BAR_INSET
        track_x = x0 + PAD_X

        if geom is None:
            # Fallback: sequential percent slots (pre-span mapping).
            app_w = [self._slot_width(a) for a in apps]
            total = sum(app_w)
            offset = self._rail_offset(segs, total, live)
            pad = 0.0
            vx, vw = self._viewport_slot(track_x, offset, pad, state)
            clip_vx0 = max(vx, track_x)
            clip_vx1 = min(vx + vw, track_x + view_w)
            cursor = track_x + pad - offset
            for i, (app, w) in enumerate(zip(apps, app_w)):
                if w <= 0:
                    continue
                if cursor + w < clip_l or cursor > clip_r:
                    cursor += w
                    continue
                if app.get("col_gap"):
                    cx = cursor + w / 2.0
                    y_ball = icon_y + ICON_H * 0.78
                    self._paint_gap_divider(
                        cr, cx, y_ball, bar_y + BAR_H, self.gap_grow if live else 0.0
                    )
                    if live:
                        left_col = right_col = None
                        for j in range(i - 1, -1, -1):
                            if apps[j].get("col_gap") or apps[j].get("spacer"):
                                continue
                            if apps[j].get("column") is not None:
                                left_col = int(apps[j]["column"])
                                break
                        for j in range(i + 1, len(apps)):
                            if apps[j].get("col_gap") or apps[j].get("spacer"):
                                continue
                            if apps[j].get("column") is not None:
                                right_col = int(apps[j]["column"])
                                break
                        if left_col is not None and right_col is not None:
                            delta = self._gap_scroll_delta(state, left_col, right_col)
                            if delta is not None:
                                self.gap_hits.append(
                                    (cx, y_ball, GAP_HIT_R, float(delta))
                                )
                    cursor += w
                    continue
                self._paint_app_icon(
                    cr, app, cursor, w, icon_y, live
                )
                cursor += w
            real = [
                s
                for s in segs
                if not s.get("spacer")
                and not s.get("col_tick")
                and not s.get("ghost")
                and float(s.get("percent") or 0) > 0
            ]
            if real:
                _fill_neutral_bar(cr, track_x, bar_y, view_w, BAR_H, TRACK_RGBA)
                _fill_neutral_bar(
                    cr, clip_vx0, bar_y, clip_vx1 - clip_vx0, BAR_H, VIEWPORT_RGBA
                )
            sx = track_x + pad - offset
            for i, seg in enumerate(segs):
                w = self._seg_slot_width(seg)
                if w <= 0:
                    continue
                if sx + w < clip_l or sx > clip_r:
                    sx += w
                    continue
                if seg.get("col_tick"):
                    sx += w
                    continue
                self._paint_seg_bar(cr, segs, i, seg, sx, w, bar_y, real)
                sx += w
            content = cr.pop_group()
            cr.save()
            cr.rectangle(clip_l, -4096.0, pill_w, 8192.0)
            cr.clip()
            cr.set_source(content)
            cr.paint()
            cr.restore()
            return

        scale = float(geom["scale"])
        content_l = float(geom["content_l"])
        spans: dict[int, tuple[float, float]] = geom["spans"]
        total = float(geom["total"])
        offset = self._rail_offset_geom(geom, view_w, live)
        base = track_x - offset

        def x_at(niri_x: float) -> float:
            return base + (niri_x - content_l) * scale

        vx = x_at(float(geom["clip_l"]))
        vw = float(geom["usable"]) * scale
        clip_vx0 = max(vx, track_x)
        clip_vx1 = min(vx + vw, track_x + view_w)

        # Icons: place each column's windows inside its on-screen span.
        col_apps: dict[int, list[dict]] = {}
        for app in apps:
            if app.get("col_gap") or app.get("spacer"):
                continue
            col = app.get("column")
            if col is None:
                continue
            col_apps.setdefault(int(col), []).append(app)

        for col in sorted(spans):
            left, width = spans[col]
            rx = x_at(left)
            rw = max(0.0, width * scale)
            if rx + rw < clip_l or rx > clip_r:
                continue
            group = col_apps.get(col) or []
            if not group:
                continue
            shares = [max(0.05, float(a.get("height_share") or 1.0)) for a in group]
            share_sum = sum(shares) or 1.0
            cx0 = rx
            for app, share in zip(group, shares):
                w = rw * (share / share_sum)
                self._paint_app_icon(cr, app, cx0, w, icon_y, live)
                cx0 += w

        # Gap balls between adjacent columns (midpoint of the niri gap).
        ordered_cols = sorted(spans)
        for i in range(len(ordered_cols) - 1):
            c0, c1 = ordered_cols[i], ordered_cols[i + 1]
            l0, w0 = spans[c0]
            l1, _w1 = spans[c1]
            gap_mid = (l0 + w0 + l1) / 2.0
            cx = x_at(gap_mid)
            if cx < clip_l - GAP_HIT_R or cx > clip_r + GAP_HIT_R:
                continue
            y_ball = icon_y + ICON_H * 0.78
            self._paint_gap_divider(
                cr, cx, y_ball, bar_y + BAR_H, self.gap_grow if live else 0.0
            )
            if live:
                delta = self._gap_scroll_delta(state, c0, c1)
                if delta is not None:
                    self.gap_hits.append((cx, y_ball, GAP_HIT_R, float(delta)))

        real = [
            s
            for s in segs
            if not s.get("spacer")
            and not s.get("col_tick")
            and not s.get("ghost")
            and float(s.get("percent") or 0) > 0
        ]
        if real:
            _fill_neutral_bar(cr, track_x, bar_y, view_w, BAR_H, TRACK_RGBA)
            _fill_neutral_bar(
                cr, clip_vx0, bar_y, clip_vx1 - clip_vx0, BAR_H, VIEWPORT_RGBA
            )

        # Underline segments: ghosts + visible colors inside each column span.
        clip0 = float(geom["clip_l"])
        clip1 = float(geom["clip_r"])
        for col in sorted(spans):
            left, width = spans[col]
            right = left + width
            col_segs = [
                s
                for s in segs
                if s.get("column") is not None
                and int(s["column"]) == col
                and not s.get("col_tick")
                and not s.get("spacer")
            ]
            if not col_segs:
                continue
            ghosts_l = [s for s in col_segs if s.get("ghost") and s.get("offscreen") == "left"]
            ghosts_r = [s for s in col_segs if s.get("ghost") and s.get("offscreen") == "right"]
            body = [s for s in col_segs if not s.get("ghost")]
            # Fully off-screen: one ghost spanning the whole column.
            if not body and col_segs:
                sx = x_at(left)
                sw = max(0.0, width * scale)
                if sx + sw >= clip_l and sx <= clip_r:
                    self._paint_seg_bar(cr, col_segs, 0, col_segs[0], sx, sw, bar_y, real)
                continue
            vis_l = max(left, clip0)
            vis_r = min(right, clip1)
            if ghosts_l and vis_l > left + 0.5:
                sx = x_at(left)
                sw = max(0.0, (vis_l - left) * scale)
                if sx + sw >= clip_l and sx <= clip_r:
                    self._paint_seg_bar(cr, col_segs, 0, ghosts_l[0], sx, sw, bar_y, real)
            if body and vis_r > vis_l + 0.5:
                shares = [max(0.05, float(s.get("height_share") or 1.0)) for s in body]
                share_sum = sum(shares) or 1.0
                bx = x_at(vis_l)
                bw = max(0.0, (vis_r - vis_l) * scale)
                for i, (seg, share) in enumerate(zip(body, shares)):
                    w = bw * (share / share_sum)
                    if bx + w >= clip_l and bx <= clip_r:
                        self._paint_seg_bar(cr, body, i, seg, bx, w, bar_y, real)
                    bx += w
            if ghosts_r and right > vis_r + 0.5:
                sx = x_at(vis_r)
                sw = max(0.0, (right - vis_r) * scale)
                if sx + sw >= clip_l and sx <= clip_r:
                    self._paint_seg_bar(cr, col_segs, 0, ghosts_r[0], sx, sw, bar_y, real)

        content = cr.pop_group()
        cr.save()
        cr.rectangle(clip_l, -4096.0, pill_w, 8192.0)
        cr.clip()
        cr.set_source(content)
        cr.paint()
        cr.restore()

    def _paint_app_icon(
        self,
        cr: cairo.Context,
        app: dict,
        cursor: float,
        w: float,
        icon_y: float,
        live: bool,
    ) -> None:
        wid = app.get("window_id")
        label = app.get("label") or ""
        icon_cx = cursor + w / 2.0
        icon_cy = icon_y + ICON_H / 2.0
        side = 0.0
        if label and not app.get("col_gap"):
            st = self.hover.get(int(wid)) if wid is not None else None
            grow = float(st["grow"]) if st else 0.0
            appear = self._appear_ease(wid)
            side = min(ICON_MAX, ICON_H - 2.0, max(0.0, w - 1.0))
            icon_t = min(1.0, appear * 1.15)
            side *= icon_t
            if wid is not None:
                pst = self.pulse.get(int(wid))
                if pst is not None and float(pst["mix"]) > 0.001:
                    breath = self._breathe(float(pst["phase"]))
                    side *= 1.0 + float(pst["mix"]) * 0.34 * breath
            side *= 1.0 + 0.16 * grow
            alpha = icon_t
            shake_dx, shake_angle = self._shake(wid)
            if side >= 3:
                _paint_icon(
                    cr,
                    icon_cx + shake_dx,
                    icon_cy,
                    side,
                    app.get("app_id"),
                    label,
                    alpha,
                    self._spin_angle(int(wid) if wid is not None else None)
                    + shake_angle,
                )
        if live and wid is not None and not app.get("col_gap") and side >= 3:
            half = max(side * 0.55, 12.0)
            self.hits.append(
                (
                    icon_cx - half,
                    icon_cx + half,
                    icon_cy - half,
                    icon_cy + half,
                    int(wid),
                    "app",
                )
            )

    def _paint_seg_bar(
        self,
        cr: cairo.Context,
        segs: list[dict],
        i: int,
        seg: dict,
        sx: float,
        w: float,
        bar_y: float,
        real: list[dict],
    ) -> None:
        if w <= 0.5:
            return
        appear = self._appear_ease(seg.get("window_id"))
        alpha = min(1.0, appear * 1.1)
        prev = segs[i - 1] if i else None
        nxt = segs[i + 1] if i + 1 < len(segs) else None
        joins_l = bool(
            prev
            and prev.get("ghost")
            and prev.get("window_id") == seg.get("window_id")
        )
        joins_r = bool(
            nxt
            and nxt.get("ghost")
            and nxt.get("window_id") == seg.get("window_id")
        )
        real_ids = [s.get("window_id") for s in real]
        cap_l = bool(real_ids) and seg.get("window_id") == real_ids[0] and not joins_l
        cap_r = bool(real_ids) and seg.get("window_id") == real_ids[-1] and not joins_r
        radius = BAR_H / 2 if (cap_l or cap_r or seg.get("ghost") or appear < 0.999) else 0.0
        if seg.get("ghost"):
            r, g, b, a = _parse_color("ghost")
            _round_rect(cr, sx, bar_y, w, BAR_H, radius)
            cr.set_source_rgba(r, g, b, a * alpha)
            cr.fill()
        else:
            palette = _seg_palette(seg)
            if len(palette) > 1:
                _fill_round_rect_palette(
                    cr, sx, bar_y, w, BAR_H, radius, palette, alpha
                )
            else:
                color = palette[0][0] if palette else str(seg.get("color") or "")
                if color.startswith("#"):
                    _fill_glassy_solid(
                        cr, sx, bar_y, w, BAR_H, radius, color, alpha
                    )
                else:
                    r, g, b, a = _parse_color(color)
                    _round_rect(cr, sx, bar_y, w, BAR_H, radius)
                    cr.set_source_rgba(r, g, b, a * alpha)
                    cr.fill()
        if (
            appear >= 0.999
            and nxt
            and not nxt.get("ghost")
            and not nxt.get("col_tick")
            and nxt.get("column") is not None
            and nxt.get("column") == seg.get("column")
        ):
            cr.set_source_rgba(1, 1, 1, 0.92)
            cr.rectangle(sx + w - 2, bar_y, 1.5, BAR_H)
            cr.fill()

    def _rail_offset_geom(self, geom: dict, view_w: float, live: bool = True) -> float:
        """Scroll the rail so the current viewport sits in the middle of the track."""
        total = float(geom["total"])
        if total <= view_w + 0.5:
            if live:
                self._last_rail_offset = 0.0
            return 0.0
        scale = float(geom["scale"])
        content_l = float(geom["content_l"])
        v0 = (float(geom["clip_l"]) - content_l) * scale
        v1 = (float(geom["clip_r"]) - content_l) * scale
        offset = (v0 + v1) / 2.0 - view_w / 2.0
        offset = min(max(0.0, offset), max(0.0, total - view_w))
        if live:
            self._last_rail_offset = offset
        return offset

    def _rail_offset(self, segs: list[dict], total: float, live: bool = True) -> float:
        """Scroll the rail content so niri's current viewport sits in the middle."""
        if total <= RAIL_MAX_W:
            if live:
                self._last_rail_offset = 0.0
            return 0.0
        x = 0.0
        v0: float | None = None
        v1 = 0.0
        for seg in segs:
            w = self._seg_slot_width(seg)
            if (
                w > 0
                and not seg.get("ghost")
                and not seg.get("col_tick")
                and not seg.get("spacer")
                and float(seg.get("percent") or 0) > 0
            ):
                if v0 is None:
                    v0 = x
                v1 = x + w
            x += w
        if v0 is None:
            # Centering can leave no on-screen column in the strip; keep last offset.
            # Normal scroll/pan stays on content edges (niri free_pan clamps).
            offset = self._last_rail_offset
        else:
            offset = (v0 + v1) / 2.0 - RAIL_MAX_W / 2.0
        # Only scroll the strip when content exceeds 4 viewports (useful width).
        offset = min(max(0.0, offset), max(0.0, total - RAIL_MAX_W))
        if live:
            self._last_rail_offset = offset
        return offset


def main() -> int:
    # Waybar custom module: one JSON line, then stay alive for the canvas.
    sys.stdout.write('{"text":"","class":"hidden","tooltip":""}\n')
    sys.stdout.flush()
    app = Rail()
    return app.run(None)


if __name__ == "__main__":
    raise SystemExit(main())
