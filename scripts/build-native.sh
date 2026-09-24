#!/usr/bin/env bash
#
# Cross-compile the diary's Rust engine into the Android module's jniLibs.
#
# Requires the Rust aarch64-linux-android target and an Android NDK. Both are
# discovered rather than hard-coded: see android/README.md for how to install
# them, and environment.sh for the search order.
#
# Usage: scripts/build-native.sh [--debug]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../env.sh
. "$HERE/../env.sh"

BUILD_KIND="release"
CARGO_FLAGS=()
if [ "${1:-}" = "--debug" ]; then
  BUILD_KIND="debug"
  CARGO_FLAGS+=()
else
  CARGO_FLAGS+=(--release)
fi

rust::require
ndk::require
ndk::configure_linker

echo "==> rust:    $(rustc --version) ($(rustup target list --installed | tr '\n' ' '))"
echo "==> ndk:     $NDK_HOME"
echo "==> linker:  $RIDDLE_ANDROID_LINKER"
echo "==> building riddle ($BUILD_KIND) for aarch64-linux-android"

cd "$RIDDLE_CORE"
cargo build --target aarch64-linux-android "${CARGO_FLAGS[@]}"

# Ask cargo where it actually put the artifact rather than assuming
# `target/`: CARGO_TARGET_DIR and a workspace root both change the answer.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SO="$TARGET_DIR/aarch64-linux-android/$BUILD_KIND/libriddle.so"
if [ ! -f "$SO" ]; then
  echo "error: expected library not found: $SO" >&2
  exit 1
fi

ABI="arm64-v8a"
DEST="$RIDDLE_ANDROID/app/jniLibs/$ABI"
mkdir -p "$DEST"
cp "$SO" "$DEST/libriddle.so"

# The version installed on device is stripped by the release profile, which
# discards the symbol table a native backtrace would need. Keep an unstripped
# copy beside it for symbolising a crash — this is the file to run
# `llvm-symbolizer` against.
cp "$SO" "$TARGET_DIR/aarch64-linux-android/$BUILD_KIND/libriddle-unstripped.so"

echo "==> wrote $DEST/libriddle.so ($(du -h "$DEST/libriddle.so" | cut -f1))"
