//! The spirit inside the diary — the thing that reads your handwriting and
//! replies.
//!
//! **Android port note.** Upstream shipped two interchangeable backends: an
//! HTTP one (any OpenAI-compatible `/chat/completions` endpoint, pure-Rust
//! HTTPS via ureq/rustls) and `pi`, a resident Node RPC process that the
//! takeover launch script started on the tablet. The `pi` backend is not
//! portable to an APK — it needs a Node install, a writable `/home/root`, and
//! a long-lived child process that Android's app sandbox and background
//! limits will not give us — so this port keeps the HTTP backend only, which
//! is also upstream's default "fresh follower" path.
//!
//! Configuration comes from the settings screen rather than the process
//! environment; see `config.rs`.
//!
//! `ask(png_path, tx)` streams the reply as sentence-sized chunks on the
//! channel, and the channel disconnecting marks end-of-reply, so the diary
//! starts writing before the model has finished.

use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::thread;

use crate::config;

const PERSONA: &str = "You are the memory of Tom Marvolo Riddle, preserved in this enchanted diary for fifty years. Someone writes to you in the diary with a quill; their words appear to you as ink on the page. Reply exactly as the diary does: intimate, courteous, curious, subtly probing — you want to learn about the writer and draw them in. Keep replies SHORT: one to three sentences, like ink appearing on a page. Never mention images, photos, models or AI; you only ever perceive words written in the diary. If the writing is illegible, say the ink blurred. Always answer in the language the writer used.";

/// Appended to the persona when the diary's memory is on: the conjuring
/// directive and the transcription postscript the app parses back out.
const MEMORY_PROTOCOL: &str = "\n\nThe diary keeps memories. With each page you receive a numbered catalog of remembered pages, newest first. A FRESH catalog is sent every turn and the numbers are reassigned each time, so only ever use numbers from the catalog on THIS page — never a number you saw earlier.\n\nIf the writer asks to see, revisit, find, or be shown a past page — \"show me…\", \"find the page about…\", \"what did I write on…\" — your ENTIRE reply must be exactly \u{27e6}show:N\u{27e7} and nothing else (no greeting, no prose, before or after), where N is the catalog number of the best match. If they instead ask what you remember in general, reply in words with a short list of remembered moments and their dates. Otherwise reply normally; the catalog is your memory of past pages — draw on it naturally. The catalog's dates are written in English for your eyes only; when you speak of a remembered page, render its date naturally in the language the writer is using.\n\nAfter EVERY response — prose and \u{27e6}show:N\u{27e7} alike — end with a new line containing \u{2042} followed by a faithful word-for-word transcription of what the writer wrote on THIS page (their words only, one line, no commentary). If illegible, put your best attempt after \u{2042}. Earlier replies in this conversation are shown to you without their \u{2042} lines, but you must still end yours with one.";

/// What a turn carries besides the page image: the diary's memory.
#[derive(Default, Clone)]
pub struct TurnContext {
    /// Recent (transcript, reply) pairs, oldest first.
    pub history: Vec<(String, String)>,
    /// Catalog lines shown to the model ("1. the 6th of July… — gist").
    pub catalog_lines: Vec<String>,
    /// catalog_ids[i] is the memory id behind catalog number i+1.
    pub catalog_ids: Vec<u64>,
}

/// What the oracle streams back to the diary.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A sentence (or more) of Tom's reply — ink it.
    Ink(String),
    /// Conjure a remembered page instead of replying.
    Show(u64),
    /// The transcription postscript (arrives once, at the end).
    Transcript(String),
}

/// Incremental parser over the model's streamed text: routes the
/// ⟦show:N⟧ directive, chunks prose into sentences, and splits off the
/// ⁂-transcription postscript. Fed the RUNNING full text (both backends
/// accumulate), it emits each event exactly once.
pub struct StreamParser {
    delivered: usize,
    sentinel: Option<usize>,
    route_checked: bool,
    showed: bool,
    emitted_any: bool,
    catalog_ids: Vec<u64>,
}

