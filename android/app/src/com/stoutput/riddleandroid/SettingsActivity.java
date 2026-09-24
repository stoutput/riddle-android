package com.stoutput.riddleandroid;

import android.app.Activity;
import android.content.SharedPreferences;
import android.graphics.Color;
import android.os.Bundle;
import android.text.InputType;
import android.util.TypedValue;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.io.IOException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Where the diary is told which spirit to ask.
 *
 * <p>Upstream configured this with {@code oracle.env} and the remagic config
 * form (a browser page served by the tablet). On Android the natural home is a
 * settings screen, which also makes the API key reachable without a terminal.
 */
public class SettingsActivity extends Activity {

    private EditText apiKey;
    private EditText baseUrl;
    private EditText model;
    private EditText reasoning;
    private EditText maxTokens;
    private EditText tzOffset;
    private CheckBox memoryOn;
    private TextView testResult;

    private final ExecutorService worker = Executors.newSingleThreadExecutor();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        SharedPreferences p = Config.prefs(this);

        LinearLayout form = new LinearLayout(this);
        form.setOrientation(LinearLayout.VERTICAL);
        form.setPadding(dp(20), dp(20), dp(20), dp(20));
        form.setBackgroundColor(Color.WHITE);

        heading(form, R.string.settings_oracle_heading);
        body(form, R.string.settings_oracle_help);

        apiKey = field(form, R.string.settings_api_key, R.string.settings_api_key_hint,
                Config.get(this, Config.KEY_API_KEY, ""));
        // Keep the key out of screenshots and shoulder-surfing, but let it be
        // revealed for editing: a wrong key is the most common failure.
        apiKey.setInputType(InputType.TYPE_CLASS_TEXT
                | InputType.TYPE_TEXT_VARIATION_PASSWORD);
        apiKey.setSingleLine(true);

        baseUrl = field(form, R.string.settings_base, 0,
                Config.get(this, Config.KEY_BASE, Config.DEFAULT_BASE));
        model = field(form, R.string.settings_model, 0,
                Config.get(this, Config.KEY_MODEL, Config.DEFAULT_MODEL));
        reasoning = field(form, R.string.settings_reasoning, R.string.settings_reasoning_hint,
                Config.get(this, Config.KEY_REASONING, ""));
        maxTokens = field(form, R.string.settings_max_tokens, 0,
                Config.get(this, Config.KEY_MAX_TOKENS, Config.DEFAULT_MAX_TOKENS));

        heading(form, R.string.settings_memory_heading);
        memoryOn = new CheckBox(this);
        memoryOn.setText(R.string.settings_memory_on);
        memoryOn.setChecked(!Config.get(this, Config.KEY_MEMORY, "on").equalsIgnoreCase("off"));
        form.addView(memoryOn);

        body(form, R.string.settings_memory_help);

        // UTC offset, for the dates the diary speaks when it conjures a page.
        // Its own row, like the other fields: sharing a row with the label
        // squeezed the input down to nothing next to the long label.
        tzOffset = field(form, R.string.settings_tz, R.string.settings_tz_hint,
                Config.get(this, Config.KEY_TZ_OFFSET, ""));
        tzOffset.setInputType(InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_SIGNED);

        LinearLayout buttons = new LinearLayout(this);
        buttons.setOrientation(LinearLayout.HORIZONTAL);

