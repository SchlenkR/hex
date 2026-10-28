#!/bin/sh
# Fork: installs a local build signed with the self-signed "Hex Local Build" certificate.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
HEX_LOCAL_BUILD=1 HEX_LOCAL_CODESIGN_IDENTITY="${HEX_LOCAL_CODESIGN_IDENTITY:-Hex Local Build}" \
  exec "$root/scripts/install-app.sh"
