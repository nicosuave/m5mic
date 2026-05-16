#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release_env="$repo_root/.env.release.local"

if [[ -f "$release_env" ]]; then
  set -a
  # shellcheck source=/dev/null
  . "$release_env"
  set +a
fi

version="${M5MIC_RELEASE_VERSION:-$(awk -F\" '/^version =/ { print $2; exit }' "$repo_root/statusbar/Cargo.toml")}"
arch="$(uname -m)"
name="m5mic-${version}-macos-${arch}"
stage="$repo_root/target/release/$name"
archive="$repo_root/target/release/$name.zip"

find_developer_id_application() {
  if [[ -n "${M5MIC_CODESIGN_IDENTITY:-}" ]]; then
    printf '%s\n' "$M5MIC_CODESIGN_IDENTITY"
    return
  fi

  while IFS= read -r identity_line; do
    if [[ "$identity_line" == *"Developer ID Application:"* ]]; then
      identity="${identity_line#*\"}"
      printf '%s\n' "${identity%%\"*}"
      return
    fi
  done < <(security find-identity -v -p codesigning 2>/dev/null || true)
}

sign_identity="$(find_developer_id_application)"
if [[ -z "$sign_identity" ]]; then
  echo "No Developer ID Application signing identity found." >&2
  echo "Install one or set M5MIC_CODESIGN_IDENTITY in .env.release.local." >&2
  exit 1
fi
export M5MIC_CODESIGN_IDENTITY="$sign_identity"

"$repo_root/scripts/build-coreaudio-driver.sh" >/dev/null
"$repo_root/scripts/build-statusbar-app.sh" >/dev/null

rm -rf "$stage" "$archive"
mkdir -p "$stage"
cp -R "$repo_root/target/m5mic.app" "$stage/m5mic.app"
cp -R "$repo_root/target/m5mic.driver" "$stage/m5mic.driver"

cat > "$stage/install-driver.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

release_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
driver_dir="/Library/Audio/Plug-Ins/HAL"
driver_path="$driver_dir/m5mic.driver"

run_as_root() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

run_as_root mkdir -p "$driver_dir"
run_as_root rm -rf "$driver_path"
run_as_root cp -R "$release_dir/m5mic.driver" "$driver_path"
run_as_root chown -R root:wheel "$driver_path"
run_as_root killall coreaudiod

echo "Installed $driver_path"
EOF
chmod +x "$stage/install-driver.sh"

cat > "$stage/uninstall-driver.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

driver_path="/Library/Audio/Plug-Ins/HAL/m5mic.driver"

run_as_root() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

run_as_root rm -rf "$driver_path"
run_as_root killall coreaudiod

echo "Removed $driver_path"
EOF
chmod +x "$stage/uninstall-driver.sh"

cat > "$stage/README.txt" <<EOF
m5mic ${version}

Install:
1. Drag m5mic.app to /Applications.
2. Open m5mic.app from /Applications.
3. Click Install when m5mic asks to install the CoreAudio virtual microphone driver.
4. Select "m5mic" as your microphone, or use the menu-bar app to switch wireless/USB mode.

Manual driver repair:
Run ./install-driver.sh if the in-app installer is skipped or interrupted.

Uninstall:
1. Quit m5mic from the menu bar.
2. Run ./uninstall-driver.sh.
3. Delete /Applications/m5mic.app.
EOF

codesign --verify --deep --strict --verbose=2 "$stage/m5mic.app"
codesign --verify --deep --strict --verbose=2 "$stage/m5mic.driver"

ditto -c -k --sequesterRsrc --keepParent "$stage" "$archive"

if [[ -n "${M5MIC_NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$archive" \
    --keychain-profile "$M5MIC_NOTARY_PROFILE" \
    --wait

  xcrun stapler staple "$stage/m5mic.app"
  xcrun stapler staple "$stage/m5mic.driver" || true
  xcrun stapler validate "$stage/m5mic.app"
  xcrun stapler validate "$stage/m5mic.driver" || true

  rm -f "$archive"
  ditto -c -k --sequesterRsrc --keepParent "$stage" "$archive"
else
  echo "M5MIC_NOTARY_PROFILE is not set; created a signed but unnotarized archive." >&2
fi

echo "$archive"
