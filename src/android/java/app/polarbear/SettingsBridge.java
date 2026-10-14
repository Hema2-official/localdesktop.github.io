package app.polarbear;

import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;
import android.provider.Settings;
import android.view.WindowManager;
import android.webkit.JavascriptInterface;
import java.io.IOException;
import org.json.JSONException;
import org.json.JSONObject;

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

    private final SettingsActivity activity;
    /** The files Android's file picker last chose. */
    private volatile Uri backupFile;
    private volatile Uri restoreFile;
    private volatile boolean packageCache;
    /**
     * The backup file as Android opened it, while the app writes a copy of the descriptor:
     * closing it tells the provider the file is complete (its size, for file managers).
     */
    private ParcelFileDescriptor backupHandle;

    SettingsBridge(SettingsActivity activity) {
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

    /**
     * Ask Android's file picker where a backup named `name` goes; the page hears back through
     * its `chosen` function once the backup has started, or with null.
     */
    @JavascriptInterface
    public void chooseBackup(String name, boolean packageCache) {
        this.packageCache = packageCache;
        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("application/octet-stream")
                .putExtra(Intent.EXTRA_TITLE, name);
        pick(intent, SettingsActivity.CHOOSE_BACKUP);
    }

    /** Ask Android's file picker for a backup to restore; the page hears what it holds. */
    @JavascriptInterface
    public void chooseRestore() {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("*/*");
        pick(intent, SettingsActivity.CHOOSE_RESTORE);
    }

    /** Restore the backup the page was told about, from the start of the file. */
    @JavascriptInterface
    public String startRestore() {
        return call("restore", restoreFile, "r", new JSONObject(), false);
    }

    /** The backup ended: Android may look at the file. */
    @JavascriptInterface
    public synchronized void backupEnded() {
        close(backupHandle);
        backupHandle = null;
    }

    /** Delete the file of a backup that failed, which would only look like one. */
    @JavascriptInterface
    public void discardBackupFile() {
        backupEnded();
        Uri file = backupFile;
        backupFile = null;
        if (file == null) {
            return;
        }
        try {
            DocumentsContract.deleteDocument(activity.getContentResolver(), file);
        } catch (Exception e) {
            // The provider may not delete; the file stays.
        }
    }

    /** Keep the screen on while a backup or a restore goes on. */
    @JavascriptInterface
    public void keepScreenOn(final boolean on) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                int flag = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON;
                if (on) {
                    activity.getWindow().addFlags(flag);
                } else {
                    activity.getWindow().clearFlags(flag);
                }
            }
        });
    }

    private void pick(final Intent intent, final int request) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    activity.startActivityForResult(intent, request);
                } catch (ActivityNotFoundException e) {
                    activity.answer("chosen", error("Android has no file picker here"));
                }
            }
        });
    }

    /** The file picker's answer: start the backup, or look into the backup to restore. */
    void chosen(final int request, final Uri uri) {
        if (uri == null) {
            activity.answer("chosen", "null");
            return;
        }
        // Providers may be slow (Drive): not on the main thread.
        new Thread(new Runnable() {
            @Override
            public void run() {
                String reply;
                try {
                    if (request == SettingsActivity.CHOOSE_BACKUP) {
                        backupFile = uri;
                        JSONObject extra = new JSONObject()
                                .put("package_cache", packageCache)
                                .put("version", version());
                        JSONObject answer = new JSONObject(call("backup", uri, "w", extra, true));
                        if (answer.has("error")) {
                            discardBackupFile();
                        }
                        reply = answer.put("kind", "backup").toString();
                    } else {
                        restoreFile = uri;
                        reply = new JSONObject(
                                call("restore-inspect", uri, "r", new JSONObject(), false))
                                .put("kind", "restore")
                                .put("name", displayName(uri))
                                .toString();
                    }
                } catch (JSONException e) {
                    reply = error("The app's answer didn't read");
                }
                activity.answer("chosen", reply);
            }
        }).start();
    }

    /**
     * A request for the app with a file opened for it: the app owns a copy of the descriptor.
     * The file stays open here until the backup ends if `keep`, or else closes at once.
     */
    private String call(String op, Uri uri, String mode, JSONObject request, boolean keep) {
        if (uri == null) {
            return error("No file chosen");
        }
        ParcelFileDescriptor file = null;
        try {
            file = activity.getContentResolver().openFileDescriptor(uri, mode);
            if (file == null) {
                return error("Android didn't open the file");
            }
            String reply = nativeCall(
                    request.put("op", op).put("fd", file.dup().detachFd()).toString());
            if (keep) {
                synchronized (this) {
                    close(backupHandle);
                    backupHandle = file;
                }
                file = null;
            }
            return reply;
        } catch (Exception e) {
            return error("Android didn't open the file: " + e.getMessage());
        } finally {
            close(file);
        }
    }

    private static void close(ParcelFileDescriptor file) {
        if (file == null) {
            return;
        }
        try {
            file.close();
        } catch (IOException e) {
            // Nothing left to do with it.
        }
    }

    private static String error(String message) {
        return "{\"error\": " + JSONObject.quote(message) + "}";
    }

    private String displayName(Uri uri) {
        Cursor cursor = null;
        try {
            cursor = activity.getContentResolver().query(
                    uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null);
            if (cursor != null && cursor.moveToFirst()) {
                return cursor.getString(0);
            }
        } catch (Exception e) {
            // No name to show.
        } finally {
            if (cursor != null) {
                cursor.close();
            }
        }
        return null;
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
