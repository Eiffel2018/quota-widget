#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin ]] || { echo 'This script requires an authorized Mac with an Apple SDK.' >&2; exit 1; }
[[ "$(uname -m)" == arm64 ]] || { echo 'This package targets Apple Silicon arm64.' >&2; exit 1; }
xcrun --sdk macosx --show-sdk-path >/dev/null
command -v cargo >/dev/null
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo test --locked
cargo build --release --locked --target aarch64-apple-darwin
app='dist/Quota Widget.app'
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/aarch64-apple-darwin/release/quota-widget "$app/Contents/MacOS/quota-widget"
cp scripts/Info.plist "$app/Contents/Info.plist"
file "$app/Contents/MacOS/quota-widget" | grep -q arm64
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --sequesterRsrc --keepParent "$app" dist/quota-widget-0.3.3-macos-arm64.zip
printf '\nBuilt locally: %s\n' "$app"
