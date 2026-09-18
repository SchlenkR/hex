#!/bin/sh
# Local fork build: no Developer ID, no Sparkle updater, no Commands SDK.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
identity=${HEX_LOCAL_CODESIGN_IDENTITY:-Hex Local Build}
bundle="$root/target/app/Hex.app"
executable="$bundle/Contents/MacOS/hex"
icon_output="$root/target/AppIcon.assets"
icon_info="$root/target/AppIcon-info.plist"
version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" | jq -r '.packages[0].version')
build_number=$(printf '%s\n' "$version" | awk -F. '{ print ($1 * 10000) + ($2 * 100) + $3 }')

export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --release --manifest-path "$root/Cargo.toml" >&2
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$root/target/release/voice-control" "$executable"
cp "$root/app/Info.plist" "$bundle/Contents/Info.plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$bundle/Contents/Resources/"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build_number" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier com.kitlangton.hex2" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable hex" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Delete :SUFeedURL" "$bundle/Contents/Info.plist" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Delete :SUPublicEDKey" "$bundle/Contents/Info.plist" 2>/dev/null || true
rm -rf "$icon_output"
mkdir -p "$icon_output"
xcrun actool "$root/app/AppIcon.icon" \
  --compile "$icon_output" \
  --platform macosx \
  --minimum-deployment-target 15.0 \
  --app-icon AppIcon \
  --standalone-icon-behavior all \
  --output-partial-info-plist "$icon_info" \
  --output-format human-readable-text \
  --warnings \
  --notices >&2
cp "$icon_output/AppIcon.icns" "$icon_output/Assets.car" "$bundle/Contents/Resources/"
codesign --force --options runtime --entitlements "$root/app/VoiceControl.entitlements" --sign "$identity" "$bundle" >&2
codesign --verify --deep --strict "$bundle" >&2
printf '%s\n' "$bundle"
