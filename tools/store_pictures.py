"""Makes the store pictures from a real picture of the extension's popup: store/screenshot-1280x800.png and store/promo-440x280.png.

    python tools/store_pictures.py <popup.png>

The popup picture is taken from the extension itself (loaded unpacked, its popup page pictured at 2x); nothing in it is drawn by hand.
"""
import os, sys
from PIL import Image, ImageDraw, ImageFont

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
popup = Image.open(sys.argv[1]).convert("RGBA")
FONTS = r"C:\Windows\Fonts"
EMBER, INK, DIM, BG = (255, 106, 26), (242, 240, 236), (157, 155, 162), (5, 5, 5)


def font(name, size):
    return ImageFont.truetype(os.path.join(FONTS, name), size)


def rounded(im, radius):
    mask = Image.new("L", im.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, im.size[0] - 1, im.size[1] - 1], radius, fill=255)
    out = Image.new("RGBA", im.size, (0, 0, 0, 0))
    out.paste(im, (0, 0), mask)
    return out


# ---- the screenshot: what it does in words at the left, the popup itself at the right
shot = Image.new("RGBA", (1280, 800), BG + (255,))
d = ImageDraw.Draw(shot)
logo = Image.open(os.path.join(root, "extension", "icons", "128.png")).convert("RGBA").resize((72, 72), Image.LANCZOS)
shot.alpha_composite(logo, (88, 150))
d.text((176, 162), "HyperDownload", font=font("seguisb.ttf", 44), fill=INK)
d.text((88, 262), "Your browser's downloads,", font=font("seguisb.ttf", 40), fill=INK)
d.text((88, 314), "handed to a download manager.", font=font("seguisb.ttf", 40), fill=EMBER)
y = 404
for line in ["Many connections per file, pause and resume",
             "Small files and the sites you list stay with the browser",
             "Hold Alt while clicking to let the browser download",
             "Needs rustyfiles and its HyperDownload add-on",
             "Free and open source"]:
    d.ellipse([92, y + 11, 100, y + 19], fill=EMBER)
    d.text((116, y), line, font=font("segoeui.ttf", 22), fill=DIM)
    y += 44
p = rounded(popup.resize((520, 520), Image.LANCZOS), 18)
frame = Image.new("RGBA", (524, 524), (0, 0, 0, 0))
ImageDraw.Draw(frame).rounded_rectangle([0, 0, 523, 523], 20, outline=(60, 60, 66, 255), width=2)
shot.alpha_composite(p, (688, 140))
shot.alpha_composite(frame, (686, 138))
shot.convert("RGB").save(os.path.join(root, "store", "screenshot-1280x800.png"))

# ---- the small tile
tile = Image.new("RGBA", (440, 280), BG + (255,))
d = ImageDraw.Draw(tile)
big = Image.open(os.path.join(root, "extension", "icons", "128.png")).convert("RGBA").resize((88, 88), Image.LANCZOS)
tile.alpha_composite(big, (176, 46))
f = font("seguisb.ttf", 30)
w = d.textlength("HyperDownload", font=f)
d.text(((440 - w) / 2, 150), "HyperDownload", font=f, fill=INK)
f2 = font("segoeui.ttf", 16)
t = "The download manager of rustyfiles"
w = d.textlength(t, font=f2)
d.text(((440 - w) / 2, 196), t, font=f2, fill=DIM)
tile.convert("RGB").save(os.path.join(root, "store", "promo-440x280.png"))
print("wrote store/screenshot-1280x800.png and store/promo-440x280.png")
