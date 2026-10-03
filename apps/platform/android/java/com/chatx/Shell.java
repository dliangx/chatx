package com.chatx;

import android.Manifest;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.util.Log;

/**
 * NativeActivity subclass that auto-starts the Kotlin capture loop on create
 * and stops it on destroy. If CAMERA / RECORD_AUDIO are not yet granted,
 * requests them and retries the start once the dialog returns.
 *
 * Slint's android backend drives the rest: the platform's
 * `android.app.NativeActivity` finds the `android.app.lib_name` meta-data,
 * loads `libchatx.so`, finds the `android_main` C symbol, and calls it with a
 * `slint::android::AndroidApp`. That entrypoint initialises the Slint UI and
 * installs the bridge sinks that {@link NativeBridge} talks to.
 *
 * Also acts as the activity-side funnel for
 * {@link ScreenShare#onActivityResult} — MediaProjection requires the
 * consent flow ({@code startActivityForResult} → {@code onActivityResult})
 * to live on an {@code Activity}.
 */
public final class Shell extends android.app.NativeActivity {
    private static final String TAG = "chatx.shell";
    private static final String[] PERMS = {
        Manifest.permission.CAMERA,
        Manifest.permission.RECORD_AUDIO,
    };
    private static final int REQ = 0x4C;
    private static final int REQ_MEDIA = 0x5E; // matches ScreenShare.REQ_MEDIA

    private static volatile android.app.Activity activityRef;

    /** Exposed to the sibling {@link NativeBridge} (same package) so
     *  {@link NativeBridge#startScreenShare()} can hand the live activity to
     *  {@link ScreenShare#requestPermission} for the startActivityForResult
     *  call. */
    public static android.app.Activity activity() { return activityRef; }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        activityRef = this;
        if (allGranted()) {
            NativeBridge.autoStart(this);
        } else {
            requestPermissions(PERMS, REQ);
        }
    }

    @Override
    protected void onDestroy() {
        try { NativeBridge.autoStop(); } catch (Throwable ignored) {}
        try { ScreenShare.stop(); } catch (Throwable ignored) {}
        activityRef = null;
        super.onDestroy();
    }

    /** MediaProjection's consent dialog is returned to the activity that
     *  launched it — forward to {@link ScreenShare#onActivityResult}. */
    @Override
    public void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQ_MEDIA) {
            Log.i(TAG, "onActivityResult: forwarding MediaProjection consent");
            try {
                ScreenShare.onActivityResult(resultCode, data);
            } catch (Throwable t) {
                Log.e(TAG, "ScreenShare.onActivityResult failed: " + t);
            }
        }
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] names, int[] results) {
        super.onRequestPermissionsResult(requestCode, names, results);
        if (requestCode != REQ) return;
        boolean all = true;
        for (int r : results) if (r != PackageManager.PERMISSION_GRANTED) { all = false; break; }
        if (all) {
            NativeBridge.autoStart(this);
        } else {
            // User denied. Try again next launch; UI stays interactive.
            // (M5-A smoke stage — the Slint UI does not yet expose a re-prompt
            //  button; that's part of M7.)
        }
    }

    private boolean allGranted() {
        for (String p : PERMS) {
            if (checkSelfPermission(p) != PackageManager.PERMISSION_GRANTED) return false;
        }
        return true;
    }
}
