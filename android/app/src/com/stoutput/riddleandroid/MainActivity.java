package com.stoutput.riddleandroid;

import android.app.Activity;
import android.content.Intent;
import android.graphics.Color;
import android.os.Bundle;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import java.io.IOException;

/**
 * The diary itself: a full-bleed page with the smallest possible chrome.
 *
 * <p>Upstream drew its own toolbar into the framebuffer, because it owned the
 * whole panel. Here the platform's widgets do that job — they are clearer to
 * use, and they adapt to system dark mode, touch exploration and text scaling
 * for free.
 */
public class MainActivity extends Activity {

    private static final int REQUEST_SETTINGS = 1;

    private DiaryView diary;
    private Button eraserButton;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Configure logging before anything else can fail, so the reason is
        // recoverable even on a device whose logcat hides app tags.
        Logs.useFileSink(new java.io.File(getFilesDir(), "riddle.log"));

        // Settings must be on disk before the engine reads them.
        try {
            Config.write(this);
        } catch (IOException e) {
            Logs.e("could not write settings", e);
        }

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.WHITE);

        diary = new DiaryView(this);
        diary.setOnFiveFingerTap(new Runnable() {
            @Override
            public void run() {
                openSettings();
            }
        });
        root.addView(diary, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));

        root.addView(buildToolbar(), toolbarParams());

        setContentView(root);

        if (!DiaryView.nativeReady) {
            Logs.e("libriddle did not load");
            showFatal(getString(R.string.native_missing));
            return;
        }

        DiaryView.nativeCreate(
                Config.configFile(this).getAbsolutePath(),
                diary,
                getFilesDir().getAbsolutePath(),
                getCacheDir().getAbsolutePath());

        // The view exists but has not been laid out yet; the engine's opening
        // page is drawn now so the first onDraw has something to show.
        DiaryView.nativeStart();
    }

    /**
     * Two buttons, bottom-right, floating over the page. Kept deliberately
     * understated: the diary is a page of paper, not a form.
     */
    private View buildToolbar() {
        LinearLayout bar = new LinearLayout(this);
        bar.setOrientation(LinearLayout.HORIZONTAL);
        bar.setGravity(Gravity.END);

        eraserButton = new Button(this);
        eraserButton.setText(R.string.eraser);
        eraserButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                boolean on = !diary.isEraserMode();
                diary.setEraserMode(on);
                eraserButton.setText(on ? R.string.eraser_on : R.string.eraser);
            }
        });

        Button settings = new Button(this);
        settings.setText(R.string.settings);
        settings.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                openSettings();
            }
        });

        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.setMarginStart(dp(4));
        bar.addView(eraserButton, lp);
        bar.addView(settings, lp);
        return bar;
    }

    private FrameLayout.LayoutParams toolbarParams() {
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.gravity = Gravity.BOTTOM | Gravity.END;
        lp.setMargins(dp(8), dp(8), dp(8), dp(8));
        return lp;
    }

    private int dp(int value) {
        return (int) TypedValue.applyDimension(
                TypedValue.COMPLEX_UNIT_DIP, value, getResources().getDisplayMetrics());
    }

    private void openSettings() {
        startActivityForResult(new Intent(this, SettingsActivity.class), REQUEST_SETTINGS);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQUEST_SETTINGS && resultCode == RESULT_OK) {
            // The engine reads its configuration once, at startup, so applying
            // new settings means opening the diary again. Recreating the
            // activity also re-reads the settings file from scratch.
            Toast.makeText(this, R.string.settings_applied, Toast.LENGTH_SHORT).show();
            recreate();
        }
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        if (DiaryView.nativeReady) {
            DiaryView.nativeDestroy();
        }
    }

    /**
     * Replace the page with an explanation when the engine cannot run at all.
     *
     * <p>Nothing else in this activity can draw, so this is the only signal the
     * writer gets; it must not itself depend on the engine or on resources.
     */
    private void showFatal(String message) {
        TextView tv = new TextView(this);
        tv.setText(message);
        tv.setTextSize(18f);
        tv.setPadding(dp(32), dp(32), dp(32), dp(32));
        tv.setBackgroundColor(Color.WHITE);
        tv.setTextColor(Color.BLACK);
        tv.setGravity(Gravity.CENTER);
        ((ViewGroup) diary.getParent()).addView(tv, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
    }
}
