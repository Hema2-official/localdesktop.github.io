package app.polarbear;

import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import android.widget.Toast;
import java.io.File;
import java.util.ArrayList;

/**
 * What the desktop asks of Android through the `localdesktop` command
 * (src/android/guest/control.rs): links for Android's apps, files for the share sheet, the app's
 * settings and terminal, and apps to install (Installer). The guest link calls these from its own
 * thread.
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

    /** Local Desktop's settings, in a task of their own (a window of their own in DeX). */
    static void openSettings(Context activity) {
        Intent intent = new Intent(activity, SettingsActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        start(activity, intent, "Local Desktop's settings didn't open");
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

    /**
     * Android's share sheet for files of the desktop, each at paths[i] in roots[i] (see
     * ShareProvider). The app the user picks may read those files, and only those.
     */
    static void share(Context activity, String[] roots, String[] paths) {
        ArrayList<Uri> uris = new ArrayList<>();
        String type = null;
        String refusal = null;
        int refused = 0;
        for (int i = 0; i < paths.length; i++) {
            Uri uri = ShareProvider.uri(activity, roots[i], paths[i]);
            String name = String.valueOf(uri.getLastPathSegment());
            File file = ShareProvider.resolve(activity, uri);
            if (file == null || !file.isFile()) {
                refusal = file != null && file.isDirectory()
                        ? name + " is a folder: Local Desktop shares files"
                        : "Local Desktop can't share " + name;
                refused++;
                continue;
            }
            uris.add(uri);
            type = common(type, ShareProvider.typeOf(name));
        }
        if (refused == 1) {
            notice(activity, refusal);
        } else if (refused > 1) {
            notice(activity, "Local Desktop can't share " + refused + " of them");
        }
        if (uris.isEmpty()) {
            return;
        }
        Intent intent;
        if (uris.size() == 1) {
            intent = new Intent(Intent.ACTION_SEND).putExtra(Intent.EXTRA_STREAM, uris.get(0));
        } else {
            intent = new Intent(Intent.ACTION_SEND_MULTIPLE)
                    .putParcelableArrayListExtra(Intent.EXTRA_STREAM, uris);
        }
        // The grant covers the clip's URIs, and createChooser() passes it to the share sheet too,
        // which shows the files.
        ClipData clip = ClipData.newRawUri("", uris.get(0));
        for (int i = 1; i < uris.size(); i++) {
            clip.addItem(new ClipData.Item(uris.get(i)));
        }
        intent.setClipData(clip);
        intent.setType(type).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        start(activity, Intent.createChooser(intent, null), "No app on Android takes these files");
    }

    /** A type for files of both types: the type, the kind (image/*), or any. */
    private static String common(String a, String b) {
        if (a == null || a.equals(b)) {
            return b;
        }
        String kind = a.substring(0, a.indexOf('/') + 1);
        return !kind.isEmpty() && b.startsWith(kind) ? kind + "*" : "*/*";
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
