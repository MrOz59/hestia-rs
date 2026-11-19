package com.mroz59.hestia;

import android.app.Activity;
import android.content.Context;
import android.content.pm.ActivityInfo;
import android.os.Build;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.text.InputType;
import android.view.InputDevice;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

/**
 * Hestia RS's Android glue that needs the UI thread or a Java class:
 * immersive landscape streaming and a soft keyboard that types straight
 * into the host (the way Artemis does it, no text box).
 *
 * Loaded at runtime from a dex embedded in the native library; the native
 * methods are registered from Rust.
 */
public final class HestiaHelper {
    private HestiaHelper() {}

    /** Text the keyboard committed (accented letters, emoji, swipe words…). */
    static native void nativeCommitText(String text);

    /** The keyboard deleted `count` characters before the cursor. */
    static native void nativeDeleteBefore(int count);

    private static KeyboardView sKeyboardView;

    /**
     * Streaming on: the chosen orientation (0 landscape, 1 portrait, either
     * way up; 2 follow the device), system bars hidden until swiped in,
     * screen kept on, drawing under the display cutout.
     */
    public static void setStreamMode(final Activity activity, final boolean on, final int orientation) {
        activity.runOnUiThread(() -> {
            Window window = activity.getWindow();
            int requested = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED;
            if (on && orientation == 0) {
                requested = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE;
            } else if (on && orientation == 1) {
                requested = ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT;
            } else if (on) {
                requested = ActivityInfo.SCREEN_ORIENTATION_FULL_USER;
            }
            activity.setRequestedOrientation(requested);
            if (on) {
                window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
            if (Build.VERSION.SDK_INT >= 28) {
                WindowManager.LayoutParams attrs = window.getAttributes();
                attrs.layoutInDisplayCutoutMode = on
                        ? WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
                        : WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT;
                window.setAttributes(attrs);
            }
            if (Build.VERSION.SDK_INT >= 30) {
                window.setDecorFitsSystemWindows(!on);
                WindowInsetsController controller = window.getInsetsController();
                if (controller != null) {
                    if (on) {
                        controller.setSystemBarsBehavior(
                                WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                        controller.hide(WindowInsets.Type.systemBars());
                    } else {
                        controller.show(WindowInsets.Type.systemBars());
                    }
                }
            } else {
                int flags = View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN;
                window.getDecorView().setSystemUiVisibility(on ? flags : 0);
            }
        });
    }

    /**
     * Rumble from the host: on the controller's own motor when it has one,
     * otherwise on the phone. Strengths are 0-65535; 0 and 0 stops it.
     * GameStream rumble is a level that holds until changed, hence the
     * long one-shot that the next call replaces or cancels.
     */
    public static void rumble(final Activity activity, final int deviceId, final int low, final int high) {
        Vibrator vibrator = null;
        if (deviceId >= 0) {
            InputDevice device = InputDevice.getDevice(deviceId);
            if (device != null) {
                Vibrator own = device.getVibrator();
                if (own != null && own.hasVibrator()) {
                    vibrator = own;
                }
            }
        }
        if (vibrator == null) {
            vibrator = (Vibrator) activity.getSystemService(Context.VIBRATOR_SERVICE);
        }
        if (vibrator == null || !vibrator.hasVibrator()) {
            return;
        }
        int strength = Math.max(low, high) >> 8;
        if (strength <= 0) {
            vibrator.cancel();
            return;
        }
        int amplitude = vibrator.hasAmplitudeControl()
                ? Math.max(1, Math.min(255, strength))
                : VibrationEffect.DEFAULT_AMPLITUDE;
        vibrator.vibrate(VibrationEffect.createOneShot(60_000, amplitude));
    }

    /** Shows or hides the soft keyboard that types into the host. */
    public static void setKeyboardVisible(final Activity activity, final boolean visible) {
        activity.runOnUiThread(() -> {
            InputMethodManager imm =
                    (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
            if (imm == null) {
                return;
            }
            KeyboardView view = keyboardView(activity);
            if (visible) {
                view.setFocusable(true);
                view.setFocusableInTouchMode(true);
                view.requestFocus();
                imm.restartInput(view);
                imm.showSoftInput(view, 0);
            } else {
                imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
                view.clearFocus();
                view.setFocusable(false);
            }
        });
    }

    private static KeyboardView keyboardView(Activity activity) {
        if (sKeyboardView == null || sKeyboardView.getContext() != activity) {
            sKeyboardView = new KeyboardView(activity);
            activity.addContentView(sKeyboardView, new ViewGroup.LayoutParams(1, 1));
        }
        return sKeyboardView;
    }

    /**
     * An invisible view the keyboard types into. Its input type is "null",
     * so keyboards send plain key events for ordinary keys (those reach the
     * native side like a hardware keyboard's); what they can only send as
     * text, or deletions, comes through this InputConnection.
     */
    static final class KeyboardView extends View {
        KeyboardView(Context context) {
            super(context);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = InputType.TYPE_NULL;
            // No full-screen editor in landscape; no action button.
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_ACTION_NONE;
            return new BaseInputConnection(this, false) {
                @Override
                public boolean commitText(CharSequence text, int newCursorPosition) {
                    if (text != null && text.length() > 0) {
                        nativeCommitText(text.toString());
                    }
                    return true;
                }

                @Override
                public boolean deleteSurroundingText(int beforeLength, int afterLength) {
                    if (beforeLength > 0) {
                        nativeDeleteBefore(beforeLength);
                    }
                    return true;
                }
            };
        }
    }
}
