package app.polarbear;

import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageInstaller;
import android.os.Build;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.Enumeration;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

/**
 * Installs the apps the desktop hands Android ("Install on Android", `localdesktop install`): an
 * APK, or a bundle of split APKs (.xapk, .apks: a zip with the APKs in it), through Android's
 * package installer, which asks the user first, and the first time also whether Local Desktop may
 * install apps at all. A bundle's other files (an .xapk's OBB data) aren't copied.
 */
final class Installer {
    private static final String ACTION_STATUS = "app.polarbear.action.INSTALL_STATUS";
    private static final String EXTRA_NAME = "name";

    private static BroadcastReceiver receiver;

    private Installer() {}

    static void install(Context activity, final String path, final String name) {
        final Context context = activity.getApplicationContext();
        listen(context);
        Commands.notice(context, "Installing " + name + " on Android…");
        new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    commit(context, path, name);
                } catch (IOException | RuntimeException e) {
                    String why = e.getMessage() != null ? e.getMessage() : e.toString();
                    Commands.notice(context, "Couldn't install " + name + ": " + why);
                }
            }
        }, "install").start();
    }

    private static void commit(Context context, String path, String name) throws IOException {
        PackageInstaller installer = context.getPackageManager().getPackageInstaller();
        PackageInstaller.SessionParams params =
                new PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL);
        int id = installer.createSession(params);
        PackageInstaller.Session session = installer.openSession(id);
        boolean committed = false;
        try {
            if (write(session, path) == 0) {
                throw new IOException("there is no Android app in it");
            }
            Intent status = new Intent(ACTION_STATUS)
                    .setPackage(context.getPackageName())
                    .putExtra(EXTRA_NAME, name);
            // The installer fills in the result.
            int flags = PendingIntent.FLAG_UPDATE_CURRENT
                    | (Build.VERSION.SDK_INT >= 31 ? PendingIntent.FLAG_MUTABLE : 0);
            PendingIntent result = PendingIntent.getBroadcast(context, id, status, flags);
            session.commit(result.getIntentSender());
            committed = true;
        } finally {
            if (!committed) {
                session.abandon();
            }
            session.close();
        }
    }

    /** The APK, or a bundle's APKs, into the session; how many. */
    private static int write(PackageInstaller.Session session, String path) throws IOException {
        try (ZipFile zip = new ZipFile(path)) {
            if (zip.getEntry("AndroidManifest.xml") != null) {
                copy(new FileInputStream(path), session, "base.apk", new File(path).length());
                return 1;
            }
            int count = 0;
            Enumeration<? extends ZipEntry> entries = zip.entries();
            while (entries.hasMoreElements()) {
                ZipEntry entry = entries.nextElement();
                String entryName = entry.getName();
                // The bundle's APKs are at its top.
                if (entry.isDirectory() || entryName.contains("/") || !entryName.endsWith(".apk")) {
                    continue;
                }
                copy(zip.getInputStream(entry), session, entryName, entry.getSize());
                count++;
            }
            return count;
        }
    }

    private static void copy(InputStream from, PackageInstaller.Session session, String name, long size)
            throws IOException {
        try (InputStream in = from; OutputStream out = session.openWrite(name, 0, size)) {
            byte[] buffer = new byte[1 << 16];
            int read;
            while ((read = in.read(buffer)) > 0) {
                out.write(buffer, 0, read);
            }
            session.fsync(out);
        }
    }

    private static synchronized void listen(Context context) {
        if (receiver != null) {
            return;
        }
        receiver = new BroadcastReceiver() {
            @Override
            public void onReceive(Context context, Intent intent) {
                result(context, intent);
            }
        };
        IntentFilter filter = new IntentFilter(ACTION_STATUS);
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            context.registerReceiver(receiver, filter);
        }
    }

    private static void result(Context context, Intent intent) {
        int status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
        String name = intent.getStringExtra(EXTRA_NAME);
        switch (status) {
            case PackageInstaller.STATUS_PENDING_USER_ACTION:
                // Android's own dialog, where the user says yes or no.
                Intent confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT);
                if (confirm != null) {
                    confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                    context.startActivity(confirm);
                }
                break;
            case PackageInstaller.STATUS_SUCCESS:
                Commands.notice(context, name + " is installed");
                break;
            case PackageInstaller.STATUS_FAILURE_ABORTED:
                // The user said no.
                break;
            default:
                String message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE);
                Commands.notice(context, "Couldn't install " + name + (message != null ? ": " + message : ""));
        }
    }
}
