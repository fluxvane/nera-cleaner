#!/bin/zsh
# macOS: build "Nera Cleaner.app" from the Rust binary.
# (Windows: scripts/package.ps1.)
#   scripts/bundle.sh                  # debug build   -> target/debug/Nera Cleaner.app
#   scripts/bundle.sh --release        # release build -> target/release/Nera Cleaner.app
#   scripts/bundle.sh --release --zip  # also dist/NeraCleaner-<version>-macos-<arch>.zip
set -euo pipefail

ROOT=${0:A:h:h}
cd "$ROOT"

PROFILE=debug
CARGO_FLAGS=()
ZIP=0
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE=release; CARGO_FLAGS=(--release) ;;
    --zip) ZIP=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

cargo build -p nera-app "${CARGO_FLAGS[@]}"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP="target/$PROFILE/Nera Cleaner.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/nera-cleaner" "$APP/Contents/MacOS/nera-cleaner"

# App icon from logos/logo-icon.png (cached between runs).
ICNS="target/AppIcon.icns"
if [[ ! -f "$ICNS" || logos/logo-icon.png -nt "$ICNS" ]]; then
  ICONSET=$(mktemp -d)/AppIcon.iconset
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z $size $size logos/logo-icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) logos/logo-icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$ICNS"
fi
cp "$ICNS" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Nera Cleaner</string>
  <key>CFBundleDisplayName</key><string>Nera Cleaner</string>
  <key>CFBundleIdentifier</key><string>com.nextera.neracleaner</string>
  <key>CFBundleExecutable</key><string>nera-cleaner</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>Nera Cleaner reads your login items from System Events.</string>
</dict>
</plist>
PLIST

# Ad-hoc sign so macOS keeps permissions (Full Disk Access) stable per build.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true
echo "$APP"

if (( ZIP )); then
  mkdir -p dist
  ARCHIVE="dist/NeraCleaner-$VERSION-macos-$(uname -m).zip"
  rm -f "$ARCHIVE"
  # ditto keeps the bundle's signature and extended attributes intact.
  ditto -c -k --keepParent "$APP" "$ARCHIVE"
  echo "$ARCHIVE"
fi
