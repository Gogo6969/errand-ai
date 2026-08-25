#!/usr/bin/env python3
"""Draw the Errand-AI app icon and write every size the platforms want.

The mark is a dial with a tick breaking out of it: the dial is the schedule,
the tick is the errand done, and it crosses the ring because the point of this
program is that something actually happened out in the world rather than a
reminder going off. It has to survive being 16 pixels wide in a menu bar, so it
is one bold shape on a plain ground, with the dial detail only readable when
the icon is large.

Run:  python3 scripts/make-icon.py
"""

import math
import os
import subprocess
import sys

from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "..", "app", "icons")

# Ink navy ground, amber mark. Amber on navy stays legible at small sizes and
# in both light and dark docks, and it avoids the purple-to-blue gradient that
# every other tool in this category already uses.
GROUND_TOP = (32, 42, 74)
GROUND_BOTTOM = (18, 24, 45)
RING = (68, 84, 138)
TICK = (86, 104, 166)
TICK_LIVE = (246, 166, 35)
MARK_TOP = (255, 200, 92)
MARK_BOTTOM = (243, 154, 26)

S = 4096  # drawn large, then reduced, which is where the smooth edges come from


def squircle(size, inset=0.0):
    """The rounded-square macOS icons actually use: a superellipse, not a
    rectangle with rounded corners. The difference is visible next to Apple's
    own icons, which is where this will sit."""
    n = 5.0
    cx = cy = size / 2
    r = size / 2 - inset
    pts = []
    steps = 2048
    for i in range(steps):
        t = 2 * math.pi * i / steps
        ct, st = math.cos(t), math.sin(t)
        x = cx + r * math.copysign(abs(ct) ** (2 / n), ct)
        y = cy + r * math.copysign(abs(st) ** (2 / n), st)
        pts.append((x, y))
    return pts


def vertical_gradient(size, top, bottom):
    grad = Image.new("RGB", (1, size))
    px = grad.load()
    for y in range(size):
        f = y / (size - 1)
        # Eased, so the ground reads as lit from above rather than as a ramp.
        f = f * f * (3 - 2 * f)
        px[0, y] = tuple(int(top[c] + (bottom[c] - top[c]) * f) for c in range(3))
    return grad.resize((size, size), Image.NEAREST)


def thick_line(draw, a, b, width, fill):
    """A stroke with round ends. PIL joins lines with a square corner, which
    looks broken on a checkmark, so the ends and the elbow get a circle."""
    draw.line([a, b], fill=fill, width=width)
    for p in (a, b):
        draw.ellipse(
            [p[0] - width / 2, p[1] - width / 2, p[0] + width / 2, p[1] + width / 2],
            fill=fill,
        )


def build():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # The ground.
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon(squircle(S), fill=255)
    img.paste(vertical_gradient(S, GROUND_TOP, GROUND_BOTTOM).convert("RGBA"), (0, 0), mask)

    d = ImageDraw.Draw(img)
    cx = cy = S / 2

    # The dial. Sits low-contrast against the ground on purpose: at small sizes
    # it should melt away and leave the tick, rather than turning into fuzz.
    r = S * 0.315
    ring_w = int(S * 0.026)
    d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=RING, width=ring_w)

    # Twelve marks, one of them lit: the hour this errand runs.
    tick_len = S * 0.052
    tick_w = int(S * 0.022)
    for h in range(12):
        ang = math.radians(-90 + h * 30)
        inner = r - ring_w * 0.5 - S * 0.028
        outer = inner - tick_len
        x1, y1 = cx + inner * math.cos(ang), cy + inner * math.sin(ang)
        x2, y2 = cx + outer * math.cos(ang), cy + outer * math.sin(ang)
        thick_line(d, (x1, y1), (x2, y2), tick_w, TICK_LIVE if h == 0 else TICK)

    # The tick. Drawn on its own layer so it can carry a gradient, and extended
    # past the ring at the top right: the errand left the schedule and got done.
    mark = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    md = ImageDraw.Draw(mark)
    w = int(S * 0.105)
    elbow = (cx - S * 0.045, cy + S * 0.135)
    start = (cx - S * 0.235, cy - S * 0.005)
    end = (cx + S * 0.315, cy - S * 0.245)
    thick_line(md, start, elbow, w, (255, 255, 255, 255))
    thick_line(md, elbow, end, w, (255, 255, 255, 255))

    tinted = vertical_gradient(S, MARK_TOP, MARK_BOTTOM).convert("RGBA")
    img.paste(tinted, (0, 0), mark.split()[3])

    return img


def write_sizes(icon):
    os.makedirs(OUT, exist_ok=True)

    def save(px, name):
        icon.resize((px, px), Image.LANCZOS).save(os.path.join(OUT, name))
        return px

    # What Tauri looks for, plus the Windows and Linux sizes so the same icon
    # is used when those builds happen rather than a second placeholder.
    save(512, "icon.png")
    save(32, "32x32.png")
    save(128, "128x128.png")
    save(256, "128x128@2x.png")
    for px in (16, 32, 48, 64, 128, 256, 512, 1024):
        save(px, f"{px}x{px}.png")

    # macOS wants an .icns, built from an .iconset by iconutil.
    iconset = os.path.join(OUT, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)
    for px, name in [
        (16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png"),
    ]:
        icon.resize((px, px), Image.LANCZOS).save(os.path.join(iconset, name))

    try:
        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", os.path.join(OUT, "icon.icns")],
            check=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError) as e:
        print(f"could not build icon.icns: {e}", file=sys.stderr)

    # Windows.
    icon.resize((256, 256), Image.LANCZOS).save(
        os.path.join(OUT, "icon.ico"),
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    write_sizes(build())
    print(f"icons written to {os.path.normpath(OUT)}")
