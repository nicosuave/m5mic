#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
firmware_dir="$repo_root/firmware"
elf="$firmware_dir/target/xtensa-esp32s3-espidf/release/m5mic-firmware"
out="${1:-$repo_root/target/m5mic-sticks3-m5launcher.bin}"
max_launcher_app_size=$((0x4D0000))

if [[ -f "$HOME/export-esp.sh" ]]; then
  # shellcheck source=/dev/null
  . "$HOME/export-esp.sh"
fi

if [[ "${M5MIC_INCLUDE_LOCAL_WIFI:-0}" == "1" && -f "$repo_root/.env.local" ]]; then
  set -a
  # shellcheck source=/dev/null
  . "$repo_root/.env.local"
  set +a
else
  unset WIFI_SSID WIFI_PASS M5MIC_SERVER_URL
fi

(
  cd "$firmware_dir"
  cargo +esp build --release
)

mkdir -p "$(dirname "$out")"
espflash save-image \
  --chip esp32s3 \
  --flash-size 8mb \
  --flash-mode qio \
  --flash-freq 80mhz \
  "$elf" \
  "$out"

first_byte="$(od -An -tx1 -N1 "$out")"
first_byte="${first_byte//[[:space:]]/}"
if [[ "$first_byte" != "e9" ]]; then
  echo "Expected ESP app image magic byte e9, found $first_byte" >&2
  exit 1
fi

size="$(wc -c < "$out")"
size="${size//[[:space:]]/}"
if (( size > max_launcher_app_size )); then
  printf 'Firmware is too large for M5Launcher StickS3 app partition: %s > %s bytes\n' \
    "$size" "$max_launcher_app_size" >&2
  exit 1
fi

read -r sha256 _ < <(shasum -a 256 "$out")
printf 'M5Launcher firmware: %s\n' "$out"
printf 'Size: %s bytes / %s bytes\n' "$size" "$max_launcher_app_size"
printf 'SHA-256: %s\n' "$sha256"
