package com.mroz59.hestia.rs;

import android.app.NativeActivity;
import android.app.PictureInPictureParams;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.os.Build;
import android.util.Log;
import android.util.Rational;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;

/** Owns Android's PiP lifecycle and debug-only input dispatch. Streaming stays in Rust. */
public final class HestiaActivity extends NativeActivity {
    private static final Rational DEFAULT_ASPECT = new Rational(16, 9);
    private Rational aspect = DEFAULT_ASPECT;
    private boolean streaming;
    private boolean resumed;
    // Kept through a possible mode-changed(false) callback during dismissal.
    private boolean pipSession;

    // Registered by Rust before enabling the offline tester. The native
    // queue remains a fallback for events that bypass Activity dispatch.
    private volatile boolean controllerTesterEnabled;
    private native boolean nativeControllerTesterKey(int deviceId, int source, int scanCode,
            int keyCode, int action, boolean repeat, boolean candidate);
    private native boolean nativeControllerTesterMotion(int deviceId, int source, int action,
            boolean candidate, float x, float y, float z, float rz, float rx, float ry,
            float lt, float rt, float brake, float gas, float hatX, float hatY);

    @SuppressWarnings("unused") // Called from Rust through JNI.
    private void setControllerTesterEnabled(boolean enabled) {
        controllerTesterEnabled = enabled;
    }

    private static boolean hasJoystick(InputDevice device) {
        return device != null
                && (device.getSources() & InputDevice.SOURCE_JOYSTICK) == InputDevice.SOURCE_JOYSTICK;
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        if (controllerTesterEnabled) {
            int scan = event.getScanCode();
            // Some remotes advertise GAMEPAD along with DPAD. Ordinary TV
            // navigation must remain usable; real controller keys can also
            // arrive with a keyboard source or an unknown Android keycode.
            boolean candidate = hasJoystick(event.getDevice())
                    || KeyEvent.isGamepadButton(event.getKeyCode())
                    || (scan >= 0x130 && scan <= 0x13f);
            if (nativeControllerTesterKey(event.getDeviceId(), event.getSource(), scan,
                    event.getKeyCode(), event.getAction(), event.getRepeatCount() > 0, candidate)) {
                return true;
            }
        }
        return super.dispatchKeyEvent(event);
    }

    @Override
    public boolean dispatchGenericMotionEvent(MotionEvent event) {
        if (controllerTesterEnabled) {
            boolean candidate = hasJoystick(event.getDevice())
                    || event.isFromSource(InputDevice.SOURCE_JOYSTICK);
            if (nativeControllerTesterMotion(event.getDeviceId(), event.getSource(),
                    event.getActionMasked(), candidate,
                    event.getAxisValue(MotionEvent.AXIS_X), event.getAxisValue(MotionEvent.AXIS_Y),
                    event.getAxisValue(MotionEvent.AXIS_Z), event.getAxisValue(MotionEvent.AXIS_RZ),
                    event.getAxisValue(MotionEvent.AXIS_RX), event.getAxisValue(MotionEvent.AXIS_RY),
                    event.getAxisValue(MotionEvent.AXIS_LTRIGGER), event.getAxisValue(MotionEvent.AXIS_RTRIGGER),
                    event.getAxisValue(MotionEvent.AXIS_BRAKE), event.getAxisValue(MotionEvent.AXIS_GAS),
                    event.getAxisValue(MotionEvent.AXIS_HAT_X), event.getAxisValue(MotionEvent.AXIS_HAT_Y))) {
                return true;
            }
        }
        return super.dispatchGenericMotionEvent(event);
    }

    // Registered by Rust before enabling PiP.
    private native void onNativePictureInPictureChanged(boolean inPip, boolean dismissed);

    @SuppressWarnings("unused") // Called through JNI on the native UI thread.
    private void setStreamPictureInPicture(boolean active, float videoAspect) {
        runOnUiThread(() -> {
            if (isFinishing() || isDestroyed()
                    || !getPackageManager().hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)
                    || (getResources().getConfiguration().uiMode & Configuration.UI_MODE_TYPE_MASK)
                            == Configuration.UI_MODE_TYPE_TELEVISION) {
                return;
            }
            Rational nextAspect = active && videoAspect > 0
                    ? new Rational(Math.round(Math.max(0.42f, Math.min(2.39f, videoAspect)) * 10000), 10000)
                    : active ? aspect : DEFAULT_ASPECT;
            if (streaming == active && aspect.equals(nextAspect)) {
                return;
            }
            streaming = active;
            aspect = nextAspect;
            updatePictureInPictureParams();
            if (!active && isInPictureInPictureMode()) {
                // The stream ended while minimized. Don't leave a report or
                // host list floating over the other app.
                finish();
            }
        });
    }

    private PictureInPictureParams pictureInPictureParams() {
        PictureInPictureParams.Builder params = new PictureInPictureParams.Builder().setAspectRatio(aspect);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            params.setAutoEnterEnabled(streaming);
        }
        return params.build();
    }

    private void updatePictureInPictureParams() {
        try {
            setPictureInPictureParams(pictureInPictureParams());
        } catch (IllegalStateException e) {
            Log.d("hestia", "This activity can no longer update picture-in-picture", e);
        }
    }

    @Override
    protected void onUserLeaveHint() {
        super.onUserLeaveHint();
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S && streaming && !isInPictureInPictureMode()) {
            try {
                enterPictureInPictureMode(pictureInPictureParams());
            } catch (IllegalStateException e) {
                Log.d("hestia", "This activity can no longer enter picture-in-picture", e);
            }
        }
    }

    @Override
    public void onPictureInPictureModeChanged(boolean inPip, Configuration config) {
        super.onPictureInPictureModeChanged(inPip, config);
        if (inPip && streaming && !pipSession) {
            pipSession = true;
            onNativePictureInPictureChanged(true, false);
        } else if (!inPip && resumed) {
            restoreFromPictureInPicture();
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        resumed = true;
        if (!isInPictureInPictureMode()) {
            restoreFromPictureInPicture();
        }
    }

    @Override
    protected void onPause() {
        resumed = false;
        super.onPause();
    }

    private void restoreFromPictureInPicture() {
        if (pipSession) {
            pipSession = false;
            onNativePictureInPictureChanged(false, false);
        }
    }

    @Override
    protected void onStop() {
        // Expanding goes back through onResume. Removing the PiP window
        // stops the Activity, even if the platform keeps its task around.
        if (pipSession || (streaming && isInPictureInPictureMode())) {
            pipSession = false;
            streaming = false;
            aspect = DEFAULT_ASPECT;
            updatePictureInPictureParams();
            onNativePictureInPictureChanged(false, true);
        }
        super.onStop();
    }
}
