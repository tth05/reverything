"""Renders the Reverything icon into assets/reverything.ico (all sizes Windows uses) and
assets/reverything.png. Needs Pillow: pip install pillow"""

from pathlib import Path

from PIL import Image, ImageDraw

SCALE = 1024
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
TOP = (99, 102, 241)  # indigo 500
BOTTOM = (67, 56, 202)  # indigo 700


def render() -> Image.Image:
    s = SCALE
    # Rounded square with a vertical gradient
    gradient = Image.new("RGBA", (s, s))
    for y in range(s):
        t = y / (s - 1)
        color = tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,)
        ImageDraw.Draw(gradient).line([(0, y), (s, y)], fill=color)
    mask = Image.new("L", (s, s), 0)
    margin = round(s * 0.04)
    ImageDraw.Draw(mask).rounded_rectangle(
        [margin, margin, s - margin, s - margin], radius=round(s * 0.22), fill=255
    )
    icon = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    icon.paste(gradient, (0, 0), mask)

    # Magnifying glass: a ring and a handle with round caps
    draw = ImageDraw.Draw(icon)
    white = (255, 255, 255, 255)
    cx, cy, r = s * 0.44, s * 0.44, s * 0.21
    stroke = s * 0.085
    draw.ellipse([cx - r - stroke / 2, cy - r - stroke / 2, cx + r + stroke / 2, cy + r + stroke / 2], fill=white)
    inner = r - stroke / 2
    ImageDraw.Draw(icon).ellipse([cx - inner, cy - inner, cx + inner, cy + inner], fill=(0, 0, 0, 0))
    # Re-fill the lens with the background so it is not transparent
    lens = Image.new("L", (s, s), 0)
    ImageDraw.Draw(lens).ellipse([cx - inner, cy - inner, cx + inner, cy + inner], fill=255)
    icon.paste(gradient, (0, 0), lens)
    # A highlight in the lens
    hl = r * 0.55
    draw = ImageDraw.Draw(icon)
    draw.arc([cx - hl, cy - hl, cx + hl, cy + hl], start=200, end=260, fill=(255, 255, 255, 170), width=round(s * 0.035))

    start = cx + (r + stroke * 0.3) * 0.7071
    end = s * 0.78
    width = round(s * 0.115)
    draw.line([(start, start), (end, end)], fill=white, width=width)
    draw.ellipse([end - width / 2, end - width / 2, end + width / 2, end + width / 2], fill=white)
    return icon


def main() -> None:
    assets = Path(__file__).resolve().parent.parent / "assets"
    assets.mkdir(exist_ok=True)
    icon = render()
    images = [icon.resize((size, size), Image.LANCZOS) for size in SIZES]
    images[-1].save(assets / "reverything.png")
    images[-1].save(assets / "reverything.ico", sizes=[(s, s) for s in SIZES], append_images=images[:-1])
    print(f"Wrote {assets / 'reverything.ico'}")


if __name__ == "__main__":
    main()
