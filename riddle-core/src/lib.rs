//! JNI surface for the Android app.
//!
//! Java cannot touch Rust's types, so this is the whole contract: a handful of
//! `external fun` entry points on `com.stoutput.riddleandroid.DiaryView` plus the
//! callbacks Rust makes back into the view (invalidate / open settings).
//!
//! **Threading.** Input arrives on the UI thread; the animation must tick even
//! when nothing is happening. To keep that ordering trivial and avoid holding a
//! lock across a 60Hz tick, the `App` is owned outright by one engine thread.
//! JNI calls never touch the `App`; they push work onto queues that the engine
//! drains. The only shared state is the bitmap, which the engine writes and the
//! view reads on the UI thread — the same pixels-to-screen handoff any Android
//! game uses, and unavoidable without a copy per frame.

// `app` is public so the host-side harness (`examples/dump_page.rs`) can drive
// the engine without a device; the JNI entry points below are the real API.
pub mod app;
mod config;
mod fb;
mod help;
mod ink;
mod memory;
mod oracle;
mod script;
mod surface;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use jni::objects::{GlobalRef, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jstring, JNI_VERSION_1_6};
use jni::{JNIEnv, JavaVM};

use app::{App, Host, PenSample, Tool};

/// Messages the UI thread sends to the engine thread.
enum Cmd {
    Input(PenSample),
    PenUp,
    /// The view is ready to display the page.
    Start,
    Forget,
    Stop,
}

struct Shared {
    cmds: Mutex<VecDeque<Cmd>>,
    /// Signalled when a command is queued, so the engine wakes promptly.
    ready: Condvar,
    running: AtomicBool,
    /// The most recent finished frame, waiting for the UI thread to collect it.
    pixels: Mutex<Option<Vec<u16>>>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            cmds: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            running: AtomicBool::new(false),
            pixels: Mutex::new(None),
        })
    })
}

/// The engine's view of the Android UI. Holds a global ref to the view so a
/// background thread can legally call back into it.
struct AndroidHost {
    vm: JavaVM,
    view: GlobalRef,
}

impl AndroidHost {
    /// Call `view.onRiddleDirty()` on the UI thread's message queue
    /// (`View.postInvalidate` is thread-safe, unlike `invalidate`).
    fn post_invalidate(&self) -> bool {
        let mut env = match self.vm.attach_current_thread_as_daemon() {
            Ok(e) => e,
            Err(e) => {
                log_error(&format!("attach failed: {e}"));
                return false;
            }
        };
        let res = env
            .call_method(&self.view, "postInvalidateOnAnimation", "()V", &[])
            .map(|_| ())
            .map_err(|e| e.to_string());
        report(&mut env, "postInvalidateOnAnimation", res)
    }

    fn open_settings(&self) -> bool {
        let mut env = match self.vm.attach_current_thread_as_daemon() {
            Ok(e) => e,
            Err(e) => {
                log_error(&format!("attach failed: {e}"));
                return false;
            }
        };
        let res = env
            .call_method(&self.view, "onRiddleOpenSettings", "()V", &[])
            .map(|_| ())
            .map_err(|e| e.to_string());
        report(&mut env, "onRiddleOpenSettings", res)
    }
}

/// Shared tail for the two callbacks: pending Java exceptions are described and
/// cleared. A `panic = "abort"` build cannot unwind out of JNI, so swallowing
/// here is what keeps a UI hiccup from killing the diary.
fn report(env: &mut JNIEnv, what: &str, res: Result<(), String>) -> bool {
    match res {
        Ok(()) => true,
        Err(e) => {
            let mut msg = e;
            if let Ok(true) = env.exception_check() {
                if let Ok(t) = env.exception_occurred() {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                    // JThrowable has no Debug; its ToString is the useful part.
                    let detail = env
                        .call_method(&t, "toString", "()Ljava/lang/String;", &[])
                        .ok()
                        .and_then(|v| v.l().ok())
                        .and_then(|o| {
                            let js = JString::from(o);
                            // Materialize now: JavaStr borrows `js`, which dies here.
                            env.get_string(&js).ok().map(String::from)
                        })
                        .unwrap_or_else(|| "unknown Java exception".into());
                    msg = format!("{msg} ({detail})");
                }
            }
            log_error(&format!("{what}: {msg}"));
            false
        }
    }
}

