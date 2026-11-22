#!/usr/bin/env python3
"""Generate the fdt app icon SVGs: a rocket climbing out of a swooping exhaust trail.

    python3 assets/icon/generate.py      # writes the SVGs next to this file; build.sh renders them

Outputs:
  fdt-icon.svg              macOS: the rocket on the graphite plate, 1024 canvas with Apple's margin
  fdt-icon-small.svg        the same for 16 and 32 px: no stars, no speed streak
  fdt-icon-flat.svg         Windows and Linux: rocket and trail alone on transparency, filling the canvas
  fdt-icon-flat-small.svg   the same for 16 and 32 px
  layers/*.svg              Icon Composer layers for macOS 26+ (stars, trail, rocket); no plate, no blur.
                            Set the background in Icon Composer to the plate gradient below.
"""
import math
from pathlib import Path

OUT = Path(__file__).resolve().parent

# Plate gradient, bottom to top (fdt_ui tokens `bg` and `elevated`, dark theme).
PLATE_BOTTOM, PLATE_TOP = "#0D0E11", "#1A1C22"

# The trail is a quadratic curve; the rocket sits on its end, pointing along it.
P0, P1, P2 = (150, 560), (300, 900), (470, 650)
ROCKET_SCALE = 0.86
STREAK_OFFSET = 64          # the thin speed streak runs this far outside the main trail
STARS = [(232, 262, 7, 0.9), (336, 196, 4, 0.6), (190, 420, 4, 0.5),
         (446, 290, 5, 0.7), (850, 640, 4, 0.5), (760, 800, 5, 0.6)]

# Rocket drawn nose-up around (0, 0): hull, fins, porthole, nozzle and flame.
HULL = ("M0 -220 C58 -184 84 -100 82 -10 L74 140 Q72 162 50 162 L-50 162 "
        "Q-72 162 -74 140 L-82 -10 C-84 -100 -58 -184 0 -220 Z")


def rocket_art(effects):
    glow = ('<g filter="url(#blur24)" opacity="0.9"><path d="M-50 186 C-50 260 -14 340 0 372 '
            'C14 340 50 260 50 186 Z" fill="#E5734B"/></g>') if effects else ""
    return f'''
      {glow}
      <path d="M-46 186 C-46 258 -14 332 0 362 C14 332 46 258 46 186 Z" fill="url(#flame)"/>
      <path d="M-22 186 C-22 236 -6 278 0 298 C6 278 22 236 22 186 Z" fill="#FFF4E0" opacity="0.95"/>
      <path d="M-76 30 C-128 64 -152 124 -152 198 C-152 207 -143 210 -136 203 L-72 150 Z" fill="url(#fin)"/>
      <path d="M76 30 C128 64 152 124 152 198 C152 207 143 210 136 203 L72 150 Z" fill="url(#fin)"/>
      <path d="M-40 160 L40 160 L50 192 L-50 192 Z" fill="url(#nozzle)"/>
      <path d="{HULL}" fill="url(#hullFill)"/>
      <g clip-path="url(#hullClip)">
        <rect x="-100" y="-240" width="200" height="88" fill="url(#calm)"/>
        <rect x="-100" y="72" width="200" height="26" fill="url(#calm)"/>
        <rect x="18" y="-240" width="90" height="420" fill="#0D0E11" opacity="0.18"/>
      </g>
      <circle cx="0" cy="-44" r="52" fill="#2B3040"/>
      <circle cx="0" cy="-44" r="40" fill="url(#window)"/>
      <ellipse cx="-13" cy="-58" rx="13" ry="9" fill="#FFFFFF" opacity="0.8" transform="rotate(-35 -13 -58)"/>
      <rect x="-10" y="96" width="20" height="112" rx="10" fill="url(#fin)"/>'''


def curve(t):
    """Point and unit normal on the trail at t in [0, 1]."""
    x = (1 - t) ** 2 * P0[0] + 2 * (1 - t) * t * P1[0] + t * t * P2[0]
    y = (1 - t) ** 2 * P0[1] + 2 * (1 - t) * t * P1[1] + t * t * P2[1]
    dx = 2 * (1 - t) * (P1[0] - P0[0]) + 2 * t * (P2[0] - P1[0])
    dy = 2 * (1 - t) * (P1[1] - P0[1]) + 2 * t * (P2[1] - P1[1])
    length = math.hypot(dx, dy)
    return x, y, -dy / length, dx / length


