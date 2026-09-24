#!/usr/bin/env bash
#
# Shared environment discovery for the diary's Android build.
#
# Sourced by the build scripts; never executed directly. Everything is
# overridable from the environment so the build works with a toolchain in a
# non-standard place.
#
#   RUSTUP_HOME / CARGO_HOME   rust installation (default: ~/.rustup, ~/.cargo)
#   ANDROID_NDK_HOME           NDK root (else discovered, see ndk::find)
#   ANDROID_SDK_ROOT           Android SDK root (for the platform android.jar)
#   ANDROID_MIN_SDK            minimum API level (default 24)

RIDDLE_PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RIDDLE_CORE="$RIDDLE_PROJECT/riddle-core"
RIDDLE_ANDROID="$RIDDLE_PROJECT/android"

ANDROID_MIN_SDK="${ANDROID_MIN_SDK:-24}"
TARGET_TRIPLE="aarch64-linux-android"
RIDDLE_ANDROID_LINKER="$TARGET_TRIPLE$ANDROID_MIN_SDK-clang"

# Put a rustup-managed cargo/rustc ahead of any system install: a distro or
# Homebrew Rust cannot install the Android target's std, so it will fail
# confusingly later if it wins the PATH race.
rust::add_to_path() {
  local home="${CARGO_HOME:-$HOME/.cargo}"
  if [ -x "$home/bin/cargo" ]; then
    PATH="$home/bin:$PATH"
    export PATH
  fi
}

rust::require() {
  rust::add_to_path
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found." >&2
    echo "       Install Rust: https://rustup.rs" >&2
    exit 1
  fi
  if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET_TRIPLE"; then
    echo "error: the Rust target '$TARGET_TRIPLE' is not installed." >&2
    echo "       rustup target add $TARGET_TRIPLE" >&2
    echo "       (A Homebrew/distro Rust cannot do this; use rustup.)" >&2
    exit 1
  fi
}

# Find an Android NDK. Order: explicit env, then the standard SDK layout
# (newest revision wins), then Android Studio's bundled copy.
ndk::find() {
  if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
    echo "$ANDROID_NDK_HOME"
    return 0
  fi
  local roots=()
  [ -n "${ANDROID_SDK_ROOT:-}" ] && roots+=("$ANDROID_SDK_ROOT")
  [ -n "${ANDROID_HOME:-}" ] && roots+=("$ANDROID_HOME")
  roots+=("$HOME/Library/Android/sdk" "$HOME/Android/Sdk")
  local root candidate
  for root in "${roots[@]}"; do
    [ -d "$root/ndk" ] || continue
    # Sort by version, not by name: r9 < r10 < r26 < r30.
    candidate="$(find "$root/ndk" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -1)"
    [ -n "$candidate" ] && { echo "$candidate"; return 0; }
  done
  for candidate in \
      "/Applications/Android Studio.app/Contents/ndk" \
      "$HOME/Library/Android/sdk/ndk-bundle"; do
    [ -d "$candidate" ] && { echo "$candidate"; return 0; }
  done
  return 1
}

ndk::require() {
  if ! NDK_HOME="$(ndk::find)"; then
    echo "error: no Android NDK found." >&2
    echo "       Install one, e.g.:" >&2
    echo "         sdkmanager --install 'ndk;27.0.12077973'" >&2
    echo "       or set ANDROID_NDK_HOME." >&2
    exit 1
  fi
  export NDK_HOME
}

# The NDK ships prebuilt clang wrappers per minimum API level. Point cargo at
# the right one by rewriting the single linker line in the crate's config, so
# the value in git stays a sane default rather than a machine-specific path.
ndk::configure_linker() {
  local host_tag
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  host_tag="darwin-x86_64" ;;  # NDK ships one macOS toolchain
    Darwin-x86_64) host_tag="darwin-x86_64" ;;
    Linux-x86_64)  host_tag="linux-x86_64" ;;
    Linux-aarch64) host_tag="linux-x86_64" ;;
    *) echo "error: unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac

  local bin="$NDK_HOME/toolchains/llvm/prebuilt/$host_tag/bin"
  if [ ! -x "$bin/$RIDDLE_ANDROID_LINKER" ]; then
    echo "error: NDK linker wrapper not found: $bin/$RIDDLE_ANDROID_LINKER" >&2
    echo "       Available:" >&2
    ls "$bin" 2>/dev/null | grep -E "^aarch64-linux-android[0-9]+-clang$" >&2 || true
    exit 1
  fi

  local cfg="$RIDDLE_CORE/.cargo/config.toml"
  python3 - "$cfg" "$RIDDLE_ANDROID_LINKER" <<'PY'
import re, sys, pathlib
path, linker = sys.argv[1], sys.argv[2]
p = pathlib.Path(path)
text = p.read_text()
new = re.sub(r'^linker = ".*"$', f'linker = "{linker}"', text, count=1, flags=re.M)
if new != text:
    p.write_text(new)
PY

  # cc-rs (used by ring, a transitive TLS dependency) looks for the NDK
  # compilers by these names rather than reading cargo's linker setting.
  export CC_aarch64_linux_android="$bin/${RIDDLE_ANDROID_LINKER}"
  export AR_aarch64_linux_android="$bin/llvm-ar"
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$bin/$RIDDLE_ANDROID_LINKER"
}

# The Android platform jar for javac/d8. Prefers the newest installed platform.
sdk::android_jar() {
  local roots=()
  [ -n "${ANDROID_SDK_ROOT:-}" ] && roots+=("$ANDROID_SDK_ROOT")
  [ -n "${ANDROID_HOME:-}" ] && roots+=("$ANDROID_HOME")
  roots+=("$HOME/Library/Android/sdk" "$HOME/Android/Sdk")
  local root jar
  for root in "${roots[@]}"; do
    [ -d "$root/platforms" ] || continue
    jar="$(find "$root/platforms" -maxdepth 2 -name android.jar | sort -V | tail -1)"
    [ -n "$jar" ] && { echo "$jar"; return 0; }
  done
  return 1
}

# The build-tools directory (aapt2, d8, zipalign, apksigner).
sdk::build_tools() {
  local roots=()
  [ -n "${ANDROID_SDK_ROOT:-}" ] && roots+=("$ANDROID_SDK_ROOT")
  [ -n "${ANDROID_HOME:-}" ] && roots+=("$ANDROID_HOME")
  roots+=("$HOME/Library/Android/sdk" "$HOME/Android/Sdk")
  local root candidate
  for root in "${roots[@]}"; do
    [ -d "$root/build-tools" ] || continue
    candidate="$(find "$root/build-tools" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -1)"
    [ -n "$candidate" ] && { echo "$candidate"; return 0; }
  done
  return 1
}

java::require() {
  if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/javac" ]; then
    PATH="$JAVA_HOME/bin:$PATH"
    export PATH
    return 0
  fi
  # Android Studio bundles a JDK; use it rather than demanding a system one.
  local bundled="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
  if [ -x "$bundled/bin/javac" ]; then
    JAVA_HOME="$bundled"
    PATH="$JAVA_HOME/bin:$PATH"
    export JAVA_HOME PATH
    return 0
  fi
  if command -v javac >/dev/null 2>&1; then
    return 0
  fi
  echo "error: no JDK found. Set JAVA_HOME, or install Android Studio." >&2
  exit 1
}
