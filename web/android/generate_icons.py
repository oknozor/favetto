#!/usr/bin/env python3
"""Derive the favetto app icons from docs/public/logo.png.

Run from the repository root: `python3 web/android/generate_icons.py`.
Requires Pillow. The generated files are committed, so this only needs to be
re-run when the brand logo changes.
"""
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "docs/public/logo.png"
BADGE_BOX = (252, 147, 989, 884)  # 737x737 square around the badge, above the wordmark
BG = (27, 29, 32, 255)  # #1B1D20, the badge interior
ADAPTIVE_FILL = 0.60  # badge diameter / canvas (safe zone is 66/108 = 0.611)
LEGACY_FILL = 0.72


def badge():
    return Image.open(SRC).convert("RGBA").crop(BADGE_BOX)


def compose(canvas_px, fill, background):
    art = badge()
    d = max(1, round(canvas_px * fill))
    art = art.resize((d, d), Image.LANCZOS)
    canvas = Image.new("RGBA", (canvas_px, canvas_px), background)
    off = (canvas_px - d) // 2
    canvas.alpha_composite(art, (off, off))
    return canvas


def main():
    (ROOT / "web/assets").mkdir(parents=True, exist_ok=True)
    badge().resize((512, 512), Image.LANCZOS).save(ROOT / "web/assets/icon.png")

    res = ROOT / "web/android/res"
    legacy = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}
    adaptive = {"mdpi": 108, "hdpi": 162, "xhdpi": 216, "xxhdpi": 324, "xxxhdpi": 432}
    for dpi, px in legacy.items():
        out = res / f"mipmap-{dpi}"
        out.mkdir(parents=True, exist_ok=True)
        compose(px, LEGACY_FILL, BG).save(out / "ic_launcher.png")
    for dpi, px in adaptive.items():
        out = res / f"mipmap-{dpi}"
        out.mkdir(parents=True, exist_ok=True)
        compose(px, ADAPTIVE_FILL, (0, 0, 0, 0)).save(out / "ic_launcher_foreground.png")


if __name__ == "__main__":
    main()
