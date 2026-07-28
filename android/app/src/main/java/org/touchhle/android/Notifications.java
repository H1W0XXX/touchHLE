/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import android.app.AlarmManager;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.Build;

import org.libsdl.app.SDLActivity;

import java.util.HashSet;
import java.util.Set;

/**
 * Bridges iOS {@code UILocalNotification}s (scheduled by emulated apps) to
 * Android's AlarmManager + NotificationManager.
 *
 * Called from Rust via JNI; see
 * {@code src/frameworks/uikit/ui_local_notification.rs}. All methods are static
 * and resolve the app Context from SDL, so the native side doesn't have to pass
 * one across JNI.
 *
 * Because the alarm is registered with the OS at schedule time, it still fires
 * after touchHLE is closed — matching iOS local-notification semantics. Known
 * limitation: AlarmManager alarms do not survive a device reboot, so a
 * notification scheduled far in the future will be lost if the phone restarts
 * before it fires.
 */
public final class Notifications {
    private Notifications() {}

    public static final String CHANNEL_ID = "touchhle_local_notifications";
    static final String EXTRA_BODY = "org.touchhle.android.extra.BODY";
    static final String EXTRA_ID = "org.touchhle.android.extra.ID";
    private static final String ACTION_PREFIX = "org.touchhle.android.NOTIFY.";
    private static final String PREFS = "touchhle_notifications";
    private static final String KEY_IDS = "scheduled_ids";

    private static Context appContext() {
        Context c = SDLActivity.getContext();
        return c != null ? c.getApplicationContext() : null;
    }

    static void ensureChannel(Context ctx) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return;
        }
        NotificationManager nm =
                (NotificationManager) ctx.getSystemService(Context.NOTIFICATION_SERVICE);
        if (nm == null) {
            return;
        }
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            NotificationChannel channel = new NotificationChannel(
                    CHANNEL_ID,
                    "Game notifications",
                    NotificationManager.IMPORTANCE_DEFAULT);
            channel.setDescription("Reminders from games running in touchHLE");
            nm.createNotificationChannel(channel);
        }
    }

    static int pendingIntentFlags() {
        int flags = PendingIntent.FLAG_UPDATE_CURRENT;
        // FLAG_IMMUTABLE exists from API 23 and is mandatory from API 31.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            flags |= PendingIntent.FLAG_IMMUTABLE;
        }
        return flags;
    }

    private static PendingIntent buildAlarmIntent(Context ctx, String body, int id) {
        Intent intent = new Intent(ctx, NotificationReceiver.class);
        intent.setAction(ACTION_PREFIX + id);
        intent.putExtra(EXTRA_BODY, body);
        intent.putExtra(EXTRA_ID, id);
        return PendingIntent.getBroadcast(ctx, id, intent, pendingIntentFlags());
    }

    /**
     * Schedule (or replace) a notification. Called from native.
     *
     * @param whenMs absolute fire time, Unix epoch milliseconds
     * @param body   notification text
     * @param id     stable id so the notification can be cancelled/replaced
     */
    public static void scheduleLocalNotification(long whenMs, String body, int id) {
        Context ctx = appContext();
        if (ctx == null) {
            return;
        }
        ensureChannel(ctx);

        // Fire time already passed: post immediately instead of scheduling.
        if (whenMs <= System.currentTimeMillis()) {
            NotificationReceiver.postNotification(ctx, body, id);
            return;
        }

        AlarmManager am = (AlarmManager) ctx.getSystemService(Context.ALARM_SERVICE);
        if (am == null) {
            return;
        }
        PendingIntent pi = buildAlarmIntent(ctx, body, id);

        // We deliberately avoid requiring the SCHEDULE_EXACT_ALARM permission
        // (which carries Play Store policy restrictions). If exact alarms are
        // available we use them; otherwise setAndAllowWhileIdle still fires in
        // Doze, just within a small window — fine for a farming game's timers.
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                if (am.canScheduleExactAlarms()) {
                    am.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, whenMs, pi);
                } else {
                    am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, whenMs, pi);
                }
            } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                am.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, whenMs, pi);
            } else {
                am.setExact(AlarmManager.RTC_WAKEUP, whenMs, pi);
            }
        } catch (SecurityException e) {
            am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, whenMs, pi);
        }

        rememberId(ctx, id);
    }

    /** Cancel every not-yet-fired notification. Called from native. */
    public static void cancelAllLocalNotifications() {
        Context ctx = appContext();
        if (ctx == null) {
            return;
        }
        AlarmManager am = (AlarmManager) ctx.getSystemService(Context.ALARM_SERVICE);
        NotificationManager nm =
                (NotificationManager) ctx.getSystemService(Context.NOTIFICATION_SERVICE);

        for (String s : loadIds(ctx)) {
            int id;
            try {
                id = Integer.parseInt(s);
            } catch (NumberFormatException e) {
                continue;
            }
            if (am != null) {
                Intent intent = new Intent(ctx, NotificationReceiver.class);
                intent.setAction(ACTION_PREFIX + id);
                am.cancel(PendingIntent.getBroadcast(ctx, id, intent, pendingIntentFlags()));
            }
            if (nm != null) {
                nm.cancel(id);
            }
        }
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .edit().remove(KEY_IDS).apply();
    }

    private static Set<String> loadIds(Context ctx) {
        SharedPreferences prefs = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
        // Copy: the Set returned by getStringSet must not be mutated.
        return new HashSet<>(prefs.getStringSet(KEY_IDS, new HashSet<String>()));
    }

    private static synchronized void rememberId(Context ctx, int id) {
        Set<String> ids = loadIds(ctx);
        ids.add(Integer.toString(id));
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .edit().putStringSet(KEY_IDS, ids).apply();
    }

    static synchronized void forgetId(Context ctx, int id) {
        Set<String> ids = loadIds(ctx);
        if (ids.remove(Integer.toString(id))) {
            ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                    .edit().putStringSet(KEY_IDS, ids).apply();
        }
    }
}
