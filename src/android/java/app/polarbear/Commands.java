package app.polarbear;

import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.widget.Toast;

/**
 * What the desktop asks of Android through the `localdesktop` command
 * (src/android/guest/control.rs): links for Android's apps, the app's settings and terminal, and
 * apps to install (Installer). The guest link calls these from its own thread.
 */
final class Commands {
    private Commands() {}

    static void openUrl(Context activity, String url) {
        Uri uri = Uri.parse(url);
        Intent intent = new Intent(Intent.ACTION_VIEW, uri)
                .addCategory(Intent.CATEGORY_BROWSABLE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        start(activity, intent, "No app on Android opens " + uri.getScheme() + " links");
    }

    static void openSettings(Context activity) {
        Uri app = Uri.fromParts("package", activity.getPackageName(), null);
        Intent intent = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, app)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        start(activity, intent, "Android's settings for Local Desktop didn't open");
    }

    static void openTerminal(Context activity, String url) {
        Intent intent = new Intent(activity, WebPageActivity.class)
                .putExtra(WebPageActivity.EXTRA_URL, url)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        start(activity, intent, "The terminal didn't open");
    }

    static void install(Context activity, String path, String name) {
        Installer.install(activity, path, name);
    }

    /** Tell the user, from any thread. */
    static void notice(Context context, String text) {
        final Context app = context.getApplicationContext();
        final String message = text;
        new Handler(Looper.getMainLooper()).post(new Runnable() {
            @Override
            public void run() {
                Toast.makeText(app, message, Toast.LENGTH_LONG).show();
            }
        });
    }

    private static void start(Context activity, Intent intent, String failure) {
        try {
            activity.startActivity(intent);
        } catch (ActivityNotFoundException | SecurityException e) {
            notice(activity, failure);
        }
    }
}
