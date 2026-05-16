# M5StickS3 Firmware

Put build-time fallback Wi-Fi credentials in `.env.local` at the repo root; that file is ignored by git:

```sh
WIFI_SSID='your ssid'
WIFI_PASS='your pass'
```

Build with the ESP Rust toolchain:

```sh
cd firmware
. ~/export-esp.sh
set -a
. ../.env.local
set +a
cargo +esp build --release
```

Flash:

```sh
espflash flash --port <serial-port> target/xtensa-esp32s3-espidf/release/m5mic-firmware
```

The firmware discovers the receiver in this order:

1. `M5MIC_SERVER_URL`, if set at build time.
2. mDNS query for `_m5mic._tcp.local`.
3. UDP broadcast on `255.255.255.255:47777`.

Audio format is `pcm_s16le`, mono, 16 kHz, sent as 40 ms binary WebSocket frames.

Press BtnA to start recording. Press BtnA again to stop and close the current WAV on the receiver.

Direct receiver override:

```sh
set -a
. ../.env.local
set +a
M5MIC_SERVER_URL='ws://192.168.1.10:47776/audio' cargo +esp build --release
espflash flash --port <serial-port> target/xtensa-esp32s3-espidf/release/m5mic-firmware
```
