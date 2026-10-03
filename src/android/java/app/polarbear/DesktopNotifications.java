package app.polarbear;

import android.app.Activity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Intent;
import android.os.Build;

/**
 * The desktop's notifications on Android, while Local Desktop isn't in front: a build that
 * finished while the phone was in a pocket. The desktop shows them itself otherwise, and keeps
 * them in its own history, so they're taken off Android once the app is back in front (see
 * guest/notifications.rs). They have a channel of their own, which Android's settings can mute.
 */
public final class DesktopNotifications {
    private static final String CHANNEL_ID = "desktop-notifications";
    private static final String TAG = "desktop";
    /** How many are kept; a new one takes the place of the oldest. */
    private static final int KEPT = 20;
    private static int next = 0;

    private DesktopNotifications() {}

    public static void post(Activity activity, String app, String title, String text) {
        NotificationManager manager = activity.getSystemService(NotificationManager.class);
        if (manager == null) {
            return;
        }
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            manager.createNotificationChannel(new NotificationChannel(
                    CHANNEL_ID, "Desktop notifications", NotificationManager.IMPORTANCE_DEFAULT));
            builder = new Notification.Builder(activity, CHANNEL_ID);
        } else {
            builder = new Notification.Builder(activity);
        }
        Intent open = activity.getPackageManager()
                .getLaunchIntentForPackage(activity.getPackageName());
        builder.setSmallIcon(activity.getApplicationInfo().icon)
                .setContentTitle(title.isEmpty() ? app : title)
                .setContentText(text)
                .setStyle(new Notification.BigTextStyle().bigText(text))
                .setAutoCancel(true);
        if (!title.isEmpty() && !app.isEmpty()) {
            builder.setSubText(app);
        }
        if (open != null) {
            builder.setContentIntent(PendingIntent.getActivity(activity, 0, open,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE));
        }
        manager.notify(TAG, next % KEPT, builder.build());
        next++;
    }

    /** Take them all off Android. */
    public static void cancelAll(Activity activity) {
        NotificationManager manager = activity.getSystemService(NotificationManager.class);
        if (manager == null) {
            return;
        }
        for (int id = 0; id < KEPT; id++) {
            manager.cancel(TAG, id);
        }
    }
}
