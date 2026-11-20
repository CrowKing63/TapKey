# TapKey tray icon — full-bleed "t" mark variants.
# Renders 3 variants at 2048 -> 512 masters (2x+ over-render, zero padding),
# derives 160/48/32/24/16 by downsampling, composes a light/dark comparison board.
# Output: docs/assets/icon-concepts/  (never touches existing assets)
import math, os
from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "docs", "assets", "icon-concepts")
os.makedirs(OUT, exist_ok=True)

LAV = (196, 181, 253, 255)   # #C4B5FD
DARK = (42, 27, 77, 255)     # #2A1B4D
G = 2                        # geometry scale: design space 1024 -> canvas 2048
S = 1024 * G

def squircle(draw, cx, cy, r, fill, n=4.0, steps=1440):
    pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        c, s = math.cos(t), math.sin(t)
        x = cx + r * math.copysign(abs(c) ** (2 / n), c)
        y = cy + r * math.copysign(abs(s) ** (2 / n), s)
        pts.append((x, y))
    draw.polygon(pts, fill=fill)

def rrect(draw, x0, y0, x1, y1, r, fill):
    draw.rounded_rectangle([x0 * G, y0 * G, x1 * G, y1 * G], radius=r * G, fill=fill)

def disc(draw, cx, cy, r, fill):
    draw.ellipse([(cx - r) * G, (cy - r) * G, (cx + r) * G, (cy + r) * G], fill=fill)

def glyph_t(draw, fill, dot_bar=False):
    # ONE continuous stroke: round top cap + straight stem + quarter-arc tail.
    # The arc's first stamp is centered on the stem centerline at the rect bottom,
    # tangent to both stem edges -> no seam/notch at the bend.
    disc(draw, 496, 222, 80, fill)                            # rounded top
    draw.rectangle([416 * G, 222 * G, 576 * G, 642 * G], fill=fill)
    if dot_bar:
        rrect(draw, 266, 302, 576, 462, 80, fill)   # left arm only
        disc(draw, 716, 382, 95, fill)              # tap dot as right arm
    else:
        rrect(draw, 266, 302, 756, 462, 80, fill)   # full crossbar
    cx, cy, R = 656, 642, 160
    for deg in range(180, 89, -1):
        a = math.radians(deg)
        disc(draw, cx + R * math.cos(a), cy + R * math.sin(a), 80, fill)

def render(tile, glyph, dot_bar):
    im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    squircle(d, S / 2, S / 2, S / 2, tile)          # full-bleed: touches all 4 edges
    glyph_t(d, glyph, dot_bar)
    return im.resize((512, 512), Image.LANCZOS)

variants = [
    ("a", "A · plain t", LAV, DARK, False),
    ("b", "B · dot crossbar", LAV, DARK, True),
    ("c", "C · inverted", DARK, LAV, False),
]
masters = {}
for key, label, tile, glyph, db in variants:
    m = render(tile, glyph, db)
    p = os.path.join(OUT, f"tapkey-t-{key}.png")
    m.save(p)
    masters[key] = m
    # --- pixel verification (ground truth) ---
    bb = m.getbbox()
    edge = m.getpixel((256, 2))          # top edge midpoint: must be tile, opaque
    corner = m.getpixel((2, 2))          # outside squircle corner: transparent
    stem = m.getpixel((256, 300))        # on stem below crossbar: glyph color
    print(f"[{key}] bbox={bb} edge={edge} corner_alpha={corner[3]} stem={stem}")
    assert bb == (0, 0, 512, 512), "not full-bleed"
    assert edge[3] == 255 and edge[:3] == tile[:3], "edge not tile color"
    assert corner[3] == 0, "corner should be transparent"
    assert stem[:3] == glyph[:3], "stem probe wrong"

# --- derived sizes ---
sizes = [160, 48, 32, 24, 16]
derived = {k: {s: masters[k].resize((s, s), Image.LANCZOS) for s in sizes} for k in masters}

# 24px hue check: lavender must survive (B > G separation)
for k in masters:
    im24 = derived[k][24].convert("RGB")
    px = list(im24.getdata())
    lav_like = sum(1 for r, g, b in px if b > g + 15 and b > 120)
    print(f"[{k}] 24px lavender-ish pixels: {lav_like}/576")

# --- comparison board ---
FB = "C:/Windows/Fonts/malgunbd.ttf"
f_title = ImageFont.truetype(FB, 34)
f_sub = ImageFont.truetype(FB, 22)
f_lab = ImageFont.truetype(FB, 26)
f_sz = ImageFont.truetype(FB, 20)

BG = (20, 20, 26, 255)
LIGHT = (245, 245, 247, 255)
DARKCHIP = (38, 38, 46, 255)
MUTED = (150, 150, 160, 255)
WHITE = (240, 240, 245, 255)

W, H = 924, 830
board = Image.new("RGBA", (W, H), BG)
d = ImageDraw.Draw(board)
d.text((32, 24), "TapKey tray icon — t-mark variants", font=f_title, fill=WHITE)
d.text((32, 70), "Full-bleed · tray slot = 24px @150% scaling", font=f_sub, fill=MUTED)

def chip(x, y, bg, img):
    d.rounded_rectangle([x, y, x + 200, y + 200], radius=16, fill=bg)
    board.alpha_composite(img, (x + 20, y + 20))

for i, (key, label, tile, glyph, db) in enumerate(variants):
    y0 = 110 + i * 232
    d.text((32, y0 + 86), label, font=f_lab, fill=WHITE)
    chip(200, y0, LIGHT, derived[key][160])
    chip(416, y0, DARKCHIP, derived[key][160])
    # small-size strip (taskbar-like dark)
    sx = 648
    d.rounded_rectangle([sx - 12, y0 + 40, sx + 244, y0 + 160], radius=12, fill=DARKCHIP)
    cx = sx + 8
    for s in sizes[1:]:
        img = derived[key][s]
        board.alpha_composite(img, (cx, y0 + 100 - s // 2))
        col = LAV if s == 24 else MUTED
        tw = d.textlength(str(s), font=f_sz)
        d.text((cx + s / 2 - tw / 2, y0 + 100 + s // 2 + 6), str(s), font=f_sz, fill=col)
        cx += s + 28

bp = os.path.join(OUT, "tapkey-t-board.png")
board.convert("RGB").save(bp)
print("board:", bp)
print("masters:", [os.path.join(OUT, f"tapkey-t-{k}.png") for k in masters])
