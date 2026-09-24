//! The diary's engine, ported to Android.
//!
//! Upstream `main.rs` owned the whole process: it opened a reMarkable display
//! backend (qtfb or the vendor quill engine), read the pen straight off
//! `/dev/input/event*`, and drove a blocking `loop {}` that slept 2ms per
//! iteration. None of that exists inside an APK, so this module keeps the part
//! that *is* the diary — the state machine, the ink, the animations, the
//! oracle turn — and exposes it as something the Android UI thread can drive:
//!
//!   * [`App::new`] loads the font, opens memory, and warms the oracle.
//!   * [`App::start`] marks the page ready once the view can display it.
//!   * [`App::push_input`] takes pen samples decoded from `MotionEvent`s.
//!   * [`App::step`] advances the animation state machine and reports whether
//!     the page changed (so the view can redraw).
//!
//! The state machine, the timings, and the drawing are otherwise unchanged
//! from upstream, including the magic numbers that make the writing feel
//! right (2.8s idle commit, 14-stage ink dissolve, 26 points per frame).
//!
//! What is deliberately gone: the e-ink waveform modes (a phone/tablet LCD has
//! no ghosting to flush), the five-finger takeover exit, and the power-button
//! suspend page. What replaced them is in the Android UI layer.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ab_glyph::FontRef;

use crate::config;
use crate::fb::{BBox, SCREEN_H, SCREEN_W};
use crate::help::{self, Mode as HelpMode};
use crate::ink;
use crate::memory;
use crate::oracle::{self, Event};
use crate::surface::{Surface, BLACK, FADED, WHITE};

const FONT_TTF: &[u8] = include_bytes!("../fonts/DancingScript.ttf");

const IDLE_COMMIT: Duration = Duration::from_millis(2800);
/// How long the diary waits on a silent oracle before giving up on the turn.
/// Generous: thinking models can lead with a long silence.
const ORACLE_PATIENCE: Duration = Duration::from_secs(120);
const REPLY_PX: f32 = 96.0;
const MARGIN_X: i32 = 120;

/// Pressure at or above which a stylus sample counts as writing rather than a
/// hover. Upstream's raw evdev threshold (40/4096); Android's `getPressure()`
/// is normalized 0..1, so the UI layer scales it into the same range.
pub const PRESSURE_FLOOR: i32 = 40;

type OracleRx = mpsc::Receiver<Result<Event, String>>;

/// One pen sample, already decoded from a `MotionEvent` and mapped into the
/// diary's 1620x2160 coordinate space by the Android layer.
#[derive(Debug, Clone, Copy)]
pub struct PenSample {
    pub x: i32,
    pub y: i32,
    /// 0..4096, matching upstream's digitizer range.
    pub pressure: i32,
    pub tool: Tool,
    pub touching: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Eraser,
}

enum State {
    Listening { last_pen: Option<Instant> },
    Drinking { stage: u32, next: Instant, region: BBox, rx: OracleRx },
    Thinking { rx: OracleRx, pulse: Instant, blot_on: bool, since: Instant },
    Replying { plan: WritePlan, next: Instant, rx: Option<OracleRx> },
    Lingering { until: Instant, region: BBox },
    FadingReply { stage: u32, next: Instant, region: BBox },
    /// The guide panel. `panel: None` = dismissed, waiting for pen-up so the
    /// dismissing touch doesn't leave a mark on the page. `press` times a
    /// still pen, which is the diary's only way into settings.
    Help { panel: Option<help::Help>, until: Instant, press: Option<(i32, i32, Instant)> },
    /// A remembered page rising through the paper.
    Conjuring { plan: ConjurePlan, next: Instant, saved: Vec<u8> },
    /// The conjured memory rests on the page.
    MemoryShown { saved: Option<Vec<u8>>, until: Instant, region: BBox },
}

/// A memory being rewritten onto the page: pre-positioned strokes with their
/// original radii, drawn in faded ink.
struct ConjurePlan {
    strokes: Vec<Vec<(i32, i32, i32)>>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
}

struct WritePlan {
    strokes: Vec<Vec<(i32, i32)>>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
    /// Where the next streamed chunk's first line starts.
    next_y: i32,
}

/// What the Android view asks the engine for after a `step`.
#[derive(Default, Clone, Copy)]
pub struct StepResult {
    /// Any pixels changed: the view should blit the bitmap again.
    pub dirty: bool,
    /// The page changed wholesale (memory conjured/dismissed, fresh page), so
    /// the whole bitmap is new rather than a small region of it.
    #[allow(dead_code)] // set and returned, but no caller distinguishes it yet
    pub full: bool,
}

/// Everything that changed in the rendered page during one step. Upstream
/// pushed each region straight to the panel; here we only need to know
/// *whether* something changed, because the view blits the whole bitmap, so we
/// keep a single boolean plus a full-redraw flag.
#[derive(Default)]
struct Damage {
    any: bool,
    full: bool,
}

impl Damage {
    fn rect(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) {
        self.any = true;
    }
    fn all(&mut self) {
        self.any = true;
        self.full = true;
    }
}

