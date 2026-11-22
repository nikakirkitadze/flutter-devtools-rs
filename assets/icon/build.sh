#!/usr/bin/env bash
# Render the icon SVGs into platform files. Needs rsvg-convert and ImageMagick (brew install librsvg
# imagemagick); the macOS .icns also needs iconutil, which ships with macOS.
#   assets/icon/build.sh
set -euo pipefail
cd "$(dirname "$0")"
python3 generate.py >/dev/null

render() { rsvg-convert -w "$3" -h "$3" "$1" -o "$2"; }
# Sizes up to 32 px use the simplified artwork.
pick() { if [ "$2" -le 32 ]; then echo "$1-small.svg"; else echo "$1.svg"; fi; }

rm -rf macos windows linux && mkdir -p macos windows linux
render fdt-icon.svg fdt-icon-1024.png 1024

# macOS .icns
iconset=$(mktemp -d)/fdt.iconset && mkdir "$iconset"
for s in 16 32 128 256 512; do
  render "$(pick fdt-icon $s)" "$iconset/icon_${s}x${s}.png" $s
  render "$(pick fdt-icon $((s * 2)))" "$iconset/icon_${s}x${s}@2x.png" $((s * 2))
done
iconutil -c icns "$iconset" -o macos/fdt.icns
rm -rf "$(dirname "$iconset")"

# Windows .ico
tmp=$(mktemp -d)
for s in 16 24 32 48 64 128 256; do render "$(pick fdt-icon-flat $s)" "$tmp/$s.png" $s; done
magick "$tmp"/{16,24,32,48,64,128,256}.png windows/fdt.ico
rm -rf "$tmp"

# Linux: hicolor PNGs plus the scalable SVG
for s in 16 24 32 48 64 128 256 512; do render "$(pick fdt-icon-flat $s)" "linux/fdt-$s.png" $s; done
cp fdt-icon-flat.svg linux/fdt.svg

echo "built macos/fdt.icns, windows/fdt.ico, linux/ and fdt-icon-1024.png"