impl Host for AndroidHost {
    fn on_open_settings(&self) {
        self.open_settings();
    }
    fn request_repaint(&self) {
        self.post_invalidate();
    }
    fn log(&self, msg: &str) {
        log_info(msg);
    }
}

// ---------------------------------------------------------------------------
// Android logging. `liblog` is present in every Android process, so we call it
// directly instead of adding a logging crate. stderr is redirected to /dev/null
// for apps, so eprintln! alone would be invisible.
// ---------------------------------------------------------------------------

const ANDROID_LOG_INFO: i32 = 4;
const ANDROID_LOG_ERROR: i32 = 6;

#[cfg(target_os = "android")]
extern "C" {
    fn __android_log_write(prio: i32, tag: *const std::os::raw::c_char, text: *const std::os::raw::c_char) -> i32;
}

#[cfg(target_os = "android")]
fn android_log(prio: i32, msg: &str) {
    let tag = b"riddle\0";
    match std::ffi::CString::new(msg) {
        Ok(text) => unsafe {
            __android_log_write(prio, tag.as_ptr() as *const _, text.as_ptr());
        },
        Err(_) => unsafe {
            let fallback = b"riddle: (message contained a NUL byte)\0";
            __android_log_write(prio, tag.as_ptr() as *const _, fallback.as_ptr() as *const _);
        },
    }
}

/// Host builds (the unit tests and `examples/dump_page`) have no liblog, so
/// send the same lines to stderr.
#[cfg(not(target_os = "android"))]
fn android_log(prio: i32, msg: &str) {
    let level = match prio {
        ANDROID_LOG_ERROR => "E",
        _ => "I",
    };
    eprintln!("[{level}] {msg}");
}

/// Mirror each line into a file as well as logcat.
///
/// logcat is the right destination and is where these normally go, but some
/// devices filter app tags out of the buffer entirely — including the emulator
/// this port was verified on, where *no* app log line is ever visible. A file
/// in the app's private directory can then be read back with
/// `adb shell run-as com.stoutput.riddleandroid cat files/riddle.log`, which turns
/// an unreproducible "it just shows a blank page" into a readable trace.
fn log_to_file(level: &str, msg: &str) {
    let path = match config::var("RIDDLE_LOG_FILE") {
        Some(p) => p,
        None => return,
    };
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        // No timestamps: Android's logcat has them, and the engine's own
        // ordering is what matters here.
        let _ = writeln!(f, "[{level}] {msg}");
    }
}

fn log_info(msg: &str) {
    android_log(ANDROID_LOG_INFO, msg);
    log_to_file("I", msg);
}

fn log_error(msg: &str) {
    android_log(ANDROID_LOG_ERROR, msg);
    log_to_file("E", msg);
}

// ---------------------------------------------------------------------------
// JNI entry points
// ---------------------------------------------------------------------------

/// Read a Java string into a Rust `String`; missing nulls become empty.
fn get_string(env: &mut JNIEnv, s: &JString) -> String {
    if s.is_null() {
        return String::new();
    }
    env.get_string(s).map(|v| v.into()).unwrap_or_default()
}

/// `nativeCreate(configPath, view, dataDir, cacheDir)`.
///
/// `configPath` is the `KEY=value` store the settings screen wrote; `dataDir`
/// and `cacheDir` are the app's private directories from `Context`. The engine
/// thread is started here and runs until `nativeDestroy`.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeCreate(
    mut env: JNIEnv,
    _class: JClass,
    config_path: JString,
    view: JObject,
    data_dir: JString,
    cache_dir: JString,
) -> jboolean {
    let config_path = get_string(&mut env, &config_path);
    let data_dir = get_string(&mut env, &data_dir);
    let cache_dir = get_string(&mut env, &cache_dir);

    // Install the settings before anything reads them.
    config::init_from_file(&config_path);

    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            log_error(&format!("get_java_vm: {e}"));
            return 0; // JNI_FALSE
        }
    };
    let view_ref = match env.new_global_ref(&view) {
        Ok(r) => r,
        Err(e) => {
            log_error(&format!("new_global_ref: {e}"));
            return 0;
        }
    };

    let host = AndroidHost { vm, view: view_ref };
    let sh = Arc::clone(shared());
    if sh.running.swap(true, Ordering::SeqCst) {
        log_error("nativeCreate called twice; ignoring");
        return 1;
    }

    // Keep the directories reachable through the same config table the ported
    // modules already read from, so no module needs a new parameter.
    std::env::set_var("RIDDLE_DATA_DIR", &data_dir);
    std::env::set_var("RIDDLE_CACHE_DIR", &cache_dir);

    std::thread::Builder::new()
        .name("riddle-engine".into())
        .spawn(move || engine_main(host, sh))
        .is_ok() as jboolean
}

