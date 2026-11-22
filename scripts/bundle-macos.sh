#!/usr/bin/env bash
# Build fdt.app for this Mac: a release build wrapped in an app bundle with the icon, signed ad hoc
# so it runs locally. This is for daily use on the author's machine; signed, notarized universal
# DMGs come with M8.
#
#   scripts/bundle-macos.sh            # builds target/release/bundle/fdt.app
#   scripts/bundle-macos.sh --install  # ...and copies it to /Applications (replacing an older copy)
set -euo pipefail
cd "$(dirname "$0")/.."

# Placeholder until the product name and publisher are chosen (spec §12.2).
BUNDLE_ID="dev.fdt.app"
NAME="fdt"

install=false
case "${1:-}" in
  --install) install=true ;;
  "") ;;
  *) echo "usage: scripts/bundle-macos.sh [--install]" >&2; exit 2 ;;
esac

[ "$(uname -s)" = Darwin ] || { echo "bundle-macos.sh only runs on macOS" >&2; exit 1; }

cargo build --release -p fdt_app
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)

icns=assets/icon/macos/fdt.icns
[ -f "$icns" ] || assets/icon/build.sh

app=target/release/bundle/$NAME.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/fdt "$app/Contents/MacOS/fdt"
cp "$icns" "$app/Contents/Resources/fdt.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleExecutable</key><string>fdt</string>
  <key>CFBundleIconFile</key><string>fdt</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"
echo "built $app ($version)"

if $install; then
  dest=/Applications/$NAME.app
  if pgrep -xq fdt; then
    echo "fdt is running; quit it first so the copy in /Applications can be replaced" >&2
    exit 1
  fi
  rm -rf "$dest"
  ditto "$app" "$dest"
  # Make Finder and the Dock pick up the new icon straight away.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$dest"
  touch "$dest"
  echo "installed $dest"
fi
