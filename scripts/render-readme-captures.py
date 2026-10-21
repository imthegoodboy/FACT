"""Render actual ConPTY terminal-cell captures as portable README PNGs.

Requires Pillow. Capture instructions: docs/screenshots/README.md.
No terminal content is synthesized or replaced by this renderer.
"""

import argparse
import json
from pathlib import Path

from PIL import Image, ImageColor, ImageDraw, ImageFont


ANSI = [
    "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00",
    "#0037da", "#881798", "#3a96dd", "#cccccc",
    "#767676", "#e74856", "#16c60c", "#f9f1a5",
    "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
    "#e7e9ed", "#000000", "#e7e9ed", "#ffffff", "#000000",
]


def color(value):
    if "Rgb" in value:
        rgb = value["Rgb"]
        return rgb["r"], rgb["g"], rgb["b"]
    if "Named" in value:
        return ANSI[value["Named"]]
    index = value["Indexed"]
    if index < 16:
        return ANSI[index]
    if index < 232:
        index -= 16
        levels = [0, 95, 135, 175, 215, 255]
        return levels[index // 36], levels[index // 6 % 6], levels[index % 6]
    return (8 + (index - 232) * 10,) * 3


def render(source, destination, font_directory):
    capture = json.loads(source.read_text(encoding="utf-8"))
    cols, rows = capture["cols"], capture["rows"]
    cell_width, cell_height, padding = 12, 26, 20
    fonts = {
        (False, False): "consola.ttf",
        (True, False): "consolab.ttf",
        (False, True): "consolai.ttf",
        (True, True): "consolaz.ttf",
    }
    fonts = {key: ImageFont.truetype(str(font_directory / name), 20)
             for key, name in fonts.items()}
    image = Image.new("RGB", (cols * cell_width + padding * 2,
                              rows * cell_height + padding * 2), "black")
    draw = ImageDraw.Draw(image)
    # Backgrounds first, so a wide glyph is never erased by its spacer cell.
    for index, cell in enumerate(capture["cells"]):
        x = padding + index % cols * cell_width
        y = padding + index // cols * cell_height
        background = color(cell["fg"] if cell["flags"] & 16 else cell["bg"])
        draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=background)
    for index, cell in enumerate(capture["cells"]):
        flags = cell["flags"]
        if flags & (128 | 512):  # hidden text or a wide-character spacer
            continue
        x = padding + index % cols * cell_width
        y = padding + index // cols * cell_height
        foreground = color(cell["bg"] if flags & 16 else cell["fg"])
        if flags & 32:  # dim text, matching a typical terminal's reduced intensity
            rgb = ImageColor.getrgb(foreground) if isinstance(foreground, str) else foreground
            foreground = tuple(round(channel * 0.66) for channel in rgb)
        text = cell["ch"] + "".join(cell["zero"])
        if flags & 8:
            for offset in (22, 24):
                draw.line((x, y + offset, x + cell_width - 1, y + offset), fill=foreground)
        elif flags & 4:
            draw.line((x, y + 23, x + cell_width - 1, y + 23), fill=foreground)
        if flags & 64:
            draw.line((x, y + 14, x + cell_width - 1, y + 14), fill=foreground)
        # Terminal box glyphs join cell boundaries; font line spacing must not
        # turn continuous pane gutters into dashed lines.
        connections = {
            "│": "ud", "─": "lr", "┌": "rd", "┐": "ld",
            "└": "ru", "┘": "lu", "├": "udr", "┤": "udl",
            "┬": "lrd", "┴": "lru", "┼": "udlr",
        }.get(text)
        if connections:
            center = (x + cell_width // 2, y + cell_height // 2)
            ends = {"u": (center[0], y), "d": (center[0], y + cell_height),
                    "l": (x, center[1]), "r": (x + cell_width, center[1])}
            for direction in connections:
                draw.line((center, ends[direction]), fill=foreground)
            continue
        if text == "▐":
            draw.rectangle((x + cell_width // 2, y, x + cell_width - 1,
                            y + cell_height - 1), fill=foreground)
            continue
        draw.text((x, y + 2), text, fill=foreground,
                  font=fonts[bool(flags & 1), bool(flags & 2)])
    image.save(destination / f"{source.stem}.png", optimize=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--fonts", type=Path, default=Path("C:/Windows/Fonts"))
    args = parser.parse_args()
    args.destination.mkdir(parents=True, exist_ok=True)
    for source in sorted(args.captures.glob("*.json")):
        render(source, args.destination, args.fonts)
