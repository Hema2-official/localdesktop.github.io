package app.polarbear;

import android.content.Context;
import android.content.Intent;
import android.content.pm.ShortcutInfo;
import android.content.pm.ShortcutManager;
import android.graphics.drawable.Icon;
import android.os.Build;
import java.util.Collections;

/** The launcher icon's long-press menu: Settings, which open without starting the desktop. */
final class Shortcuts {
    private static boolean published;

    private Shortcuts() {}

    /** Once per process: Android keeps them, and limits how often apps change them. */
    static synchronized void publish(Context context) {
        if (published || Build.VERSION.SDK_INT < 25) {
            return;
        }
        ShortcutManager manager = context.getSystemService(ShortcutManager.class);
        if (manager == null) {
            return;
        }
        // The launcher starts it with a fresh task, which is the settings' own (the manifest).
        Intent settings = new Intent(Intent.ACTION_VIEW).setClass(context, SettingsActivity.class);
        ShortcutInfo shortcut = new ShortcutInfo.Builder(context, "settings")
                .setShortLabel("Settings")
                .setLongLabel("Local Desktop settings")
                .setIcon(Icon.createWithResource(context, context.getApplicationInfo().icon))
                .setIntent(settings)
                .build();
        try {
            manager.setDynamicShortcuts(Collections.singletonList(shortcut));
            published = true;
        } catch (RuntimeException e) {
            // Over Android's limit, or a launcher without shortcuts: the next start tries again.
        }
    }
}
