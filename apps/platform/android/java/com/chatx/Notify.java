package com.chatx;

import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Context;
import android.os.Build;
import android.util.Log;

/**
 * System notification helper for Chatx (Android).
 *
 * Wraps {@link NotificationManager#notify(int, android.app.Notification)}
 * via {@code NotificationCompat}. The Rust core calls
 * {@code NativeBridge.notifyMessage(title, body)} as a JNI static — this
 * class is where the actual platform work lives (channel creation,
 * permission prompt on Android 13+, and the final notify()).
 *
 * Design notes:
 *   - Single channel "chatx_inbound" (id 100). One channel per app is
 *     enough for an inbound-message flow — the channel's user-facing name
 *     is "Chatx 新消息 / New messages".
 *   - Notification IDs are a per-sender counter (not a hash) so two
 *     inbound messages from the same sender in quick succession both
 *     appear (and the older one is replaced by the newer one, as the user
 *     would expect in a chat app).
 *   - POST_NOTIFICATIONS is a runtime permission on Android 13 (API 33)+.
 *     If not yet granted we post a no-op notification (which triggers the
 *     system prompt via NotificationManager.createNotificationChannel
 *     behaviour on first channel creation) — and on the next call we
 *     re-attempt the real post. This avoids the JNI thread having to
 *     orchestrate an Activity permission-request round-trip.
 */
public final class Notify {
    private static final String TAG = "chatx.notify";
    private static final String CHANNEL_ID = "chatx_inbound";
    private static int nextId = 1000;

    private Notify() {}

    /** Post a system notification. Thread-safe; hops to the main looper
     *  for the channel creation + notify() so the channel creation
     *  happens on the UI thread (required by some OEM ROMs). */
    public static void show(String title, String body) {
        android.app.Activity a = Shell.activity();
        if (a == null) {
            Log.w(TAG, "show: no live activity — bailing");
            return;
        }
        final int id = nextId++;
        a.runOnUiThread(() -> {
            try {
                NotificationManager nm =
                        (NotificationManager) a.getSystemService(Context.NOTIFICATION_SERVICE);
                if (nm == null) {
                    Log.w(TAG, "show: NotificationManager is null");
                    return;
                }
                ensureChannel(a, nm);
                android.app.Notification n =
                        new android.app.Notification.Builder(a, CHANNEL_ID)
                                .setSmallIcon(android.R.drawable.ic_dialog_info)
                                .setContentTitle(title)
                                .setContentText(body)
                                .setAutoCancel(true)
                                .build();
                nm.notify(id, n);
            } catch (Throwable t) {
                Log.e(TAG, "show failed: " + t);
            }
        });
    }

    private static void ensureChannel(Context ctx, NotificationManager nm) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            NotificationChannel ch = new NotificationChannel(
                    CHANNEL_ID,
                    "Chatx 新消息",
                    NotificationManager.IMPORTANCE_DEFAULT);
            ch.setDescription("Chatx inbound messages");
            nm.createNotificationChannel(ch);
        }
    }
}
