package com.stoutput.riddleandroid;

import android.content.Context;
import android.content.SharedPreferences;

import java.io.BufferedWriter;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;

/**
 * The diary's settings, and the bridge that hands them to the Rust engine.
 *
 * <p>Upstream configured the diary through {@code oracle.env} plus
 * {@code RIDDLE_*} environment variables, sourced by its launch script. An APK
 * has no launch script, so the values live in {@link SharedPreferences} (edited
 * by {@link SettingsActivity}) and are exported to a {@code KEY=value} file that
 * the engine reads once at startup.
 *
 * <p>The file is written to the app's private data directory, so the API key
 * stays inside the app sandbox and is never world-readable.
 */
final class Config {

    static final String PREFS = "riddle";

    static final String KEY_API_KEY = "RIDDLE_OPENAI_KEY";
    static final String KEY_BASE = "RIDDLE_OPENAI_BASE";
    static final String KEY_MODEL = "RIDDLE_OPENAI_MODEL";
    static final String KEY_REASONING = "RIDDLE_OPENAI_REASONING";
    static final String KEY_MAX_TOKENS = "RIDDLE_OPENAI_MAX_TOKENS";
    static final String KEY_MEMORY = "RIDDLE_MEMORY";
    static final String KEY_TZ_OFFSET = "RIDDLE_TZ_OFFSET";
    static final String KEY_MEMORY_TURNS = "RIDDLE_MEMORY_TURNS";

    /** Set by the app, not the settings screen; see the note in {@link #write}. */
    static final String KEY_LOG_FILE = "RIDDLE_LOG_FILE";

    static final String DEFAULT_BASE = "https://api.openai.com/v1";
    static final String DEFAULT_MODEL = "gpt-4o-mini";
    static final String DEFAULT_MAX_TOKENS = "2000";

    private Config() {
    }

    static SharedPreferences prefs(Context ctx) {
        return ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    static String get(Context ctx, String key, String fallback) {
        String v = prefs(ctx).getString(key, fallback);
        return v == null ? fallback : v;
    }

    /** True when an API key is present, i.e. the diary has a spirit to talk to. */
    static boolean hasOracle(Context ctx) {
        String k = get(ctx, KEY_API_KEY, "");
        return !k.trim().isEmpty();
    }

    static File configFile(Context ctx) {
        return new File(ctx.getFilesDir(), "riddle.env");
    }

    /**
     * Export the settings for the engine. Called before the engine starts and
     * again whenever settings change, so the caller restarts the diary to apply
     * them.
     */
    static void write(Context ctx) throws IOException {
        File out = configFile(ctx);
        try (BufferedWriter w = new BufferedWriter(new OutputStreamWriter(
                new FileOutputStream(out), StandardCharsets.UTF_8))) {
            w.write("# Written by the diary's settings screen; read once at startup.\n");
            w.write("# Mirrors upstream's oracle.env variables one-for-one.\n");

            put(w, KEY_API_KEY, get(ctx, KEY_API_KEY, ""));

            String base = get(ctx, KEY_BASE, DEFAULT_BASE).trim();
            if (!base.isEmpty()) {
                put(w, KEY_BASE, base);
            }
            String model = get(ctx, KEY_MODEL, DEFAULT_MODEL).trim();
            if (!model.isEmpty()) {
                put(w, KEY_MODEL, model);
            }
            put(w, KEY_REASONING, get(ctx, KEY_REASONING, "").trim());

            String maxTokens = get(ctx, KEY_MAX_TOKENS, DEFAULT_MAX_TOKENS).trim();
            if (!maxTokens.isEmpty()) {
                put(w, KEY_MAX_TOKENS, maxTokens);
            }

            // "on" unless the writer turned remembering off.
            if (!get(ctx, KEY_MEMORY, "on").equalsIgnoreCase("on")) {
                put(w, KEY_MEMORY, "off");
            }

            String tz = get(ctx, KEY_TZ_OFFSET, "").trim();
            if (!tz.isEmpty()) {
                put(w, KEY_TZ_OFFSET, tz);
            }
            String turns = get(ctx, KEY_MEMORY_TURNS, "").trim();
            if (!turns.isEmpty()) {
                put(w, KEY_MEMORY_TURNS, turns);
            }

            // Not a user setting: the engine mirrors its log here so a failure
            // can be read back with `adb shell run-as <pkg> cat files/riddle.log`
            // on devices whose logcat drops app tags.
            put(w, KEY_LOG_FILE, new File(ctx.getFilesDir(), "riddle.log").getAbsolutePath());
        }
    }

    /**
     * An empty value is written as a bare {@code KEY=} line; the engine treats
     * an empty value as unset, which is what a cleared settings field means.
     */
    private static void put(BufferedWriter w, String key, String value) throws IOException {
        // A newline in a value would forge a second config line.
        String safe = value.replace("\n", " ").replace("\r", " ");
        w.write(key);
        w.write('=');
        w.write(safe);
        w.write('\n');
    }
}
