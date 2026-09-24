package com.stoutput.riddleandroid;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.view.MotionEvent;
import android.view.View;

import java.nio.ShortBuffer;

/**
 * The diary page.
 *
 * <p>This view is deliberately thin: it owns the {@link Bitmap} the page is
 * displayed from, scales it into whatever space the activity gives it, and
 * translates {@link MotionEvent}s into the engine's pen-sample format. All the
 * diary's behaviour — ink, the dissolve, the handwriting synthesis, the oracle
 * turn — lives in Rust. See {@code riddle-core/src/app.rs}.
 *
 * <p>The page is a fixed 1620x2160 and letterboxed into the view, so the layout
 * is identical on every screen and a stroke keeps its proportions.
 *
 * <p>Pixels flow one way: the engine draws into a buffer it owns and the view
 * copies it out each frame ({@link #nativeCopyPixels}). Drawing straight into
 * the bitmap from native code would need either the NDK's {@code
 * libjnigraphics} or {@code Bitmap.mBuffer}, and the latter is not visible to
 * JNI on current Android — see {@code Surface::pixels} in the Rust source.
 */
public final class DiaryView extends View {

    /** The diary's native page geometry, inherited from the reMarkable build. */
    private static final int PAGE_W = 1620;
    private static final int PAGE_H = 2160;

    /** Engine action codes (must match riddle-core/src/lib.rs). */
    private static final int ACTION_MOVE_OR_DOWN = 0;
    private static final int ACTION_UP = 1;

    /** Engine tool codes. */
    private static final int TOOL_PEN = 0;
    private static final int TOOL_ERASER = 1;

    /**
     * Pressure in the engine's 0..4096 scale for input that reports none.
     *
     * <p>A finger or a mouse reports exactly 1.0, which would be maximum
     * pressure and draw a fat five-pixel stroke; this yields the same two-pixel
     * nib as a light pen stroke, so the diary is usable — and testable — on a
     * device with no stylus at all.
     */
    private static final int TOUCH_PRESSURE = 1200;

    /**
     * How long after a stylus event a finger counts as a resting palm. A palm
     * lands while the pen is in use; fingers alone still draw.
     */
    private static final long PALM_GUARD_MS = 1500;

    private final Paint paint = new Paint(Paint.FILTER_BITMAP_FLAG | Paint.DITHER_FLAG);
    private final Bitmap page;
    /** RGB565 page pixels, shared with the engine as a short[]. */
    private final short[] pixels;
    private final ShortBuffer pixelBuffer;

    private float scale = 1f;
    private float offsetX = 0f;
    private float offsetY = 0f;

    /** True once the bitmap holds a frame the engine produced. */
    private boolean frameValid = false;

    /** Stylus seen recently: ignore finger input as a likely palm. */
    private long lastStylusMs = 0L;

    /** Set by the toolbar: route all drawing through the eraser. */
    private boolean eraserMode = false;

    /** Five-finger tap detection (an Android-friendly way into settings). */
    private boolean fiveFingerGesture = false;

    /** The most pointers seen in the current gesture. */
    private int maxPointersThisGesture = 0;

    private Runnable onFiveFingerTap;

    public DiaryView(Context context) {
        super(context);
        setFocusable(true);
        setFocusableInTouchMode(true);

        // RGB_565 is not a memory compromise: the engine's drawing code has
        // always produced RGB565 (it drove the reMarkable panel in that
        // format), so this keeps the ported raster arithmetic byte-for-byte
        // identical — and Android's 565 row is 2 bytes per pixel, the stride
        // the engine assumes.
        page = Bitmap.createBitmap(PAGE_W, PAGE_H, Bitmap.Config.RGB_565);
        pixels = new short[PAGE_W * PAGE_H];
        // Unbuffered: the array IS the bitmap's source, so there is no second
        // copy to keep in step with it.
        pixelBuffer = ShortBuffer.wrap(pixels);
    }

    public void setOnFiveFingerTap(Runnable r) {
        this.onFiveFingerTap = r;
    }

    public void setEraserMode(boolean on) {
        this.eraserMode = on;
    }

    public boolean isEraserMode() {
        return eraserMode;
    }

    @Override
    protected void onSizeChanged(int w, int h, int oldw, int oldh) {
        super.onSizeChanged(w, h, oldw, oldh);
        if (w > 0 && h > 0) {
            // Letterbox the page into the view.
            scale = Math.min(w / (float) PAGE_W, h / (float) PAGE_H);
            offsetX = (w - PAGE_W * scale) / 2f;
            offsetY = (h - PAGE_H * scale) / 2f;
        }
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);

        // Pull whatever the engine has finished. A redraw with nothing new is
        // expected — the engine invalidates on its own schedule — so this is
        // the cheapest place to ask.
        if (DiaryView.nativeReady && nativeCopyPixels(pixels)) {
            pixelBuffer.rewind();
            page.copyPixelsFromBuffer(pixelBuffer);
            frameValid = true;
        }