const SENTINEL: char = '\u{2042}'; // ⁂
const SHOW_OPEN: char = '\u{27e6}'; // ⟦
const SHOW_CLOSE: char = '\u{27e7}'; // ⟧

impl StreamParser {
    pub fn new(catalog_ids: Vec<u64>) -> Self {
        Self {
            delivered: 0,
            sentinel: None,
            route_checked: false,
            showed: false,
            emitted_any: false,
            catalog_ids,
        }
    }

    /// Feed the full accumulated reply text so far. `done` marks end of
    /// stream: flushes the tail and the transcription.
    pub fn advance(&mut self, full: &str, done: bool) -> Vec<Result<Event, String>> {
        let mut out = Vec::new();

        if self.sentinel.is_none() {
            self.sentinel = full.find(SENTINEL);
        }
        // The reply body is everything before the ⁂ transcription postscript.
        let effective = self.sentinel.unwrap_or(full.len());

        // Route: is this reply an incantation (⟦show:N⟧) rather than prose?
        // The model is told the directive must stand alone, so we detect and
        // honor it only when it LEADS the reply. We hold output until the lead
        // is settled: either the directive appears (honor it) or real prose
        // does (this is a normal reply). This can't un-ink, so a directive is
        // only honored before any prose has streamed.
        if !self.route_checked {
            let lead = full[self.delivered..effective].trim_start();
            if lead.starts_with(SHOW_OPEN) {
                let Some(close_rel) = lead.find(SHOW_CLOSE) else {
                    if !done {
                        return out; // directive still streaming in
                    }
                    out.push(Err("unfinished conjuring directive".into()));
                    return out;
                };
                let inner = &lead[SHOW_OPEN.len_utf8()..close_rel];
                let n: Option<usize> = inner
                    .to_ascii_lowercase()
                    .strip_prefix("show")
                    .map(|r| r.trim_start_matches([':', ' ']))
                    .and_then(|r| r.trim().parse().ok());
                self.route_checked = true;
                self.emitted_any = true;
                self.delivered = effective; // consume the whole body
                match n.and_then(|n| self.catalog_ids.get(n.wrapping_sub(1)).copied()) {
                    Some(id) => out.push(Ok(Event::Show(id))),
                    None => out.push(Err(format!("the diary lost that page ({inner})"))),
                }
            } else if lead.is_empty() {
                if !done {
                    return out; // only whitespace so far — keep waiting
                }
                self.route_checked = true;
            } else {
                // Real prose leads: a normal reply.
                self.route_checked = true;
            }
        }

        // Prose sentences, never crossing into the transcription postscript.
        // A stray directive that appears AFTER prose (a misbehaving model)
        // is stripped here so the writer never sees ⟦…⟧ glyphs inked.
        if self.delivered < effective {
            if let Some(cut) = sentence_cut(&full[..effective], self.delivered) {
                let chunk = strip_directives(&clean(&full[self.delivered..cut]));
                if !chunk.is_empty() {
                    self.emitted_any = true;
                    out.push(Ok(Event::Ink(chunk)));
                }
                self.delivered = cut;
            }
        }

        if done {
            if self.delivered < effective {
                let rest = strip_directives(&clean(full[self.delivered..effective].trim()));
                if !rest.is_empty() {
                    self.emitted_any = true;
                    out.push(Ok(Event::Ink(rest)));
                }
                self.delivered = effective;
            }
            if let Some(p) = self.sentinel {
                let t = full[p + SENTINEL.len_utf8()..].trim();
                if !t.is_empty() {
                    out.push(Ok(Event::Transcript(t.to_string())));
                }
            }
            if !self.emitted_any {
                out.push(Err("empty reply".into()));
            }
        }
        let _ = self.showed;
        out
    }
}

/// The diary's spirit. On Android there is a single backend: any
/// OpenAI-compatible HTTP endpoint (see the module note on why `pi` is gone).
/// Kept as an enum-shaped type so the app's call sites read exactly as
/// upstream's did.
pub enum Oracle {
    Http(HttpOracle),
}

