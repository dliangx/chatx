package com.chatx;

import android.Manifest;
import android.content.pm.PackageManager;
import android.os.Bundle;

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
 */
public final class Shell extends android.app.NativeActivity {
    private static final String[] PERMS = {
        Manifest.permission.CAMERA,
        Manifest.permission.RECORD_AUDIO,
    };
    private static final int REQ = 0x4C;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        if (allGranted()) {
            NativeBridge.autoStart(this);
        } else {
            requestPermissions(PERMS, REQ);
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

    @Override
    protected void onDestroy() {
        try { NativeBridge.autoStop(); } catch (Throwable ignored) {}
        super.onDestroy();
    }

    private boolean allGranted() {
        for (String p : PERMS) {
            if (checkSelfPermission(p) != PackageManager.PERMISSION_GRANTED) return false;
        }
        return true;
    }
}
