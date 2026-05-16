# m5mic

Rust firmware and a Rust desktop receiver for using an M5StickS3 as a live microphone.

The firmware has two transports and the Mac side has two receive modes:

1. USB Audio Class microphone. On macOS, it lists as `m5mic` from manufacturer `M5Stack`, 1 channel at 16 kHz.
2. Raw `pcm_s16le` over WebSocket to the Rust receiver.
3. Wireless virtual microphone on macOS. The Rust status-bar app hosts the receiver and feeds a Rust CoreAudio driver named `m5mic`.

Wireless discovery is handled two ways:

1. The receiver advertises `_m5mic._tcp.local` via mDNS.
2. The firmware falls back to UDP broadcast on port `47777`.

## Photos

<p>
  <img src="docs/images/m5mic-0944.jpg" alt="M5StickS3 ready screen" width="320">
  <img src="docs/images/m5mic-0945.jpg" alt="M5StickS3 recording screen" width="320">
</p>

## Receiver

Foreground development:

```sh
mkdir -p captures
cargo run -p m5mic-receiver -- --output-dir captures
```

It listens on `0.0.0.0:47776`, accepts WebSocket connections at `/audio`, and writes each stream to a WAV file.

Virtual mic mode without WAV files:

```sh
cargo run -p m5mic-receiver -- --virtual-mic --no-recordings
```

Detached tmux session:

```sh
tmux new-session -d -s m5mic-receiver -c "$PWD" 'mkdir -p captures && cargo run -p m5mic-receiver -- --output-dir captures'
```

Useful commands:

```sh
tmux attach -t m5mic-receiver
tmux kill-session -t m5mic-receiver
lsof -nP -iTCP:47776
```

In wireless mode, tap BtnA once to start a locked recording and tap BtnA again to stop. Hold BtnA for push-to-talk; release it to stop. Each start/stop cycle creates a separate WAV file on the receiver.

Short-tap BtnB to toggle between wireless mode and USB mic mode. Hold BtnB during boot, or hold BtnB for about two seconds while idle, to start the captive setup portal.

Wi-Fi setup is optional. Join the `M5Mic-XXXX` access point and open `http://192.168.71.1` if the captive page does not appear automatically. Saved Wi-Fi credentials are stored in NVS and take priority over the build-time `WIFI_SSID` / `WIFI_PASS` fallback.

## Wireless Virtual Mic

Install the CoreAudio driver. macOS loads HAL plug-ins from the system audio plug-in directory, so this uses `sudo`:

```sh
scripts/install-coreaudio-driver.sh
```

Run the status-bar receiver:

```sh
cargo run -p m5mic-statusbar
```

Or build a menu-bar `.app` bundle:

```sh
scripts/build-statusbar-app.sh
open target/m5mic.app
```

The status-bar app owns mDNS, UDP discovery, WebSocket receive, and reconnect behavior. It feeds a shared 48 kHz mono float ring buffer. The CoreAudio driver only reads that ring and returns silence when the app is not receiving audio.

After install, macOS apps can select `m5mic` as an input device. Use the status-bar menu to open Sound Settings or quit the receiver.

Uninstall the driver:

```sh
scripts/uninstall-coreaudio-driver.sh
```

## UI Preview

```sh
tmux new-session -d -s m5mic-preview -c "$PWD" 'uv run python -m http.server 4177 --bind 127.0.0.1 --directory preview'
```

Open `http://127.0.0.1:4177/`.

## Firmware

Install the ESP Rust toolchain if needed:

```sh
espup install
. ~/export-esp.sh
```

Put build-time fallback Wi-Fi credentials in `.env.local` at the repo root; that file is ignored by git:

```sh
WIFI_SSID='your ssid'
WIFI_PASS='your pass'
```

Build for StickS3:

```sh
cd firmware
. ~/export-esp.sh
set -a
. ../.env.local
set +a
cargo +esp build --release
```

Flash the StickS3:

```sh
cd firmware
espflash flash --port <serial-port> target/xtensa-esp32s3-espidf/release/m5mic-firmware
```

After flashing USB Audio firmware, the app owns the native USB device stack while running. If serial monitoring over the same USB cable is unavailable, flash without `--monitor` and use the screen state for basic feedback.

Optional direct receiver override:

```sh
cd firmware
set -a
. ../.env.local
set +a
M5MIC_SERVER_URL='ws://192.168.1.10:47776/audio' cargo +esp build --release
espflash flash --port <serial-port> target/xtensa-esp32s3-espidf/release/m5mic-firmware
```

## Hardware Notes

The StickS3 audio path uses an ES8311 codec at I2C `0x18`, not a direct PDM mic.

Relevant pins from the M5Stack StickS3 docs and M5Unified:

| Signal | GPIO |
|---|---:|
| ES8311 MCLK | 18 |
| ES8311 DOUT to ESP32-S3 DIN | 16 |
| ES8311 BCLK | 17 |
| ES8311 LRCK | 15 |
| ES8311 I2C SCL | 48 |
| ES8311 I2C SDA | 47 |
| BtnA / KEY1 | 11 |
| BtnB / KEY2 | 12 |
