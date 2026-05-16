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

sign_identity="${M5MIC_CODESIGN_IDENTITY:-}"
if [[ -z "$sign_identity" ]]; then
  sign_identity="-"
  while IFS= read -r identity_line; do
    if [[ "$identity_line" == *"Developer ID Application:"* ]]; then
      sign_identity="${identity_line#*\"}"
      sign_identity="${sign_identity%%\"*}"
      break
    fi
  done < <(security find-identity -v -p codesigning 2>/dev/null || true)
fi

sign_args=(--force --sign "$sign_identity")
if [[ "$sign_identity" == "-" ]]; then
  sign_args+=(--timestamp=none)
else
  sign_args+=(--timestamp --options runtime)
fi

codesign "${sign_args[@]}" "$app" >/dev/null

echo "$app"
