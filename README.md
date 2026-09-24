# riddle for Android

The diary of Tom Riddle, ported to Android. Write on the page with a pen; after
a pause the diary drinks your ink, thinks, and writes its answer back by hand,
stroke by stroke.

This is a port of [MaximeRivest/riddle](https://github.com/MaximeRivest/riddle)
(revision `0d1a9feea75027543e2ed49c721656cd261f03a1`, version 0.3.0), which
targets the **reMarkable Paper Pro** — a 1620×2160 e-ink tablet. Upstream is a
Rust binary that talks to the tablet's display engine directly and runs as root
under systemd. None of that exists inside an APK, so the platform layer was
replaced while the diary itself was kept.

- **Download:** grab the APK from the
  [latest release](../../releases/latest), or build it yourself (below)
- **Package:** `com.stoutput.riddleandroid` · arm64-v8a · Android 7.0+ (minSdk 24)
- **Install:** `adb install -r riddle-<version>-arm64-v8a.apk`

Upstream's handwriting engine is reused, not reimplemented — see
[NOTICE](NOTICE) for exactly which files came from where.

## What was kept, and what was replaced

The interesting part of upstream is not the platform plumbing — it is the
handwriting. `script.rs` rasterizes text in Dancing Script, thins it to
one-pixel skeletons with Zhang–Suen, traces those skeletons into ordered
polylines, and replays them stroke by stroke so the reply looks written rather
than printed. That, the ink model, the dissolve effect, the memory store, the
question-mark detector, and the oracle client are all device-agnostic and were
ported unchanged.

| Upstream module | Disposition |
|---|---|
| `surface.rs`, `fb.rs`, `ink.rs`, `script.rs`, `help.rs`, `memory.rs` | Ported; `surface` gained an owned RGB565 buffer |
| `oracle.rs` | Kept the HTTP backend; the `pi` backend was dropped (see below) |
| `main.rs` | Replaced by `app.rs` — the same state machine, driven by Android instead of a `loop {}` |
| `display.rs`, `qtfb.rs`, `pen.rs`, `touch.rs`, `power.rs` | **Deleted.** Android supplies the display and input |

### Why the `pi` backend is gone

Upstream can drive its oracle either over HTTP or through `pi`, a resident Node
RPC process. The second needs a Node install, a writable `/home/root`, and a
long-lived child process — none of which an Android app sandbox provides. The
HTTP backend (any OpenAI-compatible endpoint, pure-Rust HTTPS via `ureq` +
`rustls`) is upstream's own recommended default, so that is what shipped. Any
vision-capable model works.

### Why the engine owns its pixels

The Rust engine draws into a buffer it owns, and the view copies it into a
`Bitmap` after each dirty frame (`nativeCopyPixels`). Drawing straight into the
bitmap from native code would be the obvious approach, but:

- `AndroidBitmap_lockPixels` lives in `libjnigraphics` behind a C header, which
  would put a C stub in an otherwise pure-Rust library; and
- `Bitmap.mBuffer` (the field the NDK helper wraps) is **not visible to JNI on
  current Android** — `GetFieldID` fails on it, which is how the first build of
  this port died.

An owned buffer has neither problem, and it makes the engine testable on the
host without a device (see `examples/dump_page.rs`).

## Building

You need Rust with the Android target, an Android NDK, a JDK, and the SDK build
tools. Android Studio supplies the last two; its bundled JDK is detected
automatically.

```sh
# Rust (a Homebrew/distro Rust cannot install cross targets — use rustup)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add aarch64-linux-android

# Android NDK + SDK pieces, if you do not have them
sdkmanager --install 'ndk;27.0.12077973' 'platforms;android-34' 'build-tools;34.0.0'
```

Then:

```sh
./scripts/build-apk.sh                # native library + signed APK
./scripts/build-apk.sh --skip-native  # repackage only (Java/resources changed)
./scripts/build-apk.sh --debug        # also mark it debuggable, for run-as
```

The version comes from `riddle-core/Cargo.toml`, and the output is named after
it. Bump that field to release: pushing the bump to `main` is what triggers the
release workflow.

`env.sh` discovers the toolchain, and the build takes these variables:

| Variable | Meaning |
|---|---|
| `ANDROID_NDK_HOME` | NDK root, if it is not in a standard SDK location |
| `ANDROID_SDK_ROOT` / `ANDROID_HOME` | SDK root, for `android.jar` and build-tools |
| `CARGO_HOME` / `RUSTUP_HOME` | a rustup installation outside `~/.cargo` |
| `ANDROID_MIN_SDK` | minimum API level (default 24) |
| `RIDDLE_VERSION_CODE` | Android `versionCode` (default 1; CI uses the run number) |

### Signing

| Variable | Meaning |
|---|---|
| `RIDDLE_KEYSTORE` | an existing keystore to sign with |
| `RIDDLE_KEY_ALIAS` | the key alias inside it |
| `RIDDLE_KEYSTORE_PASSWORD` | the keystore password |
| `RIDDLE_KEY_PASSWORD` | the key password (defaults to the keystore password) |

With none of these set, a local build generates a throwaway key in `keystore/`
(gitignored) so that repeated builds install over each other. That key is fine
for your own device and **wrong for a release** — anyone on an older build could
not update to a differently-signed one.

Set all four and the same script signs properly: that is exactly what CI does,
reading them from repository secrets.

### Releasing

`.github/workflows/release.yml` runs on every push to `main`. It reads the
version from `Cargo.toml`, and if no release exists for it yet, builds, signs,
and publishes the APK with generated notes. If the version is unchanged it does
nothing, so ordinary pushes cost nothing.

It needs four repository secrets (`Settings → Secrets and variables → Actions`):

| Secret | Value |
|---|---|
| `KEYSTORE_BASE64` | `base64 -i release.jks \| pbcopy` (macOS) |
| `KEYSTORE_PASSWORD` | the keystore password |
| `KEY_ALIAS` | the key alias |
| `KEY_PASSWORD` | the key password |

Generate the release key once and keep it safe — losing it means you can never
update an installed copy:

```sh
keytool -genkeypair -v -keystore release.jks -alias riddle \
  -keyalg RSA -keysize 4096 -validity 10000 \
  -dname "CN=The Diary, O=riddle, C=CA"
```

`.github/workflows/tests.yml` runs the unit tests and clippy on every push and
pull request.

### Why there is no Gradle

The app is two activities, five small Java classes, and a Rust library. The build
script drives `aapt2`, `javac`, `d8`, `zipalign` and `apksigner` directly, which
keeps the whole build readable and means the only things to install are an NDK
and a JDK. There is no AndroidX dependency, so the APK has no support-library
weight. If this ever grows a second screen or a background service, moving to
Gradle would be the right call.

## Configuration

Upstream read its settings from `oracle.env` and `RIDDLE_*` environment
variables, sourced by a launch script. An APK has no launch script, so the
settings screen owns them. It writes a `KEY=value` file
(`files/riddle.env`, app-private) that the engine reads once at startup — the
same variable names as upstream, so anyone who knows `oracle.env` already knows
what these mean.

| Setting | Upstream equivalent |
|---|---|
| API key | `RIDDLE_OPENAI_KEY` |
| Endpoint base URL | `RIDDLE_OPENAI_BASE` |
| Model | `RIDDLE_OPENAI_MODEL` |
| Reasoning effort | `RIDDLE_OPENAI_REASONING` |
| Max reply tokens | `RIDDLE_OPENAI_MAX_TOKENS` |
| Let the diary remember | `RIDDLE_MEMORY` |
| Hours from UTC | `RIDDLE_TZ_OFFSET` |

**Test** on the settings screen runs upstream's `riddle --oracle-test` against a
blank page and shows the reply, which is the quickest way to tell a bad key from
a bad model name. On the tablet that diagnostic was only reachable over SSH.

Settings are read once at startup, so changing them reopens the diary.

## Gestures

| Do this | And |
|---|---|
| Write, then rest the pen | The diary drinks your ink and Tom replies |
| Write *"show me what I wrote about…"* | The remembered page rises through the paper |
| Write *"what do you remember?"* | Tom answers with a handwritten index |
| Use the eraser tip, or the **Erase** button | Rub ink out (erased ink is also forgotten) |
| Draw a large **?** | Summon the guide |
| Hold the pen still for ~1.5s on the page | Open settings |
| Tap five fingers at once | Open settings (the tablet's exit gesture, reused) |
| **Settings** button | Open settings |

The guide panel is shown on first launch when no API key is configured, so the
diary explains itself instead of sitting blank.

## Differences from the reMarkable build

These are deliberate, and are the places where the port is *not* upstream:

- **The e-ink waveform modes are gone.** Upstream selected between a fast
  waveform for ink and a flashing full refresh to clear ghosting. An LCD has no
  ghosting, so every update is a plain repaint.
- **No takeover mode.** Upstream could stop the tablet's whole UI and drive the
  display engine directly for the lowest possible latency, and its five-finger
  tap exited the app. Here the diary is an ordinary app; five fingers is a
  shortcut into settings instead.
- **No power-button sleep page.** Upstream grabbed the power button, drew
  "The diary sleeps.", and suspended the tablet itself. Android backgrounds the
  app instead, so there is no page to draw.
- **The `pi` oracle backend is gone** (above).
- **The guide's gesture list says what the Android build actually does.**

## How this was verified

Built for `arm64-v8a` and run on an Android 14 (API 34) emulator:

- The APK installs, launches, and renders the opening guide — pixel-identical to
  the same page rendered on the host by `examples/dump_page.rs`.
- Pen input inks: injected strokes stay on the page.
- The idle commit works: after ~2.8s the page becomes a turn. With no oracle
  configured, upstream's rule holds — the writing is **kept** and the reason is
  written below it, rather than being drunk by something that cannot answer.
- The reply animation runs to completion in the synthesized hand.
- The reply then fades out over 10 stages, leaving the writer's ink.
- The settings screen opens from both the toolbar and the in-page affordance.
- `scripts/build-apk.sh --debug` produces a debuggable build whose
  `files/riddle.log` can be read with `run-as` (see below).

32 unit tests pass (`cargo test`), covering the ported handwriting pipeline, the
memory store, the SSE/stream parser, the eraser's stroke splitting, and the
config parser — plus regressions for the two bugs this port hit: `start()`
erasing the opening page, and `step()` discarding the damage it had drawn.

### Logging

The engine logs to logcat under the tag `riddle` **and** to `files/riddle.log`.
The file exists because the emulator used to verify this port filters app tags
out of logcat entirely — no app log line is ever visible there — which turned
"blank page" into an unanswerable question:

```sh
adb shell run-as com.stoutput.riddleandroid cat files/riddle.log   # needs --debug build
adb logcat -s riddle                                            # on a normal device
```

## Known limitations

- **arm64-v8a only.** No `armeabi-v7a` or `x86_64` build. Every current Android
  tablet with a pen is 64-bit ARM; an emulator testing this app needs an arm64
  image.
- **Not verified against a real vision model.** The oracle path is exercised up
  to the request being issued; answering needs your API key. Use **Test** in
  settings to confirm the endpoint end-to-end.
- **No stylus hardware was available**, so pressure response was exercised with
  a fixed mid-pressure nib (which is also what a finger or mouse provides). Real
  stylus pressure maps through unchanged from upstream's 0–4096 scale, but that
  mapping has not been checked against a physical pen.
- **Rotation is pinned to portrait**, which is how the 1620×2160 page is
  proportioned. Landscape would need a reflow of the page geometry.
- **No launcher icon of its own.** Upstream ships only a 256×256 `icon.png`,
  which is scaled into the density buckets; a proper adaptive icon would be
  better.

## Layout

```
riddle-core/            Rust engine (cdylib, loaded by JNI)
  src/app.rs            the diary's state machine, ported from main.rs
  src/lib.rs            JNI entry points and the frame handoff
  src/config.rs         the settings store that replaces oracle.env
  src/{surface,ink,script,help,memory,oracle}.rs   ported from upstream
  examples/dump_page.rs render a page on the host, for inspection
  UPSTREAM_REVISION     the upstream commit this was ported from
android/app/            the Android module
  src/.../DiaryView.java      the page: bitmap, scaling, MotionEvent decoding
  src/.../MainActivity.java   the page, plus the Erase and Settings buttons
  src/.../SettingsActivity.java  the oracle, memory, and forget-everything
  src/.../Config.java         settings <-> the engine's KEY=value file
  src/.../Logs.java           logcat + the file sink
.github/workflows/      tests.yml (every push), release.yml (on a version bump)
scripts/                build-native.sh, build-apk.sh, make-debuggable-manifest.py
env.sh                  toolchain discovery
```

## Credits and license

Upstream [riddle](https://github.com/MaximeRivest/riddle) is by **Maxime
Rivest**, MIT licensed. The handwriting engine — rasterizing text in Dancing
Script, thinning it to single-pixel skeletons, tracing those into strokes, and
replaying them — is his work, reused here rather than reimplemented.

MIT. `LICENSE` is upstream's, reproduced verbatim with his copyright; this port
adds its own copyright under the same terms. [NOTICE](NOTICE) records exactly
which files came from upstream, which were modified, and which are new.

The Dancing Script font is SIL OFL 1.1 — see `riddle-core/fonts/OFL.txt`.

Upstream's reMarkable-only components (`libqsgepaper.so`, Qt, `libquill.so`) are
proprietary, are not distributed there, and are not used here at all.

