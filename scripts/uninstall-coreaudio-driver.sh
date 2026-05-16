#!/usr/bin/env bash
set -euo pipefail

run_as_root() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

user_install=0
driver_path="/Library/Audio/Plug-Ins/HAL/m5mic.driver"
if [[ "${1:-}" == "--user" ]]; then
  user_install=1
  driver_path="$HOME/Library/Audio/Plug-Ins/HAL/m5mic.driver"
fi

if [[ "$user_install" -eq 1 ]]; then
  rm -rf "$driver_path"
elif [[ -w "$(dirname "$driver_path")" ]]; then
  rm -rf "$driver_path"
else
  run_as_root rm -rf "$driver_path"
fi
run_as_root killall coreaudiod
echo "removed $driver_path"