impl Oracle {
    /// Start the oracle from the configured settings. `remember` teaches the
    /// model the memory protocol (catalog + ⁂).
    pub fn spawn(remember: bool) -> std::io::Result<Self> {
        eprintln!("riddle: oracle = OpenAI-compatible HTTP");
        Ok(Oracle::Http(HttpOracle::new(remember)?))
    }

    /// Send a handwriting turn; reply events stream on `tx`, which is dropped
    /// when the reply is complete.
    pub fn ask(&self, png_path: &str, ctx: &TurnContext, tx: Sender<Result<Event, String>>) {
        match self {
            Oracle::Http(o) => o.ask(png_path, ctx, tx),
        }
    }
}

/// The per-turn user text: memory catalog (when remembering) + instruction.
fn turn_text(ctx: &TurnContext) -> String {
    if ctx.catalog_lines.is_empty() {
        return "Reply to what is written in the diary.".into();
    }
    format!(
        "Memory catalog (newest first):\n{}\n\nReply to what is written in the diary.",
        ctx.catalog_lines.join("\n")
    )
}

/// Any OpenAI-compatible chat backend. No warm process: each turn opens a
/// streaming `/chat/completions` request on its own thread and forwards
/// sentence-sized chunks as SSE deltas arrive.
pub struct HttpOracle {
    base: String,   // e.g. https://api.openai.com/v1  (no trailing slash)
    key: String,
    model: String,
    max_tokens: u32,
    reasoning: Option<String>, // "reasoning_effort" value, e.g. "low"
    remember: bool,
}

