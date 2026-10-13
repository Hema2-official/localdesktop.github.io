package app.polarbear;

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.provider.Settings;
import android.webkit.JavascriptInterface;

/**
 * The settings page's way into the app (assets/settings.html, as `localdesktop`): its requests
 * go to src/android/settings.rs, and what only Android does is done here. Only the settings
 * page's WebView has it, which loads nothing but the app's own pages.
 */
final class SettingsBridge {
    static {
        // The settings run without the desktop's activity, which loads the library otherwise.
        System.loadLibrary("localdesktop");
    }

    private final Activity activity;

    SettingsBridge(Activity activity) {
        this.activity = activity;
    }

    private static native String nativeCall(String request);

    /** A request for the app, in JSON, and its answer. */
    @JavascriptInterface
    public String call(String request) {
        return nativeCall(request);
    }

    /** The build's version, as Android shows it (2.1.1-bleeding-edge.20261012.34, say). */
    @JavascriptInterface
    public String version() {
        try {
            return activity.getPackageManager()
                    .getPackageInfo(activity.getPackageName(), 0).versionName;
        } catch (PackageManager.NameNotFoundException e) {
            return null;
        }
    }

    /** Android's phantom process killer, as PhantomProcessKiller.state tells it. */
    @JavascriptInterface
    public String phantomProcessKiller() {
        return PhantomProcessKiller.state(activity);
    }

    /** Whether the notice about the phantom process killer comes up when Local Desktop starts. */
    @JavascriptInterface
    public void remindOfPhantomProcessKiller(boolean on) {
        PhantomProcessKiller.remind(activity, on);
    }

    /** The phone's address on its network, for the SSH login, or null. */
    @JavascriptInterface
    public String address() {
        return SessionService.ipv4Address(activity);
    }

    /**
     * Open "app-info" (Android's settings for the app), "desktop" (the app itself) or one of the
     * app's pages (the terminal), each in its own task.
     */
    @JavascriptInterface
    public void open(String what) {
        final Intent intent;
        if ("app-info".equals(what)) {
            intent = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", activity.getPackageName(), null));
        } else if ("desktop".equals(what)) {
            intent = activity.getPackageManager()
                    .getLaunchIntentForPackage(activity.getPackageName());
        } else if (what.startsWith("file:///android_asset/")) {
            intent = new Intent(activity, WebPageActivity.class)
                    .putExtra(WebPageActivity.EXTRA_URL, what);
        } else {
            return;
        }
        if (intent == null) {
            return;
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    activity.startActivity(intent);
                } catch (ActivityNotFoundException e) {
                    Commands.notice(activity, "That didn't open");
                }
            }
        });
    }
}
