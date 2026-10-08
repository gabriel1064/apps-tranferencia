#!/usr/bin/env python3
"""Compute visible Niri column/window layout for Waybar strip + app slots."""
from __future__ import annotations

import ctypes
import ctypes.util
import json
import os
import re
import select
import shutil
import struct
import subprocess
import sys
import time
from collections import defaultdict
from typing import Any

# The fork's client: the stock `niri msg` drops fields it does not know (is_minimized, ...).
NIRI_BIN = shutil.which("niri-live-reflow") or os.path.expanduser("~/.local/bin/niri-live-reflow")

CACHE_DIR = os.path.expanduser("~/.cache/waybar-niri")
STATE_PATH = os.path.join(CACHE_DIR, "state.json")
WIDTHS_CSS = os.path.join(CACHE_DIR, "widths.css")
PX_LADDER_CSS = os.path.join(CACHE_DIR, "px-ladder.css")
ANIM_LOCK = os.path.join(CACHE_DIR, "anim.lock")
# Icon row design width. 100% of the in-viewport strip == this many px.
ROW_PX = 255.0
MIN_SLOT_PX = 7.0
COMPACT_PX = 30.0
VIEWPORT_BAR_PX = 3.0
# Dedicated white tick between columns (px ladder width).
COL_TICK_PX = 2
# Horizontal footprint of the tick in the strip (width + L/R margins in CSS).
COL_GAP_PX = COL_TICK_PX + 5
GAPS_FALLBACK = 16.0
# Width is an instant class swap (pxN). Never CSS-transition min-width:
# Waybar strips all classes on each JSON line, so a width tween flashes/collapses.
# Ladder steps are quantized to 2px so 1px IPC noise does not flip-flop classes.
ANIM_MS = 0.0
ANIM_MIN_DELTA_PX = 12.0
PX_LADDER_MAX = 320
_IPC_SNAP: dict[str, Any] | None = None
COLUMN_COLORS = [
    ("#5b8def", "#3d6fd4", "#2a56b0"),
    ("#6bcf8e", "#45b56f", "#2f9454"),
    ("#f0c14a", "#d4a62e", "#b0891a"),
    ("#e8796a", "#d45a4a", "#b33f32"),
    ("#b07ce8", "#9359d1", "#7640b3"),
    ("#5ecfcf", "#3aafaf", "#288f8f"),
    ("#f09ac0", "#d66f9e", "#b34f7e"),
    ("#9aa4b2", "#7b8494", "#5e6674"),
]

# app_key → [(#rrggbb, weight), ...] weights sum ≈ 1; extracted from icon pixels.
_ICON_PALETTE: dict[str, list[tuple[str, float]]] = {}
_ICON_PATH_CACHE: dict[str, str] = {}
_DESKTOP_ICON_NAMES: dict[str, str] | None = None
_GTK_OK: bool | None = None

BLOCK = "█"
STRIP_CELLS = 48


def _ensure_gtk() -> bool:
    global _GTK_OK
    if _GTK_OK is not None:
        return _GTK_OK
    try:
        import gi

        gi.require_version("Gtk", "4.0")
        gi.require_version("Gdk", "4.0")
        gi.require_version("GdkPixbuf", "2.0")
        from gi.repository import GdkPixbuf  # noqa: F401

        # Pixbuf works without a display; IconTheme prefers one when present.
        _GTK_OK = True
    except Exception:
        _GTK_OK = False
    return _GTK_OK


def _gdk_display_available() -> bool:
    if not _ensure_gtk():
        return False
    try:
        from gi.repository import Gdk

        return Gdk.Display.get_default() is not None
    except Exception:
        return False


def _desktop_icon_names() -> dict[str, str]:
    global _DESKTOP_ICON_NAMES
    if _DESKTOP_ICON_NAMES is not None:
        return _DESKTOP_ICON_NAMES
    found: dict[str, str] = {}
    for root in (
        "/usr/share/applications",
        os.path.expanduser("~/.local/share/applications"),
    ):
        if not os.path.isdir(root):
            continue
        try:
            names = os.listdir(root)
        except OSError:
            continue
        for fname in names:
            if not fname.endswith(".desktop"):
                continue
            path = os.path.join(root, fname)
            icon = None
            wm = None
            app_id = fname[:-8]
            try:
                with open(path, encoding="utf-8", errors="ignore") as fh:
                    for line in fh:
                        if line.startswith("Icon="):
                            icon = line.split("=", 1)[1].strip()
                        elif line.startswith("StartupWMClass="):
                            wm = line.split("=", 1)[1].strip()
            except OSError:
                continue
            if not icon:
                continue
            for key in (app_id, app_id.lower(), wm or "", (wm or "").lower()):
                if key:
                    found.setdefault(key, icon)
    _DESKTOP_ICON_NAMES = found
    return found


def _resolve_icon_path(app_id: str | None) -> str | None:
    if not app_id:
        return None
    cached = _ICON_PATH_CACHE.get(app_id)
    if cached is not None:
        return cached or None
    names: list[str] = []
    desk = _desktop_icon_names()
    for key in (app_id, app_id.lower()):
        if key in desk:
            names.append(desk[key])
    tail = app_id.split(".")[-1]
    names.extend((app_id, app_id.lower(), tail, tail.lower()))
    path = None
    if _ensure_gtk() and _gdk_display_available():
        from gi.repository import Gdk, Gtk

        display = Gdk.Display.get_default()
        theme = Gtk.IconTheme.get_for_display(display) if display else None
        seen: set[str] = set()
        for name in names:
            if not name or name in seen:
                continue
            seen.add(name)
            if os.path.isabs(name) and os.path.isfile(name):
                path = name
                break
            if theme is not None and theme.has_icon(name):
                paintable = theme.lookup_icon(
                    name, None, 128, 1, Gtk.TextDirection.NONE, 0
                )
                file = paintable.get_file()
                if file is not None and file.get_path():
                    path = file.get_path()
                    break
    if path is None:
        theme_roots = (
            os.path.expanduser("~/.local/share/icons"),
            "/usr/share/icons",
            "/usr/share/pixmaps",
        )
        theme_names = (
            os.environ.get("ICON_THEME", ""),
            "breeze-dark",
            "breeze",
            "Adwaita",
            "hicolor",
        )
        sizes = ("256x256", "128x128", "96x96", "64x64", "48x48", "scalable", "32x32")
        cats = ("apps", "places", "devices", "mimetypes", "status")
        for name in names:
            if not name:
                continue
            if os.path.isabs(name) and os.path.isfile(name):
                path = name
                break
            for troot in theme_roots:
                if troot.endswith("pixmaps"):
                    for ext in (".png", ".svg", ".xpm"):
                        cand = os.path.join(troot, name + ext)
                        if os.path.isfile(cand):
                            path = cand
                            break
                    if path:
                        break
                    continue
                for tname in theme_names:
                    if not tname:
                        continue
                    for size in sizes:
                        for cat in cats:
                            for ext in (".png", ".svg", ".xpm"):
                                cand = os.path.join(
                                    troot, tname, size, cat, name + ext
                                )
                                if os.path.isfile(cand):
                                    path = cand
                                    break
                            if path:
                                break
                        if path:
                            break
                    if path:
                        break
                if path:
                    break
            if path:
                break
    _ICON_PATH_CACHE[app_id] = path or ""
    return path


def _rgb_to_hex(r: int, g: int, b: int) -> str:
    return f"#{r:02x}{g:02x}{b:02x}"


def _load_icon_rgba(path: str, size: int = 96) -> tuple[bytes, int, int] | None:
    """Return (RGBA bytes, w, h) or None."""
    if _ensure_gtk():
        try:
            from gi.repository import GdkPixbuf

            pb = GdkPixbuf.Pixbuf.new_from_file_at_size(path, size, size)
            if not pb.get_has_alpha():
                pb = pb.add_alpha(False, 0, 0, 0)
            w, h = pb.get_width(), pb.get_height()
            row = pb.get_rowstride()
            n_ch = pb.get_n_channels()
            src = pb.get_pixels()
            out = bytearray(w * h * 4)
            for y in range(h):
                for x in range(w):
                    i = y * row + x * n_ch
                    o = (y * w + x) * 4
                    out[o] = src[i]
                    out[o + 1] = src[i + 1]
                    out[o + 2] = src[i + 2]
                    out[o + 3] = src[i + 3] if n_ch >= 4 else 255
            return (bytes(out), w, h)
        except Exception:
            pass
    try:
        from PIL import Image

        im = Image.open(path).convert("RGBA")
        im.thumbnail((size, size))
        return (im.tobytes(), im.width, im.height)
    except Exception:
        return None


def _pixel_chroma(r: int, g: int, b: int) -> float:
    """0..1 saturation proxy (max−min)/max. Near-grey / near-black → low."""
    mx = max(r, g, b)
    if mx < 1:
        return 0.0
    return (mx - min(r, g, b)) / float(mx)


def _pixel_luma(r: int, g: int, b: int) -> float:
    """0..1 perceived brightness."""
    return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0