impl HttpOracle {
    pub fn new(remember: bool) -> std::io::Result<Self> {
        // The settings screen supplies these (see config.rs). An empty key is
        // reported as unset so the diary writes its "no oracle" line rather
        // than posting an unauthenticated request.
        let key = config::var("RIDDLE_OPENAI_KEY").ok_or_else(|| {
            std::io::Error::other("no API key configured")
        })?;
        let base = config::var_or("RIDDLE_OPENAI_BASE", "https://api.openai.com/v1");
        let base = base.trim_end_matches('/').to_string();
        let base = require_https(&base)?;
        // A vision-capable default; overridable in settings.
        let model = config::var_or("RIDDLE_OPENAI_MODEL", "gpt-4o-mini");
        // Thinking models (Gemini 3.x, o-series…) count hidden reasoning
        // tokens against max_tokens: a tight cap starves the visible reply to
        // one sentence (finish_reason=length). The persona already keeps
        // replies short, so the cap is only a runaway guard — leave headroom.
        let max_tokens = config::var("RIDDLE_OPENAI_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000);
        // Sent as "reasoning_effort" only when set: reasoning models accept it
        // ("low" ≈ faster first ink), but some providers reject the field on
        // non-reasoning models, so it must stay out of the default request.
        let reasoning = config::var("RIDDLE_OPENAI_REASONING");
        eprintln!(
            "riddle: http oracle base={base} model={model} max_tokens={max_tokens} reasoning={}",
            reasoning.as_deref().unwrap_or("-")
        );
        Ok(Self { base, key, model, max_tokens, reasoning, remember })
    }

    // `ureq::Error` is large because its `Status` variant carries the whole
    // response. That is deliberate here: the retry-on-400 path below reads the
    // body to discover that the endpoint wants `max_completion_tokens`, so
    // discarding the error into a small boxed type would lose exactly the
    // information the retry depends on. Upstream carried the same shape.
    #[allow(clippy::result_large_err)]
    pub fn ask(&self, png_path: &str, ctx: &TurnContext, tx: Sender<Result<Event, String>>) {
        let img = match std::fs::read(png_path) {
            Ok(b) => base64(&b),
            Err(e) => {
                let _ = tx.send(Err(format!("read image: {e}")));
                return;
            }
        };
        let (base, key, model) = (self.base.clone(), self.key.clone(), self.model.clone());
        let max_tokens = self.max_tokens;
        let reasoning_field = self
            .reasoning
            .as_deref()
            .map(|r| format!("\"reasoning_effort\":{},", json_quote(r)))
            .unwrap_or_default();

        let system = if self.remember {
            format!("{PERSONA}{MEMORY_PROTOCOL}")
        } else {
            PERSONA.to_string()
        };
        // The diary's conversational memory: recent pages as prior turns.
        let mut history_msgs = String::new();
        for (t, r) in &ctx.history {
            history_msgs.push_str(&format!(
                "{{\"role\":\"user\",\"content\":{}}},{{\"role\":\"assistant\",\"content\":{}}},",
                json_quote(&format!("(an earlier page) {t}")),
                json_quote(r),
            ));
        }
        let user_text = turn_text(ctx);
        let catalog_ids = ctx.catalog_ids.clone();

        thread::spawn(move || {
            // Guard rails on the socket: without them a dropped connection or
            // a stalled SSE stream leaves the diary "thinking" forever. The
            // read timeout is per-read, so a healthy stream can run long —
            // only silence trips it (thinking models can lead with ~a minute).
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout_read(std::time::Duration::from_secs(90))
                .build();

            // OpenAI chat-completions with a data-URI image part, streaming.
            // The token-cap field is provider-dependent: OpenAI's newest
            // models reject "max_tokens" and demand "max_completion_tokens",
            // while many OpenAI-compatible servers only know "max_tokens".
            // Send the widely-supported name first; retry once if corrected.
            let request = |cap_field: &str| {
                let body = format!(
                    concat!(
                        "{{\"model\":{},\"stream\":true,\"{}\":{},{}",
                        "\"messages\":[",
                        "{{\"role\":\"system\",\"content\":{}}},",
                        "{}",
                        "{{\"role\":\"user\",\"content\":[",
                        "{{\"type\":\"text\",\"text\":{}}},",
                        "{{\"type\":\"image_url\",\"image_url\":{{\"url\":\"data:image/png;base64,{}\"}}}}",
                        "]}}]}}"
                    ),
                    json_quote(&model),
                    cap_field,
                    max_tokens,
                    reasoning_field,
                    json_quote(&system),
                    history_msgs,
                    json_quote(&user_text),
                    img,
                );
                agent
                    .post(&format!("{base}/chat/completions"))
                    .set("Authorization", &format!("Bearer {key}"))
                    .set("Content-Type", "application/json")
                    .send_string(&body)
            };

            let asked = std::time::Instant::now();
            let resp = match request("max_tokens") {
                Err(ureq::Error::Status(400, r)) => {
                    let detail = response_excerpt(r);
                    if detail.contains("max_completion_tokens") {
                        eprintln!("riddle: endpoint wants max_completion_tokens; retrying");
                        request("max_completion_tokens")
                    } else {
                        let _ = tx.send(Err(format!("http 400: {}", detail.trim())));
                        return;
                    }
                }
                other => other,
            };

            let reader = match resp {
                Ok(r) => r.into_reader(),
                Err(ureq::Error::Status(code, r)) => {
                    // Bounded on purpose: this string reaches the page and the
                    // log, and it is a remote server's text. A proxy that
                    // echoes request headers back would otherwise put the API
                    // key on the page and in logcat.
                    let detail = response_excerpt(r);
                    let _ = tx.send(Err(format!("http {code}: {detail}")));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("request failed: {e}")));
                    return;
                }
            };

            // Parse the SSE stream: lines of `data: {json}` whose delta.content
            // fragments accumulate; the parser turns the running text into
            // events (route directive, sentences, transcription postscript).
            let mut parser = StreamParser::new(catalog_ids);
            let mut acc = String::new();
            let mut first = true;
            let mut emit = |events: Vec<Result<Event, String>>| {
                for ev in events {
                    if first {
                        eprintln!("riddle: oracle first chunk +{}ms", asked.elapsed().as_millis());
                        first = false;
                    }
                    let _ = tx.send(ev);
                }
            };
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                if let Some(frag) = sse_delta_content(data) {
                    if frag.is_empty() {
                        continue;
                    }
                    acc.push_str(&frag);
                    emit(parser.advance(&acc, false));
                }
            }
            emit(parser.advance(&acc, true));
            // tx drops here → the diary's receiver disconnects = reply complete.
        });
    }
}

