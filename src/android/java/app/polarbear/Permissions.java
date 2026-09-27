package app.polarbear;

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.PowerManager;
import android.provider.Settings;

/**
 * What the app asks the user for, one system dialog per start and each only once: showing the
 * session notification, then running in the background. Battery optimization would otherwise
 * stop the app (and cut its network while the phone dozes) as soon as it's out of sight.
 */
final class Permissions {
    private static final String NOTIFICATIONS = "android.permission.POST_NOTIFICATIONS";
    private static final String PREFERENCES = "permissions";
    private static final String NOTIFICATIONS_ASKED = "notifications_asked";
    private static final String BACKGROUND_ASKED = "background_asked";

    private Permissions() {}

    static void askNext(Activity activity) {
        SharedPreferences preferences =
                activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE);
        if (Build.VERSION.SDK_INT >= 33
                && activity.checkSelfPermission(NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
                && !preferences.getBoolean(NOTIFICATIONS_ASKED, false)) {
            preferences.edit().putBoolean(NOTIFICATIONS_ASKED, true).apply();
            activity.requestPermissions(new String[] {NOTIFICATIONS}, 1);
            return;
        }
        if (Build.VERSION.SDK_INT >= 23
                && !activity.getSystemService(PowerManager.class)
                        .isIgnoringBatteryOptimizations(activity.getPackageName())
                && !preferences.getBoolean(BACKGROUND_ASKED, false)) {
            preferences.edit().putBoolean(BACKGROUND_ASKED, true).apply();
            try {
                activity.startActivity(new Intent(
                        Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                        Uri.parse("package:" + activity.getPackageName())));
            } catch (ActivityNotFoundException e) {
                // Some builds leave the dialog out; the app's battery settings still work.
            }
        }
    }
}
