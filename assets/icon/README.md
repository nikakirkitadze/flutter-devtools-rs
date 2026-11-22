# App icon

A rocket climbing out of a swooping exhaust trail, on the graphite plate. The colours come from
the dark theme tokens in `fdt_ui`: indigo `accent` for the nose and fins, the `lane-state` violet
in the trail, and the `warning` and `danger` amber and red in the flame.

`generate.py` is the source of truth. It writes the SVGs, and `build.sh` renders them into the
platform files. Change the drawing in `generate.py`, then run:

```
assets/icon/build.sh
```

It needs `rsvg-convert` and ImageMagick (`brew install librsvg imagemagick`). The `.icns` also
needs `iconutil`, which ships with macOS.

| File | Use |
|------|-----|
| `fdt-icon.svg`, `fdt-icon-1024.png` | macOS master: rocket on the plate, with Apple's 100 px margin |
| `fdt-icon-small.svg` | 16 and 32 px: no stars, no speed streak |
| `fdt-icon-flat.svg` | Windows and Linux: no plate, fills the canvas |
| `macos/fdt.icns` | app bundle icon (`scripts/bundle-macos.sh` uses it) |
| `windows/fdt.ico` | 16 to 256 px |
| `linux/` | hicolor PNGs, 16 to 512 px, plus `fdt.svg` |
| `layers/` | Icon Composer layers for macOS 26+ (stars, trail, rocket) |

For the macOS 26 Liquid Glass icon, import the three `layers/` files into Icon Composer in order,
and set the background to a vertical gradient from `#0D0E11` (bottom) to `#1A1C22` (top).