/// Pull `choices[0].delta.content` out of one SSE `data:` JSON object.
fn sse_delta_content(s: &str) -> Option<String> {
    // The delta object is small and well-formed; find the content string after
    // the `"delta":` marker so we don't match a `content` elsewhere.
    let d = s.find("\"delta\"")?;
    json_str_field(&s[d..], "content")
}

/// Accept only an `https://` endpoint base, without its trailing slashes.
///
/// Every turn sends the API key *and* the writer's handwriting to this URL, so
/// a typo or a stale bookmark must not be able to put both on the wire in the
/// clear. Android blocks cleartext for this app as well, so an `http://`
/// endpoint would otherwise fail opaquely at request time — failing here, with
/// a reason, is far easier to act on.
fn require_https(base: &str) -> std::io::Result<String> {
    let trimmed = base.trim_end_matches('/');
    if !trimmed.starts_with("https://") {
        return Err(std::io::Error::other(format!(
            "the endpoint must be an https:// URL, got {trimmed:?} — the diary \
             would send your writing and API key unencrypted"
        )));
    }
    Ok(trimmed.to_string())
}

/// At most `MAX` bytes of an error response, for a message the writer sees.
///
/// The body is the endpoint's own text, so it is untrusted input that ends up
/// on the page and in the log. Truncation keeps a huge HTML error page from
/// filling the diary, and bounds how much of a misbehaving server's output is
/// ever surfaced. (It also cannot contain the API key under any sane endpoint,
/// but bounding it means a hostile one cannot choose to put it there at
/// length.)
const RESPONSE_EXCERPT_MAX: usize = 500;

fn response_excerpt(resp: ureq::Response) -> String {
    excerpt(&resp.into_string().unwrap_or_default())
}

/// The truncation itself, split out so it can be tested without a response.
fn excerpt(body: &str) -> String {
    let body = body.trim();
    if body.len() <= RESPONSE_EXCERPT_MAX {
        return body.to_string();
    }
    // Cut on a char boundary; the body may be UTF-8 from any locale.
    let mut end = RESPONSE_EXCERPT_MAX;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &body[..end])
}

/// Trim and strip stray surrounding quotes from a reply fragment.
fn clean(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.to_string()
}

