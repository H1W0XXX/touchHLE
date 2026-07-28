/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.Build;

/**
 * Fired by AlarmManager when a scheduled local notification comes due; posts the
 * actual notification. Also used directly for notifications whose fire time is
 * already in the past. See {@link Notifications}.
 */
public class NotificationReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        String body = intent.getStringExtra(Notifications.EXTRA_BODY);
        int id = intent.getIntExtra(Notifications.EXTRA_ID, 0);
        postNotification(context, body, id);
        Notifications.forgetId(context.getApplicationContext(), id);
    }

    /** Build and post a notification immediately. */
    static void postNotification(Context context, String body, int id) {
        Context ctx = context.getApplicationContext();
        Notifications.ensureChannel(ctx);

        NotificationManager nm =
                (NotificationManager) ctx.getSystemService(Context.NOTIFICATION_SERVICE);
        if (nm == null) {
            return;
        }
        if (body == null) {
            body = "";
        }

        CharSequence title = ctx.getApplicationInfo().loadLabel(ctx.getPackageManager());

        // Tapping the notification relaunches the game.
        PendingIntent contentIntent = null;
        Intent launch = ctx.getPackageManager().getLaunchIntentForPackage(ctx.getPackageName());
        if (launch != null) {
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
            contentIntent = PendingIntent.getActivity(
                    ctx, id, launch, Notifications.pendingIntentFlags());
        }

        Notification notification;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder b = new Notification.Builder(ctx, Notifications.CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.ic_popup_reminder)
                    .setContentTitle(title)
                    .setContentText(body)
                    .setAutoCancel(true);
            if (contentIntent != null) {
                b.setContentIntent(contentIntent);
            }
            notification = b.build();
        } else {
            Notification.Builder b = new Notification.Builder(ctx)
                    .setSmallIcon(android.R.drawable.ic_popup_reminder)
                    .setContentTitle(title)
                    .setContentText(body)
                    .setAutoCancel(true)
                    .setPriority(Notification.PRIORITY_DEFAULT);
            if (contentIntent != null) {
                b.setContentIntent(contentIntent);
            }
            notification = b.build();
        }

        nm.notify(id, notification);
    }
}
