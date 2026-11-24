"""Generate tray icons for vpose: native / vpn / error, 32x32 + 16x16 ICO."""
from PIL import Image, ImageDraw

OUT = r"C:\Users\Admin\Desktop\GeoProof\crates\app\assets"

SPECS = {
    "icon_native": (67, 160, 71, 255),    # green: native region
    "icon_vpn": (30, 136, 229, 255),      # blue: VPN region applied
    "icon_error": (229, 57, 53, 255),     # red: last operation failed
}


def draw_circle(size, color, excl):
    scale = size / 32.0
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    r = 15 * scale
    cx = cy = size / 2.0
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=color)
    # darker rim for contrast on both light/dark trays
    rim = tuple(max(0, c - 60) for c in color[:3]) + (255,)
    d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=rim, width=max(1, round(2 * scale)))
    if excl:
        # white "!" built from rectangles (font-free, scales cleanly)
        w = max(1, round(3 * scale))
        d.rectangle(
            [cx - w / 2, cy - 9 * scale, cx + w / 2, cy + 2 * scale],
            fill=(255, 255, 255, 255),
        )
        d.ellipse(
            [cx - w / 2, cy + 5 * scale, cx + w / 2, cy + 5 * scale + w],
            fill=(255, 255, 255, 255),
        )
    else:
        d.ellipse(
            [cx - 5 * scale, cy - 5 * scale, cx + 5 * scale, cy + 5 * scale],
            fill=(255, 255, 255, 255),
        )
    return img


for name, color in SPECS.items():
    sizes = [(32, 32), (16, 16)]
    frames = [draw_circle(s, color, excl=(name == "icon_error")) for (s, _) in sizes]
    path = rf"{OUT}\{name}.ico"
    frames[0].save(path, format="ICO", sizes=sizes, append_images=frames[1:])
    print(path)
