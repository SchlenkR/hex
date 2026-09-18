#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
team_id=${VOICE_CONTROL_TEAM_ID:?Set VOICE_CONTROL_TEAM_ID to the Apple Developer signing team}
identity=${VOICE_CONTROL_CODESIGN_IDENTITY:-}
if [ -z "$identity" ]; then
  identity=$(security find-identity -v -p codesigning | sed -n "s/.*\"\(Developer ID Application:.*($team_id)\)\"/\1/p" | head -1)
fi
if [ -z "$identity" ]; then
  echo "No Developer ID signing identity found for team $team_id. Set VOICE_CONTROL_CODESIGN_IDENTITY." >&2
  exit 1
fi
case "$identity" in
  "Developer ID Application:"*"($team_id)") ;;
  *)
    echo "Signing identity is not a Developer ID Application identity for team $team_id." >&2
    exit 1
    ;;
esac
icon_output="$root/target/AppIcon.assets"
icon_info="$root/target/AppIcon-info.plist"
sparkle_dir=$("$root/scripts/setup-sparkle.sh")
version=${HEX_VERSION:-$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" | jq -r '.packages[0].version')}
build_number=${HEX_BUILD_NUMBER:-$(printf '%s\n' "$version" | awk -F. '{ print ($1 * 10000) + ($2 * 100) + $3 }')}
bundle_name=Hex.app
bundle_id=com.kitlangton.hex2
executable_name=hex

bundle="$root/target/app/$bundle_name"
executable="$bundle/Contents/MacOS/$executable_name"
frameworks="$bundle/Contents/Frameworks"

export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --release --manifest-path "$root/Cargo.toml"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources" "$frameworks"
cp "$root/target/release/voice-control" "$executable"
cp "$root/app/Info.plist" "$bundle/Contents/Info.plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$bundle/Contents/Resources/"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build_number" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_id" "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable $executable_name" "$bundle/Contents/Info.plist"
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

/usr/bin/ditto "$sparkle_dir/Sparkle.framework" "$frameworks/Sparkle.framework"

sparkle="$frameworks/Sparkle.framework/Versions/B"
codesign --force --timestamp --options runtime --sign "$identity" "$sparkle/XPCServices/Installer.xpc"
codesign --force --timestamp --options runtime --preserve-metadata=entitlements --sign "$identity" "$sparkle/XPCServices/Downloader.xpc"
codesign --force --timestamp --options runtime --sign "$identity" "$sparkle/Autoupdate"
codesign --force --timestamp --options runtime --sign "$identity" "$sparkle/Updater.app"
codesign --force --timestamp --options runtime --sign "$identity" "$frameworks/Sparkle.framework"
codesign --force --timestamp --options runtime --entitlements "$root/app/VoiceControl.entitlements" --sign "$identity" "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
echo "$bundle"
