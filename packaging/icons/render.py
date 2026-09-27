#!/usr/bin/env python3
"""Renders every icon of RDM from the two SVG masters in crates/app/assets/icon/.

    python3 packaging/icons/render.py      (needs rsvg-convert and Pillow:
                                            sudo apt install librsvg2-bin python3-pil)

rdm-small.svg (heavier strokes) is used up to 32 px, rdm.svg above.
"""
import io
import shutil
import struct
import subprocess
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "crates/app/assets/icon"
ASSETS = ROOT / "crates/app/assets"
HICOLOR = ROOT / "packaging/linux/icons/hicolor"
EXTENSION = ROOT / "extension/icons"


def render(size: int) -> Image.Image:
    master = SRC / ("rdm-small.svg" if size <= 32 else "rdm.svg")
    png = subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(master)], check=True, capture_output=True
    ).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def save_png(size: int, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    render(size).save(path, optimize=True)


def save_rgba(size: int, path: Path) -> None:
    """Raw straight-alpha RGBA, as egui / tray-icon take it (no image decoder in the binary)."""
    path.write_bytes(render(size).tobytes())


def save_ico(sizes, path: Path) -> None:
    """One PNG-compressed entry per size, each rendered from its own master (Pillow would downscale one)."""
    images = []
    for s in sizes:
        buf = io.BytesIO()
        render(s).save(buf, format="PNG", optimize=True)
        images.append((s, buf.getvalue()))
    offset = 6 + 16 * len(images)
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    for s, data in images:
        dim = 0 if s >= 256 else s
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in images:
        out += data
    path.write_bytes(bytes(out))


save_ico([16, 20, 24, 32, 40, 48, 64, 128, 256], ASSETS / "rdm.ico")
save_png(256, ASSETS / "rdm.png")
save_rgba(64, ASSETS / "icon64.rgba")
save_rgba(128, ASSETS / "logo128.rgba")
save_rgba(32, ASSETS / "tray32.rgba")

for size in (16, 24, 32, 48, 64, 128, 256, 512):
    save_png(size, HICOLOR / f"{size}x{size}/apps/rdm.png")
(HICOLOR / "scalable/apps").mkdir(parents=True, exist_ok=True)
shutil.copyfile(SRC / "rdm.svg", HICOLOR / "scalable/apps/rdm.svg")

for size in (16, 32, 48, 128):
    save_png(size, EXTENSION / f"icon{size}.png")

print("icons rendered")
