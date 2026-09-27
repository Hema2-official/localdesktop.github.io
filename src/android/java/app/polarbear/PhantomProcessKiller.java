package app.polarbear;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.os.Build;
import android.provider.Settings;

/**
 * Android 12+ kills apps' child processes when there are more than 32 of them across all apps or
 * they use a lot of CPU in the background: Local Desktop's Linux programs vanish. Apps can't read
 * whether the user turned that off, so: turn it off if the user granted WRITE_SECURE_SETTINGS
 * once over adb, otherwise point to the guide until they've dealt with it.
 */
final class PhantomProcessKiller {
    static final String ACTION_DISMISS = "app.polarbear.action.DISMISS_PHANTOM_NOTICE";
    private static final String SETTING = "settings_enable_monitor_phantom_procs";
    private static final String CHANNEL_ID = "tips";
    private static final int NOTIFICATION_ID = 2;
    private static final String PREFERENCES = "phantom_process_killer";
    private static final String DISMISSED = "dismissed";

    private PhantomProcessKiller() {}

    static void check(Context context) {
        if (Build.VERSION.SDK_INT < 31) {
            return;
        }
        if (turnOff(context) || preferences(context).getBoolean(DISMISSED, false)) {
            cancelNotice(context);
            return;
        }
        showNotice(context);
    }

    static void dismiss(Context context) {
        preferences(context).edit().putBoolean(DISMISSED, true).apply();
        cancelNotice(context);
    }

    /** The guide, with the commands for this package and Android version. */
    static String guideUrl(Context context) {
        return "file:///android_asset/phantom-process-killer.html?package="
                + context.getPackageName() + "&sdk=" + Build.VERSION.SDK_INT;
    }

    /** Android 12L+ keeps the switch in a global setting; Android 12 in device_config, which apps can't write. */
    private static boolean turnOff(Context context) {
        if (Build.VERSION.SDK_INT < 32
                || context.checkSelfPermission("android.permission.WRITE_SECURE_SETTINGS")
                        != PackageManager.PERMISSION_GRANTED) {
            return false;
        }
        try {
            return Settings.Global.putString(context.getContentResolver(), SETTING, "false");
        } catch (SecurityException e) {
            return false;
        }
    }

    private static void showNotice(Context context) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            manager.createNotificationChannel(new NotificationChannel(
                    CHANNEL_ID, "Tips", NotificationManager.IMPORTANCE_LOW));
            builder = new Notification.Builder(context, CHANNEL_ID);
        } else {
            builder = new Notification.Builder(context);
        }
        int flags = PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE;
        PendingIntent guide = PendingIntent.getActivity(context, 10,
                new Intent(context, WebPageActivity.class)
                        .putExtra(WebPageActivity.EXTRA_URL, guideUrl(context)),
                flags);
        PendingIntent dismiss = PendingIntent.getService(context, 11,
                new Intent(context, SessionService.class).setAction(ACTION_DISMISS), flags);
        builder.setSmallIcon(context.getApplicationInfo().icon)
                .setContentTitle("Android may stop Linux programs")
                .setContentText("Its phantom process killer can end them at any time. Tap for the fix.")
                .setContentIntent(guide)
                .addAction(0, "How to fix", guide)
                .addAction(0, "Don't show again", dismiss);
        manager.notify(NOTIFICATION_ID, builder.build());
    }

    private static void cancelNotice(Context context) {
        context.getSystemService(NotificationManager.class).cancel(NOTIFICATION_ID);
    }

    private static SharedPreferences preferences(Context context) {
        return context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE);
    }
}
