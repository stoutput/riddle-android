#!/usr/bin/env bash
#
# Build the diary's APK.
#
# The app has no Gradle project and no AndroidX: one activity, two Java files'
# worth of UI, and a Rust engine. So this script drives the SDK build tools
# directly — aapt2, javac, d8, zipalign, apksigner — which keeps the build
# reproducible and inspectable, and means the only toolchain to install is an
# NDK and a JDK.
#
# Usage: scripts/build-apk.sh [--skip-native]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../env.sh
. "$HERE/../env.sh"

# Usage: scripts/build-apk.sh [--skip-native] [--debug]
#
#   --debug  also mark the APK debuggable, so `adb shell run-as` can read the
#            app's private files (the engine's riddle.log). This is for
#            diagnosis on a device whose logcat hides app tags; the default
#            build is a normal, non-debuggable release APK.

SKIP_NATIVE=0
DEBUGGABLE=0
for arg in "$@"; do
  case "$arg" in
    --skip-native) SKIP_NATIVE=1 ;;
    --debug) DEBUGGABLE=1 ;;
    *) echo "error: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

java::require

# Version comes from the crate, which is the single source of truth: CI reads
# the same field to decide whether a release already exists. The Android version
# *code* must only ever increase, so it is not derived from the semver string —
# pass RIDDLE_VERSION_CODE (CI uses the run number) or fall back to 1.
VERSION="$(grep -m1 '^version' "$RIDDLE_CORE/Cargo.toml" | cut -d'"' -f2)"
if [ -z "$VERSION" ]; then
  echo "error: could not read version from $RIDDLE_CORE/Cargo.toml" >&2
  exit 1
fi
VERSION_CODE="${RIDDLE_VERSION_CODE:-1}"
case "$VERSION_CODE" in
  ''|*[!0-9]*) echo "error: RIDDLE_VERSION_CODE must be an integer" >&2; exit 1 ;;
esac

ANDROID_JAR="$(sdk::android_jar)" || {
  echo "error: no Android platform (android.jar) found." >&2
  echo "       Install one: sdkmanager --install 'platforms;android-34'" >&2
  exit 1
}
BUILD_TOOLS="$(sdk::build_tools)" || {
  echo "error: no Android build-tools found." >&2
  echo "       Install them: sdkmanager --install 'build-tools;34.0.0'" >&2
  exit 1
}

AAPT2="$BUILD_TOOLS/aapt2"
D8="$BUILD_TOOLS/d8"
ZIPALIGN="$BUILD_TOOLS/zipalign"
APKSIGNER="$BUILD_TOOLS/apksigner"
for tool in "$AAPT2" "$D8" "$ZIPALIGN" "$APKSIGNER"; do
  [ -x "$tool" ] || { echo "error: missing build tool: $tool" >&2; exit 1; }
done

echo "==> android.jar:  $ANDROID_JAR"
echo "==> build-tools:  $BUILD_TOOLS"

if [ "$SKIP_NATIVE" -eq 0 ]; then
  "$HERE/build-native.sh"
fi

APP="$RIDDLE_ANDROID/app"
SO="$APP/jniLibs/arm64-v8a/libriddle.so"
if [ ! -f "$SO" ]; then
  echo "error: $SO is missing; run without --skip-native first." >&2
  exit 1
fi

OUT="$RIDDLE_PROJECT/build"
rm -rf "$OUT"
mkdir -p "$OUT/res-compiled" "$OUT/gen" "$OUT/classes" "$OUT/dex" "$OUT/apk"

# ---------------------------------------------------------------------------
# 1. Compile resources, then link them with the manifest into a base APK.
# ---------------------------------------------------------------------------
MANIFEST="$APP/AndroidManifest.xml"
if [ "$DEBUGGABLE" -eq 1 ]; then
  MANIFEST="$OUT/AndroidManifest.debug.xml"
  "$HERE/make-debuggable-manifest.py" "$APP/AndroidManifest.xml" "$MANIFEST"
  echo "==> debuggable variant (so run-as can read the app's log file)"
fi

echo "==> aapt2 compile"
"$AAPT2" compile --dir "$APP/res" -o "$OUT/res-compiled/res.zip"

echo "==> aapt2 link"
"$AAPT2" link \
  -o "$OUT/apk/base.apk" \
  -I "$ANDROID_JAR" \
  --manifest "$MANIFEST" \
  --java "$OUT/gen" \
  --min-sdk-version "$ANDROID_MIN_SDK" \
  --target-sdk-version 34 \
  --version-code "$VERSION_CODE" \
  --version-name "$VERSION" \
  --no-version-vectors \
  "$OUT/res-compiled/res.zip"

# ---------------------------------------------------------------------------
# 2. Compile the Java UI against the generated R class and the platform jar.
# ---------------------------------------------------------------------------
echo "==> javac"
find "$APP/src" "$OUT/gen" -name '*.java' > "$OUT/sources.txt"
javac \
  -source 8 -target 8 \
  -encoding UTF-8 \
  -bootclasspath "$ANDROID_JAR" \
  -classpath "$ANDROID_JAR" \
  -d "$OUT/classes" \
  -Xlint:-options \
  @"$OUT/sources.txt"

