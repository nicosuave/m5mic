#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="$repo_root/target/m5mic.app"
binary="$repo_root/target/release/m5mic-statusbar"

cargo build -p m5mic-statusbar --release --target-dir "$repo_root/target"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$repo_root/statusbar/Info.plist" "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/m5mic-statusbar"
codesign --force --sign - "$app" >/dev/null

echo "$app"