        Button test = new Button(this);
        test.setText(R.string.settings_test);
        test.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                runOracleTest();
            }
        });
        buttons.addView(test);

        Button save = new Button(this);
        save.setText(R.string.settings_save);
        save.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                save();
            }
        });
        buttons.addView(save);

        Button cancel = new Button(this);
        cancel.setText(R.string.settings_cancel);
        cancel.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                finish();
            }
        });
        buttons.addView(cancel);
        form.addView(buttons);

        testResult = new TextView(this);
        testResult.setPadding(0, dp(12), 0, 0);
        form.addView(testResult);

        // Destructive, so it lives at the bottom and away from Save.
        Button forget = new Button(this);
        forget.setText(R.string.settings_forget);
        forget.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                confirmForget();
            }
        });
        form.addView(forget);
        body(form, R.string.settings_forget_help);

        ScrollView scroll = new ScrollView(this);
        scroll.addView(form);
        setContentView(scroll);

        // Only write settings that already existed; do not create a keyless
        // config just by opening the screen.
        if (p.contains(Config.KEY_API_KEY)) {
            persist();
        }
    }

    /** Collect the fields into SharedPreferences and the engine's config file. */
    private void persist() {
        SharedPreferences.Editor e = Config.prefs(this).edit();
        e.putString(Config.KEY_API_KEY, apiKey.getText().toString().trim());
        e.putString(Config.KEY_BASE, baseUrl.getText().toString().trim());
        e.putString(Config.KEY_MODEL, model.getText().toString().trim());
        e.putString(Config.KEY_REASONING, reasoning.getText().toString().trim());
        e.putString(Config.KEY_MAX_TOKENS, maxTokens.getText().toString().trim());
        e.putString(Config.KEY_TZ_OFFSET, tzOffset.getText().toString().trim());
        e.putString(Config.KEY_MEMORY, memoryOn.isChecked() ? "on" : "off");
        e.apply();
        try {
            Config.write(this);
        } catch (IOException io) {
            Toast.makeText(this, getString(R.string.settings_write_failed, io.getMessage()),
                    Toast.LENGTH_LONG).show();
        }
    }

    private void save() {
        if (apiKey.getText().toString().trim().isEmpty()) {
            // A blank key is allowed — the diary just cannot answer — but the
            // writer should know that is what they are choosing.
            Toast.makeText(this, R.string.settings_no_key, Toast.LENGTH_LONG).show();
        }
        persist();
        setResult(RESULT_OK);
        finish();
    }

    /**
     * Ask the oracle for one reply, using a blank page, to prove the key,
     * endpoint and model all work. This is upstream's {@code riddle
     * --oracle-test}, which was otherwise only reachable over SSH.
     */
    private void runOracleTest() {
        persist();
        testResult.setTextColor(Color.DKGRAY);
        testResult.setText(R.string.settings_testing);

        worker.execute(new Runnable() {
            @Override
            public void run() {
                String reply;
                final java.io.File blank = new java.io.File(getCacheDir(), "oracle-test.png");
                try {
                    writeBlankPng(blank);
                    reply = DiaryView.nativeOracleTest(blank.getAbsolutePath());
                } catch (Exception ex) {
                    reply = "error: " + ex;
                } finally {
                    //noinspection ResultOfMethodCallIgnored
                    blank.delete();
                }
                final String out = reply == null ? "" : reply;
                runOnUiThread(new Runnable() {
                    @Override
                    public void run() {
                        testResult.setText(out.isEmpty()
                                ? getString(R.string.settings_test_empty) : out);
                        testResult.setTextColor(Color.BLACK);
                    }
                });
            }
        });
    }

    /** A small white page, so the oracle has an image to read. */
    private void writeBlankPng(java.io.File out) throws IOException {
        android.graphics.Bitmap bmp =
                android.graphics.Bitmap.createBitmap(200, 200, android.graphics.Bitmap.Config.RGB_565);
        bmp.eraseColor(Color.WHITE);
        try (java.io.FileOutputStream fos = new java.io.FileOutputStream(out)) {
            bmp.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, fos);
        } finally {
            bmp.recycle();
        }
    }

    private void confirmForget() {
        new android.app.AlertDialog.Builder(this)
                .setTitle(R.string.settings_forget)
                .setMessage(R.string.settings_forget_confirm)
                .setPositiveButton(android.R.string.ok,
                        new android.content.DialogInterface.OnClickListener() {
                            @Override
                            public void onClick(android.content.DialogInterface d, int w) {
                                DiaryView.nativeForget();
                                Toast.makeText(SettingsActivity.this,
                                        R.string.settings_forgotten, Toast.LENGTH_SHORT).show();
                            }
                        })
                .setNegativeButton(android.R.string.cancel, null)
                .show();
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        worker.shutdown();
    }

    // --- small layout helpers, to keep the screen free of an XML file ---

    private void heading(LinearLayout parent, int res) {
        TextView tv = new TextView(this);
        tv.setText(res);
        tv.setTextSize(18f);
        tv.setPadding(0, dp(16), 0, dp(4));
        parent.addView(tv);
    }

    private void body(LinearLayout parent, int res) {
        TextView tv = new TextView(this);
        tv.setText(res);
        tv.setTextSize(13f);
        tv.setTextColor(Color.DKGRAY);
        tv.setPadding(0, 0, 0, dp(8));
        parent.addView(tv);
    }

    private EditText field(LinearLayout parent, int labelRes, int hintRes, String value) {
        TextView label = new TextView(this);
        label.setText(labelRes);
        label.setPadding(0, dp(8), 0, 0);
        parent.addView(label);

        EditText edit = new EditText(this);
        edit.setSingleLine(true);
        edit.setText(value);
        if (hintRes != 0) {
            edit.setHint(hintRes);
        }
        parent.addView(edit, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        return edit;
    }

    private int dp(int value) {
        return (int) TypedValue.applyDimension(
                TypedValue.COMPLEX_UNIT_DIP, value, getResources().getDisplayMetrics());
    }
}
