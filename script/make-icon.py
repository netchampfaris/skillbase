#!/usr/bin/env python3
"""Draw the application icon.

The icon is generated rather than checked in as a hand-drawn file so that a
change to the mark is a change to a few numbers here. Run it after editing:

    python3 script/make-icon.py

It writes `assets/icon.png` at 1024x1024, which `script/bundle-macos.sh` turns
into the `.icns` the bundle carries.

The mark is three offset cards, the front one solid and the two behind it at
falling opacity. That is the same duotone language as the Phosphor set the
interface uses, and it says what the application is for: one skill, held once,
showing up in several places at once.
"""

from PIL import Image, ImageDraw

# Drawn at 4x and downsampled, which is cheaper than antialiasing by hand and
# gives clean edges on the superellipse.
SIZE = 1024
SCALE = 4
S = SIZE * SCALE

# macOS leaves the outer eighth of the canvas empty and rounds the rest into a
# superellipse rather than a plain rounded rectangle.
MARGIN = 100 * SCALE
SQUIRCLE_N = 4.3

TOP = (0xC0, 0x9B, 0xFB)
BOTTOM = (0x7C, 0x4D, 0xE0)

CARD = 372 * SCALE
CARD_RADIUS = 84 * SCALE
OFFSET = 62 * SCALE
# Back to front. The back cards are far more opaque than a duotone icon's
# secondary path, because at 32 points anything fainter disappears and the mark
# collapses into one white square.
CARD_ALPHA = [92, 168, 255]

# Three lines on the front card, which is what makes the mark a document rather
# than a blank tile. They are knocked out in the background colour.
LINE_HEIGHT = 26 * SCALE
LINE_GAP = 42 * SCALE
LINE_WIDTHS = [0.60, 0.74, 0.42]


def superellipse(cx, cy, half, n, steps=2048):
    """Points on |x/half|^n + |y/half|^n = 1, centred on (cx, cy)."""
    points = []
    for i in range(steps):
        t = 2.0 * 3.141592653589793 * i / steps
        ct, st = __import__("math").cos(t), __import__("math").sin(t)
        x = half * (abs(ct) ** (2.0 / n)) * (1 if ct >= 0 else -1)
        y = half * (abs(st) ** (2.0 / n)) * (1 if st >= 0 else -1)
        points.append((cx + x, cy + y))
    return points


def main():
    icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # The gradient is painted over the whole canvas and then masked to the
    # squircle, so the shape is defined in exactly one place.
    gradient = Image.new("RGB", (1, S))
    for y in range(S):
        t = y / (S - 1)
        gradient.putpixel(
            (0, y),
            tuple(round(TOP[c] + (BOTTOM[c] - TOP[c]) * t) for c in range(3)),
        )
    gradient = gradient.resize((S, S))

    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon(
        superellipse(S / 2, S / 2, (S - 2 * MARGIN) / 2, SQUIRCLE_N), fill=255
    )
    icon.paste(gradient, (0, 0), mask)

    # The three cards, drawn on their own layer so their alpha composites
    # against the gradient rather than against each other.
    span = CARD + 2 * OFFSET
    # The solid front card carries most of the visual weight, so centring the
    # bounding box leaves the mark reading low and left. Nudge it back.
    left = (S - span) / 2 + 8 * SCALE
    top = (S - span) / 2 - 12 * SCALE
    for index, alpha in enumerate(CARD_ALPHA):
        # Back card up and right, front card down and left.
        x = left + (2 - index) * OFFSET
        y = top + index * OFFSET
        layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
        draw = ImageDraw.Draw(layer)
        draw.rounded_rectangle(
            (x, y, x + CARD, y + CARD), radius=CARD_RADIUS, fill=(255, 255, 255, alpha)
        )
        if index == len(CARD_ALPHA) - 1:
            # The lines take the gradient's colour at the card's own height, so
            # the knockout matches whatever is behind it.
            shade = gradient.getpixel((0, int(y + CARD / 2)))
            block = LINE_HEIGHT * len(LINE_WIDTHS) + LINE_GAP * (len(LINE_WIDTHS) - 1)
            first = y + (CARD - block) / 2
            inset = CARD * 0.16
            for row, width in enumerate(LINE_WIDTHS):
                top_y = first + row * (LINE_HEIGHT + LINE_GAP)
                draw.rounded_rectangle(
                    (
                        x + inset,
                        top_y,
                        x + inset + (CARD - 2 * inset) * width,
                        top_y + LINE_HEIGHT,
                    ),
                    radius=LINE_HEIGHT / 2,
                    fill=(*shade, 255),
                )
        icon = Image.alpha_composite(icon, layer)

    icon.resize((SIZE, SIZE), Image.LANCZOS).save("assets/icon.png")
    print("assets/icon.png")


if __name__ == "__main__":
    main()
