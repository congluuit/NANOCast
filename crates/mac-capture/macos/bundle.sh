#!/bin/sh
# Builds NANOCast.app as a universal binary (x86_64 + arm64), ad-hoc signed.
# Run on a Mac, from the repository root:
#
#   sh crates/mac-capture/macos/bundle.sh                     # GUI (package nanocast, bin nanocast)
#   sh crates/mac-capture/macos/bundle.sh --example mac_cast  # CLI test sender, no GUI needed
#
# Output: target/macos/NANOCast.app (and target/macos/NANOCast-<version>-macos.zip).
# Needs: Xcode Command Line Tools (xcode-select --install), rustup.
# Signing identity: ad-hoc by default; set SIGN_IDENTITY="NANOCast Dev" (a self-signed
# code-signing certificate in your keychain) so the Screen Recording permission survives
# rebuilds, see docs/macos.md.
set -eu

KIND=bin
NAME=nanocast
PACKAGE=nanocast
if [ "${1:-}" = "--example" ]; then
    KIND=example
    NAME=${2:?example name}
    PACKAGE=nanocast-mac
fi

ROOT=$(pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
OUT="$ROOT/target/macos"
APP="$OUT/NANOCast.app"
SIGN_IDENTITY=${SIGN_IDENTITY:--}

# ScreenCaptureKit is 12.3+; build against that so older SDK symbols are weak-linked.
export MACOSX_DEPLOYMENT_TARGET=12.3

for TARGET in x86_64-apple-darwin aarch64-apple-darwin; do
    rustup target add "$TARGET" >/dev/null
    if [ "$KIND" = example ]; then
        cargo build --release -p "$PACKAGE" --example "$NAME" --target "$TARGET"
    else
        cargo build --release -p "$PACKAGE" --bin "$NAME" --target "$TARGET"
    fi
done

if [ "$KIND" = example ]; then
    SUB=release/examples
else
    SUB=release
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create \
    "$ROOT/target/x86_64-apple-darwin/$SUB/$NAME" \
    "$ROOT/target/aarch64-apple-darwin/$SUB/$NAME" \
    -output "$APP/Contents/MacOS/$NAME"
lipo -info "$APP/Contents/MacOS/$NAME"

sed -e "s/__VERSION__/$VERSION/g" -e "s/__EXECUTABLE__/$NAME/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

# Icon from the PNGs icongen already renders (assets/generated).
ICONSET="$OUT/NANOCast.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
G="$ROOT/assets/generated"
cp "$G/icon-16.png" "$ICONSET/icon_16x16.png"
cp "$G/icon-32.png" "$ICONSET/icon_16x16@2x.png"
cp "$G/icon-32.png" "$ICONSET/icon_32x32.png"
cp "$G/icon-64.png" "$ICONSET/icon_32x32@2x.png"
cp "$G/icon-128.png" "$ICONSET/icon_128x128.png"
cp "$G/icon-256.png" "$ICONSET/icon_128x128@2x.png"
cp "$G/icon-256.png" "$ICONSET/icon_256x256.png"
cp "$G/icon-512.png" "$ICONSET/icon_256x256@2x.png"
cp "$G/icon-512.png" "$ICONSET/icon_512x512.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/NANOCast.icns" || echo "warning: iconutil failed, app has no icon"
rm -rf "$ICONSET"

# Ad-hoc ("-") or the given identity. No hardened runtime: not notarized anyway.
codesign --force --sign "$SIGN_IDENTITY" --timestamp=none "$APP"
codesign --verify --verbose=2 "$APP"

(cd "$OUT" && rm -f "NANOCast-$VERSION-macos.zip" && ditto -c -k --keepParent NANOCast.app "NANOCast-$VERSION-macos.zip")
echo "done: $APP"
echo "      $OUT/NANOCast-$VERSION-macos.zip"
