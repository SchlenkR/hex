#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_bundle=$("$root/scripts/build-local.sh")
installed_bundle="/Applications/Hex.app"

if [ -d "$installed_bundle" ]; then
  existing_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$installed_bundle/Contents/Info.plist" 2>/dev/null || true)
  if [ "$existing_id" != com.kitlangton.hex2 ]; then
    echo "Refusing to replace $installed_bundle: it is not the Rust Hex app." >&2
    exit 1
  fi
  pkill -TERM -x hex || true
  attempts=30
  while pgrep -x hex >/dev/null; do
    [ "$attempts" -gt 0 ] || { echo "Timed out waiting for Hex to stop." >&2; exit 1; }
    attempts=$((attempts - 1))
    sleep 1
  done
fi
rm -rf "$installed_bundle"
/usr/bin/ditto "$source_bundle" "$installed_bundle"
/usr/bin/open "$installed_bundle"
echo "Installed $installed_bundle"