        if (frameValid) {
            canvas.drawBitmap(page, offsetX, offsetY, paint);
        }
    }

    /** Called from the Rust engine thread via JNI. */
    @SuppressWarnings("unused")
    private void onRiddleOpenSettings() {
        post(new Runnable() {
            @Override
            public void run() {
                if (onFiveFingerTap != null) {
                    onFiveFingerTap.run();
                }
            }
        });
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        final int action = event.getActionMasked();

        // --- five-finger tap: the tablet's exit gesture, reused here as a
        // shortcut into settings ---
        switch (action) {
            case MotionEvent.ACTION_DOWN:
                maxPointersThisGesture = 1;
                fiveFingerGesture = false;
                break;
            case MotionEvent.ACTION_POINTER_DOWN:
                maxPointersThisGesture = Math.max(maxPointersThisGesture, event.getPointerCount());
                if (event.getPointerCount() >= 5) {
                    fiveFingerGesture = true;
                }
                break;
            case MotionEvent.ACTION_UP:
                if (fiveFingerGesture && onFiveFingerTap != null) {
                    onFiveFingerTap.run();
                }
                maxPointersThisGesture = 0;
                fiveFingerGesture = false;
                break;
            default:
                break;
        }

        // The engine draws with one pen; extra fingers are ignored.
        final int index = 0;
        final int toolType = event.getToolType(index);
        if (toolType == MotionEvent.TOOL_TYPE_STYLUS
                || toolType == MotionEvent.TOOL_TYPE_ERASER) {
            lastStylusMs = event.getEventTime();
        }

        switch (action) {
            case MotionEvent.ACTION_DOWN:
                return true;

            case MotionEvent.ACTION_POINTER_DOWN:
                // A second finger arriving mid-stroke means a palm or a
                // multi-touch gesture, not writing: close the stroke cleanly.
                if (event.getPointerCount() > 1) {
                    send(ACTION_UP, 0, 0, 0, TOOL_PEN);
                }
                return true;

            case MotionEvent.ACTION_MOVE:
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_CANCEL: {
                if (fiveFingerGesture) {
                    // A multi-finger gesture is in progress: never ink it.
                    if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
                        send(ACTION_UP, 0, 0, 0, TOOL_PEN);
                    }
                    return true;
                }

                final float vx = event.getX(index);
                final float vy = event.getY(index);
                final int px = Math.round((vx - offsetX) / scale);
                final int py = Math.round((vy - offsetY) / scale);
                if (px < 0 || py < 0 || px >= PAGE_W || py >= PAGE_H) {
                    if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
                        send(ACTION_UP, 0, 0, 0, TOOL_PEN);
                    }
                    return true;
                }

                // Palm rejection: a finger right after the pen is a hand
                // resting on the page.
                final boolean finger = toolType == MotionEvent.TOOL_TYPE_FINGER
                        || toolType == MotionEvent.TOOL_TYPE_MOUSE;
                if (finger && lastStylusMs != 0L
                        && event.getEventTime() - lastStylusMs < PALM_GUARD_MS) {
                    return true;
                }

                final int tool;
                final int pressure;
                if (eraserMode || toolType == MotionEvent.TOOL_TYPE_ERASER) {
                    tool = TOOL_ERASER;
                    pressure = TOUCH_PRESSURE;
                } else if (finger) {
                    // No pressure signal: use a fixed, light nib.
                    tool = TOOL_PEN;
                    pressure = TOUCH_PRESSURE;
                } else {
                    tool = TOOL_PEN;
                    // Android reports stylus pressure normalized 0..1.
                    pressure = Math.round(event.getPressure(index) * 4096f);
                }

                if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
                    send(ACTION_UP, px, py, 0, TOOL_PEN);
                } else {
                    send(ACTION_MOVE_OR_DOWN, px, py, pressure, tool);
                }
                return true;
            }

            default:
                return super.onTouchEvent(event);
        }
    }

    private void send(int action, int x, int y, int pressure, int tool) {
        if (DiaryView.nativeReady) {
            nativeInput(action, x, y, pressure, tool);
        }
    }

    // ------------------------------------------------------------------
    // Native engine (riddle-core). Loaded defensively so a device without a
    // matching ABI reports a message instead of dying in a static initializer.
    // ------------------------------------------------------------------

    static boolean nativeReady = false;

    static {
        try {
            System.loadLibrary("riddle");
            nativeReady = true;
        } catch (UnsatisfiedLinkError e) {
            nativeReady = false;
            Logs.e("could not load libriddle: " + e.getMessage());
        }
    }

    static native void nativeCreate(
            String configPath, DiaryView view, String dataDir, String cacheDir);

    /** Tell the engine the page can be displayed; it draws the opening sheet. */
    static native void nativeStart();

    /**
     * Copy the engine's finished frame into {@code out}, if one is waiting.
     *
     * @return true when {@code out} was filled.
     */
    private native boolean nativeCopyPixels(short[] out);

    static native void nativeInput(int action, int x, int y, int pressure, int tool);

    static native void nativeForget();

    static native void nativeDestroy();

    static native String nativeOracleTest(String pngPath);
}
