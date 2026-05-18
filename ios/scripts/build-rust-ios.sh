#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
IOS_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(CDPATH= cd -- "$IOS_DIR/.." && pwd)
OUT_DIR="$IOS_DIR/Build/Rust/${PLATFORM_NAME:-iphonesimulator}"
TARGET_DIR="$REPO_ROOT/target/ios"
LIB_NAME=libm5mic_ios_core.a

mkdir -p "$OUT_DIR"

targets=""
for arch in ${ARCHS:-arm64}; do
    case "${PLATFORM_NAME:-iphonesimulator}:$arch" in
        iphoneos:arm64)
            rust_target=aarch64-apple-ios
            ;;
        iphonesimulator:arm64)
            rust_target=aarch64-apple-ios-sim
            ;;
        iphonesimulator:x86_64)
            rust_target=x86_64-apple-ios
            ;;
        *)
            echo "unsupported iOS Rust target for PLATFORM_NAME=${PLATFORM_NAME:-unknown} ARCH=$arch" >&2
            exit 1
            ;;
    esac

    rustup target add "$rust_target"
    CARGO_TARGET_DIR="$TARGET_DIR" cargo build \
        --manifest-path "$REPO_ROOT/Cargo.toml" \
        -p m5mic-ios-core \
        --lib \
        --release \
        --target "$rust_target"
    targets="$targets $TARGET_DIR/$rust_target/release/$LIB_NAME"
done

set -- $targets
if [ "$#" -eq 1 ]; then
    cp "$1" "$OUT_DIR/$LIB_NAME"
else
    lipo -create "$@" -output "$OUT_DIR/$LIB_NAME"
fi
