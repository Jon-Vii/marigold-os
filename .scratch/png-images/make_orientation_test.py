"""Test PNGs that make image orientation unambiguous on the 1-bit panel.

Two variants:
- orientation-test.png (640x900): tall marker, scales into a reader page.
- orientation-fullpage.png (784x440): matches the X4 landscape page box
  (left 8, right W-8, top 6, bottom H-23) so it renders 1:1, no scaling.

Both: up/down arrows, TOP/BOTTOM labels, a dark-to-light brightness ladder
(row order + dithering), a thick LEFT bar and a thin RIGHT bar (mirroring).
8-bit grayscale, non-interlaced -- exactly what the firmware decoder takes.
"""

from PIL import Image, ImageDraw, ImageFont

BLACK, WHITE = 0, 255
font = ImageFont.load_default(size=64)
small = ImageFont.load_default(size=40)

def center_text(d, y, text, f, w, x0=0):
    box = d.textbbox((0, 0), text, font=f)
    d.text((x0 + ((w - (box[2] - box[0])) // 2), y), text, font=f, fill=BLACK)

def tri(d, cx, cy, size, up=True):
    w = size // 2
    pts = ([(cx, cy - size // 2), (cx - w, cy + size // 3), (cx + w, cy + size // 3)]
           if up else
           [(cx, cy + size // 2), (cx - w, cy - size // 3), (cx + w, cy - size // 3)])
    d.polygon(pts, fill=BLACK)

def frame(d, w, h):
    d.rectangle([0, 0, w, 9], fill=BLACK)
    d.rectangle([0, h - 9, w, h], fill=BLACK)
    d.rectangle([0, 0, 26, h], fill=BLACK)      # thick LEFT bar
    d.rectangle([w - 10, 0, w, h], fill=BLACK)  # thin RIGHT bar

def ladder(d, l, r, top, bottom, bands=6):
    h = (bottom - top) // bands
    for i in range(bands):
        value = 40 + i * 40
        t = top + i * h
        d.rectangle([l, t, r, t + h - 8], fill=value)
        label = str(i)
        box = d.textbbox((0, 0), label, font=small)
        d.text((r - (box[2] - box[0]) - 12, t + 8), label,
               font=small, fill=BLACK if value > 128 else WHITE)

# --- tall variant ---------------------------------------------------------
W, H = 640, 900
img = Image.new("L", (W, H), 255)
d = ImageDraw.Draw(img)
frame(d, W, H)
tri(d, W // 2, 130, 200, up=True)
center_text(d, 250, "TOP", font, W)
ladder(d, 90, W - 130, 370, 700)
center_text(d, 720, "BOTTOM", font, W)
tri(d, W // 2, 850, 130, up=False)
img.save(".scratch/png-images/orientation-test.png", optimize=True)
print("orientation-test.png", img.size, img.mode)

# --- full-page variant (X4 landscape page box: 784 x 451) -----------------
W, H = 784, 451
img = Image.new("L", (W, H), 255)
d = ImageDraw.Draw(img)
frame(d, W, H)
# Left half: up arrow + TOP. Right half: down arrow + BOTTOM.
tri(d, W // 4, 150, 180, up=True)
center_text(d, 262, "TOP", font, W // 2)
tri(d, 3 * W // 4, 330, 180, up=False)
center_text(d, 90, "BOTTOM", font, W // 2, x0=W // 2)
# Horizontal ladder along the bottom: dark at left -> light at right.
ladder_w, ladder_h = 420, 56
for i in range(6):
    value = 40 + i * 40
    x = 60 + i * (ladder_w // 6)
    d.rectangle([x, H - ladder_h - 20, x + ladder_w // 6 - 8, H - 30], fill=value)
    label = str(i)
    box = d.textbbox((0, 0), label, font=small)
    d.text((x + 8, H - ladder_h - 12), label, font=small,
           fill=BLACK if value > 128 else WHITE)
img.save(".scratch/png-images/orientation-fullpage.png", optimize=True)
print("orientation-fullpage.png", img.size, img.mode)
