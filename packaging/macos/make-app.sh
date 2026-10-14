#!/bin/bash
# Builds Joyscroller.app around a compiled joyscroller and packs it in a DMG
# with an Applications shortcut to drag it onto. macOS only (sips, iconutil,
# codesign, hdiutil).
#
#   packaging/macos/make-app.sh <binary> <version> <label> <arch> <out dir>
#
# <version> is Cargo.toml's (the bundle's version must be numbers and dots);
# <label> names the file: joyscroller-<label>-macos-<arch>.dmg.
#
# The app is never signed with an Apple Developer ID or notarized. Apple
# Silicon won't run code without any signature, so it gets the free ad-hoc
# one (`codesign --sign -`), which needs no Apple account.
set -euo pipefail

binary=$1 version=$2 label=$3 arch=$4 out=$5
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

app="$work/Joyscroller.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/joyscroller"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

iconset="$work/AppIcon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$here/icon.png" --out "$iconset/icon_${size}x${size}.png" > /dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$here/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns -o "$app/Contents/Resources/AppIcon.icns" "$iconset"

codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"

stage="$work/dmg"
mkdir "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
mkdir -p "$out"
dmg="$out/joyscroller-$label-macos-$arch.dmg"
hdiutil create -volname "Joyscroller $label" -srcfolder "$stage" -ov -format UDZO "$dmg" > /dev/null
echo "$dmg"
