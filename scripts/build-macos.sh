#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --locked --release
bundle="dist/Drifter.app"
mkdir -p "$bundle/Contents/MacOS"
cp target/release/drifter "$bundle/Contents/MacOS/drifter"
cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Drifter</string>
<key>CFBundleDisplayName</key><string>Drifter</string>
<key>CFBundleIdentifier</key><string>dev.drifter.downloader</string>
<key>CFBundleExecutable</key><string>drifter</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>12.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict></plist>
PLIST
codesign --force --deep --sign - "$bundle"
printf 'Built %s/%s\n' "$(pwd)" "$bundle"
