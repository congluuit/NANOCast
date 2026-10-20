#!/usr/bin/env bash
# Builds the macOS release of NANOCast on a Mac (Intel or Apple Silicon):
#   1. cargo build --release -p nanocast for x86_64-apple-darwin and aarch64-apple-darwin
#   2. lipo -> one universal binary
#   3. NANOCast.app (Info.plist from installer/macos/Info.plist, icon from assets/generated)
#   4. codesign ad-hoc (or $SIGN_IDENTITY)
#   5. NANOCast-<version>-macos-universal.dmg (app + link to /Applications)
#
# Usage (from anywhere inside the repo):
#   bash installer/macos/build-mac.sh              # universal .dmg
#   bash installer/macos/build-mac.sh --per-arch   # also -arm64.dmg and -x86_64.dmg
#   bash installer/macos/build-mac.sh --no-build   # repackage the binaries already in target/
#
# Output: installer/out/NANOCast-<version>-macos-*.dmg (same folder as the Windows installer).
# Needs: Xcode Command Line Tools (xcode-select --install) and rustup. Safe to run again:
# every output is deleted and recreated.
#
# Env:
#   SIGN_IDENTITY  codesign identity, default "-" (ad-hoc). A self-signed "Code Signing"
#                  certificate (e.g. "NANOCast Dev") keeps the Screen Recording permission
#                  across rebuilds, see docs/macos.md.
set -euo pipefail

PER_ARCH=0
BUILD=1
for arg in "$@"; do
    case "$arg" in
        --per-arch) PER_ARCH=1 ;;
        --no-build) BUILD=0 ;;
        -h|--help)
            sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown option: $arg (try --help)" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
cd "$ROOT"

[ "$(uname -s)" = Darwin ] || die "build-mac.sh must run on macOS (this is $(uname -s))"

step "Checking tools"
for tool in cargo rustup lipo iconutil codesign hdiutil plutil ditto shasum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found (Xcode Command Line Tools: xcode-select --install; Rust: https://rustup.rs)"
done
echo "macOS $(sw_vers -productVersion) on $(uname -m), $(cargo --version)"

# [workspace.package] version is the first `version = "..."` line of the root Cargo.toml.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || die "could not read version from Cargo.toml"
echo "NANOCast $VERSION"

SIGN_IDENTITY=${SIGN_IDENTITY:--}
TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
WORK="$ROOT/target/macos-pkg"
OUT="$ROOT/installer/out"
G="$ROOT/assets/generated"

# ScreenCaptureKit is 12.3+; matches LSMinimumSystemVersion in Info.plist.
export MACOSX_DEPLOYMENT_TARGET=12.3

if [ "$BUILD" = 1 ]; then
    step "Rust targets"
    # Idempotent: rustup only downloads what is missing.
    rustup target add $TARGETS

    for target in $TARGETS; do
        step "cargo build --release -p nanocast --target $target"
        cargo build --release -p nanocast --bin nanocast --target "$target"
    done
fi

for target in $TARGETS; do
    [ -f "target/$target/release/nanocast" ] || die "missing target/$target/release/nanocast (run without --no-build)"
done

rm -rf "$WORK"
mkdir -p "$WORK" "$OUT"

step "Universal binary (lipo)"
lipo -create \
    "target/aarch64-apple-darwin/release/nanocast" \
    "target/x86_64-apple-darwin/release/nanocast" \
    -output "$WORK/nanocast-universal"
lipo -info "$WORK/nanocast-universal"

step "Icon (iconutil)"
ICONSET="$WORK/NANOCast.iconset"
mkdir -p "$ICONSET"
# name in the .iconset <- pixel size rendered by tools/icongen (cargo run -p icongen)
for pair in \
    icon_16x16:16 icon_16x16@2x:32 \
    icon_32x32:32 icon_32x32@2x:64 \
    icon_128x128:128 icon_128x128@2x:256 \
    icon_256x256:256 icon_256x256@2x:512 \
    icon_512x512:512 icon_512x512@2x:1024; do
    name=${pair%%:*}
    px=${pair##*:}
    [ -f "$G/icon-$px.png" ] || die "missing $G/icon-$px.png (run: cargo run -p icongen)"
    cp "$G/icon-$px.png" "$ICONSET/$name.png"
done
iconutil -c icns "$ICONSET" -o "$WORK/NANOCast.icns"
echo "$WORK/NANOCast.icns"

# make_app <binary> <app dir>
make_app() {
    local bin=$1 app=$2
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin" "$app/Contents/MacOS/nanocast"
    chmod 755 "$app/Contents/MacOS/nanocast"
    cp "$WORK/NANOCast.icns" "$app/Contents/Resources/NANOCast.icns"
    sed "s/__VERSION__/$VERSION/g" "$HERE/Info.plist" > "$app/Contents/Info.plist"
    plutil -lint "$app/Contents/Info.plist"
    printf 'APPL????' > "$app/Contents/PkgInfo"
    # Finder metadata / resource forks make codesign fail ("detritus not allowed").
    xattr -cr "$app"
    # No hardened runtime / entitlements: the app is not notarized anyway.
    codesign --force --deep --sign "$SIGN_IDENTITY" --timestamp=none "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
}

# make_dmg <app dir> <dmg path>
make_dmg() {
    local app=$1 dmg=$2 stage
    stage="$WORK/dmg-$(basename "$dmg" .dmg)"
    rm -rf "$stage" "$dmg"
    mkdir -p "$stage"
    ditto "$app" "$stage/NANOCast.app"
    ln -s /Applications "$stage/Applications"
    # Plain Finder window (app + Applications link). A custom background needs Finder
    # AppleScript in a logged-in GUI session, which CI runners do not have.
    # hdiutil sometimes fails with "Resource busy" right after a previous image; retry.
    local try
    for try in 1 2 3; do
        if hdiutil create -volname "NANOCast $VERSION" -srcfolder "$stage" \
            -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$dmg"; then
            break
        fi
        [ "$try" = 3 ] && die "hdiutil create failed for $dmg"
        echo "hdiutil failed, retrying in 5 s..."
        sleep 5
    done
    hdiutil verify "$dmg"
    rm -rf "$stage"
}

DMGS=()

step "NANOCast.app (universal)"
make_app "$WORK/nanocast-universal" "$WORK/universal/NANOCast.app"
step "DMG (universal)"
DMG="$OUT/NANOCast-$VERSION-macos-universal.dmg"
make_dmg "$WORK/universal/NANOCast.app" "$DMG"
DMGS+=("$DMG")

if [ "$PER_ARCH" = 1 ]; then
    for arch in arm64 x86_64; do
        if [ "$arch" = arm64 ]; then target=aarch64-apple-darwin; else target=x86_64-apple-darwin; fi
        step "NANOCast.app ($arch)"
        make_app "target/$target/release/nanocast" "$WORK/$arch/NANOCast.app"
        step "DMG ($arch)"
        DMG="$OUT/NANOCast-$VERSION-macos-$arch.dmg"
        make_dmg "$WORK/$arch/NANOCast.app" "$DMG"
        DMGS+=("$DMG")
    done
fi

step "Done"
echo "App (universal, for testing without the DMG): $WORK/universal/NANOCast.app"
for dmg in "${DMGS[@]}"; do
    printf '%s  %s\n' "$(du -h "$dmg" | cut -f1)" "$dmg"
done
(cd "$OUT" && for dmg in "${DMGS[@]}"; do shasum -a 256 "$(basename "$dmg")"; done)