fn engine_main(host: AndroidHost, sh: Arc<Shared>) {
    // A panic here would otherwise vanish: release builds are panic=abort and
    // an Android app has no stderr. Record the location before dying.
    std::panic::set_hook(Box::new(|info| {
        log_error(&format!("PANIC: {info}"));
    }));

    host.log("riddle: the diary is opening");
    let mut app = match App::new(&host) {
        Ok(a) => a,
        Err(e) => {
            host.log(&format!("riddle: fatal: {e}"));
            sh.running.store(false, Ordering::SeqCst);
            return;
        }
    };
    host.log("riddle: the diary is open");

    while sh.running.load(Ordering::SeqCst) {
        // Drain everything queued before this tick, so input and commands are
        // applied in order and the state machine sees the latest pen position.
        let cmds: Vec<Cmd> = {
            let mut q = sh.cmds.lock().unwrap();
            q.drain(..).collect()
        };
        for cmd in cmds {
            match cmd {
                Cmd::Start => {
                    app.start();
                    // Stage the opening sheet at once. The UI has nothing to
                    // show until a frame arrives, and when the guide is drawn
                    // there may be no animation tick to produce one later.
                    *sh.pixels.lock().unwrap() = Some(app.pixels().to_vec());
                    host.request_repaint();
                }
                Cmd::Input(s) => app.push_input(s),
                Cmd::PenUp => app.pen_up(),
                Cmd::Forget => app.forget_everything(),
                Cmd::Stop => {
                    sh.running.store(false, Ordering::SeqCst);
                }
            }
        }

        for line in app.take_logs() {
            host.log(&line);
        }

        if app.is_ready() {
            let res = app.step(&host);
            if res.dirty {
                // Stage the finished page. The clone is what makes the handoff
                // race-free: the UI thread takes a whole frame rather than
                // sharing the engine's live buffer.
                *sh.pixels.lock().unwrap() = Some(app.pixels().to_vec());
                host.request_repaint();
            }
        }

        // ~120Hz ceiling: the engine's animation stamps advance per tick, so
        // ticking faster would only make Tom write faster than a hand.
        std::thread::sleep(Duration::from_millis(8));
    }

    host.log("riddle: the diary closes");
}

/// `nativeStart()` — the view is ready to show the page.
///
/// There is no bitmap pointer to pass any more: the engine owns the page bytes
/// and the view pulls them with `nativeCopyPixels`. See `Surface::pixels`.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeStart(
    _env: JNIEnv,
    _class: JClass,
) {
    let sh = shared();
    sh.cmds.lock().unwrap().push_back(Cmd::Start);
    sh.ready.notify_all();
}

