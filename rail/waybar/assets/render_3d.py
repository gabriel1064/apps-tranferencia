#!/usr/bin/env python3
"""Ray-shade a glass sphere and a 3D capsule track. Real normals, not a 2D fake."""
from __future__ import annotations

import math
import os

import cairo

OUT = os.path.dirname(os.path.abspath(__file__))


def _norm(x: float, y: float, z: float) -> tuple[float, float, float]:
    m = math.sqrt(x * x + y * y + z * z) or 1.0
    return (x / m, y / m, z / m)


LIGHTS = (
    (_norm(-0.35, 0.88, 0.55), 1.0, 72.0),
    (_norm(0.65, 0.15, 0.55), 0.35, 28.0),
)


def _shade(nx: float, ny: float, nz: float, focused: bool) -> tuple[float, float, float, float]:
    diffuse = 0.0
    spec = 0.0
    for (lx, ly, lz), power, shininess in LIGHTS:
        ndl = max(0.0, nx * lx + ny * ly + nz * lz)
        hx, hy, hz = _norm(lx, ly, lz + 1.0)
        ndh = max(0.0, nx * hx + ny * hy + nz * hz)
        diffuse += ndl * power
        spec += (ndh ** shininess) * power
    fres = (1.0 - max(0.0, nz)) ** 2.4
    # Pale glass: bright where the light hits, darker glass at the rim.
    lit = 0.22 + 0.70 * min(1.0, diffuse) + 0.95 * min(1.0, spec) + 0.18 * fres
    if focused:
        lit = min(1.0, lit + 0.08)
    r = min(1.0, lit)
    g = min(1.0, lit * 0.99)
    b = min(1.0, lit * (1.04 if focused else 1.0))
    alpha = 0.16 + 0.34 * min(1.0, diffuse) + 0.62 * min(1.0, spec) + 0.16 * fres
    if focused:
        alpha = min(0.92, alpha + 0.08)
    else:
        alpha = min(0.72, alpha)
    return (r, g, b, alpha)


def _put(buf: bytearray, stride: int, x: int, y: int, r: float, g: float, b: float, a: float) -> None:
    a = max(0.0, min(1.0, a))
    i = y * stride + x * 4
    buf[i] = int(b * a * 255)
    buf[i + 1] = int(g * a * 255)
    buf[i + 2] = int(r * a * 255)
    buf[i + 3] = int(a * 255)


def _surface(w: int, h: int, paint) -> cairo.ImageSurface:
    stride = cairo.ImageSurface.format_stride_for_width(cairo.FORMAT_ARGB32, w)
    buf = bytearray(stride * h)
    paint(buf, stride, w, h)
    surf = cairo.ImageSurface.create_for_data(buf, cairo.FORMAT_ARGB32, w, h, stride)
    # create_for_data does not own the buffer; copy into a standalone surface.
    out = cairo.ImageSurface(cairo.FORMAT_ARGB32, w, h)
    cr = cairo.Context(out)
    cr.set_source_surface(surf, 0, 0)
    cr.paint()
    return out


def _sphere(focused: bool) -> cairo.ImageSurface:
    n = 192
    cx = cy = n / 2
    radius = 68.0

    def paint(buf: bytearray, stride: int, w: int, h: int) -> None:
        for y in range(h):
            for x in range(w):
                # Soft contact shadow under the sphere.
                dx = x + 0.5 - cx
                dy = y + 0.5 - (cy + radius * 0.72)
                shadow = math.exp(-((dx / (radius * 0.85)) ** 2 + (dy / (radius * 0.28)) ** 2))
                sr = sg = sb = 0.0
                sa = 0.34 * shadow

                acc = [sr * sa, sg * sa, sb * sa, sa]
                samples = ((-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25))
                for ox, oy in samples:
                    u = (x + 0.5 + ox - cx) / radius
                    v = -(y + 0.5 + oy - cy) / radius  # 3D +y is up
                    d2 = u * u + v * v
                    if d2 > 1.0:
                        continue
                    z = math.sqrt(max(0.0, 1.0 - d2))
                    r, g, b, a = _shade(u, v, z, focused)
                    # Premultiply into the accumulator (straight alpha average).
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
                k = 1.0 / len(samples)
                pr, pg, pb, pa = (c * k for c in acc)
                if pa <= 0.004:
                    continue
                _put(buf, stride, x, y, pr / pa, pg / pa, pb / pa, pa)

    return _surface(n, n, paint)