def swoosh(w0, w1, t0, t1, fill, opacity=1.0, offset=0.0):
    """A tapered stroke along the trail from t0 (width w0) to t1 (width w1)."""
    left, right = [], []
    for k in range(41):
        t = t0 + (t1 - t0) * k / 40
        x, y, nx, ny = curve(t)
        x, y = x + nx * offset, y + ny * offset
        w = (w0 + (w1 - w0) * ((t - t0) / (t1 - t0)) ** 1.25) / 2
        left.append((x + nx * w, y + ny * w))
        right.append((x - nx * w, y - ny * w))
    d = "M" + " L".join(f"{x:.1f} {y:.1f}" for x, y in left + right[::-1]) + " Z"
    return f'<path d="{d}" fill="{fill}" opacity="{opacity}"/>'


def trail(effects, streak):
    parts = []
    if effects:
        parts.append(f'<g filter="url(#blur24)" opacity="0.55">{swoosh(10, 96, 0.3, 1.0, "url(#exhaust)")}</g>')
    parts.append(swoosh(3, 74, 0.0, 1.0, "url(#exhaust)"))
    if streak:
        parts.append(swoosh(2, 16, 0.25, 0.86, "#8E9CFF", 0.45, offset=STREAK_OFFSET))
    return "\n    ".join(parts)


def rocket(effects):
    x, y, _, _ = curve(1.0)
    theta = math.degrees(math.atan2(P2[0] - P1[0], -(P2[1] - P1[1])))   # clockwise from straight up
    th = math.radians(theta)
    # Put the flame's hot core on the end of the trail.
    ox, oy = x + 290 * ROCKET_SCALE * math.sin(th), y - 290 * ROCKET_SCALE * math.cos(th)
    return (f'<g transform="translate({ox:.1f} {oy:.1f}) rotate({theta:.1f}) scale({ROCKET_SCALE})">'
            f'{rocket_art(effects)}\n    </g>')


def stars():
    return "\n    ".join(
        f'<circle cx="{sx}" cy="{sy}" r="{r * 2.2}" fill="url(#star)" opacity="{o}"/>'
        f'<circle cx="{sx}" cy="{sy}" r="{r * 0.6}" fill="#FFFFFF" opacity="{o}"/>'
        for sx, sy, r, o in STARS)