/// `nativeCopyPixels(short[])` — fill the view's RGB565 array with the newest
/// finished frame. Returns false when nothing new is waiting.
///
/// The frame handoff is a swap, not a lock held across the copy: the engine
/// builds a frame off to the side and the UI thread takes it whole. Holding a
/// lock here would put the UI thread and the engine in each other's way on
/// every frame, for no benefit — a torn frame would just be replaced 8ms later.
///
/// `GetShortArrayElements` (rather than a critical section) because this is a
/// bulk copy where the JVM's copy-in/copy-out would double the work, and
/// pinning an array across a call that also takes a lock is the combination
/// that deadlocks a moving collector.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeCopyPixels(
    mut env: JNIEnv,
    _obj: JClass,
    out: jni::objects::JShortArray,
) -> jboolean {
    if out.is_null() {
        return 0; // JNI_FALSE
    }
    let len = match env.get_array_length(&out) {
        Ok(l) if l > 0 => l as usize,
        _ => return 0,
    };
    let pending: Option<Vec<u16>> = shared().pixels.lock().unwrap().take();
    let Some(src) = pending else { return 0 };

    let guard = match unsafe {
        env.get_array_elements(&out, jni::objects::ReleaseMode::NoCopyBack)
    } {
        Ok(g) => g,
        Err(e) => {
            log_error(&format!("nativeCopyPixels: {e}"));
            // Put the frame back: dropping it would leave the view blank until
            // the next animation tick, and an idle page draws none.
            *shared().pixels.lock().unwrap() = Some(src);
            return 0;
        }
    };
    // The array is deliberately over-sized by the view if it ever changes
    // geometry; copy what fits rather than panicking.
    let n = src.len().min(len);
    let dst: &mut [u16] =
        unsafe { std::slice::from_raw_parts_mut(guard.as_ptr() as *mut u16, n) };
    for (d, v) in dst.iter_mut().zip(src.iter()) {
        // RGB565 little-endian, the byte order `put_px` writes into the
        // engine's own buffer, so Android reads the same colours upstream's
        // panel did.
        *d = v.to_le();
    }
    1 // JNI_TRUE
}

/// `nativeInput(action, x, y, pressure, toolType)`.
///
/// `action` is 0=down/move, 1=up/cancel; `toolType` is 0=pen, 1=eraser.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeInput(
    _env: JNIEnv,
    _class: JClass,
    action: jint,
    x: jint,
    y: jint,
    pressure: jint,
    tool: jint,
) {
    let sample = PenSample {
        x,
        y,
        pressure: pressure.clamp(0, 4096),
        tool: if tool == 1 { Tool::Eraser } else { Tool::Pen },
        touching: action == 0,
    };
    let sh = shared();
    let mut q = sh.cmds.lock().unwrap();
    q.push_back(Cmd::Input(sample));
    // A release is also pushed as an explicit pen-up so the ink stroke is
    // closed even if the release sample is filtered out upstream.
    if action != 0 {
        q.push_back(Cmd::PenUp);
    }
}

/// Make the diary forget every remembered page.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeForget(_env: JNIEnv, _class: JClass) {
    shared().cmds.lock().unwrap().push_back(Cmd::Forget);
}

/// Stop the engine thread (called from `onDestroy`).
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeDestroy(_env: JNIEnv, _class: JClass) {
    let sh = shared();
    sh.cmds.lock().unwrap().push_back(Cmd::Stop);
    sh.running.store(false, Ordering::SeqCst);
    sh.ready.notify_all();
}

/// Run one oracle turn against a PNG and return the reply. Mirrors upstream's
/// `riddle --oracle-test`: the way to check a key, endpoint and model without
/// opening the diary. Returns an empty string on failure.
#[no_mangle]
pub extern "C" fn Java_com_stoutput_riddleandroid_DiaryView_nativeOracleTest(
    mut env: JNIEnv,
    _class: JClass,
    png_path: JString,
) -> jstring {
    let path = get_string(&mut env, &png_path);
    let store = memory::MemoryStore::open();
    let oracle = match oracle::Oracle::spawn(store.is_some()) {
        Ok(o) => o,
        Err(e) => return new_string(&mut env, &format!("oracle spawn failed: {e}")),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    oracle.ask(&path, &app::build_ctx(&store), tx);
    let mut got = String::new();
    loop {
        match rx.recv() {
            Ok(Ok(oracle::Event::Ink(chunk))) => {
                got.push_str(&chunk);
                got.push(' ');
            }
            Ok(Ok(oracle::Event::Show(id))) => {
                got.push_str(&format!("[would conjure memory {id}]"));
            }
            Ok(Ok(oracle::Event::Transcript(_))) => {}
            Ok(Err(e)) => return new_string(&mut env, &format!("oracle error: {e}")),
            Err(_) => break,
        }
    }
    new_string(&mut env, got.trim())
}

fn new_string(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|j| j.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// `JNI_OnLoad`: nothing to set up, but returning the version is required for
/// the library to be usable at all.
#[no_mangle]
pub extern "C" fn JNI_OnLoad(_vm: JavaVM, _reserved: *mut std::ffi::c_void) -> jint {
    JNI_VERSION_1_6
}
