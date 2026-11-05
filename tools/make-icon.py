"""Draw the app icon (assets/icon-*.png, assets/idrac6.ico). Needs Pillow.

usage: python3 tools/make-icon.py assets
"""
import sys
from PIL import Image, ImageDraw
out = sys.argv[1]
S = 1024
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
# background tile with vertical gradient
grad = Image.new("RGBA", (S, S))
gd = ImageDraw.Draw(grad)
for y in range(S):
    t = y / S
    c = (int(28 + 10 * t), int(78 - 30 * t), int(160 - 50 * t), 255)
    gd.line([(0, y), (S, y)], fill=c)
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle([40, 40, S - 40, S - 40], radius=200, fill=255)
img.paste(grad, (0, 0), mask)
d = ImageDraw.Draw(img)
# monitor
d.rounded_rectangle([170, 210, 854, 700], radius=48, fill=(236, 241, 248, 255))
d.rounded_rectangle([214, 254, 810, 656], radius=20, fill=(20, 27, 38, 255))
# stand
d.polygon([(452, 700), (572, 700), (602, 800), (422, 800)], fill=(236, 241, 248, 255))
d.rounded_rectangle([352, 790, 672, 836], radius=22, fill=(236, 241, 248, 255))
# server rack units on screen
for i, y in enumerate((300, 410, 520)):
    d.rounded_rectangle([280, y, 744, y + 82], radius=14, fill=(58, 70, 88, 255))
    for k in range(4):
        x = 320 + k * 52
        d.rounded_rectangle([x, y + 24, x + 36, y + 58], radius=6, fill=(32, 40, 52, 255))
    d.ellipse([650, y + 25, 682, y + 57], fill=(80, 220, 110, 255) if i != 1 else (255, 190, 60, 255))
    d.ellipse([600, y + 31, 620, y + 51], fill=(90, 160, 255, 255))
for size in (16, 24, 32, 48, 64, 128, 256, 512):
    img.resize((size, size), Image.LANCZOS).save(f"{out}/icon-{size}.png")
img.resize((256, 256), Image.LANCZOS).save(f"{out}/idrac6.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
