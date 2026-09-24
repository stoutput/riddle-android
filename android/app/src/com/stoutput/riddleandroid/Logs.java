package com.stoutput.riddleandroid;

import android.util.Log;

/**
 * Minimal facade over {@link Log}, so startup progress and failures can be
 * followed with a single tag: {@code adb logcat -s riddle}.
 */
final class Logs {

    static final String TAG = "riddle";

    private Logs() {
    }

    /** Set once the app's files directory is known; see {@link #mirror}. */
    private static java.io.File sink;

    static void useFileSink(java.io.File file) {
        sink = file;
    }

    /** Append a line to the file sink, if one is configured. */
    private static void mirror(String level, String message) {
        java.io.File file = sink;
        if (file == null) {
            return;
        }
        try (java.io.FileWriter w = new java.io.FileWriter(file, true)) {
            w.write("[" + level + "] " + message + "\n");
        } catch (java.io.IOException ignored) {
            // Logging must never be the reason the diary fails to open.
        }
    }

    static void i(String message) {
        Log.i(TAG, message);
        mirror("I", message);
    }

    static void e(String message) {
        Log.e(TAG, message);
        mirror("E", message);
    }

    static void e(String message, Throwable t) {
        Log.e(TAG, message, t);
        mirror("E", message + ": " + t);
    }
}
