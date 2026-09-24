//! Android configuration: the replacement for upstream's `oracle.env` and
//! `RIDDLE_*` environment variables.
//!
//! On the tablet, upstream read everything from the process environment
//! (`oracle.env` sourced by the AppLoad launch script). An APK has no such
//! launcher, so the settings screen owns these values: the activity writes the
//! key=value store to a file in its own data directory before library init,
//! and this module reads it once into a process-global table.
//!
//! Keeping the file format as `KEY=value` (one pair per line) means the store
//! is trivially inspectable and mirrors the upstream variables one-for-one —
//! anyone who knows `oracle.env` already knows what these keys mean.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

static CONFIG: OnceLock<Config> = OnceLock::new();

pub struct Config {
    values: HashMap<String, String>,
}

impl Config {
    /// Parse a `KEY=value` store. Unparsable lines and comments are skipped so
    /// one bad line can never disable the diary.
    pub fn parse(text: &str) -> Self {
        let mut values = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                values.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Self { values }
    }

    pub fn empty() -> Self {
        Self { values: HashMap::new() }
    }

    /// Look up a key, treating empty values as absent so a cleared settings
    /// field behaves like an unset variable.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }
}

/// Load the store from `path` and install it process-globally.
///
/// Called once by the JNI layer at startup. A missing or unreadable file is
/// not an error: the diary opens with no oracle configured and says so on the
/// page, exactly as upstream did with no API key.
pub fn init_from_file(path: &str) {
    let cfg = std::fs::read_to_string(Path::new(path))
        .map(|t| Config::parse(&t))
        .unwrap_or_else(|_| Config::empty());
    let _ = CONFIG.set(cfg);
}

/// The installed configuration. Panics only if called before `init_from_file`,
/// which the JNI entry point guarantees.
pub fn get() -> &'static Config {
    CONFIG.get().unwrap_or_else(|| {
        // Defensive: rather than panic inside a JNI callback (which would
        // abort the process), fall back to an empty config.
        CONFIG.get_or_init(Config::empty)
    })
}

/// Convenience: `RIDDLE_*` lookup used across the ported modules.
pub fn var(key: &str) -> Option<String> {
    get().get(key).map(str::to_string)
}

pub fn var_or(key: &str, default: &str) -> String {
    get().get_or(key, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_skips_comments_and_blank_lines() {
        let c = Config::parse(
            "# a comment\n\
             \n\
             RIDDLE_OPENAI_KEY=sk-abc\n\
             RIDDLE_OPENAI_BASE = https://example.test/v1 \n\
             malformed-line-without-equals\n",
        );
        assert_eq!(c.get("RIDDLE_OPENAI_KEY"), Some("sk-abc"));
        // Surrounding whitespace must not leak into the value.
        assert_eq!(c.get("RIDDLE_OPENAI_BASE"), Some("https://example.test/v1"));
        assert_eq!(c.get("malformed-line-without-equals"), None);
    }

    #[test]
    fn empty_value_reads_as_unset() {
        let c = Config::parse("RIDDLE_OPENAI_KEY=\n");
        assert_eq!(c.get("RIDDLE_OPENAI_KEY"), None);
        assert_eq!(c.get_or("RIDDLE_OPENAI_KEY", "fallback"), "fallback");
    }

    #[test]
    fn values_may_contain_equals_signs() {
        // Base URLs carry query strings; only the first '=' splits.
        let c = Config::parse("RIDDLE_OPENAI_BASE=https://h/v1?a=b\n");
        assert_eq!(c.get("RIDDLE_OPENAI_BASE"), Some("https://h/v1?a=b"));
    }
}