/// Called by the engine when it needs to hand something back to the UI layer.
/// Implemented by the JNI shim; keeping it a trait means the engine itself
/// stays free of JNI types and remains testable on the host.
pub trait Host {
    /// The oracle produced the "open settings" request (guide long-press).
    fn on_open_settings(&self);
    /// A frame is ready: ask the UI to repaint.
    fn request_repaint(&self);
    /// The engine has something for the log.
    fn log(&self, msg: &str);
}



pub struct App {
    font: FontRef<'static>,
    surf: Surface,
    mode: HelpMode,

    store: Option<memory::MemoryStore>,
    oracle: Option<oracle::Oracle>,

    user_ink: ink::Ink,
    state: State,
    pen_down: bool,

    turn_id: u64,
    turn_strokes: memory::Strokes,
    turn_reply: String,
    turn_transcript: Option<String>,
    turn_failed: bool,

    /// True once the view can display the page; see [`App::start`].
    started: bool,
    /// Set by [`App::new`]/[`App::start`] so the first `step` reports the
    /// opening sheet instead of discarding it — `step` resets the damage
    /// accumulator, and the opening page is drawn outside `step`.
    opening_frame: bool,
    /// Raw stylus contact, tracked in every state (the guide dismisses on it).
    stylus_on: bool,
    stylus_tapped: bool,
    /// Most recent pen position, for the guide's hold-to-open-settings test.
    last_input: (i32, i32),

    damage: Damage,
    pending_log: Vec<String>,
}

impl App {
    /// Build the diary. Mirrors upstream's `run()` prologue.
    pub fn new(host: &dyn Host) -> std::io::Result<Self> {
        let font = FontRef::try_from_slice(FONT_TTF).map_err(std::io::Error::other)?;

        let store = memory::MemoryStore::open();
        if let Some(ref s) = store {
            host.log(&format!("riddle: memory holds {} pages", s.entries.len()));
        }

        // Warm the oracle at startup: spawn() resolves the endpoint and key
        // while the writer is still picking up the pen.
        let oracle = match oracle::Oracle::spawn(store.is_some()) {
            Ok(o) => {
                host.log("riddle: oracle ready");
                Some(o)
            }
            Err(e) => {
                host.log(&format!("riddle: oracle spawn failed: {e}"));
                None
            }
        };

        let mut app = Self {
            font,
            // The engine owns the page bytes; the Android view copies them
            // into a Bitmap after each dirty frame. See `Surface::pixels`.
            surf: Surface::new_owned(SCREEN_W, SCREEN_H),
            mode: HelpMode::Android,
            store,
            oracle,
            user_ink: ink::Ink::new(),
            state: State::Listening { last_pen: None },
            pen_down: false,
            turn_id: 0,
            turn_strokes: Vec::new(),
            turn_reply: String::new(),
            turn_transcript: None,
            turn_failed: false,
            started: false,
            opening_frame: true,
            stylus_on: false,
            stylus_tapped: false,
            last_input: (0, 0),
            damage: Damage::default(),
            pending_log: Vec::new(),
        };
        app.white_page();
        Ok(app)
    }

    /// Show the guide panel, as if a large "?" had been drawn.
    pub fn open_guide(&mut self) {
        let panel = help::show(&mut self.surf, &self.font, self.mode);
        let (px, py, pw, ph) = panel.region.rect();
        self.damage.rect(px, py, pw, ph);
        self.state = State::Help {
            panel: Some(panel),
            until: Instant::now() + Duration::from_secs(45),
            press: None,
        };
    }

    /// The view is ready to display the page: draw the opening sheet.
    ///
    /// There is no attach step here (unlike upstream's `set_surface`): the
    /// engine's pixels live in an ordinary buffer, not in the Android bitmap, so
    /// starting only means "the UI can show what I draw now".
    pub fn start(&mut self) {
        self.started = true;
        self.white_page();
        self.opening_frame = true;

        // With no spirit available there is nothing to write to, so the diary
        // introduces itself instead of sitting blank. This must key off the
        // resulting state, not off "was it configured": a *failed* spawn leaves
        // exactly as little to write to as a missing key, and the guide is also
        // where settings is explained.
        //
        // Drawn here rather than in `new`, because the `white_page` above would
        // erase anything drawn earlier — which is the bug this replaced.
        if self.oracle.is_none() {
            self.open_guide();
        }
    }

    /// A fresh blank page, as if the diary had just been opened.
    fn white_page(&mut self) {
        self.surf.fill_rect(0, 0, self.surf.w, self.surf.h, WHITE);
        self.user_ink.clear();
        self.state = State::Listening { last_pen: None };
        self.pen_down = false;
        self.damage.all();
    }

    pub fn is_ready(&self) -> bool {
        self.started
    }

    /// The page as RGB565 pixels, in row-major order, for the view to blit.
    pub fn pixels(&self) -> &[u16] {
        self.surf.pixels()
    }

