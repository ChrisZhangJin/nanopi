"""Regenerate assets/nanopi-logo.png (needs Pillow; not a build dependency).

    python3 assets/make_logo.py

A gradient badge with a white pi, then the "nanopi" wordmark in the same
cyan -> violet -> magenta ramp the agents strip and progress bars use.
Transparent background, so it reads on dark and light terminals.
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

S = 4                      # supersampling factor
W, H = 640, 160            # final size (4:1)
RAMP = [(0, 215, 255), (95, 135, 255), (175, 95, 255), (215, 95, 215)]
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"


def ramp(t):
    t = min(max(t, 0.0), 1.0) * (len(RAMP) - 1)
    i = min(int(t), len(RAMP) - 2)
    f = t - i
    a, b = RAMP[i], RAMP[i + 1]
    return tuple(round(a[k] + (b[k] - a[k]) * f) for k in range(3))


def gradient(w, h, start=0.0, end=1.0):
    img = Image.new("RGBA", (w, h))
    px = img.load()
    for x in range(w):
        c = ramp(start + (end - start) * x / max(w - 1, 1)) + (255,)
        for y in range(h):
            px[x, y] = c
    return img


canvas = Image.new("RGBA", (W * S, H * S), (0, 0, 0, 0))

# Badge: rounded square, diagonal-ish gradient, soft inner highlight.
side, pad = 136 * S, 12 * S
mask = Image.new("L", (side, side), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, side - 1, side - 1), radius=34 * S, fill=255)
badge = gradient(side, side, 0.0, 0.75)
hl = Image.new("L", (side, side), 0)
ImageDraw.Draw(hl).ellipse((-side // 3, -side // 2, side, side // 2), fill=60)
badge = Image.composite(Image.new("RGBA", (side, side), (255, 255, 255, 255)), badge, hl.filter(ImageFilter.GaussianBlur(18 * S)))
canvas.paste(badge, (pad, pad), mask)

# The pi, white, centred in the badge.
d = ImageDraw.Draw(canvas)
pi_font = ImageFont.truetype(BOLD, 104 * S)
box = d.textbbox((0, 0), "π", font=pi_font)
pw, ph = box[2] - box[0], box[3] - box[1]
d.text((pad + (side - pw) // 2 - box[0], pad + (side - ph) // 2 - box[1] - 2 * S), "π", font=pi_font, fill=(255, 255, 255, 255))

# Wordmark: gradient-filled "nanopi".
word_font = ImageFont.truetype(BOLD, 92 * S)
x0 = pad + side + 26 * S
wb = d.textbbox((0, 0), "nanopi", font=word_font)
ww, wh = wb[2] - wb[0], wb[3] - wb[1]
text_mask = Image.new("L", (ww, wh), 0)
ImageDraw.Draw(text_mask).text((-wb[0], -wb[1]), "nanopi", font=word_font, fill=255)
fill = gradient(ww, wh, 0.05, 1.0)
canvas.paste(fill, (x0, (H * S - wh) // 2 + 6 * S), text_mask)

out = canvas.resize((W, H), Image.LANCZOS)
# Crop the empty right margin so the logo takes no wasted columns.
right = out.getchannel("A").getbbox()[2]
out = out.crop((0, 0, min(W, right + 12), H))
dest = Path(__file__).with_name("nanopi-logo.png")
out.save(dest, optimize=True)
print(dest, out.size, dest.stat().st_size, "bytes")