def _capsule_column(kind: str) -> cairo.ImageSurface:
    """kind: left | mid | right. Height includes a soft shadow."""
    radius = 36.0
    pad_x = 10
    stub = 8
    shadow = 16
    top = 6
    h = int(top + radius * 2 + shadow)
    cy = top + radius
    if kind == "mid":
        w = 8
        sphere_cx = None
    elif kind == "left":
        w = int(pad_x + radius + stub)
        sphere_cx = pad_x + radius
    else:
        w = int(pad_x + radius + stub)
        sphere_cx = w - (pad_x + radius)

    def paint(buf: bytearray, stride: int, width: int, height: int) -> None:
        for y in range(height):
            for x in range(width):
                acc = [0.0, 0.0, 0.0, 0.0]
                samples = ((-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25))
                for ox, oy in samples:
                    px = x + 0.5 + ox
                    py = y + 0.5 + oy
                    # Shadow under the tube.
                    sy = py - (cy + radius)
                    if sy > 0:
                        along = 1.0
                        if sphere_cx is not None and kind == "left":
                            along = max(0.0, min(1.0, (px - pad_x) / (width - pad_x)))
                        elif sphere_cx is not None and kind == "right":
                            along = max(0.0, min(1.0, (width - pad_x - px) / (width - pad_x)))
                        sa = 0.22 * math.exp(-(sy / 7.0) ** 2) * along
                        acc[3] += sa
                    nx = ny = nz = None
                    if sphere_cx is not None:
                        u = (px - sphere_cx) / radius
                        v = -(py - cy) / radius
                        if kind == "left" and px <= sphere_cx and u * u + v * v <= 1.0:
                            z = math.sqrt(max(0.0, 1.0 - u * u - v * v))
                            nx, ny, nz = u, v, z
                        elif kind == "right" and px >= sphere_cx and u * u + v * v <= 1.0:
                            z = math.sqrt(max(0.0, 1.0 - u * u - v * v))
                            nx, ny, nz = u, v, z
                    if nx is None:
                        dy = -(py - cy) / radius
                        if abs(dy) <= 1.0 and (
                            sphere_cx is None
                            or (kind == "left" and px >= sphere_cx)
                            or (kind == "right" and px <= sphere_cx)
                        ):
                            ny = dy
                            nz = math.sqrt(max(0.0, 1.0 - dy * dy))
                            nx = 0.0
                    if nx is None:
                        continue
                    r, g, b, a = _shade(nx, ny, nz, False)
                    # Bar stays lighter than the orbs.
                    a *= 0.72
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
                k = 1.0 / len(samples)
                pr, pg, pb, pa = (c * k for c in acc)
                if pa <= 0.004:
                    continue
                _put(buf, stride, x, y, pr / pa, pg / pa, pb / pa, min(1.0, pa))

    return _surface(w, h, paint)


def main() -> None:
    pairs = {
        "orb.png": _sphere(False),
        "orb-focus.png": _sphere(True),
        "bar-left.png": _capsule_column("left"),
        "bar-mid.png": _capsule_column("mid"),
        "bar-right.png": _capsule_column("right"),
    }
    for name, surf in pairs.items():
        path = os.path.join(OUT, name)
        surf.write_to_png(path)
        print(path, surf.get_width(), surf.get_height())


if __name__ == "__main__":
    main()
