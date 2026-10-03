package app.polarbear;

import android.app.Activity;
import android.provider.Settings;
import android.view.WindowManager;

/**
 * Whether the screen may turn off while Local Desktop is in front. The app keeps it on from the
 * start; the desktop's compositor tells when nobody has used it for a while and nothing (a video)
 * asks to stay awake, and then Android's own timeout applies (see guest/screen.rs).
 */
public final class Screen {
    private Screen() {}

    /** Keep the screen on, or let it turn off after Android's timeout. */
    public static void keepOn(final Activity activity, final boolean on) {
        // The window only takes changes on the thread that made it.
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (on) {
                    activity.getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                } else {
                    activity.getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                }
            }
        });
    }

    /** Android's screen timeout, in milliseconds. */
    public static int timeout(Activity activity) {
        return Settings.System.getInt(
                activity.getContentResolver(), Settings.System.SCREEN_OFF_TIMEOUT, 60000);
    }
}