# ---------------------------------------------------------------------------
# 3. Dex it.
# ---------------------------------------------------------------------------
echo "==> d8"
find "$OUT/classes" -name '*.class' > "$OUT/classes.txt"
"$D8" \
  --lib "$ANDROID_JAR" \
  --min-api "$ANDROID_MIN_SDK" \
  --output "$OUT/dex" \
  @"$OUT/classes.txt"

# ---------------------------------------------------------------------------
# 4. Assemble: base APK + classes.dex + native library.
# ---------------------------------------------------------------------------
echo "==> packaging"
mkdir -p "$OUT/apk/lib/arm64-v8a"
cp "$SO" "$OUT/apk/lib/arm64-v8a/libriddle.so"

# Zip is required for entry order and method: the native library must be STORED
# (uncompressed) so Android can mmap it straight out of the APK instead of
# extracting a copy on install, and classes.dex must belong at the archive root.
# Zipping from inside the staging directory is what makes those the stored
# paths; `zip` records the names it is given, not their absolute locations.
cp "$OUT/dex/classes.dex" "$OUT/apk/classes.dex"
rm -f "$OUT/apk/unaligned.apk"
cp "$OUT/apk/base.apk" "$OUT/apk/unaligned.apk"
(
  cd "$OUT/apk"
  zip -q -0 unaligned.apk lib/arm64-v8a/libriddle.so
  zip -q unaligned.apk classes.dex
)

echo "==> zipalign"
"$ZIPALIGN" -f -p 4 "$OUT/apk/unaligned.apk" "$OUT/apk/aligned.apk"

# ---------------------------------------------------------------------------
# 5. Sign. A throwaway key is generated on first build: this is a side-loaded
#    app, and a stable key is needed so that reinstalling updates in place
#    rather than failing with a signature mismatch.
# ---------------------------------------------------------------------------
# The key lives beside the project, NOT in build/: build/ is wiped on every
# run, and regenerating the key would make each build's APK un-installable over
# the previous one (signature mismatch).
if [ -n "${RIDDLE_KEYSTORE:-}" ]; then
  # CI: a real release key, supplied from repository secrets. Nothing here
  # knows or prints the passwords.
  KEYSTORE="$RIDDLE_KEYSTORE"
  if [ ! -f "$KEYSTORE" ]; then
    echo "error: RIDDLE_KEYSTORE is set but $KEYSTORE does not exist" >&2
    exit 1
  fi
  KS_ALIAS="${RIDDLE_KEY_ALIAS:?RIDDLE_KEY_ALIAS must be set with RIDDLE_KEYSTORE}"
  KS_PASS="${RIDDLE_KEYSTORE_PASSWORD:?RIDDLE_KEYSTORE_PASSWORD must be set with RIDDLE_KEYSTORE}"
  KEY_PASS="${RIDDLE_KEY_PASSWORD:-$KS_PASS}"
elif [ "$DEBUGGABLE" -eq 1 ]; then
  # The diagnostic build is signed with the SDK's AOSP debug key, which every
  # emulator already trusts. Its manifest differs from the release build, so it
  # could not be installed over it regardless of which key is used.
  KEYSTORE="${ANDROID_DEBUG_KEYSTORE:-$HOME/.android/debug.keystore}"
  if [ ! -f "$KEYSTORE" ]; then
    echo "error: --debug needs the AOSP debug keystore at $KEYSTORE" >&2
    exit 1
  fi
  KS_ALIAS="androiddebugkey"
  KS_PASS="android"
  KEY_PASS="android"
else
  # The key lives beside the project, NOT in build/: build/ is wiped on every
  # run, and regenerating the key would make each build's APK un-installable
  # over the previous one (signature mismatch). It is gitignored — publish real
  # builds by setting the RIDDLE_KEY* variables, as CI does.
  KEYSTORE="$RIDDLE_PROJECT/keystore/riddle.keystore"
  KS_ALIAS="riddle"
  KS_PASS="android"
  KEY_PASS="android"
  mkdir -p "$(dirname "$KEYSTORE")"
  if [ ! -f "$KEYSTORE" ]; then
    echo "==> generating a throwaway signing key for this working copy"
    keytool -genkeypair -v \
      -keystore "$KEYSTORE" \
      -storepass "$KS_PASS" -keypass "$KEY_PASS" \
      -alias "$KS_ALIAS" \
      -keyalg RSA -keysize 2048 -validity 10000 \
      -dname "CN=The Diary, OU=Android port, O=riddle, C=CA" >/dev/null 2>&1
  fi
fi

echo "==> apksigner"
if [ "$DEBUGGABLE" -eq 1 ]; then
  FINAL="$RIDDLE_PROJECT/riddle-$VERSION-arm64-v8a-debug.apk"
else
  FINAL="$RIDDLE_PROJECT/riddle-$VERSION-arm64-v8a.apk"
fi
"$APKSIGNER" sign \
  --ks "$KEYSTORE" \
  --ks-pass "pass:$KS_PASS" \
  --key-pass "pass:$KEY_PASS" \
  --ks-key-alias "$KS_ALIAS" \
  --out "$FINAL" \
  "$OUT/apk/aligned.apk"

"$APKSIGNER" verify --print-certs "$FINAL" | head -5

echo
echo "==> $FINAL ($(du -h "$FINAL" | cut -f1))"
echo "    version $VERSION (code $VERSION_CODE)"
echo "    install: adb install -r '$FINAL'"
