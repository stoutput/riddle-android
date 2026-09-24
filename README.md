# riddle for Android

The diary of Tom Riddle, for Android. Write on the page with a pen; after a
pause the diary drinks your ink, thinks for a moment, and an answer writes
itself back in a flowing hand, stroke by stroke, then fades away.

No screen glow, no keyboard, no chat UI. Just ink appearing on paper.

- **Download:** grab the APK from the [latest release](../../releases/latest)
- **Requires:** an arm64 Android device, Android 7.0+ (minSdk 24), and an
  OpenAI-compatible API key for the diary to answer
- **Install:** `adb install -r riddle-<version>-arm64-v8a.apk`

> This repo is based on the work of **MaximeRivest**, here:
> <https://github.com/MaximeRivest/riddle>. The handwriting engine is his; this
> project builds an Android app around it. See [NOTICE](NOTICE) for what came
> from where.

## How it works

```
 pen (MotionEvent, full stylus pressure)
   │ strokes
   ▼
 riddle ── idle 2.8s → commit page → PNG ──► oracle (any OpenAI-compatible
   │                                          endpoint, streams the reply
   ▼                                          sentence by sentence)
 strokes (Dancing Script → thinned to single-pixel pen paths → traced)
   │
   ▼
 Android Canvas
```

A pen stroke is drawn the moment it arrives, at the hardware event rate. When
you stop writing for ~2.8s the page is committed: the ink is rasterized to a
small grayscale PNG and sent to a vision model, and the reply comes back as
text that is then *written* — rasterized in Dancing Script, thinned to
single-pixel skeletons with Zhang–Suen, traced into ordered polylines, and
replayed stroke by stroke so it looks written rather than printed.

The model never sees the screen, only your handwriting. Everything else is
local.

### Deliberate scope

The diary is a page of paper, not an app with a chat log:

- **The oracle is any OpenAI-compatible endpoint** — OpenAI, OpenRouter, Groq,
  a local server. Pure-Rust HTTPS via `ureq` + `rustls`, no extra libraries.
  There is no built-in `pi`/Node backend: an Android app sandbox cannot host a
  resident Node process.
- **No e-ink waveform handling.** This draws to an LCD or OLED panel, so every
  update is a plain repaint.
- **No takeover mode.** The diary is an ordinary app; it does not stop the rest
  of the system or own the power button.

## The diary remembers

Every finished page is kept — your actual pen strokes, a transcription, and
Tom's reply — so the diary can do three things:

- **Follow the conversation.** Recent pages ride along with each request, so
  Tom remembers what you wrote yesterday.
- **Conjure the past.** Ask in ink — *"show me the page about the garden"*,
  *"find what I wrote on Tuesday"* — and the diary rewrites that page in front
  of you, in your own hand, dated, in faded ink. Touch the pen anywhere and
  today's page returns.
- **Answer from memory.** *"What do you remember?"* gets a handwritten index.

Memories live only on the device, in the app's private storage. Turning
remembering off in settings stores nothing and sends no history with a request;
**Forget everything** deletes what is there.

The page image is deleted as soon as the oracle has read it. Nothing else ever
leaves the device, and there is no telemetry.

## Gestures

| Do this | And |
|---|---|
| Write, then rest the pen | The diary drinks your ink and Tom replies |
| Write *"show me what I wrote about…"* | The remembered page rises through the paper |
| Write *"what do you remember?"* | Tom answers with a handwritten index |
| Use the eraser tip, or the **Erase** button | Rub ink out (erased ink is also forgotten) |
| Draw a large **?** | Summon the guide |
| Hold the pen still for ~1.5s on the page | Open settings |
| Tap five fingers at once | Open settings |
| **Settings** button | Open settings |

The guide is shown on first launch when no API key is set, so the diary
explains itself instead of sitting blank.

## Configuration

All of it lives in the settings screen, and is stored in the app's private
directory as a `KEY=value` file that the engine reads at startup:

| Setting | Default |
|---|---|
| API key | *(none — the diary opens but cannot answer)* |
| Endpoint base URL | `https://api.openai.com/v1` |
| Model | `gpt-4o-mini` (must be vision-capable) |
| Reasoning effort | *(unset — for thinking models, `low` gives faster first ink)* |
| Max reply tokens | `2000` |
| Let the diary remember | on |
| Hours from UTC | *(unset — used for remembered dates)* |

**Test** asks the oracle for one reply from a blank page and shows it, which is
the quickest way to tell a bad key from a bad model name.

Two gotchas with thinking models (Gemini 3.x, o-series): set reasoning effort to
`low` for faster first ink, and keep the token cap roomy — hidden reasoning
tokens count against it, and a tight cap starves the visible reply.

Settings are read once at startup, so changing them reopens the diary.

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
and a JDK. There is no AndroidX dependency, so the APK carries no
support-library weight. If this ever grows a second screen or a background
service, moving to Gradle would be the right call.

## How it is put together

The drawing code and the Android UI are separated on purpose: the engine is
plain Rust over its own pixel buffer, and the Java layer is thin enough to
verify by hand.

The engine owns the page pixels; `DiaryView` copies them into a `Bitmap` after
each dirty frame. Drawing straight into the bitmap from native code would be the
obvious approach, but `AndroidBitmap_lockPixels` lives in `libjnigraphics`
behind a C header, and `Bitmap.mBuffer` — the field the NDK helper wraps — is not
visible to JNI on current Android. An owned buffer has neither problem, and it
keeps the engine testable on the host without a device.

```
riddle-core/            Rust engine (cdylib, loaded by JNI)
  src/app.rs            the diary's state machine
  src/lib.rs            JNI entry points, the engine thread, the frame handoff
  src/config.rs         the settings store
  src/surface.rs        the page buffer and drawing primitives
  src/ink.rs            stroke capture, erase-and-forget, the dissolve effect
  src/script.rs         text → strokes: rasterize, thin, trace, wrap
  src/oracle.rs         the streaming OpenAI-compatible client
  src/memory.rs         remembered pages, on disk
  src/help.rs           the guide panel
  examples/dump_page.rs render a page on the host, for inspection
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

### Diagnosing a problem

The engine logs to logcat under the tag `riddle`, and mirrors every line to
`files/riddle.log`. The file exists because some devices drop app tags from
logcat entirely, where a blank page is otherwise unanswerable:

```sh
adb logcat -s riddle                                            # normal case
adb shell run-as com.stoutput.riddleandroid cat files/riddle.log  # needs --debug
```

`cargo run --example dump_page -- guide|reply|ink` renders a page on the host
and writes it to a PNG, which is the quickest way to tell a drawing bug from a
frame-delivery bug.

## Limitations

- **arm64-v8a only.** No 32-bit or x86_64 build. An emulator needs an arm64
  image.
- **Rotation is pinned to portrait**, which is how the 1620×2160 page is
  proportioned. Landscape would need the page geometry reflowed.
- **The oracle needs a vision-capable model.** One that cannot see images will
  answer as though the page were blank.
- **The launcher icon is upscaled** from a single 256×256 source; a proper
  adaptive icon would be better.

## Credits and license

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The Dancing Script font is SIL OFL 1.1 — see `riddle-core/fonts/OFL.txt`.

Vendor components from the reMarkable ecosystem (`libqsgepaper.so`, Qt,
`libquill.so`) are proprietary and are not part of this repo. This app draws
with Android's own canvas and uses none of them.