    /// A short name for the current state, for the engine's trace.
    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Listening { .. } => "listening",
            State::Drinking { .. } => "drinking",
            State::Thinking { .. } => "thinking",
            State::Replying { .. } => "replying",
            State::Lingering { .. } => "lingering",
            State::FadingReply { .. } => "fading",
            State::Help { .. } => "help",
            State::Conjuring { .. } => "conjuring",
            State::MemoryShown { .. } => "memory-shown",
        }
    }

    /// A one-line summary of the page: how much ink is on it, and where.
    ///
    /// Used by the engine's startup trace. A blank page and a page whose
    /// content never reached the view look identical on a screenshot, so this
    /// is what distinguishes them in a bug report.
    pub fn describe_page(&self) -> String {
        let px = self.surf.pixels();
        let (mut n, mut x0, mut y0, mut x1, mut y1) = (0usize, usize::MAX, usize::MAX, 0usize, 0usize);
        for (i, &v) in px.iter().enumerate() {
            // Anything but pure white counts as ink. (`v` is already u16; the
            // mask that used to be here did nothing.)
            if v != 0xFFFF {
                n += 1;
                let (x, y) = (i % self.surf.w, i / self.surf.w);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        if n == 0 {
            "page is blank".to_string()
        } else {
            format!("page has {n} inked px, bbox {x0},{y0}..{x1},{y1}")
        }
    }

    /// Feed one decoded pen sample. Android delivers input on the UI thread,
    /// so these can arrive between `step` calls; each is applied immediately,
    /// exactly as upstream applied each evdev sample.
    pub fn push_input(&mut self, sample: PenSample) {
        let writing = sample.touching && sample.pressure >= PRESSURE_FLOOR;
        self.stylus_on = writing;
        self.stylus_tapped |= writing;
        self.last_input = (sample.x, sample.y);

        if !writing {
            if self.pen_down {
                self.pen_down = false;
                self.user_ink.pen_up();
                if let State::Listening { ref mut last_pen } = self.state {
                    *last_pen = Some(Instant::now());
                }
            }
            return;
        }

        match self.state {
            State::Listening { ref mut last_pen } => {
                self.pen_down = true;
                match sample.tool {
                    Tool::Pen => {
                        // Same brush law as upstream: radius follows pressure.
                        let r = 2 + sample.pressure * 3 / 4096;
                        let d = self.user_ink.pen_point(&mut self.surf, sample.x, sample.y, r);
                        if !d.is_empty() {
                            self.damage.rect(d.x0, d.y0, 0, 0);
                            self.damage.rect(d.x1, d.y1, 0, 0);
                        }
                    }
                    Tool::Eraser => {
                        let d = self.user_ink.erase_point(&mut self.surf, sample.x, sample.y, 22);
                        if !d.is_empty() {
                            self.damage.rect(d.x0, d.y0, 0, 0);
                            self.damage.rect(d.x1, d.y1, 0, 0);
                        }
                    }
                }
                *last_pen = Some(Instant::now());
            }
            // Any pen contact dismisses a lingering reply early.
            State::Lingering { region, .. } => {
                self.state = State::FadingReply { stage: 0, next: Instant::now(), region };
            }
            _ => {}
        }
    }

    /// Begin a pen stroke from outside the engine (the Android toolbar's
    /// eraser toggle pushes `Tool::Eraser` samples instead).
    pub fn pen_up(&mut self) {
        if self.pen_down {
            self.pen_down = false;
            self.user_ink.pen_up();
            if let State::Listening { ref mut last_pen } = self.state {
                *last_pen = Some(Instant::now());
            }
        }
        self.stylus_on = false;
    }

    /// Content is being written to the page by hand; used by the view to know
    /// whether a stylus sample should reach the engine at all.
    #[allow(dead_code)] // part of the engine's public surface, not yet needed
    pub fn stylus_busy(&self) -> bool {
        self.stylus_on
    }

    /// True when the last step repainted the page wholesale (memory conjured
    /// or dismissed, fresh page). The view blits the whole bitmap either way,
    /// so this is only a hint for a caller that wants to distinguish them.
    #[allow(dead_code)]
    pub fn last_step_was_full(&self) -> bool {
        self.damage.full
    }

    /// Draw a page of text as if Tom had written it (used for status lines
    /// when no oracle is configured).
    #[allow(dead_code)]
    pub fn write_line(&mut self, text: &str) {
        let plan = plan_reply(&self.font, text, None);
        self.state = State::Replying { plan, next: Instant::now(), rx: None };
    }

    /// Advance the state machine one tick. `now` is passed in so the host can
    /// drive this from a normal frame callback.
    pub fn step(&mut self, host: &dyn Host) -> StepResult {
        if !self.started {
            return StepResult::default();
        }
        let now = Instant::now();

        // The previous step's damage is reported once and cleared — except on
        // the very first step, which must report the opening sheet that
        // `white_page` drew before the engine started ticking.
        if self.opening_frame {
            self.opening_frame = false;
        } else {
            self.damage = Damage::default();
        }

        if self.store.is_none() {
            // No memory is fine; the diary simply cannot conjure.
        }

        // Snapshot the state so the borrow checker lets us touch `self.surf`
        // inside the arms; the state machine is pure bookkeeping.
        let state = std::mem::replace(&mut self.state, State::Listening { last_pen: None });
        self.state = match state {
            State::Listening { last_pen } => match last_pen {
                Some(t) if !self.pen_down && t.elapsed() >= IDLE_COMMIT && !self.user_ink.is_empty() => {
                    self.finish_turn(host)
                }
                _ => State::Listening { last_pen },
            },
            st @ State::Drinking { .. } => self.step_drinking(st),
            st @ State::Thinking { .. } => self.step_thinking(st, now),
            st @ State::Replying { .. } => self.step_replying(st),
            State::Lingering { until, region } => {
                if now >= until {
                    State::FadingReply { stage: 0, next: now, region }
                } else {
                    State::Lingering { until, region }
                }
            }
            State::Help { panel, until, press } => self.step_help(panel, until, press, host),
            st @ State::Conjuring { .. } => self.step_conjuring(st),
            st @ State::MemoryShown { .. } => self.step_memory_shown(st),
            State::FadingReply { stage, next, region } => {
                const STAGES: u32 = 10;
                if now >= next {
                    ink::dissolve_pass(&mut self.surf, region, stage, STAGES);
                    let (x, y, w, h) = region.rect();
                    self.damage.rect(x, y, w, h);
                    if stage + 1 >= STAGES {
                        self.damage.all();
                        State::Listening { last_pen: None }
                    } else {
                        State::FadingReply {
                            stage: stage + 1,
                            next: now + Duration::from_millis(80),
                            region,
                        }
                    }
                } else {
                    State::FadingReply { stage, next, region }
                }
            }
        };

        self.stylus_tapped = false;
        StepResult { dirty: self.damage.any, full: self.damage.full }
    }

    /// The writer paused: decide between the guide, an excuse, and a real turn.
    fn finish_turn(&mut self, host: &dyn Host) -> State {
        if region_all_white(&self.surf, self.user_ink.bbox) {
            // Everything was erased before the pause: nothing to commit (and
            // no phantom "?" from erased strokes).
            self.user_ink.clear();
            return State::Listening { last_pen: None };
        }

        if help::looks_like_question_mark(self.user_ink.stroke_list()) {
            // Absorb the "?" and open the guide instead of asking.
            let (qx, qy, qw, qh) = self.user_ink.bbox.rect();
            self.surf
                .fill_rect(qx as usize, qy as usize, qw as usize, qh as usize, WHITE);
            self.damage.rect(qx, qy, qw, qh);
            self.user_ink.clear();
            let panel = help::show(&mut self.surf, &self.font, self.mode);
            let (px, py, pw, ph) = panel.region.rect();
            self.damage.rect(px, py, pw, ph);
            host.log("riddle: guide shown");
            return State::Help {
                panel: Some(panel),
                until: Instant::now() + Duration::from_secs(45),
                press: None,
            };
        }

        if self.oracle.is_none() {
            // No spirit at all: don't eat ink that nothing will answer — leave
            // the writing and put the reason below.
            let y = (self.user_ink.bbox.y1 + 90).min(SCREEN_H as i32 - 400);
            let plan = plan_reply(&self.font, &oracle_excuse("no oracle"), Some(y));
            return State::Replying { plan, next: Instant::now(), rx: None };
        }

        let png_path = page_png_path();
        if let Err(e) = self.user_ink.to_png(&self.surf, &png_path) {
            host.log(&format!("riddle: rasterize failed: {e}"));
        }
        self.turn_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.turn_strokes = self.user_ink.stroke_list().to_vec();
        self.turn_reply.clear();
        self.turn_transcript = None;
        self.turn_failed = false;

        // Ask now, so the model streams while the diary drinks the ink and
        // most of the reply latency hides inside the animation.
        let (tx, rx) = mpsc::channel();
        if let Some(ref o) = self.oracle {
            o.ask(&png_path, &build_ctx(&self.store), tx);
        }
        // The request thread reads the page before ask() returns; the writer's
        // words don't need to sit on disk afterwards.
        if config::var("RIDDLE_KEEP_PAGE").is_none() {
            let _ = std::fs::remove_file(&png_path);
        }
        let region = self.user_ink.bbox;
        State::Drinking { stage: 0, next: Instant::now(), region, rx }
    }

    fn step_drinking(&mut self, state: State) -> State {
        let State::Drinking { stage, next, region, rx } = state else { unreachable!() };
        const STAGES: u32 = 14;
        if Instant::now() >= next {
            ink::dissolve_pass(&mut self.surf, region, stage, STAGES);
            let (x, y, w, h) = region.rect();
            self.damage.rect(x, y, w, h);
            if stage + 1 >= STAGES {
                self.user_ink.clear();
                State::Thinking { rx, pulse: Instant::now(), blot_on: false, since: Instant::now() }
            } else {
                State::Drinking {
                    stage: stage + 1,
                    next: Instant::now() + Duration::from_millis(70),
                    region,
                    rx,
                }
            }
        } else {
            State::Drinking { stage, next, region, rx }
        }
    }

    fn step_thinking(&mut self, state: State, now: Instant) -> State {
        let State::Thinking { rx, pulse, blot_on, since } = state else { unreachable!() };
        match rx.try_recv() {
            Ok(result) => {
                self.clear_blot();
                // First streamed event: start writing now; keep the receiver so
                // the rest of the reply can append itself.
                match result {
                    Ok(Event::Show(id)) => match self.conjure(id) {
                        Some(st) => st,
                        None => {
                            self.pending_log.push(format!("riddle: memory {id} is missing"));
                            let plan = plan_reply(&self.font, &oracle_excuse("lost page"), None);
                            self.turn_failed = true;
                            State::Replying { plan, next: Instant::now(), rx: None }
                        }
                    },
                    Ok(Event::Ink(text)) => {
                        self.turn_reply.push_str(&text);
                        let plan = plan_reply(&self.font, &text, None);
                        State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                    }
                    Ok(Event::Transcript(t)) => {
                        // Transcript with no prose yet (the model skipped the
                        // reply): remember the words, keep waiting.
                        self.turn_transcript = Some(t);
                        State::Thinking { rx, pulse, blot_on, since }
                    }
                    Err(e) => {
                        self.pending_log.push(format!("riddle: oracle failed: {e}"));
                        self.turn_failed = true;
                        let plan = plan_reply(&self.font, &oracle_excuse(&e), None);
                        State::Replying { plan, next: Instant::now(), rx: None }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                if since.elapsed() >= ORACLE_PATIENCE {
                    self.pending_log.push(format!(
                        "riddle: oracle timed out after {}s",
                        ORACLE_PATIENCE.as_secs()
                    ));
                    self.clear_blot();
                    let plan = plan_reply(&self.font, &oracle_excuse("timed out"), None);
                    State::Replying { plan, next: Instant::now(), rx: None }
                } else if pulse.elapsed() >= Duration::from_millis(600) {
                    // The thinking blot, pulsing at the centre of the page.
                    let (cx, cy) = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
                    if blot_on {
                        self.surf.fill_rect(cx as usize - 14, cy as usize - 14, 28, 28, WHITE);
                    } else {
                        self.surf.stamp(cx, cy, 9, BLACK);
                    }
                    self.damage.rect(cx - 14, cy - 14, 28, 28);
                    State::Thinking { rx, pulse: now, blot_on: !blot_on, since }
                } else {
                    State::Thinking { rx, pulse, blot_on, since }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => State::Listening { last_pen: None },
        }
    }

    fn clear_blot(&mut self) {
        let (cx, cy) = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
        self.surf.fill_rect(cx as usize - 14, cy as usize - 14, 28, 28, WHITE);
        self.damage.rect(cx - 14, cy - 14, 28, 28);
    }

    fn step_replying(&mut self, state: State) -> State {
        let State::Replying { mut plan, next, mut rx } = state else { unreachable!() };

        // More of the reply may still be streaming in: append each new chunk
        // below what is already planned, mid-animation.
        if let Some(ref r) = rx {
            let drop_rx = match r.try_recv() {
                Ok(Ok(Event::Ink(more))) => {
                    if plan.next_y > SCREEN_H as i32 - 200 {
                        // The page is full: let the rest go unwritten rather
                        // than inking below the visible page.
                        self.pending_log
                            .push("riddle: reply reached the page bottom; trailing text dropped".into());
                        true
                    } else {
                        self.turn_reply.push(' ');
                        self.turn_reply.push_str(&more);
                        append_reply(&self.font, &mut plan, &more);
                        false
                    }
                }
                Ok(Ok(Event::Transcript(t))) => {
                    self.turn_transcript = Some(t);
                    false // the disconnect is still coming
                }
                Ok(Ok(Event::Show(_))) => {
                    self.pending_log.push("riddle: conjuring directive mid-reply ignored".into());
                    false
                }
                Ok(Err(e)) => {
                    self.pending_log.push(format!("riddle: oracle failed mid-reply: {e}"));
                    self.turn_failed = true;
                    true
                }
                Err(mpsc::TryRecvError::Disconnected) => true,
                Err(mpsc::TryRecvError::Empty) => false,
            };
            if drop_rx {
                rx = None;
            }
        }

        if Instant::now() >= next {
            let mut dirty = BBox::empty();
            // Points per frame; the writing hand's speed. Upstream used 26.
            let mut budget = 26;
            {
                let surf = &mut self.surf;
                while budget > 0 && plan.stroke_i < plan.strokes.len() {
                    let stroke = &plan.strokes[plan.stroke_i];
                    if plan.point_i >= stroke.len() {
                        plan.stroke_i += 1;
                        plan.point_i = 0;
                        continue;
                    }
                    let (x, y) = stroke[plan.point_i];
                    if plan.point_i > 0 {
                        let (px, py) = stroke[plan.point_i - 1];
                        surf.brush_line(px, py, x, y, 2, BLACK);
                    } else {
                        surf.stamp(x, y, 2, BLACK);
                    }
                    dirty.add(x, y, 4);
                    plan.point_i += 1;
                    budget -= 1;
                }
            }
            if !dirty.is_empty() {
                let (x, y, w, h) = dirty.rect();
                self.damage.rect(x, y, w, h);
            }
            if plan.stroke_i >= plan.strokes.len() && rx.is_none() {
                // The turn is complete: the diary remembers it.
                self.remember_turn();
                let chars: usize = plan.strokes.iter().map(|s| s.len()).sum();
                let linger = Duration::from_millis(4000 + (chars as u64) * 2);
                let region = plan.region;
                State::Lingering {
                    until: Instant::now() + linger.min(Duration::from_secs(20)),
                    region,
                }
            } else {
                State::Replying { plan, next: Instant::now() + Duration::from_millis(14), rx }
            }
        } else {
            State::Replying { plan, next, rx }
        }
    }

    /// Store the finished turn in memory (the diary remembers).
    fn remember_turn(&mut self) {
        if self.turn_failed || self.turn_reply.is_empty() {
            self.turn_strokes = Vec::new();
            return;
        }
        if let Some(ref mut s) = self.store {
            s.append(
                self.turn_id,
                self.turn_transcript.as_deref().unwrap_or(""),
                self.turn_reply.trim(),
                &self.turn_strokes,
            );
        }
        self.turn_strokes = Vec::new();
    }

    fn step_help(
        &mut self,
        panel: Option<help::Help>,
        until: Instant,
        press: Option<(i32, i32, Instant)>,
        host: &dyn Host,
    ) -> State {
        /// A pen held this long without moving opens settings. Long enough not
        /// to fire while reading the guide, short enough to feel deliberate.
        const HOLD_FOR_SETTINGS: Duration = Duration::from_millis(1500);
        /// How far the pen may drift and still count as "still".
        const HOLD_SLOP: i32 = 40;

        // Track the still pen while the panel is up: the guide is drawn over
        // the page, so its touches are dismissed rather than inked.
        let press = if self.stylus_on {
            match press {
                Some((px, py, since)) if (px - self.last_input.0).abs() <= HOLD_SLOP
                    && (py - self.last_input.1).abs() <= HOLD_SLOP =>
                {
                    if since.elapsed() >= HOLD_FOR_SETTINGS {
                        host.on_open_settings();
                        // Consume it, so holding on does not reopen settings
                        // every tick.
                        Some((self.last_input.0, self.last_input.1, Instant::now()))
                    } else {
                        Some((px, py, since))
                    }
                }
                _ => Some((self.last_input.0, self.last_input.1, Instant::now())),
            }
        } else {
            None
        };

        match panel {
            Some(p) => {
                if self.stylus_tapped || Instant::now() >= until {
                    let region = p.dismiss(&mut self.surf);
                    let (x, y, w, h) = region.rect();
                    self.damage.rect(x, y, w, h);
                    State::Help { panel: None, until, press: None }
                } else {
                    State::Help { panel: Some(p), until, press }
                }
            }
            // Dismissed: swallow the closing touch, listen again on pen-up.
            None if self.stylus_on => State::Help { panel: None, until, press },
            None => State::Listening { last_pen: None },
        }
    }

    fn step_conjuring(&mut self, state: State) -> State {
        let State::Conjuring { mut plan, next, saved } = state else { unreachable!() };
        if self.stylus_tapped {
            // The writer interrupts: today's page returns at once.
            {
                let surf = &mut self.surf;
                surf.paste_rect(0, 0, surf.w, surf.h, &saved);
            }
            self.damage.all();
            return State::MemoryShown { saved: None, until: Instant::now(), region: plan.region };
        }
        if Instant::now() >= next {
            let mut dirty = BBox::empty();
            // The memory pours back faster than Tom writes: it is remembered,
            // not composed. Upstream used 48 points per frame.
            let mut budget = 48;
            {
                let surf = &mut self.surf;
                while budget > 0 && plan.stroke_i < plan.strokes.len() {
                    let stroke = &plan.strokes[plan.stroke_i];
                    if plan.point_i >= stroke.len() {
                        plan.stroke_i += 1;
                        plan.point_i = 0;
                        continue;
                    }
                    let (x, y, r) = stroke[plan.point_i];
                    if plan.point_i > 0 {
                        let (px, py, pr) = stroke[plan.point_i - 1];
                        surf.brush_line(px, py, x, y, r.min(pr + 1), FADED);
                    } else {
                        surf.stamp(x, y, r, FADED);
                    }
                    dirty.add(x, y, r + 2);
                    plan.point_i += 1;
                    budget -= 1;
                }
            }
            if !dirty.is_empty() {
                let (x, y, w, h) = dirty.rect();
                self.damage.rect(x, y, w, h);
            }
            if plan.stroke_i >= plan.strokes.len() {
                let region = plan.region;
                State::MemoryShown {
                    saved: Some(saved),
                    until: Instant::now() + Duration::from_secs(120),
                    region,
                }
            } else {
                State::Conjuring {
                    plan,
                    next: Instant::now() + Duration::from_millis(10),
                    saved,
                }
            }
        } else {
            State::Conjuring { plan, next, saved }
        }
    }

    fn step_memory_shown(&mut self, state: State) -> State {
        let State::MemoryShown { saved, until, region } = state else { unreachable!() };
        match saved {
            Some(s) => {
                if self.stylus_tapped || Instant::now() >= until {
                    {
                let surf = &mut self.surf;
                        surf.paste_rect(0, 0, surf.w, surf.h, &s);
                    }
                    self.damage.all();
                    self.pending_log.push("riddle: memory dismissed".into());
                    State::MemoryShown { saved: None, until, region }
                } else {
                    State::MemoryShown { saved: Some(s), until, region }
                }
            }
            // Dismissed: swallow the closing touch, listen again on pen-up.
            None if self.stylus_on => State::MemoryShown { saved: None, until, region },
            None => State::Listening { last_pen: None },
        }
    }

    /// Summon a remembered page: snapshot today's page, clear the paper, and
    /// plan the memory's rewriting — the date in a small hand, the writer's own
    /// strokes exactly as they were penned, Tom's old reply beneath — all in
    /// faded ink.
    fn conjure(&mut self, id: u64) -> Option<State> {
        // Clone out of the store first: the drawing below needs `&mut self.surf`,
        // so the store borrow must end here.
        let (entry, strokes) = {
            let store = self.store.as_ref()?;
            (store.get(id)?.clone(), store.strokes(id).unwrap_or_default())
        };
        self.pending_log
            .push(format!("riddle: conjuring memory {id} ({})", memory::spoken_date(id)));

        let surf = &mut self.surf;
        let saved = surf.copy_rect(0, 0, surf.w, surf.h);
        surf.fill_rect(0, 0, surf.w, surf.h, WHITE);
        self.damage.all();

        let mut all: Vec<Vec<(i32, i32, i32)>> = Vec::new();
        let mut region = BBox::empty();

        // The date, small and centered near the top, like a diary heading.
        let date = memory::spoken_date(entry.id);
        let mut raster = crate::script::rasterize_line(&self.font, &date, 54.0);
        crate::script::thin(&mut raster);
        let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
        let mut ink_bottom = 64;
        for stroke in crate::script::trace(&raster) {
            let mapped: Vec<(i32, i32, i32)> =
                stroke.iter().map(|&(sx, sy)| (x0 + sx, 64 + sy, 1)).collect();
            for &(x, y, r) in &mapped {
                region.add(x, y, r + 2);
                ink_bottom = ink_bottom.max(y);
            }
            all.push(mapped);
        }

        // The writer's own hand, exactly as it was penned.
        for stroke in &strokes {
            for &(x, y, r) in stroke {
                region.add(x, y, r + 2);
                ink_bottom = ink_bottom.max(y);
            }
            all.push(stroke.clone());
        }

        // Tom's old reply, below.
        if !entry.reply.is_empty() {
            let y = (ink_bottom + 130).min(SCREEN_H as i32 - 400);
            let reply = plan_reply(&self.font, &entry.reply, Some(y));
            for stroke in reply.strokes {
                let mapped: Vec<(i32, i32, i32)> = stroke.iter().map(|&(x, y)| (x, y, 2)).collect();
                for &(x, y, r) in &mapped {
                    region.add(x, y, r + 2);
                }
                all.push(mapped);
            }
        }

        Some(State::Conjuring {
            plan: ConjurePlan { strokes: all, stroke_i: 0, point_i: 0, region },
            next: Instant::now(),
            saved,
        })
    }

    /// Drain queued log lines; the host prints them to logcat.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_log)
    }

    pub fn forget_everything(&mut self) {
        if let Some(ref mut s) = self.store {
            s.forget_all();
        }
    }
}

/// Where the committed page is rasterized for the oracle. Android gives every
/// app a writable cache dir, which the settings layer passes in; upstream used
/// `/tmp`.
fn page_png_path() -> String {
    let base = match config::var("RIDDLE_CACHE_DIR") {
        Some(d) => d,
        None => std::env::temp_dir().to_string_lossy().into_owned(),
    };
    format!("{base}/riddle-page.png")
}

/// True if the region no longer holds any dark pixels (fully erased).
fn region_all_white(surf: &Surface, region: BBox) -> bool {
    if region.is_empty() {
        return true;
    }
    for y in region.y0..=region.y1 {
        for x in region.x0..=region.x1 {
            if surf.luma(x, y) < 200 {
                return false;
            }
        }
    }
    true
}

/// What Tom writes when the spirit cannot answer: short, in a diary's voice,
/// but specific enough to act on. The raw error still goes to the log.
fn oracle_excuse(e: &str) -> String {
    if e.contains("no oracle") {
        "The diary lies dormant: it found no oracle. \
         Hold the pen still on the page, then give me a key."
            .into()
    } else if e.starts_with("http 401") || e.starts_with("http 403") {
        "The oracle refused the diary's key. Check the key in settings.".into()
    } else if e.starts_with("http ") {
        let code = e.split(':').next().unwrap_or("an error");
        format!("The oracle rejected the diary's plea ({code}). Check the model and endpoint in settings.")
    } else if e.contains("request failed") || e.contains("timed out") {
        "The diary cannot reach its oracle. Is this device connected to the network?".into()
    } else if e.contains("empty reply") {
        "The spirit read your words but said nothing. Write again.".into()
    } else {
        "The ink blurred before it could answer. Write again.".into()
    }
}

/// What the diary sends alongside the page: its memory of recent turns and the
/// catalog the oracle picks conjured pages from. Empty when memory is off.
pub fn build_ctx(store: &Option<memory::MemoryStore>) -> oracle::TurnContext {
    let Some(s) = store else { return oracle::TurnContext::default() };
    let turns: usize = config::var("RIDDLE_MEMORY_TURNS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let (catalog_lines, catalog_ids) = s.catalog(40);
    oracle::TurnContext { history: s.recent_dialogue(turns), catalog_lines, catalog_ids }
}

/// Lay out reply text and produce screen-space strokes. `y_start` continues a
/// streamed reply below its previous chunk; None places the first chunk.
fn plan_reply(font: &FontRef, text: &str, y_start: Option<i32>) -> WritePlan {
    let max_w = (SCREEN_W as i32 - 2 * MARGIN_X) as f32;
    let lines = crate::script::wrap(font, text, REPLY_PX, max_w);
    let line_h = (REPLY_PX * 1.25) as i32;
    let total_h = line_h * lines.len() as i32;
    let mut y = y_start.unwrap_or(((SCREEN_H as i32 - total_h) / 3).max(60));
    let mut strokes = Vec::new();
    let mut region = BBox::empty();
    let mut seed = 0x1234u32;
    // The hand's tiny per-line wobble, so the writing is not machine-straight.
    let mut jitter = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((seed >> 16) % 7) as i32 - 3
    };

    for line_text in &lines {
        let mut raster = crate::script::rasterize_line(font, line_text, REPLY_PX);
        crate::script::thin(&mut raster);
        let line_strokes = crate::script::trace(&raster);
        let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
        let wobble = jitter();
        for s in line_strokes {
            let mapped: Vec<(i32, i32)> =
                s.iter().map(|&(sx, sy)| (x0 + sx, y + sy + wobble)).collect();
            for &(x, yy) in &mapped {
                region.add(x, yy, 5);
            }
            strokes.push(mapped);
        }
        y += line_h;
    }

    WritePlan { strokes, stroke_i: 0, point_i: 0, region, next_y: y }
}

/// Splice a streamed continuation chunk into a running write animation.
fn append_reply(font: &FontRef, plan: &mut WritePlan, more: &str) {
    let cont = plan_reply(font, more, Some(plan.next_y));
    if cont.strokes.is_empty() {
        return;
    }
    plan.region.add(cont.region.x0, cont.region.y0, 0);
    plan.region.add(cont.region.x1, cont.region.y1, 0);
    plan.strokes.extend(cont.strokes);
    plan.next_y = cont.next_y;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's callbacks, with nowhere to send them.
    struct NullHost;

    impl Host for NullHost {
        fn on_open_settings(&self) {}
        fn request_repaint(&self) {}
        fn log(&self, _msg: &str) {}
    }

    /// The number of inked (non-white) pixels on the page.
    fn inked(app: &App) -> usize {
        app.pixels().iter().filter(|&&v| v.to_le() != 0xFFFF).count()
    }

    /// `start` whitens the page, so anything drawn before it is erased. The
    /// opening guide is drawn *by* `start` for exactly this reason — an earlier
    /// revision drew it in `new` and the diary came up blank on device.
    #[test]
    fn start_draws_the_opening_page_rather_than_erasing_it() {
        let host = NullHost;
        let mut app = App::new(&host).expect("engine starts");

        // Nothing is on the page until the view says it can display one.
        assert_eq!(inked(&app), 0, "page should be blank before start");
        assert!(!app.is_ready());

        app.start();
        assert!(app.is_ready());
        assert_eq!(app.state_name(), "help", "the guide should be open");
        assert!(
            inked(&app) > 10_000,
            "opening page is blank ({} inked px): start() erased it",
            inked(&app)
        );
    }

    /// The first `step` reports the opening page. `step` clears the damage
    /// accumulator each call, so without the `opening_frame` carry-over the
    /// view is never told to repaint and the diary stays white forever.
    #[test]
    fn first_step_reports_the_opening_page() {
        let host = NullHost;
        let mut app = App::new(&host).expect("engine starts");
        app.start();

        let first = app.step(&host);
        assert!(first.dirty, "the opening page was never reported to the view");
    }

    /// A pen stroke is inked, then committed 2.8s after the pen stops — and
    /// with no oracle it is *kept* (the excuse is written below it) rather than
    /// drunk, so the writer does not lose words nothing was going to answer.
    #[test]
    fn ink_commits_after_the_idle_pause_and_survives_without_an_oracle() {
        // The host environment decides whether an oracle is available (the
        // RIDDLE_OPENAI_* variables may be set on a developer's machine), so
        // this test asserts what holds either way.
        let host = NullHost;
        let mut app = App::new(&host).expect("engine starts");
        app.start();
        // Dismiss whatever opening page is up, so the state machine listens.
        app.step(&host);
        // A pen contact dismisses the guide, and the engine swallows the
        // closing touch before listening again — so drive it to completion
        // rather than assuming a fixed number of steps.
        for _ in 0..8 {
            if app.state_name() != "help" {
                break;
            }
            app.push_input(PenSample {
                x: 100,
                y: 100,
                pressure: 1200,
                tool: Tool::Pen,
                touching: true,
            });
            app.step(&host);
            app.pen_up();
            app.step(&host);
        }
        assert_eq!(app.state_name(), "listening", "the diary should be listening");

        for i in 0..40 {
            app.push_input(PenSample {
                x: 400 + i * 8,
                y: 900 + i * 4,
                pressure: 1200,
                tool: Tool::Pen,
                touching: true,
            });
        }
        app.pen_up();
        app.step(&host);
        assert_eq!(app.state_name(), "listening");

        // Let the idle timer expire: the paused page becomes a turn.
        std::thread::sleep(IDLE_COMMIT + Duration::from_millis(150));
        app.step(&host);
        let after_commit = app.state_name();
        assert!(
            after_commit == "drinking" || after_commit == "thinking" || after_commit == "replying",
            "the paused stroke was not committed into a turn (state: {after_commit})"
        );

        // With no oracle there is nothing to answer, so the writing must be
        // kept rather than drunk — a writer should not lose words that nothing
        // was ever going to reply to.
        if after_commit == "replying" {
            assert!(inked(&app) > 100, "the committed ink was drunk anyway");
        }
    }
}