def defs(effects):
    x, y, _, _ = curve(1.0)
    filters = '''
    <filter id="blur24" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="24"/></filter>
    <filter id="drop" x="-20%" y="-20%" width="140%" height="140%">
      <feGaussianBlur in="SourceAlpha" stdDeviation="14"/><feOffset dy="12"/>
      <feComponentTransfer><feFuncA type="linear" slope="0.45"/></feComponentTransfer>
      <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <filter id="grain" x="0" y="0" width="100%" height="100%" color-interpolation-filters="sRGB">
      <feTurbulence type="fractalNoise" baseFrequency="0.85" numOctaves="2" seed="7" stitchTiles="stitch" result="n"/>
      <feColorMatrix in="n" type="matrix" values="1 0 0 0 0  1 0 0 0 0  1 0 0 0 0  0 0 0 0 1" result="g"/>
      <feComposite in="SourceGraphic" in2="g" operator="arithmetic" k1="0" k2="1" k3="0.032" k4="-0.016" result="d"/>
      <feComposite in="d" in2="SourceGraphic" operator="in"/>
    </filter>''' if effects else ""
    return f'''  <defs>
    <linearGradient id="plate" x1="0" y1="1" x2="0" y2="0">
      <stop offset="0" stop-color="{PLATE_BOTTOM}"/><stop offset="1" stop-color="{PLATE_TOP}"/>
    </linearGradient>
    <linearGradient id="rim" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.16"/>
      <stop offset="0.12" stop-color="#FFFFFF" stop-opacity="0.03"/>
      <stop offset="0.5" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="calm" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#8E9CFF"/><stop offset="1" stop-color="#4E60EF"/>
    </linearGradient>
    <linearGradient id="exhaust" gradientUnits="userSpaceOnUse" x1="{P0[0]}" y1="{P0[1]}" x2="{x:.0f}" y2="{y:.0f}">
      <stop offset="0" stop-color="#4E60EF" stop-opacity="0"/><stop offset="0.45" stop-color="#7B8CFF" stop-opacity="0.85"/>
      <stop offset="0.78" stop-color="#B07CFF"/><stop offset="0.92" stop-color="#E5534B"/><stop offset="1" stop-color="#E2A336"/>
    </linearGradient>
    <clipPath id="hullClip"><path d="{HULL}"/></clipPath>
    <linearGradient id="hullFill" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#FFFFFF"/><stop offset="0.45" stop-color="#E7E8EB"/><stop offset="1" stop-color="#A9AFC0"/>
    </linearGradient>
    <linearGradient id="fin" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#8E9CFF"/><stop offset="1" stop-color="#3A4ACF"/>
    </linearGradient>
    <linearGradient id="nozzle" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#2B3040"/><stop offset="0.5" stop-color="#596180"/><stop offset="1" stop-color="#262A36"/>
    </linearGradient>
    <radialGradient id="window" cx="0.38" cy="0.32" r="0.75">
      <stop offset="0" stop-color="#C3CBFF"/><stop offset="0.5" stop-color="#5566F2"/><stop offset="1" stop-color="#1B2250"/>
    </radialGradient>
    <linearGradient id="flame" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFD08A"/><stop offset="0.35" stop-color="#E2A336"/>
      <stop offset="0.75" stop-color="#E5534B"/><stop offset="1" stop-color="#E5534B" stop-opacity="0"/>
    </linearGradient>
    <radialGradient id="star"><stop offset="0" stop-color="#FFFFFF"/><stop offset="1" stop-color="#FFFFFF" stop-opacity="0"/></radialGradient>
    <clipPath id="plateClip"><rect x="100" y="100" width="824" height="824" rx="185"/></clipPath>{filters}
  </defs>'''


def svg(body, effects=True):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">\n'
            f'{defs(effects)}\n{body}\n</svg>\n')


def plated(small):
    scene = "\n    ".join(([] if small else [stars()]) + [trail(True, streak=not small), rocket(True)])
    return svg(f'''  <g filter="url(#drop)"><rect x="100" y="100" width="824" height="824" rx="185" fill="url(#plate)" filter="url(#grain)"/></g>
  <rect x="101.5" y="101.5" width="821" height="821" rx="183.5" fill="none" stroke="url(#rim)" stroke-width="3"/>
  <g clip-path="url(#plateClip)">
    {scene}
  </g>''')


# Trail plus rocket span x 159..727, y 287..825 on the plate (measured from a render, glow included);
# scale that to fill the canvas, a little above centre to leave room for the drop shadow.
FLAT_SCALE = 1.55
FLAT_SHIFT = (512 - FLAT_SCALE * 443, 500 - FLAT_SCALE * 556)


def flat(small):
    scene = "\n    ".join([trail(True, streak=not small), rocket(True)])
    return svg(f'''  <g filter="url(#drop)" transform="translate({FLAT_SHIFT[0]:.1f} {FLAT_SHIFT[1]:.1f}) scale({FLAT_SCALE})">
    {scene}
  </g>''')


def main():
    layers = OUT / "layers"
    layers.mkdir(exist_ok=True)
    for old in layers.glob("*.svg"):
        old.unlink()
    files = {
        "fdt-icon.svg": plated(False),
        "fdt-icon-small.svg": plated(True),
        "fdt-icon-flat.svg": flat(False),
        "fdt-icon-flat-small.svg": flat(True),
        "layers/1-stars.svg": svg(stars(), effects=False),
        "layers/2-trail.svg": svg(trail(False, streak=True), effects=False),
        "layers/3-rocket.svg": svg(rocket(False), effects=False),
    }
    for name, text in files.items():
        (OUT / name).write_text(text)
        print(name)


if __name__ == "__main__":
    main()