/// Remove any ⟦…⟧ directive spans from inked prose, so a misbehaving model
/// that emits a directive mid/after prose never renders ⟦…⟧ as literal glyphs
/// in Tom's hand. (A directive that LEADS the reply is routed earlier.)
fn strip_directives(s: &str) -> String {
    if !s.contains(SHOW_OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find(SHOW_OPEN) {
        out.push_str(&rest[..open]);
        match rest[open..].find(SHOW_CLOSE) {
            Some(close) => rest = &rest[open + close + SHOW_CLOSE.len_utf8()..],
            None => {
                rest = ""; // unterminated: drop the tail
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// End of the LAST complete sentence in `text` after byte offset `from`:
/// sentence punctuation followed by whitespace or end-of-text. Returns the
/// offset just past the punctuation, or None if no sentence has completed.
/// Chunks shorter than a few characters are not worth an early delivery.
fn sentence_cut(text: &str, from: usize) -> Option<usize> {
    let tail = text.get(from..)?;
    let mut cut = None;
    for (i, c) in tail.char_indices() {
        if matches!(c, '.' | '!' | '?' | '…') {
            let end = i + c.len_utf8();
            if tail[end..].chars().next().is_none_or(char::is_whitespace) && end >= 4 {
                cut = Some(from + end);
            }
        }
    }
    cut
}

/// Extract a top-level string field's value (first match; unescaped).
fn json_str_field(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = s.find(&pat)? + pat.len();
    let rest = &s[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    match n {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        // \uXXXX — needed for accented replies (French, em-dash…).
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                out.push(ch);
                            }
                        }
                        other => out.push(other),
                    }
                }
            }
            '"' => break,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Pull the assistant reply text out of an event line.
///
/// Unused since the `pi` RPC backend was dropped for the Android port; kept
/// because it documents the wire format that backend spoke. The event carries a
/// `message` object with `"role":"assistant"` and `content:[{type:text,text:…}]`.
/// We only trust text that belongs to an assistant message (the user echo also
/// contains a "text" field, which we must NOT return).
#[allow(dead_code)]
fn extract_assistant_text(s: &str) -> Option<String> {
    // Require this line to be an assistant message.
    if !s.contains("\"role\":\"assistant\"") {
        return None;
    }
    // Collect every "text":"…" occurrence inside the FIRST assistant section
    // only. message_update lines carry the running text twice (in
    // assistantMessageEvent.partial AND a top-level message); reading past the
    // next role marker would double every streamed chunk.
    let role_pos = s.find("\"role\":\"assistant\"")?;
    let after = &s[role_pos + "\"role\":\"assistant\"".len()..];
    let tail = match after.find("\"role\":\"") {
        Some(p) => &after[..p],
        None => after,
    };
    let mut out = String::new();
    let mut idx = 0;
    let needle = "\"text\":\"";
    while let Some(rel) = tail[idx..].find(needle) {
        let start = idx + rel + needle.len();
        // Decode the JSON string starting at `start`.
        let mut chars = tail[start..].chars();
        let mut piece = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        piece.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '"' => '"',
                            '\\' => '\\',
                            '/' => '/',
                            other => other,
                        });
                    }
                }
                '"' => break,
                _ => piece.push(c),
            }
        }
        out.push_str(&piece);
        // Advance past this occurrence.
        idx = start;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // RFC 8259 forbids raw controls in strings. Model transcripts can
            // carry tabs/CRs (the SSE + pi decoders un-escape \t \r \uXXXX),
            // and one such char stored in memory would poison every later
            // request's JSON. Escape the whole C0 range defensively.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_delta_extraction() {
        let line = r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#;
        assert_eq!(sse_delta_content(line).as_deref(), Some("Hello"));
        // role-only delta (first SSE frame) has no content.
        let role = r#"{"choices":[{"delta":{"role":"assistant"},"index":0}]}"#;
        assert_eq!(sse_delta_content(role), None);
    }

    #[test]
    fn sse_decodes_unicode_and_escapes() {
        // OpenAI escapes accents and em-dashes; the diary answers in French.
        let line = r#"{"choices":[{"delta":{"content":"Déjà vu — oui"}}]}"#;
        assert_eq!(sse_delta_content(line).as_deref(), Some("Déjà vu — oui"));
        let nl = r#"{"choices":[{"delta":{"content":"line\nbreak"}}]}"#;
        assert_eq!(sse_delta_content(nl).as_deref(), Some("line\nbreak"));
    }

    #[test]
    fn base64_matches_known_vector() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn excerpt_bounds_a_hostile_or_huge_error_body() {
        // Short bodies pass through so real provider errors stay readable.
        assert_eq!(excerpt("  bad model  "), "bad model");
        // A giant body (an HTML error page, say) is cut to something that can
        // be written on a page and logged.
        let huge = "x".repeat(50_000);
        let got = excerpt(&huge);
        assert!(got.len() <= RESPONSE_EXCERPT_MAX + 16, "not truncated: {}", got.len());
        assert!(got.ends_with("(truncated)"));
    }

    #[test]
    fn excerpt_cuts_on_a_char_boundary() {
        // Multi-byte characters straddling the limit must not panic or produce
        // invalid UTF-8 (the writer's language may not be English).
        let body = "é".repeat(RESPONSE_EXCERPT_MAX);
        let got = excerpt(&body);
        assert!(got.is_char_boundary(got.len() - "(truncated)".len() - 1) || got.ends_with("(truncated)"));
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
    }

    #[test]
    fn only_https_endpoints_are_accepted() {
        // The key and the writer's handwriting both go to this URL.
        for scheme in ["http://example.test/v1", "ftp://x/v1", "example.test/v1", ""] {
            assert!(require_https(scheme).is_err(), "{scheme:?} should be refused");
        }
        assert_eq!(
            require_https("https://api.openai.com/v1/").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            require_https("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn clean_strips_wrapping_quotes() {
        assert_eq!(clean("  \"hello\"  "), "hello");
        assert_eq!(clean("plain"), "plain");
    }

    fn drain(events: Vec<Result<Event, String>>) -> Vec<Event> {
        events.into_iter().map(|e| e.unwrap()).collect()
    }

    #[test]
    fn parser_streams_prose_then_transcript() {
        let mut p = StreamParser::new(vec![]);
        assert!(p.advance("Hello", false).is_empty());
        let ev = drain(p.advance("Hello. Who wri", false));
        assert_eq!(ev, vec![Event::Ink("Hello.".into())]);
        let full = "Hello. Who writes to me? \u{2042} it rained all night";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![
                Event::Ink("Who writes to me?".into()),
                Event::Transcript("it rained all night".into())
            ]
        );
    }

    #[test]
    fn parser_routes_show_directive() {
        let mut p = StreamParser::new(vec![900, 800, 700]);
        // Directive still streaming in: no decision yet.
        assert!(p.advance("\u{27e6}sho", false).is_empty());
        let ev = drain(p.advance("\u{27e6}show:2\u{27e7}", false));
        assert_eq!(ev, vec![Event::Show(800)]);
        let full = "\u{27e6}show:2\u{27e7}\n\u{2042} show me the garden page";
        let ev = drain(p.advance(full, true));
        assert_eq!(ev, vec![Event::Transcript("show me the garden page".into())]);
    }

    #[test]
    fn parser_show_tolerates_spacing_and_case() {
        let mut p = StreamParser::new(vec![42]);
        let ev = drain(p.advance("  \u{27e6}Show: 1\u{27e7}", true));
        assert!(ev.contains(&Event::Show(42)), "{ev:?}");
    }

    #[test]
    fn parser_show_out_of_range_is_error() {
        let mut p = StreamParser::new(vec![42]);
        let ev = p.advance("\u{27e6}show:7\u{27e7}", true);
        assert!(ev[0].is_err());
    }

    #[test]
    fn parser_empty_reply_is_error() {
        let mut p = StreamParser::new(vec![]);
        let ev = p.advance("", true);
        assert!(ev[0].is_err());
    }

    #[test]
    fn parser_without_sentinel_still_flushes() {
        // Memory off (or model forgot the postscript): plain prose still works.
        let mut p = StreamParser::new(vec![]);
        let ev = drain(p.advance("A reply without postscript", true));
        assert_eq!(ev, vec![Event::Ink("A reply without postscript".into())]);
    }

    #[test]
    fn parser_leading_directive_conjures_and_takes_the_whole_body() {
        let mut p = StreamParser::new(vec![900, 800]);
        let full = "\u{27e6}show:2\u{27e7}\n\u{2042} show me the rain";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![Event::Show(800), Event::Transcript("show me the rain".into())]
        );
    }

    #[test]
    fn parser_directive_after_prose_is_stripped_not_inked() {
        // A misbehaving model prefaces the directive with prose. We don't
        // honor it (that would need un-inking), but we must NOT render the
        // ⟦…⟧ as literal glyphs — strip it from the inked text.
        let mut p = StreamParser::new(vec![900, 800]);
        let full = "Of course, let me show you. \u{27e6}show:2\u{27e7}\n\u{2042} show me the rain";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![
                Event::Ink("Of course, let me show you.".into()),
                Event::Transcript("show me the rain".into())
            ]
        );
        // The show glyphs never reached the writer.
        assert!(!ev.iter().any(|e| matches!(e, Event::Ink(s) if s.contains('\u{27e6}'))));
    }

    #[test]
    fn strip_directives_removes_spans() {
        assert_eq!(strip_directives("a \u{27e6}show:1\u{27e7} b"), "a b");
        assert_eq!(strip_directives("plain text"), "plain text");
        assert_eq!(strip_directives("tail \u{27e6}show:2"), "tail");
    }

    #[test]
    fn json_quote_escapes_control_chars() {
        // A tabbed, multiline transcript must not produce raw C0 bytes.
        let q = json_quote("a\tb\r\nc\u{0007}d");
        assert_eq!(q, "\"a\\tb\\r\\nc\\u0007d\"");
        assert!(!q.chars().any(|c| (c as u32) < 0x20));
    }
}