def _extract_palette_from_rgba(
    pixels: bytes, w: int, h: int, *, max_colors: int = 5
) -> list[tuple[str, float]]:
    """Quantize opaque pixels; prefer bright chromatic accents over dark chrome.

    Icons like Mission Center fill large areas with Adwaita dark purple
    (``#241f31``). Area-only ranking paints that on the rail; weight by
    chroma × luma so the cyan/blue waveform wins.
    """
    buckets: dict[tuple[int, int, int], int] = {}
    step = 32  # 8 levels per channel
    for i in range(0, w * h * 4, 4):
        a = pixels[i + 3]
        if a < 40:
            continue
        r, g, b = pixels[i], pixels[i + 1], pixels[i + 2]
        mx, mn = max(r, g, b), min(r, g, b)
        if mx < 28:
            continue
        if mn > 235:
            continue
        # Dark window chrome (often purple-tinted Adwaita) — never an accent.
        if mx < 72:
            continue
        chroma = _pixel_chroma(r, g, b)
        if chroma < 0.14 and mx < 180:
            continue
        key = (r // step * step, g // step * step, b // step * step)
        buckets[key] = buckets.get(key, 0) + 1
    if not buckets:
        return []
    scored: list[tuple[tuple[int, int, int], float, int]] = []
    for key, count in buckets.items():
        r, g, b = key
        rr = min(255, r + step // 2)
        gg = min(255, g + step // 2)
        bb = min(255, b + step // 2)
        chroma = _pixel_chroma(rr, gg, bb)
        luma = _pixel_luma(rr, gg, bb)
        # Accents: saturated + not murky. Tiny bright lines still beat huge dark fills.
        score = count * (0.2 + chroma) ** 2 * (0.35 + luma) ** 2
        scored.append((key, score, count))
    scored.sort(key=lambda t: t[1], reverse=True)
    total_score = sum(s for _, s, _ in scored) or 1.0
    picked: list[tuple[str, float]] = []
    for (r, g, b), score, count in scored:
        share = score / total_score
        if share < 0.035 and picked:
            continue
        if picked and count < max(4, (w * h) // 500):
            continue
        picked.append(
            (
                _rgb_to_hex(
                    min(255, r + step // 2),
                    min(255, g + step // 2),
                    min(255, b + step // 2),
                ),
                share,
            )
        )
        if len(picked) >= max_colors:
            break
    if not picked:
        return []
    s = sum(wt for _, wt in picked) or 1.0
    return [(c, wt / s) for c, wt in picked]


def icon_palette(app_id: str | None, key: str | None = None) -> list[tuple[str, float]]:
    """Main logo colors with area weights (sum 1).

    Cached per real ``app_id`` (and icon path), NOT by ``app_key`` — otherwise
    Thorium browser / YouTube / WhatsApp PWAs all collapse to ``browser-chrome``
    and steal each other's red/blue.
    """
    del key  # kept for call-site compat; must not drive the cache
    aid = (app_id or "").strip()
    cache_key = aid.lower() or "?"
    if cache_key in _ICON_PALETTE:
        return _ICON_PALETTE[cache_key]
    path = _resolve_icon_path(aid or None)
    if path:
        path_key = f"path:{path}"
        if path_key in _ICON_PALETTE:
            palette = _ICON_PALETTE[path_key]
            _ICON_PALETTE[cache_key] = palette
            return palette
    palette: list[tuple[str, float]] = []
    if path:
        loaded = _load_icon_rgba(path)
        if loaded is not None:
            pixels, w, h = loaded
            palette = _extract_palette_from_rgba(pixels, w, h)
        _ICON_PALETTE[f"path:{path}"] = palette
    _ICON_PALETTE[cache_key] = palette
    return palette


def palette_gradient_stops(
    palette: list[tuple[str, float]],
) -> list[tuple[float, str]]:
    """Offset 0..1 → color. Midpoints keep area share; neighbors blend smoothly."""
    if not palette:
        return []
    if len(palette) == 1:
        c = palette[0][0]
        return [(0.0, c), (1.0, c)]
    stops: list[tuple[float, str]] = [(0.0, palette[0][0])]
    acc = 0.0
    for color, weight in palette:
        mid = acc + weight * 0.5
        stops.append((max(0.0, min(1.0, mid)), color))
        acc += weight
    stops.append((1.0, palette[-1][0]))
    # Drop duplicate offsets (tiny weights) keeping the later color.
    dedup: list[tuple[float, str]] = []
    for off, color in stops:
        if dedup and abs(dedup[-1][0] - off) < 1e-4:
            dedup[-1] = (off, color)
        else:
            dedup.append((off, color))
    return dedup


def palette_fill_css(palette: list[tuple[str, float]], fallback: str) -> str:
    """CSS background: solid or smooth proportional horizontal gradient."""
    if not palette:
        return fallback
    if len(palette) == 1:
        return palette[0][0]
    parts = [
        f"{color} {off * 100.0:.2f}%" for off, color in palette_gradient_stops(palette)
    ]
    return f"linear-gradient(90deg, {', '.join(parts)})"


def niri_json(cmd: list[str]) -> Any:
    env = os.environ.copy()
    # Prefer the live compositor binary if available.
    out = subprocess.check_output([NIRI_BIN, "msg", "--json", *cmd], text=True, env=env)
    return json.loads(out)


_CFG_CACHE: dict[str, Any] = {
    "niri_mtime": None,
    "noctalia_mtime": None,
    "gaps": GAPS_FALLBACK,
    "center": "never",
    "struts": (0.0, 0.0),
    "rail": (0.0, 0.0),
}


def _refresh_layout_caches() -> None:
    """Read niri/noctalia layout settings only when their files change."""
    cfg = os.path.expanduser("~/.config/niri/config.kdl")
    try:
        niri_mtime = os.path.getmtime(cfg)
    except OSError:
        niri_mtime = None
    if niri_mtime != _CFG_CACHE["niri_mtime"]:
        _CFG_CACHE["niri_mtime"] = niri_mtime
        _CFG_CACHE["gaps"] = GAPS_FALLBACK
        _CFG_CACHE["center"] = "never"
        _CFG_CACHE["struts"] = (0.0, 0.0)
        try:
            with open(cfg, encoding="utf-8") as f:
                text = f.read()
            for line in text.splitlines():
                s = line.strip()
                if s.startswith("gaps ") and not s.startswith("//"):
                    parts = s.split()
                    if len(parts) >= 2:
                        try:
                            _CFG_CACHE["gaps"] = float(parts[1])
                        except ValueError:
                            pass
                if s.startswith("center-focused-column") and not s.startswith("//"):
                    parts = s.split('"')
                    if len(parts) >= 2:
                        _CFG_CACHE["center"] = parts[1].strip().lower()
            m = re.search(r"struts\s*\{([^}]*)\}", text, re.S)
            if m:
                body = m.group(1)
                lm = re.search(r"\bleft\s+([0-9.]+)", body)
                rm = re.search(r"\bright\s+([0-9.]+)", body)
                left = float(lm.group(1)) if lm else 0.0
                right = float(rm.group(1)) if rm else 0.0
                _CFG_CACHE["struts"] = (left, right)
        except OSError:
            pass

    noctalia = os.path.expanduser("~/.local/state/noctalia/settings.toml")
    try:
        noc_mtime = os.path.getmtime(noctalia)
    except OSError:
        noc_mtime = None
    if noc_mtime != _CFG_CACHE["noctalia_mtime"]:
        _CFG_CACHE["noctalia_mtime"] = noc_mtime
        _CFG_CACHE["rail"] = (0.0, 0.0)
        try:
            text = open(noctalia, encoding="utf-8").read()
        except OSError:
            return
        m = re.search(r"\[bar\.default\](.*?)(?:\n\[|\Z)", text, re.S)
        if not m:
            return
        fields: dict[str, str] = {}
        for line in m.group(1).splitlines():
            s = line.strip()
            if not s or s.startswith("#") or "=" not in s:
                continue
            k, v = s.split("=", 1)
            fields[k.strip()] = v.strip().strip('"')
        if fields.get("reserve_space", "true").lower() not in ("true", "1"):
            return
        try:
            thickness = float(fields.get("thickness") or 0)
        except ValueError:
            thickness = 0.0
        pos = (fields.get("position") or "").lower()
        if pos == "left":
            _CFG_CACHE["rail"] = (thickness, 0.0)
        elif pos == "right":
            _CFG_CACHE["rail"] = (0.0, thickness)


def load_gaps() -> float:
    """Best-effort gaps from niri config; fallback to 16."""
    _refresh_layout_caches()
    return float(_CFG_CACHE["gaps"])


def app_key(app_id: str | None) -> str:
    """Stable identity for color: two Thorium/Chrome windows share a key."""
    aid = (app_id or "").strip().lower()
    if not aid:
        return "?"
    if "thorium" in aid or "chrom" in aid:
        return "browser-chrome"
    if "firefox" in aid:
        return "firefox"
    if aid.startswith("org.") or aid.startswith("com."):
        return aid.split(".")[-1]
    # strip instance / profile suffixes after the first hyphen-run of hashes
    base = aid.split("-")[0]
    return base or aid


def app_label(app_id: str | None, title: str | None) -> str:
    aid = (app_id or "").strip()
    if not aid:
        return (title or "?")[:2].upper() or "?"
    # Strip chromium/thorium profile suffixes
    if "thorium" in aid.lower() or "chrom" in aid.lower():
        return "󰊯"
    mapping = {
        "cursor": "󰨞",
        "code": "󰨞",
        "code-url-handler": "󰨞",
        "firefox": "󰈹",
        "firefox-esr": "󰈹",
        "org.mozilla.firefox": "󰈹",
        "Alacritty": "",
        "kitty": "",
        "foot": "",
        "org.gnome.Nautilus": "󰉋",
        "thunar": "󰉋",
        "org.gnome.TextEditor": "󰷈",
        "spotify": "󰓇",
        "Slack": "󰒱",
        "discord": "󰙯",
        "org.telegram.desktop": "",
        "steam": "󰓓",
        "mpv": "󰐹",
        "org.kde.dolphin": "󰉋",
    }
    if aid in mapping:
        return mapping[aid]
    # desktop-ish id: take last component
    base = aid.split(".")[-1]
    if len(base) <= 3:
        return base.upper()
    return base[:2].upper()


def shade_index(idx: int, total: int) -> int:
    """Which of the 3 hue variants a stacked tile uses inside its column."""
    if total <= 1 or idx <= 0:
        return 0
    if total == 2:
        return 1
    # 3+
    return 2 if idx >= total - 1 else 1


def shade(colors: tuple[str, str, str], idx: int, total: int) -> str:
    return colors[shade_index(idx, total)]


def col_tile_w(columns: dict[int, list[dict]], c: int) -> float:
    tiles = columns[c]
    return max(float(t["layout"]["tile_size"][0]) for t in tiles)


def col_span(columns: dict[int, list[dict]], c: int, gaps: float) -> float:
    """Column width including trailing gap (matches niri packing)."""
    return col_tile_w(columns, c) + gaps


def column_positions(
    columns: dict[int, list[dict]], gaps: float
) -> dict[int, float]:
    """Left edge of each column in scrolling layout coords (gap after each)."""
    ordered = sorted(columns.keys())
    pos: dict[int, float] = {}
    x = 0.0
    for c in ordered:
        pos[c] = x
        x += col_span(columns, c, gaps)
    return pos


def load_center_focused() -> str:
    """Read layout center-focused-column from niri config (never|always|on-overflow)."""
    _refresh_layout_caches()
    return str(_CFG_CACHE["center"])


def load_horizontal_struts() -> tuple[float, float]:
    """Niri layout struts left/right (logical px). Top/bottom do not affect width."""
    _refresh_layout_caches()
    left, right = _CFG_CACHE["struts"]
    return float(left), float(right)


def noctalia_side_reserve() -> tuple[float, float]:
    """Exclusive zone of the Noctalia rail (reserve_space), left and right."""
    _refresh_layout_caches()
    left, right = _CFG_CACHE["rail"]
    return float(left), float(right)


def working_area(output_w: float) -> tuple[float, float]:
    """(origin_x, width) of the area niri actually tiles into.

    Exclusive rails and left/right struts shrink this. Clipping against the
    full output counts a slice of the next column that is hidden beside the rail.
    """
    rail_l, rail_r = noctalia_side_reserve()
    strut_l, strut_r = load_horizontal_struts()
    origin = rail_l + strut_l
    width = max(320.0, output_w - origin - rail_r - strut_r)
    return origin, width


def niri_fit_view_pos(
    view_pos: float,
    col_x: float,
    col_w: float,
    working_x: float,
    working_w: float,
    gaps: float,
) -> float:
    """niri compute_new_view_offset + fit: scroll the least so the column fits.

    Returns target_view_pos (layout x of the workspace view's left edge).
    If the column is already fully on screen (with gap padding), view_pos stays.
    """
    if working_w <= col_w:
        return col_x - working_x
    padding = min(gaps, max(0.0, (working_w - col_w) / 2.0))
    cur_x = view_pos + working_x
    new_x = col_x - padding
    new_right = col_x + col_w + padding
    if cur_x <= new_x and new_right <= cur_x + working_w:
        return view_pos
    dist_left = abs(cur_x - new_x)
    dist_right = abs((cur_x + working_w) - new_right)
    if dist_left <= dist_right:
        return col_x - padding - working_x
    return col_x - (working_w - padding - col_w) - working_x


def niri_center_view_pos(
    col_x: float, col_w: float, working_x: float, working_w: float
) -> float:
    if working_w <= col_w:
        return col_x - working_x
    return col_x - (working_w - col_w) / 2.0 - working_x


def _layout_abs_view_pos(layout: dict[str, Any]) -> float | None:
    """Absolute view_pos if niri ever exposes it on a window layout object."""
    for key in ("scrolling_view_pos", "view_pos", "workspace_view_pos"):
        raw = layout.get(key)
        if raw is None:
            continue
        try:
            if isinstance(raw, (list, tuple)):
                return float(raw[0])
            return float(raw)
        except (TypeError, ValueError, IndexError):
            continue
    return None


def ipc_view_from_geometry(
    columns: dict[int, list[dict]],
    gaps: float,
    working_x: float,
) -> tuple[float, float, str] | None:
    """Prefer live IPC geometry over fit heuristics.

    niri-live-reflow fills ``tile_pos_in_workspace_view`` for scrolling tiles
    (position relative to the current workspace view, including during
    trackpad view-offset gestures). That is the source of truth — never fall
    through to ``niri_fit_view_pos`` when any of these fields is present.
    """
    if not columns:
        return None
    positions = column_positions(columns, gaps)
    ordered = sorted(columns.keys())

    for c in ordered:
        for t in columns[c]:
            layout = t.get("layout") or {}
            pos = layout.get("tile_pos_in_workspace_view")
            if pos is not None:
                view_left = positions[c] - float(pos[0])
                return view_left, view_left - working_x, "tile_pos"

    for c in ordered:
        for t in columns[c]:
            layout = t.get("layout") or {}
            abs_pos = _layout_abs_view_pos(layout)
            if abs_pos is not None:
                # Absolute layout x of the view's left edge (niri view_pos).
                return abs_pos, abs_pos - working_x, "ipc-view"

            # Relative offset from the column's layout x (if ever published).
            off = layout.get("view_offset")
            if off is not None and c in positions:
                try:
                    if isinstance(off, (list, tuple)):
                        rel = float(off[0])
                    else:
                        rel = float(off)
                except (TypeError, ValueError, IndexError):
                    continue
                view_left = positions[c] + rel
                return view_left, view_left - working_x, "ipc-view-offset"
    return None


def resolve_view(
    columns: dict[int, list[dict]],
    focused_col: int | None,
    gaps: float,
    working_x: float,
    working_w: float,
    center_mode: str,
    prev: dict[str, Any] | None,
    ws_id: Any,
) -> tuple[float, float, str]:
    """Return (view_left, view_pos, mode) in scrolling-layout coordinates.

    view_left is the layout x at the left edge of the workspace view (clip
    origin for the strip). When IPC publishes tile_pos / view offset, that
    wins and ``niri_fit_view_pos`` is never applied (fit fights the finger).

    Fallback (stock niri without tile_pos): while focus is unchanged we keep
    the last view_pos; fit/center only on focus/workspace change or cold start.
    """
    if not columns:
        return 0.0, 0.0, "empty"

    # IPC offset is authoritative — do not call niri_fit_view_pos.
    live = ipc_view_from_geometry(columns, gaps, working_x)
    if live is not None:
        return live

    positions = column_positions(columns, gaps)
    ordered = sorted(columns.keys())

    if focused_col is None or focused_col not in positions:
        focused_col = ordered[0]

    prev_focus = prev.get("focused_column") if prev else None
    same_ws = bool(prev and prev.get("workspace_id") == ws_id)
    tracked = bool(
        same_ws
        and prev.get("visibility_mode") in ("niri-view", "tile_pos", "ipc-view", "ipc-view-offset")
        and prev.get("view_pos") is not None
    )
    focus_changed = prev_focus not in (None, focused_col)

    if tracked:
        view_pos = float(prev["view_pos"])
        mode = "niri-view"
    else:
        # Cold start: niri opens the workspace with column 1 padded at the left.
        pad0 = min(gaps, max(0.0, (working_w - col_tile_w(columns, ordered[0])) / 2.0))
        view_pos = -pad0 - working_x
        mode = "niri-view"

    # Free scroll / layout-only tick with unchanged focus: keep last view_pos.
    # Applying fit here is what causes the strip hitch against the trackpad.
    if tracked and same_ws and not focus_changed:
        return view_pos + working_x, view_pos, mode

    col_x = positions[focused_col]
    col_w = col_tile_w(columns, focused_col)

    if center_mode == "always":
        view_pos = niri_center_view_pos(col_x, col_w, working_x, working_w)
    elif center_mode == "on-overflow" and focus_changed:
        try:
            src = int(prev_focus)
        except (TypeError, ValueError):
            src = focused_col
        if src in positions and src != focused_col:
            src_x = positions[src]
            src_w = col_tile_w(columns, src)
            if src_x < col_x:
                total = col_x - src_x + col_w
            else:
                total = src_x - col_x + src_w
            total += gaps * 2.0
            if total > working_w:
                view_pos = niri_center_view_pos(col_x, col_w, working_x, working_w)
            else:
                view_pos = niri_fit_view_pos(
                    view_pos, col_x, col_w, working_x, working_w, gaps
                )
        else:
            view_pos = niri_fit_view_pos(
                view_pos, col_x, col_w, working_x, working_w, gaps
            )
    else:
        view_pos = niri_fit_view_pos(
            view_pos, col_x, col_w, working_x, working_w, gaps
        )

    view_left = view_pos + working_x
    return view_left, view_pos, mode


def view_clip_x(working_x: float, usable_w: float) -> tuple[float, float]:
    """Inclusive clip range for tile_pos / scroll_x coordinates.

    ``tile_pos_in_workspace_view`` is in output/view space. The area where niri
    actually draws tiled columns is ``[working_x, working_x + usable_w]`` (after
    exclusive rails + horizontal struts), not ``[0, usable_w]``. Clipping against
    the latter invents a phantom right overhang (~rail thickness) on fully
    on-screen columns.
    """
    return working_x, working_x + usable_w


def column_overhangs(
    left: float,
    tile_w: float,
    clip_l: float,
    clip_r: float,
    gaps: float,
) -> tuple[float, float, float]:
    """Return (vis, over_l, over_r) for a tile in view coordinates.

    If the visible span is within ``gaps``/1px of ``tile_w``, treat as fully
    on-screen and zero both overhangs (no ghost).
    """
    right = left + tile_w
    vis = max(0.0, min(right, clip_r) - max(left, clip_l))
    over_l = max(0.0, min(right, clip_l) - left)
    over_r = max(0.0, right - max(left, clip_r))
    # Fully visible (rounding / gap padding): never emit ghost stubs.
    tol = max(1.0, float(gaps))
    if vis + tol >= tile_w:
        return tile_w, 0.0, 0.0
    return vis, over_l, over_r


def columns_visibility(
    columns: dict[int, list[dict]],
    gaps: float,
    usable_w: float,
    scroll_x: float,
    working_x: float = 0.0,
) -> tuple[list[int], list[int], list[int], dict[int, float]]:
    """Split columns into off_left / visible / off_right by clipping to the view."""
    if not columns:
        return [], [], [], {}
    positions = column_positions(columns, gaps)
    ordered = sorted(columns.keys())
    clip_l, clip_r = view_clip_x(working_x, usable_w)
    visible_w: dict[int, float] = {}
    for c in ordered:
        tile_w = col_tile_w(columns, c)
        left = positions[c] - scroll_x
        vis, _over_l, _over_r = column_overhangs(left, tile_w, clip_l, clip_r, gaps)
        if vis > 0.5:
            visible_w[c] = vis

    vis_cols = [c for c in ordered if c in visible_w]
    off_left = [c for c in ordered if c not in visible_w and (not vis_cols or c < vis_cols[0])]
    off_right = [c for c in ordered if c not in visible_w and vis_cols and c > vis_cols[-1]]
    # Orphan columns between gaps shouldn't happen with contiguous layout; park right.
    for c in ordered:
        if c not in visible_w and c not in off_left and c not in off_right:
            off_right.append(c)
    return off_left, vis_cols, off_right, visible_w


def make_entry(
    t: dict,
    *,
    key: str,
    color_i: int,
    stack_i: int,
    tile_i: int,
    stack_n: int,
    pct: float,
    col_share: float,
    h_share: float,
    offscreen: str | None = None,
    floating: bool = False,
) -> dict[str, Any]:
    colors = COLUMN_COLORS[color_i % len(COLUMN_COLORS)]
    fallback = shade(colors, tile_i, stack_n)
    palette = icon_palette(t.get("app_id"))
    # Dominant logo color for single-fill consumers; full mix for the strip.
    color = palette[0][0] if palette else fallback
    fill = palette_fill_css(palette, fallback)
    return {
        "window_id": t["id"],
        "app_id": t.get("app_id"),
        "app_key": key,
        "title": t.get("title") or "",
        "column": t.get("_col"),
        "row": t.get("_row"),
        "group": color_i,
        "stack_group": stack_i,
        "group_size": stack_n,
        "tile_index": tile_i,
        "percent": round(pct, 3),
        "column_share": round(col_share * 100.0, 3),
        "height_share": round(h_share * 100.0, 3),
        "color": color,
        "fill": fill,
        "palette": [[c, round(w, 4)] for c, w in palette],
        "shade": shade_index(tile_i, stack_n),
        "is_focused": bool(t.get("is_focused")),
        "is_urgent": bool(t.get("is_urgent")),
        "label": app_label(t.get("app_id"), t.get("title")),
        "offscreen": offscreen,
        "compact": bool(offscreen) or floating,
        "floating": floating,
        "spacer": False,
        "col_tick": False,
        "col_gap": False,
        "ghost": False,
    }


def make_spacer(side: str, *, percent: float = 0.0) -> dict[str, Any]:
    return {
        "window_id": None,
        "app_id": None,
        "app_key": None,
        "title": "",
        "column": None,
        "row": None,
        "group": 0,
        "stack_group": -1,
        "group_size": 1,
        "tile_index": 0,
        "percent": round(percent, 3),
        "column_share": 0.0,
        "height_share": 0.0,
        "color": "transparent",
        "shade": 0,
        "is_focused": False,
        "is_urgent": False,
        "label": "",
        "offscreen": side,
        "compact": True,
        "floating": False,
        "spacer": True,
        "col_tick": False,
        "col_gap": False,
        "ghost": False,
    }


def make_col_tick() -> dict[str, Any]:
    """White overhanging tick sitting in the gap between two column colors."""
    return {
        "window_id": None,
        "app_id": None,
        "app_key": None,
        "title": "",
        "column": None,
        "row": None,
        "group": 0,
        "stack_group": -1,
        "group_size": 1,
        "tile_index": 0,
        "percent": 0.0,
        "column_share": 0.0,
        "height_share": 0.0,
        "color": "#ffffff",
        "shade": 0,
        "is_focused": False,
        "label": "",
        "offscreen": None,
        "compact": False,
        "floating": False,
        "spacer": False,
        "col_tick": True,
        "col_gap": False,
        "ghost": False,
    }


def make_app_gap(*, percent: float = 0.0) -> dict[str, Any]:
    """Invisible spacer in the icon row.

    percent=0 → fixed width matching the column tick footprint.
    percent>0 → proportional width (e.g. to mirror a column ghost).
    """
    return {
        "window_id": None,
        "app_id": None,
        "app_key": None,
        "title": "",
        "column": None,
        "row": None,
        "group": 0,
        "stack_group": -1,
        "group_size": 1,
        "tile_index": 0,
        "percent": round(percent, 3),
        "column_share": 0.0,
        "height_share": 0.0,
        "color": "transparent",
        "shade": 0,
        "is_focused": False,
        "label": "",
        "offscreen": None,
        "compact": False,
        "floating": False,
        "spacer": False,
        "col_tick": False,
        "col_gap": True,
        "ghost": False,
    }


def make_ghost(entry: dict[str, Any]) -> dict[str, Any]:
    """Gray low-opacity underline under an off-screen (or floating) app icon."""
    g = dict(entry)
    g["ghost"] = True
    g["spacer"] = False
    g["col_tick"] = False
    g["col_gap"] = False
    g["color"] = "ghost"
    g["fill"] = "rgba(160, 168, 180, 0.28)"
    g["palette"] = []
    return g


def _ipc_snapshot(snapshot: dict[str, Any] | None = None) -> dict[str, Any]:
    """Reuse one niri query across animation frames — IPC is the lag source."""
    global _IPC_SNAP
    if snapshot is not None:
        _IPC_SNAP = snapshot
        return snapshot
    snap = {
        "windows": niri_json(["windows"]),
        "workspaces": niri_json(["workspaces"]),
        "focused_output": niri_json(["focused-output"]),
    }
    _IPC_SNAP = snap
    return snap


def compute_state(
    *,
    forced_view_pos: float | None = None,
    snapshot: dict[str, Any] | None = None,
    prev_state: dict[str, Any] | None = None,
) -> dict[str, Any]:
    snap = _ipc_snapshot(snapshot)
    windows = snap["windows"]
    workspaces = snap["workspaces"]
    focused_output = snap["focused_output"]

    output_name = focused_output.get("name")
    logical = focused_output.get("logical") or {}
    output_w = float(logical.get("width") or 1920)
    output_h = float(logical.get("height") or 1080)
    # Working area is where niri actually places tiles (output minus the
    # exclusive left rail and horizontal struts).
    working_x, usable_w = working_area(output_w)
    gaps = load_gaps()

    active_ws = None
    for ws in workspaces:
        if ws.get("is_focused") or (ws.get("is_active") and ws.get("output") == output_name):
            if ws.get("is_focused"):
                active_ws = ws
                break
            if active_ws is None and ws.get("output") == output_name:
                active_ws = ws
    if active_ws is None:
        for ws in workspaces:
            if ws.get("output") == output_name and ws.get("is_active"):
                active_ws = ws
                break

    ws_id = active_ws["id"] if active_ws else None

    tiled = [
        w
        for w in windows
        if w.get("workspace_id") == ws_id
        and not w.get("is_floating")
        and not w.get("is_minimized")
    ]
    floating = [
        w
        for w in windows
        if w.get("workspace_id") == ws_id
        and w.get("is_floating")
        and not w.get("is_minimized")
    ]
    minimized = [
        w for w in windows if w.get("workspace_id") == ws_id and w.get("is_minimized")
    ]

    columns: dict[int, list[dict]] = defaultdict(list)
    focused_col = None
    for w in tiled:
        pos = w.get("layout", {}).get("pos_in_scrolling_layout") or [1, 1]
        col, row = int(pos[0]), int(pos[1])
        w["_col"] = col
        w["_row"] = row
        columns[col].append(w)
        if w.get("is_focused"):
            focused_col = col

    for col in columns:
        columns[col].sort(key=lambda w: w["_row"])

    ordered = sorted(columns.keys())
    center_mode = load_center_focused()
    # Prefer in-memory prev (rail) over reading state.json every animation frame.
    prev = prev_state if prev_state is not None else load_state()
    if forced_view_pos is not None:
        view_pos = float(forced_view_pos)
        scroll_x = view_pos + working_x
        scroll_mode = "niri-view"
    else:
        scroll_x, view_pos, scroll_mode = resolve_view(
            columns,
            focused_col,
            gaps,
            working_x,
            usable_w,
            center_mode,
            prev,
            ws_id,
        )
    off_left, vis_cols, off_right, visible_w = columns_visibility(
        columns, gaps, usable_w, scroll_x, working_x
    )
    total_vis = sum(visible_w.values()) or 1.0
    positions = column_positions(columns, gaps)
    clip_l, clip_r = view_clip_x(working_x, usable_w)

    # Color by app across the whole workspace (stable as you scroll).
    color_index: dict[str, int] = {}
    for c in ordered:
        for t in columns[c]:
            key = app_key(t.get("app_id"))
            if key not in color_index:
                color_index[key] = len(color_index) % len(COLUMN_COLORS)
    for w in floating:
        key = app_key(w.get("app_id"))
        if key not in color_index:
            color_index[key] = len(color_index) % len(COLUMN_COLORS)

    segments: list[dict[str, Any]] = []
    apps: list[dict[str, Any]] = []

    def add_column(c: int, stack_i: int) -> None:
        """Full tile_w always. Stacked: one ghost per overhang, colors only in view."""
        tiles = columns[c]
        heights = [float(t["layout"]["tile_size"][1]) for t in tiles]
        sum_h = sum(heights) or 1.0
        tile_w = col_tile_w(columns, c)
        left = positions[c] - scroll_x
        # Clip to the real tiled area [working_x, working_x+usable_w], not [0, usable_w].
        vis, over_l, over_r = column_overhangs(left, tile_w, clip_l, clip_r, gaps)
        scale = usable_w or 1.0
        if vis <= 0.5:
            side = "right" if left >= (clip_l + clip_r) * 0.5 else "left"
            # Fully off-screen: one gray bar for the column; icons share by height.
            ghost_src = make_entry(
                tiles[0],
                key=app_key(tiles[0].get("app_id")),
                color_i=color_index[app_key(tiles[0].get("app_id"))],
                stack_i=stack_i,
                tile_i=0,
                stack_n=len(tiles),
                pct=(tile_w / scale) * 100.0,
                col_share=tile_w / scale,
                h_share=1.0,
                offscreen=side,
            )
            segments.append(make_ghost(ghost_src))
            for ti, t in enumerate(tiles):
                h_share = heights[ti] / sum_h
                key = app_key(t.get("app_id"))
                apps.append(
                    make_entry(
                        t,
                        key=key,
                        color_i=color_index[key],
                        stack_i=stack_i,
                        tile_i=ti,
                        stack_n=len(tiles),
                        pct=(tile_w / scale) * h_share * 100.0,
                        col_share=tile_w / scale,
                        h_share=h_share,
                        offscreen=side,
                    )
                )
            return

        # Partially / fully on screen:
        # Segments: [ghost_L once][color0][color1...][ghost_R once]
        # Icons: full tile_w × height share (so glyphs aren't crushed on peeks).
        if over_l > 0.5:
            over_pct = (over_l / scale) * 100.0
            g0 = make_entry(
                tiles[0],
                key=app_key(tiles[0].get("app_id")),
                color_i=color_index[app_key(tiles[0].get("app_id"))],
                stack_i=stack_i,
                tile_i=0,
                stack_n=len(tiles),
                pct=over_pct,
                col_share=over_l / scale,
                h_share=1.0,
                offscreen="left",
            )
            segments.append(make_ghost(g0))

        for ti, t in enumerate(tiles):
            h_share = heights[ti] / sum_h
            key = app_key(t.get("app_id"))
            # Icon spans the whole window (visible + overhangs).
            full_pct = (tile_w / scale) * h_share * 100.0
            apps.append(
                make_entry(
                    t,
                    key=key,
                    color_i=color_index[key],
                    stack_i=stack_i,
                    tile_i=ti,
                    stack_n=len(tiles),
                    pct=full_pct,
                    col_share=tile_w / scale,
                    h_share=h_share,
                )
            )
            # Underline only paints the viewport intersection.
            vis_pct = (vis / scale) * h_share * 100.0
            segments.append(
                dict(
                    make_entry(
                        t,
                        key=key,
                        color_i=color_index[key],
                        stack_i=stack_i,
                        tile_i=ti,
                        stack_n=len(tiles),
                        pct=vis_pct,
                        col_share=vis / scale,
                        h_share=h_share,
                    )
                )
            )

        if over_r > 0.5:
            over_pct = (over_r / scale) * 100.0
            g1 = make_entry(
                tiles[-1],
                key=app_key(tiles[-1].get("app_id")),
                color_i=color_index[app_key(tiles[-1].get("app_id"))],
                stack_i=stack_i,
                tile_i=len(tiles) - 1,
                stack_n=len(tiles),
                pct=over_pct,
                col_share=over_r / scale,
                h_share=1.0,
                offscreen="right",
            )
            segments.append(make_ghost(g1))

    stack_i = 0
    for i, c in enumerate(ordered):
        if i > 0:
            segments.append(make_col_tick())
            apps.append(make_app_gap())
        add_column(c, stack_i=stack_i)
        stack_i += 1

    for w in floating:
        key = app_key(w.get("app_id"))
        w["_col"] = None
        w["_row"] = None
        entry = make_entry(
            w,
            key=key,
            color_i=color_index[key],
            stack_i=-1,
            tile_i=0,
            stack_n=1,
            pct=0.0,
            col_share=0.0,
            h_share=1.0,
            floating=True,
            offscreen="right",
        )
        apps.append(entry)
        segments.append(make_ghost(entry))

    visibility_mode = scroll_mode

    def chip(w: dict) -> dict[str, Any]:
        return {
            "window_id": w.get("id"),
            "app_id": w.get("app_id"),
            "title": w.get("title") or "",
            "label": app_label(w.get("app_id"), w.get("title")),
            "is_focused": bool(w.get("is_focused")),
            "is_urgent": bool(w.get("is_urgent")),
        }

    by_id = lambda ws_list: sorted(ws_list, key=lambda w: int(w.get("id") or 0))  # noqa: E731
    ws_output = active_ws.get("output") if active_ws else output_name
    output_workspaces = sorted(
        (ws for ws in workspaces if ws.get("output") == ws_output),
        key=lambda ws: int(ws.get("idx") or 0),
    )

    return {
        "workspace_idx": int(active_ws.get("idx") or 1) if active_ws else 1,
        "workspaces": [
            {
                "id": ws.get("id"),
                "idx": int(ws.get("idx") or 0),
                "is_active": bool(ws.get("is_active")),
                "has_windows": any(w.get("workspace_id") == ws.get("id") for w in windows),
            }
            for ws in output_workspaces
        ],
        "desktop_shown": bool(active_ws.get("is_desktop_shown")) if active_ws else False,
        "minimized_tiled": [chip(w) for w in by_id(minimized) if not w.get("is_floating")],
        "floating_open": [chip(w) for w in by_id(floating)],
        "minimized_floating": [chip(w) for w in by_id(minimized) if w.get("is_floating")],
        "output": output_name,
        "output_width": output_w,
        "output_height": output_h,
        "usable_width": usable_w,
        "working_x": working_x,
        "workspace_id": ws_id,
        "gaps": gaps,
        "scroll_x": round(scroll_x, 2),
        "view_pos": round(view_pos, 2),
        "focused_column": focused_col,
        "center_focused_column": center_mode,
        "visibility_mode": visibility_mode,
        "segments": segments,
        "apps": apps,
        "visible_columns": vis_cols,
        "off_left": off_left,
        "off_right": off_right,
        "visible_widths": {str(k): round(v, 2) for k, v in visible_w.items()},
        # On-screen [left, width] of each tiled column, and the viewport clip,
        # in output px: lets the rail map the real viewport onto its slots.
        "column_spans": {
            str(c): [round(positions[c] - scroll_x, 2), round(col_tile_w(columns, c), 2)]
            for c in ordered
        },
        "view_clip": [clip_l, clip_r],
    }


def render_strip_markup(state: dict[str, Any]) -> str:
    segs = state.get("segments") or []
    if not segs:
        return "<span foreground='#666666'>—</span>"

    # Allocate integer cells; ensure each segment gets ≥1 if percent>0.
    raw = [max(0.0, float(s["percent"])) for s in segs]
    total = sum(raw) or 1.0
    ideal = [p / total * STRIP_CELLS for p in raw]
    cells = [max(1, int(x)) if p > 0 else 0 for x, p in zip(ideal, raw)]
    # Fix rounding to STRIP_CELLS
    while sum(cells) > STRIP_CELLS:
        # shrink largest >1
        i = max(range(len(cells)), key=lambda i: cells[i])
        if cells[i] <= 1:
            break
        cells[i] -= 1
    while sum(cells) < STRIP_CELLS and segs:
        i = max(range(len(cells)), key=lambda i: ideal[i] - cells[i])
        cells[i] += 1

    parts: list[str] = []
    prev_group = None
    for s, n in zip(segs, cells):
        if n <= 0:
            continue
        g = s["group"]
        if prev_group is not None and g != prev_group:
            parts.append("<span foreground='#1a1c20'>▏</span>")
        elif prev_group is not None and g == prev_group:
            # stacked separator (same group)
            parts.append(f"<span foreground='{s['color']}' size='x-small'>│</span>")
        block = BLOCK * n
        # Dim border via underline-ish: use slightly different fg
        parts.append(
            f"<span background='{s['color']}' foreground='{s['color']}'>{block}</span>"
        )
        prev_group = g
    return "".join(parts)


def render_tooltip(state: dict[str, Any]) -> str:
    lines = []
    mode = state.get("visibility_mode")
    lines.append(f"Viewport ({mode})")
    for s in state.get("segments") or []:
        title = (s.get("title") or s.get("app_id") or "?")[:48]
        lines.append(
            f"col{s['column']}: {s['percent']:.1f}%  {title}"
        )
    return "\n".join(lines)


def _seg_sticky_bg(seg: dict[str, Any] | None) -> str:
    """Last-known fill for #custom-niri-segN (survives Waybar's class strip)."""
    if not seg or seg.get("hidden") or seg.get("spacer"):
        return "transparent"
    if seg.get("col_tick"):
        return "#ffffff"
    if seg.get("ghost"):
        return "rgba(160, 168, 180, 0.28)"
    fill = seg.get("fill")
    if fill:
        return str(fill)
    return str(seg.get("color") or "transparent")


def render_widths_css(state: dict[str, Any]) -> str:
    """Sticky per-slot fills from icon palettes. Widths stay on the px-ladder.

    Sticky ``#custom-niri-segN { background }`` keeps the bar painted for the
    one frame Waybar strips classes. Prefer ``fill`` (proportional logo
    gradient) over legacy group/shade fallbacks.
    Text is percent-agnostic — scroll must not rewrite this file.
    """
    lines = ["/* generated by niri-layout.py — icon palette sticky fills */"]
    bar = int(VIEWPORT_BAR_PX)
    lines.append(f".niri-viewport-bar {{ min-height: {bar}px; }}")
    # Legacy group/shade fallbacks (used only if sticky fill is transparent).
    for i, shades in enumerate(COLUMN_COLORS):
        for k, color in enumerate(shades):
            id_sels = ", ".join(
                f"#custom-niri-seg{n}.group{i}.shade{k}" for n in range(10)
            )
            lines.append(
                f"{id_sels}, "
                f".niri-viewport-bar.group{i}.shade{k}, "
                f".seg.group{i}.shade{k} {{ background: {color}; }}"
            )
    lines.append(
        ".niri-viewport-bar.spacer, .seg.spacer { "
        "background: transparent; box-shadow: none; min-height: 0; }"
    )
    lines.append(
        "#custom-niri-seg0.ghost, #custom-niri-seg1.ghost, "
        "#custom-niri-seg2.ghost, #custom-niri-seg3.ghost, "
        "#custom-niri-seg4.ghost, #custom-niri-seg5.ghost, "
        "#custom-niri-seg6.ghost, #custom-niri-seg7.ghost, "
        "#custom-niri-seg8.ghost, #custom-niri-seg9.ghost, "
        ".niri-viewport-bar.ghost, .seg.ghost { "
        "background: rgba(160, 168, 180, 0.28); box-shadow: none; }"
    )
    segs = list(state.get("segments") or [])
    for n in range(10):
        bg = _seg_sticky_bg(segs[n] if n < len(segs) else None)
        # Sticky id rule wins over group/shade so logo mixes stick.
        lines.append(f"#custom-niri-seg{n} {{ background: {bg}; }}")
    return "\n".join(lines) + "\n"


_PX_LADDER_READY = False
_WIDTHS_CSS_TEXT: str | None = None


def ensure_px_ladder() -> None:
    """Static min-width ladder including px0 so the underline can shrink away.

    Called once per process — never from the scroll/focus hot path.
    """
    global _PX_LADDER_READY
    if _PX_LADDER_READY:
        return
    os.makedirs(CACHE_DIR, exist_ok=True)
    ver = "ladder-v3-nopad"
    try:
        with open(PX_LADDER_CSS, encoding="utf-8") as f:
            if ver in f.readline():
                _PX_LADDER_READY = True
                return
    except OSError:
        pass
    lines = [f"/* {ver}: instant min-width via class (no CSS transition) */"]
    for i in range(10):
        for n in range(0, PX_LADDER_MAX + 1):
            lines.append(
                f"#custom-niri-app{i}.px{n}, #custom-niri-seg{i}.px{n} "
                f"{{ min-width: {n}px; }}"
            )
    tmp = PX_LADDER_CSS + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    os.replace(tmp, PX_LADDER_CSS)
    _PX_LADDER_READY = True


def _quantize_px(px: int, *, minimum: int) -> int:
    """Snap to 2px so 1px IPC noise does not flip-flop ladder classes."""
    px = max(minimum, min(PX_LADDER_MAX, int(px)))
    if px >= 2:
        px = (px // 2) * 2
    return px


def entry_px(entry: dict[str, Any] | None) -> int:
    if not entry:
        return 0
    if entry.get("col_tick"):
        return int(COL_TICK_PX)
    if entry.get("col_gap"):
        pct = float(entry.get("percent") or 0)
        if pct > 0:
            px = int(round(pct / 100.0 * ROW_PX))
            return _quantize_px(px, minimum=1)
        # Match tick min-width + its CSS L/R margins so icon/strip columns line up.
        return int(COL_GAP_PX)
    pct = float(entry.get("percent") or 0)
    # Floating stays icon-sized.
    if entry.get("floating") and pct <= 0:
        return int(COMPACT_PX)
    if pct > 0:
        px = int(round(pct / 100.0 * ROW_PX))
        return _quantize_px(px, minimum=int(MIN_SLOT_PX))
    if entry.get("spacer") or entry.get("compact") or entry.get("offscreen"):
        return int(COMPACT_PX)
    return 0


def write_widths_css(state: dict[str, Any]) -> None:
    """Write hues CSS only when the generated text actually changes.

    Scroll/focus ticks must never touch widths.css or the px-ladder — waybar's
    @import watcher reloads GTK CSS and hitchs the strip.
    """
    global _WIDTHS_CSS_TEXT
    text = render_widths_css(state)
    if _WIDTHS_CSS_TEXT == text:
        return
    os.makedirs(CACHE_DIR, exist_ok=True)
    prev = _WIDTHS_CSS_TEXT
    if prev is None:
        try:
            with open(WIDTHS_CSS, encoding="utf-8") as f:
                prev = f.read()
        except OSError:
            prev = ""
    if prev == text:
        _WIDTHS_CSS_TEXT = text
        return
    # Truncate in place instead of rename: waybar watches @import'ed files
    # but only reloads on CHANGES_DONE_HINT, which a rename-over never emits.
    with open(WIDTHS_CSS, "w", encoding="utf-8") as f:
        f.write(text)
    _WIDTHS_CSS_TEXT = text


def write_state(state: dict[str, Any]) -> None:
    """Single state.json write. CSS files stay untouched unless hues change."""
    os.makedirs(CACHE_DIR, exist_ok=True)
    # Ladder is static; never rewrite it from the event loop.
    ensure_px_ladder()
    tmp = STATE_PATH + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, ensure_ascii=False)
    os.replace(tmp, STATE_PATH)
    write_widths_css(state)


def waybar_strip_payload(state: dict[str, Any]) -> dict[str, Any]:
    text = render_strip_markup(state)
    return {
        "text": text,
        "tooltip": render_tooltip(state),
        "class": "layout-strip",
    }


def waybar_slot_payload(state: dict[str, Any], index: int) -> dict[str, Any]:
    apps = state.get("apps") or []
    if index < 0 or index >= len(apps):
        return {"text": "", "class": ["niri-app-item", "hidden"], "tooltip": ""}
    a = apps[index]

    if a.get("col_gap"):
        return {
            "text": " ",
            "tooltip": "",
            "class": [
                "niri-app-item",
                "col-gap",
                f"px{entry_px(a)}",
            ],
            "alt": "",
            "percentage": 0,
        }

    offscreen = bool(a.get("offscreen") or a.get("floating"))
    cls = ["niri-app-item", "niri-app"]
    cls.append("off-screen" if offscreen else "in-viewport")
    # "compact" only for true icon chips (floating); off-screen keeps full width.
    if a.get("floating"):
        cls.append("compact")
    if a.get("is_focused"):
        cls.append("focused")
    if a.get("floating"):
        cls.append("floating")
    if a.get("offscreen"):
        cls.append(f"off-{a['offscreen']}")
    if a.get("group") is not None:
        cls.append(f"group{a['group']}")
        if a.get("group_size", 1) > 1:
            cls.append("stacked")
    tip = a.get("title") or a.get("app_id") or ""
    if a.get("floating"):
        tip = f"{tip}\n(floating)"
    elif a.get("offscreen"):
        tip = f"{tip}\n(fora da tela · {a['offscreen']})"
    elif a.get("column") is not None:
        tip = f"{tip}\ncol {a['column']}"
    px = entry_px(a)
    if px:
        cls.append(f"px{px}")
    # Narrow end columns: smaller glyph so it reads more centered in the slot.
    if not offscreen and px and px < 28:
        cls.append("narrow")
    return {
        "text": a.get("label") or "?",
        "tooltip": tip,
        "class": cls,
        "alt": str(a["window_id"]) if a.get("window_id") is not None else "",
        "percentage": 0,
    }


def _flex_weights(segs: list[dict[str, Any]]) -> list[int]:
    """Integer flex weights that sum to 1000 and match area percents."""
    raw = [max(0.0, float(s.get("percent") or 0)) for s in segs]
    total = sum(raw) or 1.0
    ideal = [p / total * 1000.0 for p in raw]
    weights = [max(1, int(round(x))) if p > 0 else 0 for x, p in zip(ideal, raw)]
    while sum(weights) > 1000:
        i = max(range(len(weights)), key=lambda i: weights[i])
        if weights[i] <= 1:
            break
        weights[i] -= 1
    while sum(weights) < 1000 and weights:
        i = max(range(len(weights)), key=lambda i: ideal[i] - weights[i])
        weights[i] += 1
    return weights


def waybar_seg_payload(state: dict[str, Any], index: int) -> dict[str, Any]:
    segs = state.get("segments") or []
    if index < 0 or index >= len(segs):
        return {
            "text": "",
            "class": ["niri-viewport-bar", "hidden"],
            "tooltip": "",
            "percentage": 0,
        }
    s = segs[index]

    if s.get("spacer"):
        return {
            "text": " ",
            "tooltip": "",
            "class": [
                "niri-viewport-bar",
                "seg",
                "spacer",
                "off-screen",
                f"px{entry_px(s)}",
            ],
            "percentage": 0,
        }

    if s.get("ghost"):
        return {
            "text": " ",
            "tooltip": "",
            "class": [
                "niri-viewport-bar",
                "seg",
                "ghost",
                "off-screen",
                f"px{entry_px(s)}",
            ],
            "percentage": 0,
        }

    if s.get("col_tick"):
        return {
            "text": " ",
            "tooltip": "",
            "class": [
                "niri-viewport-bar",
                "seg",
                "col-tick",
                f"px{entry_px(s)}",
            ],
            "percentage": 0,
        }

    real = [
        x
        for x in segs
        if not x.get("spacer")
        and not x.get("col_tick")
        and not x.get("ghost")
        and float(x.get("percent") or 0) > 0
    ]

    cls = [
        "niri-viewport-bar",
        "seg",
        "in-viewport",
        f"group{s['group']}",
        f"shade{s.get('shade', 0)}",
    ]
    real_ids = [x.get("window_id") for x in real]
    prev_seg = segs[index - 1] if index > 0 else None
    next_seg = segs[index + 1] if index + 1 < len(segs) else None
    # Don't round the color tip when a same-window ghost continues past it.
    joins_left_ghost = bool(
        prev_seg
        and prev_seg.get("ghost")
        and prev_seg.get("window_id") == s.get("window_id")
    )
    joins_right_ghost = bool(
        next_seg
        and next_seg.get("ghost")
        and next_seg.get("window_id") == s.get("window_id")
    )
    if real_ids and s.get("window_id") == real_ids[0] and not joins_left_ghost:
        cls.append("first")
    if real_ids and s.get("window_id") == real_ids[-1] and not joins_right_ghost:
        cls.append("last")

    nxt = None
    for j in range(index + 1, len(segs)):
        if (
            segs[j].get("spacer")
            or segs[j].get("col_tick")
            or segs[j].get("ghost")
        ):
            continue
        nxt = segs[j]
        break
    if nxt is not None:
        # Same column (vertical stack) → discreet inset; side-by-side uses col-tick.
        same_col = (
            nxt.get("column") is not None
            and s.get("column") is not None
            and nxt.get("column") == s.get("column")
        ) or (
            nxt.get("stack_group") == s.get("stack_group")
            and s.get("stack_group", -1) >= 0
        )
        if same_col:
            cls.append("sep-tile")

    # No "focused" on segs: style-niri.css has no seg.focused rule, and adding
    # it forces a Waybar class churn (flash) on every focus change.
    # No wN: flex weight churned every scroll frame without changing paint.
    if s.get("group_size", 1) > 1:
        cls.append("stacked")
    title = (s.get("title") or s.get("app_id") or "?")[:64]
    # Stable tooltip (no live percent) so scroll frames don't force JSON churn.
    tip = title
    cls.append(f"px{entry_px(s)}")
    return {
        "text": " ",
        "tooltip": tip,
        "class": cls,
        "percentage": 0,
    }


def load_state() -> dict[str, Any] | None:
    try:
        with open(STATE_PATH, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return None


def signal_waybar() -> None:
    """Unused: slots watch state.json via inotify. Do not pkill waybar per tick."""
    return


def anim_lock_held() -> bool:
    try:
        with open(ANIM_LOCK, encoding="utf-8") as f:
            pid = int((f.read() or "0").strip() or "0")
    except (OSError, ValueError):
        return False
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        try:
            os.remove(ANIM_LOCK)
        except OSError:
            pass
        return False


def acquire_anim_lock() -> bool:
    if anim_lock_held():
        return False
    os.makedirs(CACHE_DIR, exist_ok=True)
    try:
        fd = os.open(ANIM_LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    except FileExistsError:
        if anim_lock_held():
            return False
        try:
            os.remove(ANIM_LOCK)
        except OSError:
            return False
        try:
            fd = os.open(ANIM_LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        except FileExistsError:
            return False
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))
    return True


def release_anim_lock() -> None:
    try:
        os.remove(ANIM_LOCK)
    except OSError:
        pass


def _state_fingerprint(state: dict[str, Any]) -> tuple:
    apps = tuple(
        (
            a.get("window_id"),
            entry_px(a),
            bool(a.get("compact")),
            bool(a.get("is_focused")),
            bool(a.get("is_urgent")),
            bool(a.get("col_gap")),
        )
        for a in (state.get("apps") or [])
    )
    segs = tuple(
        (
            s.get("window_id"),
            bool(s.get("ghost")),
            bool(s.get("col_tick")),
            bool(s.get("spacer")),
            entry_px(s),
            s.get("group"),
            s.get("shade"),
            s.get("color"),
            s.get("fill"),
        )
        for s in (state.get("segments") or [])
    )
    return (
        state.get("workspace_id"),
        state.get("focused_column"),
        # Coarser view_pos: sub-pixel IPC chatter must not rewrite state.
        round(float(state.get("view_pos") or 0) / 2.0) * 2.0,
        apps,
        segs,
    )


def fresh_snapshot() -> dict[str, Any]:
    """One IPC pull. Scroll frames must patch this in memory, not query again."""
    return {
        "windows": niri_json(["windows"]),
        "workspaces": niri_json(["workspaces"]),
        "focused_output": niri_json(["focused-output"]),
    }


def apply_stream_event(snap: dict[str, Any], ev: dict[str, Any]) -> str:
    """Fold one event-stream object into ``snap``.

    Returns ``layout`` (view moved), ``structure`` (windows/workspaces), or ``ignore``.
    Layout patches are what the compositor already animated this frame — no re-query.
    """
    if not isinstance(ev, dict) or len(ev) != 1:
        return "ignore"
    kind, body = next(iter(ev.items()))
    if not isinstance(body, dict):
        body = {}

    if kind == "WindowLayoutsChanged":
        by_id = {w.get("id"): w for w in snap.get("windows") or []}
        for pair in body.get("changes") or []:
            if not isinstance(pair, (list, tuple)) or len(pair) < 2:
                continue
            win = by_id.get(pair[0])
            if win is not None:
                win["layout"] = pair[1]
        return "layout"

    if kind == "WindowFocusChanged":
        fid = body.get("id")
        for win in snap.get("windows") or []:
            win["is_focused"] = win.get("id") == fid
        return "layout"

    if kind == "WindowUrgencyChanged":
        wid = body.get("id")
        urgent = bool(body.get("urgent"))
        for win in snap.get("windows") or []:
            if win.get("id") == wid:
                win["is_urgent"] = urgent
                break
        return "layout"

    if kind == "WindowsChanged":
        snap["windows"] = list(body.get("windows") or [])
        return "structure"

    if kind == "WindowOpenedOrChanged":
        win = body.get("window") or {}
        wid = win.get("id")
        windows = snap.setdefault("windows", [])
        for i, old in enumerate(windows):
            if old.get("id") == wid:
                windows[i] = win
                break
        else:
            if wid is not None:
                windows.append(win)
        if win.get("is_focused"):
            for old in windows:
                if old.get("id") != wid:
                    old["is_focused"] = False
        return "structure"

    if kind == "WindowClosed":
        wid = body.get("id")
        snap["windows"] = [
            w for w in (snap.get("windows") or []) if w.get("id") != wid
        ]
        return "structure"

    if kind == "WorkspacesChanged":
        snap["workspaces"] = list(body.get("workspaces") or [])
        return "structure"

    if kind == "WorkspaceActivated":
        wid = body.get("id")
        focused = bool(body.get("focused"))
        workspaces = snap.get("workspaces") or []
        activated = next((w for w in workspaces if w.get("id") == wid), None)
        if activated is None:
            return "structure"
        output = activated.get("output")
        for ws in workspaces:
            if ws.get("output") == output:
                ws["is_active"] = ws.get("id") == wid
            if focused:
                ws["is_focused"] = ws.get("id") == wid
        return "structure"

    if kind in (
        "WindowClosed",
        "OverviewOpenedOrClosed",
        "WorkspaceActiveWindowChanged",
    ):
        return "structure"
    return "ignore"


def publish_state(
    *, animate: bool = True, snapshot: dict[str, Any] | None = None
) -> dict[str, Any]:
    """Write the destination layout once. Widths swap via pxN classes (no CSS tween)."""
    del animate  # one-shot destination; CSS does not animate min-width
    if anim_lock_held():
        return load_state() or compute_state(snapshot=snapshot)

    prev = load_state()
    target = compute_state(snapshot=snapshot)
    if prev is not None and _state_fingerprint(prev) == _state_fingerprint(target):
        return prev
    write_state(target)
    return target


def _inotify_state_loop(on_change) -> None:
    """Wake when state.json is replaced. No extra packages (Linux inotify)."""
    libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    inotify_init1 = libc.inotify_init1
    inotify_init1.restype = ctypes.c_int
    inotify_add_watch = libc.inotify_add_watch
    inotify_add_watch.restype = ctypes.c_int
    inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
    IN_CLOEXEC = 0x80000
    IN_NONBLOCK = 0x800
    IN_MODIFY = 0x00000002
    IN_CLOSE_WRITE = 0x00000008
    IN_MOVED_TO = 0x00000080
    IN_CREATE = 0x00000100
    mask = IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE
    fd = inotify_init1(IN_CLOEXEC | IN_NONBLOCK)
    if fd < 0:
        fd = libc.inotify_init()
    if fd < 0:
        raise OSError("inotify_init failed")
    os.makedirs(CACHE_DIR, exist_ok=True)
    if inotify_add_watch(fd, CACHE_DIR.encode(), mask) < 0:
        raise OSError("inotify_add_watch failed")
    hdr_fmt = "iIII"
    hdr_size = struct.calcsize(hdr_fmt)
    while True:
        select.select([fd], [], [])
        try:
            data = os.read(fd, 65536)
        except BlockingIOError:
            continue
        hit = False
        off = 0
        while off + hdr_size <= len(data):
            _wd, _mask, _cookie, namelen = struct.unpack_from(hdr_fmt, data, off)
            off += hdr_size
            raw = data[off : off + namelen]
            off += namelen
            name = raw.split(b"\x00", 1)[0].decode("utf-8", "replace")
            if name in ("state.json", ""):
                hit = True
        if hit:
            on_change()


def _visual_emit_key(payload: dict[str, Any]) -> tuple:
    """Only geometry + paint classes. Tooltip/percentage/wN must not wake Waybar.

    Waybar custom.cpp removes every CSS class then re-applies on each JSON line.
    During scroll we only print when pxN or color role (group/shade/ghost/…) changes.
    """
    cls = payload.get("class") or []
    keep: list[str] = []
    for c in cls:
        if not isinstance(c, str):
            continue
        if c.startswith("px") and c[2:].isdigit():
            keep.append(c)
        elif c.startswith("group") and c[5:].isdigit():
            keep.append(c)
        elif c.startswith("shade") and c[5:].isdigit():
            keep.append(c)
        elif c in (
            "ghost",
            "spacer",
            "col-tick",
            "col-gap",
            "hidden",
            "in-viewport",
            "off-screen",
            "first",
            "last",
            "sep-tile",
            "stacked",
            "focused",
            "narrow",
            "compact",
            "floating",
            "niri-app-item",
            "niri-app",
            "niri-viewport-bar",
            "seg",
        ):
            keep.append(c)
        elif c.startswith("off-"):
            keep.append(c)
    return (payload.get("text") or "", tuple(keep))


def watch_slot(kind: str, index: int) -> int:
    """Long-lived Waybar exec: reprint JSON when state.json changes (no respawn)."""
    last_key: tuple | None = None
    last_line: str | None = None

    def emit() -> None:
        nonlocal last_key, last_line
        state = load_state()
        if state is None:
            return
        payload = (
            waybar_slot_payload(state, index)
            if kind == "slot"
            else waybar_seg_payload(state, index)
        )
        key = _visual_emit_key(payload)
        # Scroll noise (tooltip percent, flex weight) must not strip/reapply classes.
        if key == last_key:
            return
        last_key = key
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
        if line == last_line:
            return
        last_line = line
        sys.stdout.write(line)
        sys.stdout.write("\n")
        sys.stdout.flush()

    if load_state() is None:
        publish_state(animate=False)
    emit()
    try:
        _inotify_state_loop(emit)
    except KeyboardInterrupt:
        return 0
    return 0


def event_line_interesting(line: str) -> bool:
    """Which event-stream lines should refresh the strip.

    WindowLayoutsChanged is required for trackpad/view gestures and resize
    geometry. WindowFocusTimestampChanged is noise (no geometry).
    """
    if "WindowFocusTimestampChanged" in line:
        return False
    return any(
        k in line
        for k in (
            "WindowLayoutsChanged",
            "WindowFocusChanged",
            "WindowUrgencyChanged",
            "WindowsChanged",
            "WindowOpenedOrChanged",
            "WindowClosed",
            "WorkspacesChanged",
            "WorkspaceActivated",
            "WorkspaceActiveWindowChanged",
            "Overview",
        )
    )


def _acquire_events_lock() -> int | None:
    """Exclusive lock so only one ``events`` publisher runs (no duplicate writers)."""
    os.makedirs(CACHE_DIR, exist_ok=True)
    path = os.path.join(CACHE_DIR, "events.lock")
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        import fcntl

        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        os.close(fd)
        return None
    os.ftruncate(fd, 0)
    os.write(fd, str(os.getpid()).encode())
    return fd


def run_event_stream() -> int:
    """Drive layout from niri events with light batch coalesce (no 40ms debounce).

    Drains every event ready on the socket, then publishes once. Slots observe
    state.json via inotify — never pkill/signal waybar from this loop.
    """
    json.dump({"text": "", "class": ["hidden"], "tooltip": ""}, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()

    # If another publisher holds the lock, wait and retry — a dead owner used to
    # leave this process sleeping forever while state.json went stale.
    lock_fd = _acquire_events_lock()
    while lock_fd is None:
        time.sleep(2.0)
        lock_fd = _acquire_events_lock()

    try:
        snap = fresh_snapshot()
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
        snap = {"windows": [], "workspaces": [], "focused_output": {}}

    # Reconnect event-stream if niri drops it (EOF) so state keeps updating.
    while True:
        proc = subprocess.Popen(
            [NIRI_BIN, "msg", "--json", "event-stream"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        assert proc.stdout is not None
        try:
            publish_state(animate=False, snapshot=snap)
        except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
            pass

        stream_dead = False
        while not stream_dead:
            r, _, _ = select.select([proc.stdout], [], [], 1.0)
            if not r:
                continue
            dirty = "ignore"
            while True:
                line = proc.stdout.readline()
                if line == "":
                    stream_dead = True
                    break
                if event_line_interesting(line):
                    try:
                        ev = json.loads(line)
                    except json.JSONDecodeError:
                        ev = None
                    if isinstance(ev, dict):
                        kind = apply_stream_event(snap, ev)
                        if kind == "structure":
                            dirty = "structure"
                        elif kind == "layout" and dirty != "structure":
                            dirty = "layout"
                more, _, _ = select.select([proc.stdout], [], [], 0)
                if not more:
                    break
            if stream_dead or dirty == "ignore" or anim_lock_held():
                continue
            try:
                # Structure events can race a partial patch; refresh IPC once.
                if dirty == "structure":
                    snap = fresh_snapshot()
                publish_state(animate=True, snapshot=snap)
            except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
                pass

        try:
            proc.wait(timeout=1.0)
        except Exception:
            try:
                proc.kill()
            except OSError:
                pass
        try:
            snap = fresh_snapshot()
        except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
            pass
        time.sleep(0.4)
    return 0


def self_test() -> int:
    """Assert event filter + fit-skip behaviour without a live gesture."""
    snap = {
        "windows": [{"id": 7, "layout": {"tile_size": [1, 1]}, "is_focused": False}],
        "workspaces": [{"id": 1, "output": "eDP-1", "is_active": True, "is_focused": True}],
        "focused_output": {"name": "eDP-1"},
    }
    kind = apply_stream_event(
        snap,
        {"WindowLayoutsChanged": {"changes": [[7, {"tile_size": [9, 9], "tile_pos_in_workspace_view": [3, 0]}]]}},
    )
    assert kind == "layout"
    assert snap["windows"][0]["layout"]["tile_size"] == [9, 9]
    assert apply_stream_event(snap, {"WindowFocusChanged": {"id": 7}}) == "layout"
    assert snap["windows"][0]["is_focused"] is True
    assert (
        apply_stream_event(
            snap, {"WindowUrgencyChanged": {"id": 7, "urgent": True}}
        )
        == "layout"
    )
    assert snap["windows"][0]["is_urgent"] is True
    assert event_line_interesting('{"WindowUrgencyChanged":{"id":1,"urgent":true}}')

    assert event_line_interesting('{"WindowLayoutsChanged":{"changes":[]}}')
    assert event_line_interesting('{"WindowFocusChanged":{"id":1}}')
    assert not event_line_interesting(
        '{"WindowFocusTimestampChanged":{"id":1,"focus_timestamp":null}}'
    )

    columns = {
        1: [
            {
                "id": 1,
                "layout": {
                    "pos_in_scrolling_layout": [1, 1],
                    "tile_size": [800.0, 900.0],
                    "tile_pos_in_workspace_view": None,
                },
                "is_focused": False,
            }
        ],
        2: [
            {
                "id": 2,
                "layout": {
                    "pos_in_scrolling_layout": [2, 1],
                    "tile_size": [600.0, 900.0],
                    "tile_pos_in_workspace_view": None,
                },
                "is_focused": True,
            }
        ],
    }
    gaps = 16.0
    working_x, working_w = 0.0, 1600.0
    prev = {
        "visibility_mode": "niri-view",
        "workspace_id": 1,
        "view_pos": 123.0,
        "focused_column": 2,
    }
    # Same focus → must keep view_pos (no fit fight).
    _left, vp, mode = resolve_view(
        columns, 2, gaps, working_x, working_w, "never", prev, 1
    )
    assert mode == "niri-view" and abs(vp - 123.0) < 1e-6, (vp, mode)

    # Focus change → fit may move view_pos away from the preserved value.
    _left2, vp2, _mode2 = resolve_view(
        columns, 1, gaps, working_x, working_w, "never", prev, 1
    )
    assert abs(vp2 - 123.0) > 1e-3, vp2

    # Live tile_pos wins even with same focus — and must not call fit.
    columns[2][0]["layout"]["tile_pos_in_workspace_view"] = [100.0, 0.0]
    _left3, vp3, mode3 = resolve_view(
        columns, 2, gaps, working_x, working_w, "never", prev, 1
    )
    assert mode3 == "tile_pos"
    # col2 layout x = 800+16=816 → view_left = 816-100 = 716 → view_pos = 716
    assert abs(vp3 - 716.0) < 1e-6, vp3
    assert abs(_left3 - 716.0) < 1e-6, _left3

    # Absolute scrolling_view_pos also wins over the preserved heuristic.
    columns[2][0]["layout"]["tile_pos_in_workspace_view"] = None
    columns[2][0]["layout"]["scrolling_view_pos"] = 550.0
    _left4, vp4, mode4 = resolve_view(
        columns, 2, gaps, working_x, working_w, "never", prev, 1
    )
    assert mode4 == "ipc-view"
    assert abs(_left4 - 550.0) < 1e-6 and abs(vp4 - 550.0) < 1e-6, (_left4, vp4)

    # Ghost clip: tile fully inside [working_x, working_x+usable_w] → no overhang.
    # Mirrors the YouTube false-ghost: left=990, tile=914, rail working_x=44, usable=1876.
    vis, ol, oright = column_overhangs(990.0, 914.0, 44.0, 44.0 + 1876.0, 16.0)
    assert abs(vis - 914.0) < 1e-6 and ol == 0.0 and oright == 0.0, (vis, ol, oright)
    # Old wrong clip [0, usable_w] would invent ~28px of right ghost.
    vis_bad, _ol_bad, or_bad = column_overhangs(990.0, 914.0, 0.0, 1876.0, 16.0)
    assert or_bad > 1.0 and vis_bad < 914.0, (vis_bad, or_bad)
    # Real peek off the right edge still ghosts.
    vis2, ol2, or2 = column_overhangs(1700.0, 914.0, 44.0, 44.0 + 1876.0, 16.0)
    assert or2 > 100.0 and vis2 < 914.0 and ol2 == 0.0, (vis2, ol2, or2)

    print("self-test ok", file=sys.stderr)
    return 0


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(
            "usage: niri-layout.py [state|strip|slot N|seg N|watch slot N|watch seg N|events|dump|self-test]",
            file=sys.stderr,
        )
        return 2
    cmd = argv[1]

    if cmd == "self-test":
        return self_test()
    if cmd == "watch" and len(argv) >= 4:
        kind = argv[2]
        if kind not in ("slot", "seg"):
            print("watch kind must be slot|seg", file=sys.stderr)
            return 2
        return watch_slot(kind, int(argv[3]))
    if cmd == "events":
        return run_event_stream()

    if cmd in ("slot", "seg") and len(argv) >= 3:
        # Prefer cached state so signal-driven slots don't re-query niri.
        state = load_state()
        if state is None:
            state = publish_state(animate=False)
        idx = int(argv[2])
        payload = waybar_slot_payload(state, idx) if cmd == "slot" else waybar_seg_payload(state, idx)
        json.dump(payload, sys.stdout, ensure_ascii=False)
        sys.stdout.write("\n")
        return 0

    if cmd == "state":
        state = publish_state(animate=True)
        json.dump(state, sys.stdout, ensure_ascii=False, indent=2)
        sys.stdout.write("\n")
        return 0
    if cmd == "dump":
        state = publish_state(animate=False)
        json.dump(state, sys.stdout, ensure_ascii=False, indent=2)
        sys.stdout.write("\n")
        print("--- strip ---", file=sys.stderr)
        print(render_strip_markup(state), file=sys.stderr)
        return 0
    if cmd == "strip":
        state = publish_state(animate=True)
        json.dump(waybar_strip_payload(state), sys.stdout, ensure_ascii=False)
        sys.stdout.write("\n")
        return 0
    print(f"unknown command: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
