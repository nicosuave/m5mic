#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle="$repo_root/target/m5mic.driver"
binary="$repo_root/target/release/libm5mic_coreaudio_driver.dylib"

cargo build -p m5mic-coreaudio-driver --release --target-dir "$repo_root/target"

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$repo_root/coreaudio-driver/Info.plist" "$bundle/Contents/Info.plist"
cp "$binary" "$bundle/Contents/MacOS/m5mic-coreaudio-driver"

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

codesign --force --sign "$sign_identity" --timestamp=none "$bundle" >/dev/null

echo "$bundle"
