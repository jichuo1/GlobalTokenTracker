#!/bin/bash
# Package target/macos/GlobalTokenTracker.app into the designed installer
# disk image (background, icon layout, Applications shortcut).
#
#   macos/scripts/build-dmg.sh [vX.Y.Z[-alpha.N]]
#
# Output: target/macos/GlobalTokenTracker-<tag>-macos-universal.dmg — the
# name core::update::asset_name expects. The tag defaults to v<workspace version>.
#
# Optional, for distribution outside this Mac (all must be set together):
#   CODESIGN_IDENTITY        "Developer ID Application: …" (also used by build-app.sh)
#   NOTARY_APPLE_ID / NOTARY_TEAM_ID / NOTARY_PASSWORD  (app-specific password)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/target/macos"
APP="$OUT/GlobalTokenTracker.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
TAG="${1:-v$VERSION}"
DMG="$OUT/GlobalTokenTracker-$TAG-macos-universal.dmg"

[[ -d "$APP" ]] || { echo "missing $APP — run macos/scripts/build-app.sh first" >&2; exit 1; }

# dmgbuild needs Python ≥ 3.10; isolated venv, pinned version.
VENV="$OUT/.dmgbuild-venv"
if [[ ! -x "$VENV/bin/dmgbuild" ]]; then
  if command -v uv >/dev/null 2>&1; then
    uv venv --quiet --python 3.13 "$VENV"
    uv pip install --quiet --python "$VENV/bin/python" "dmgbuild==1.6.7"
  else
    "${PYTHON:-python3}" -m venv "$VENV"
    "$VENV/bin/pip" install --quiet "dmgbuild==1.6.7"
  fi
fi

echo "==> Disk image $TAG"
rm -f "$DMG"
"$VENV/bin/dmgbuild" -s "$ROOT/macos/dmg/settings.py" -D app="$APP" \
  -D background="$ROOT/macos/dmg/background.tiff" "GlobalTokenTracker" "$DMG"

if [[ -n "${CODESIGN_IDENTITY:-}" && "${CODESIGN_IDENTITY}" != "-" ]]; then
  echo "==> Sign disk image"
  codesign --force --sign "$CODESIGN_IDENTITY" --timestamp "$DMG"
  if [[ -n "${NOTARY_APPLE_ID:-}" && -n "${NOTARY_TEAM_ID:-}" && -n "${NOTARY_PASSWORD:-}" ]]; then
    echo "==> Notarize (waits for Apple)"
    xcrun notarytool submit "$DMG" --apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" \
      --password "$NOTARY_PASSWORD" --wait
    xcrun stapler staple "$DMG"
  fi
fi

hdiutil verify -quiet "$DMG"
echo "==> $DMG ($(du -h "$DMG" | cut -f1))"
