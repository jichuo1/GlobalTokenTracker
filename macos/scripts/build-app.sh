#!/bin/bash
# Build GlobalTokenTracker.app for macOS.
#
#   macos/scripts/build-app.sh            # host arch, release
#   macos/scripts/build-app.sh --universal --zip
#
# Needs: Rust (rustup), Swift 6.2+ (Command Line Tools are enough).
# Output: target/macos/GlobalTokenTracker.app (+ the release zip with --zip,
# named as core::update::asset_name expects).
set -euo pipefail

UNIVERSAL=0 ZIP=0
for a in "$@"; do
  case "$a" in
    --universal) UNIVERSAL=1 ;;
    --zip) ZIP=1 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/macos"
OUT="$ROOT/target/macos"
APP="$OUT/GlobalTokenTracker.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
BUILD="$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 1)"
export MACOSX_DEPLOYMENT_TARGET=26.0

if [[ $UNIVERSAL == 1 ]]; then
  ARCHS=(arm64 x86_64)
else
  ARCHS=("$(uname -m)")
fi
rust_triple() { [[ $1 == arm64 ]] && echo aarch64-apple-darwin || echo x86_64-apple-darwin; }

echo "==> Rust engine (${ARCHS[*]})"
LIBS=()
TARGET_FLAGS=()
INSTALLED_TARGETS="$(rustup target list --installed 2>/dev/null || true)"
for arch in "${ARCHS[@]}"; do
  t="$(rust_triple "$arch")"
  if ! grep -qx "$t" <<<"$INSTALLED_TARGETS"; then
    rustup target add "$t" >/dev/null 2>&1 || true
  fi
  TARGET_FLAGS+=(--target "$t")
  LIBS+=("$ROOT/target/$t/release/libgtt_ffi.a")
done
cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked -p globaltokentracker-ffi "${TARGET_FLAGS[@]}"
mkdir -p "$ROOT/target/universal-release"
lipo -create "${LIBS[@]}" -output "$ROOT/target/universal-release/libgtt_ffi.a"

echo "==> Swift shell (${ARCHS[*]})"
BINS=()
for arch in "${ARCHS[@]}"; do
  swift build --package-path "$PKG" -c release --triple "$arch-apple-macosx26.0" \
    --scratch-path "$PKG/.build/$arch"
  BINS+=("$PKG/.build/$arch/release/GlobalTokenTracker")
done

echo "==> Bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create "${BINS[@]}" -output "$APP/Contents/MacOS/GlobalTokenTracker"
strip -x "$APP/Contents/MacOS/GlobalTokenTracker"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" \
  "$PKG/Resources/Info.plist" > "$APP/Contents/Info.plist"
cp -R "$PKG"/Resources/*.lproj "$APP/Contents/Resources/"

# App icon: assets/icon.png → AppIcon.icns (all standard sizes).
ICONSET="$OUT/AppIcon.iconset"
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$ROOT/assets/icon.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$ROOT/assets/icon.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

# Ad-hoc signature unless CODESIGN_IDENTITY names a Developer ID (then a secure
# timestamp too, which notarization requires).
if [[ -n "${CODESIGN_IDENTITY:-}" && "${CODESIGN_IDENTITY}" != "-" ]]; then
  codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp "$APP"
else
  codesign --force --sign - --options runtime --timestamp=none "$APP"
fi

if [[ $ZIP == 1 ]]; then
  ZIPNAME="GlobalTokenTracker-v$VERSION-macos-universal.zip"
  (cd "$OUT" && rm -f "$ZIPNAME" && ditto -c -k --keepParent GlobalTokenTracker.app "$ZIPNAME")
  echo "==> $OUT/$ZIPNAME"
fi
echo "==> $APP ($(du -sh "$APP" | cut -f1))"
