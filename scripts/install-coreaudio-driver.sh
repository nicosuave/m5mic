#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run_as_root() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

user_install=0
driver_dir="/Library/Audio/Plug-Ins/HAL"
if [[ "${1:-}" == "--user" ]]; then
  user_install=1
  driver_dir="$HOME/Library/Audio/Plug-Ins/HAL"
fi
driver_path="$driver_dir/m5mic.driver"

"$repo_root/scripts/build-coreaudio-driver.sh" >/dev/null

if [[ "$user_install" -eq 1 ]]; then
  mkdir -p "$driver_dir"
  rm -rf "$driver_path"
  cp -R "$repo_root/target/m5mic.driver" "$driver_path"
elif [[ -w "$driver_dir" ]]; then
  mkdir -p "$driver_dir"
  rm -rf "$driver_path"
  cp -R "$repo_root/target/m5mic.driver" "$driver_path"
else
  run_as_root mkdir -p "$driver_dir"
  run_as_root rm -rf "$driver_path"
  run_as_root cp -R "$repo_root/target/m5mic.driver" "$driver_path"
  run_as_root chown -R root:wheel "$driver_path"
fi

run_as_root killall coreaudiod
echo "$driver_path"
