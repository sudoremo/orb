#!/usr/bin/env bash
# Build every icon in this directory from Orb.icon.
#
# Orb.icon is the source: an Icon Composer document. Assets.car is its compiled
# form, the Liquid Glass icon of macOS 26. The PNGs and icon.icns serve
# everything that cannot read that — macOS before 26, the Dock icon of
# `tauri dev`, and the drag image. Run this after every change to Orb.icon.
#
# Needs Xcode 26, Icon Composer in /Applications, and ImageMagick.

set -euo pipefail

cd "$(dirname "$0")"

ictool="/Applications/Icon Composer.app/Contents/Executables/ictool"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The bundler could compile Orb.icon itself, but tauri-cli 2.11 hands actool a
# directory as its stdin, and actool then fails with "Bad file descriptor".
mkdir "$work/car"
xcrun actool Orb.icon --compile "$work/car" --output-format human-readable-text \
  --warnings --errors --output-partial-info-plist "$work/car/Info.plist" \
  --app-icon Orb --include-all-app-icons --enable-on-demand-resources NO \
  --development-region en --target-device mac --minimum-deployment-target 26.0 \
  --platform macosx </dev/null >"$work/car/log" ||
  { cat "$work/car/log" >&2; exit 1; }
if [[ ! -f "$work/car/Assets.car" ]]; then
  cat "$work/car/log" >&2
  exit 1
fi
cp "$work/car/Assets.car" Assets.car

# ictool renders the masked shape full-bleed. A static macOS icon carries the
# grid's margin around it — 824 of 1024 points — and a shadow beneath.
"$ictool" Orb.icon --export-image --output-file "$work/body.png" \
  --platform macOS --rendition Default --width 824 --height 824 --scale 1 >/dev/null
magick "$work/body.png" -background none -gravity center -extent 1024x1024 "$work/framed.png"
magick "$work/framed.png" -fill black -colorize 100 -channel A -blur 0x10 -evaluate multiply 0.3 +channel PNG32:"$work/shadow.png"
magick -size 1024x1024 xc:none \
  "$work/shadow.png" -geometry +0+10 -composite \
  "$work/framed.png" -geometry +0+0 -composite -depth 8 PNG32:"$work/master.png"

iconset="$work/icon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  magick "$work/master.png" -resize "${size}x${size}" PNG32:"$iconset/icon_${size}x${size}.png"
  magick "$work/master.png" -resize "$((size * 2))x$((size * 2))" PNG32:"$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o icon.icns

cp "$iconset/icon_32x32.png" 32x32.png
cp "$iconset/icon_128x128.png" 128x128.png
cp "$iconset/icon_128x128@2x.png" 128x128@2x.png
cp "$iconset/icon_512x512.png" icon.png
