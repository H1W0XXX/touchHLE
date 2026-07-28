/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Parts of this file are derived from SDL 2's Android project template, which
 * has a different license. Please see vendor/SDL/LICENSE.txt for details.
 */
package org.touchhle.android;

import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;

import org.libsdl.app.SDLActivity;

/**
 * A wrapper class over SDLActivity
 */

public class MainActivity extends SDLActivity {
    @Override
    protected String[] getLibraries() {
        return new String[]{
            "SDL2",
            "touchHLE"
        };
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Create the notification channel up front so its settings exist before
        // the first game notification, and (on Android 13+) prompt for the
        // POST_NOTIFICATIONS permission at launch rather than mid-game.
        Notifications.ensureChannel(getApplicationContext());
        requestNotificationPermissionIfNeeded();
    }

    private void requestNotificationPermissionIfNeeded() {
        // POST_NOTIFICATIONS (API 33). Constant/VERSION_CODES.TIRAMISU aren't
        // available at this compileSdk, so use literals.
        final int TIRAMISU = 33;
        final String POST_NOTIFICATIONS = "android.permission.POST_NOTIFICATIONS";
        if (Build.VERSION.SDK_INT < TIRAMISU) {
            return;
        }
        if (checkSelfPermission(POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[]{POST_NOTIFICATIONS}, 1001);
        }
    }
}
