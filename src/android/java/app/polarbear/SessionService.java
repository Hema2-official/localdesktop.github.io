package app.polarbear;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import java.net.Inet4Address;

/**
 * Keeps the app (and with it the Linux processes) running while it's in the background, and
 * shows a notification with the ways back in: the terminal, restarting the desktop, quitting.
 * Started by the app with the details the notification shows as extras.
 */
public class SessionService extends Service {
    static final String EXTRA_TERMINAL_URL = "terminal_url";
    static final String EXTRA_SSH_USER = "ssh_user";
    static final String EXTRA_SSH_PORT = "ssh_port";
    /** The desktop session ended by itself; its output is in EXTRA_SESSION_LOG. */
    static final String EXTRA_DESKTOP_STOPPED = "desktop_stopped";
    static final String EXTRA_SESSION_LOG = "session_log";
    /** What's wrong with localdesktop.toml, one problem per line. */
    static final String EXTRA_CONFIG_PROBLEMS = "config_problems";
    private static final String PROBLEMS_CHANNEL_ID = "problems";
    private static final int PROBLEMS_NOTIFICATION_ID = 3;
    private static final String ACTION_RESTART = "app.polarbear.action.RESTART_DESKTOP";
    private static final String ACTION_QUIT = "app.polarbear.action.QUIT";
    private static final String CHANNEL_ID = "session";
    private static final int NOTIFICATION_ID = 1;
    /** ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, which android-33's jar doesn't have. */
    private static final int FOREGROUND_SERVICE_TYPE_SPECIAL_USE = 1 << 30;
    /** About how long Plasma takes to come back: the restart button stays hidden that long. */
    private static final long RESTART_MILLIS = 30_000;
    /** Starts the service for the first setup instead of a running desktop. */
    static final String EXTRA_SETUP = "setup";
    /** Longer than any setup should take, so a stuck one can't keep the phone awake forever. */
    private static final long SETUP_LOCK_MILLIS = 2 * 60 * 60 * 1000;

    private static PowerManager.WakeLock setupLock;

    private final Handler handler = new Handler(Looper.getMainLooper());
    /** The extras the app started the service with, to rebuild the notification from. */
    private Intent details;
    private boolean restarting;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_RESTART.equals(action)) {
            // Once only: a restarting desktop looks the same as a dead one behind the
            // notification shade, which invites pressing again and again.
            if (!restarting && details != null) {
                restarting = true;
                details.removeExtra(EXTRA_DESKTOP_STOPPED);
                showNotification();
                handler.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        restarting = false;
                        showNotification();
                    }
                }, RESTART_MILLIS);
                callApp("restart");
            }
            return START_NOT_STICKY;
        }
        if (ACTION_QUIT.equals(action)) {
            callApp("quit");
            return START_NOT_STICKY;
        }
        if (PhantomProcessKiller.ACTION_DISMISS.equals(action)) {
            PhantomProcessKiller.dismiss(this);
            return START_NOT_STICKY;
        }
        if (intent == null) {
            stopSelf();
            return START_NOT_STICKY;
        }
        if (intent.getBooleanExtra(EXTRA_SETUP, false)) {
            startSetup();
            return START_NOT_STICKY;
        }

        details = intent;
        startForeground(buildNotification());
        PhantomProcessKiller.check(this);
        showConfigProblems();
        return START_NOT_STICKY;
    }

    private void startForeground(Notification notification) {
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION_ID, notification, FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }
    }

    /**
     * The first setup downloads and installs for a long while. Running in the foreground with
     * the CPU awake keeps it going (and its network access, which a dozing phone cuts for
     * background apps) when the screen turns off or the user switches apps.
     */
    private void startSetup() {
        if (setupLock == null) {
            setupLock = getSystemService(PowerManager.class)
                    .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "LocalDesktop:setup");
            setupLock.setReferenceCounted(false);
        }
        setupLock.acquire(SETUP_LOCK_MILLIS);
        startForeground(setupNotification(this, 0, "Starting…", false, false));
    }

    /** Setup progress in the notification, called by the app's setup threads. */
    static void showSetup(Context context, int progress, String message, boolean finished,
            boolean failed) {
        if ((finished || failed) && setupLock != null && setupLock.isHeld()) {
            setupLock.release();
        }
        context.getSystemService(NotificationManager.class).notify(
                NOTIFICATION_ID, setupNotification(context, progress, message, finished, failed));
    }

    private static Notification setupNotification(Context context, int progress, String message,
            boolean finished, boolean failed) {
        String title;
        if (finished) {
            title = "Local Desktop is installed";
            message = "Open it again to start the desktop.";
        } else if (failed) {
            title = "Local Desktop's setup stopped";
        } else {
            title = "Setting up Local Desktop";
        }
        Intent open = context.getPackageManager().getLaunchIntentForPackage(context.getPackageName());
        Notification.Builder builder = builder(context)
                .setContentTitle(title)
                .setContentText(message)
                .setOngoing(!finished && !failed)
                .setContentIntent(PendingIntent.getActivity(context, 0, open,
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE));
        if (!finished && !failed) {
            builder.setProgress(100, progress, false);
        }
        return builder.build();
    }

    private static Notification.Builder builder(Context context) {
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            NotificationManager manager = context.getSystemService(NotificationManager.class);
            manager.createNotificationChannel(new NotificationChannel(
                    CHANNEL_ID, "Desktop session", NotificationManager.IMPORTANCE_LOW));
            builder = new Notification.Builder(context, CHANNEL_ID);
        } else {
            builder = new Notification.Builder(context);
        }
        return builder.setSmallIcon(context.getApplicationInfo().icon);
    }

    private void showConfigProblems() {
        NotificationManager manager = getSystemService(NotificationManager.class);
        String problems = details.getStringExtra(EXTRA_CONFIG_PROBLEMS);
        if (problems == null) {
            manager.cancel(PROBLEMS_NOTIFICATION_ID);
            return;
        }
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            manager.createNotificationChannel(new NotificationChannel(
                    PROBLEMS_CHANNEL_ID, "Problems", NotificationManager.IMPORTANCE_DEFAULT));
            builder = new Notification.Builder(this, PROBLEMS_CHANNEL_ID);
        } else {
            builder = new Notification.Builder(this);
        }
        builder.setSmallIcon(getApplicationInfo().icon)
                .setContentTitle("Problem in localdesktop.toml")
                .setContentText(problems.split("\n")[0])
                .setStyle(new Notification.BigTextStyle().bigText(problems
                        + "\n\nThe rest of the config still applies. The file is "
                        + "/etc/localdesktop/localdesktop.toml; Local Desktop reads it when it "
                        + "starts and when the desktop restarts."));
        String terminalUrl = details.getStringExtra(EXTRA_TERMINAL_URL);
        if (terminalUrl != null) {
            int flags = PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE;
            Intent terminal = new Intent(this, WebPageActivity.class)
                    .putExtra(WebPageActivity.EXTRA_URL, terminalUrl)
                    .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
            builder.addAction(0, "Terminal", PendingIntent.getActivity(this, 4, terminal, flags));
        }
        manager.notify(PROBLEMS_NOTIFICATION_ID, builder.build());
    }

    private void callApp(String action) {
        try {
            Native.onAction(action);
        } catch (UnsatisfiedLinkError e) {
            // The app isn't running any more; nothing left to manage.
            stopSelf();
        }
    }

    private void showNotification() {
        getSystemService(NotificationManager.class).notify(NOTIFICATION_ID, buildNotification());
    }

    private Notification buildNotification() {
        Intent intent = details;
        Notification.Builder builder = builder(this);
        int flags = PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE;
        Intent open = getPackageManager().getLaunchIntentForPackage(getPackageName());
        boolean stopped = !restarting && intent.getBooleanExtra(EXTRA_DESKTOP_STOPPED, false);
        String text;
        if (restarting) {
            text = "Restarting the desktop…";
        } else if (stopped) {
            text = "Its output is in " + intent.getStringExtra(EXTRA_SESSION_LOG);
        } else {
            text = details(intent);
        }
        builder.setContentTitle(stopped ? "The desktop stopped" : "Local Desktop is running")
                .setContentText(text)
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0, open, flags));
        if (restarting) {
            builder.setProgress(0, 0, true);
        }

        String terminalUrl = intent.getStringExtra(EXTRA_TERMINAL_URL);
        if (terminalUrl != null) {
            Intent terminal = new Intent(this, WebPageActivity.class)
                    .putExtra(WebPageActivity.EXTRA_URL, terminalUrl)
                    .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
            builder.addAction(0, "Terminal", PendingIntent.getActivity(this, 1, terminal, flags));
        }
        if (!restarting) {
            builder.addAction(0, "Restart desktop", PendingIntent.getService(
                    this, 2, new Intent(this, SessionService.class).setAction(ACTION_RESTART), flags));
        }
        builder.addAction(0, "Quit", PendingIntent.getService(
                this, 3, new Intent(this, SessionService.class).setAction(ACTION_QUIT), flags));
        return builder.build();
    }

    /** How to reach the phone over SSH, when the SSH server is on. */
    private String details(Intent intent) {
        String user = intent.getStringExtra(EXTRA_SSH_USER);
        String address = ipv4Address();
        if (user == null || address == null) {
            return "Tap to return to the desktop";
        }
        int port = intent.getIntExtra(EXTRA_SSH_PORT, 8022);
        return "ssh -p " + port + " " + user + "@" + address;
    }

    private String ipv4Address() {
        if (Build.VERSION.SDK_INT < 23) {
            return null;
        }
        ConnectivityManager connectivity = getSystemService(ConnectivityManager.class);
        Network network = connectivity.getActiveNetwork();
        LinkProperties properties = network == null ? null : connectivity.getLinkProperties(network);
        if (properties == null) {
            return null;
        }
        for (LinkAddress address : properties.getLinkAddresses()) {
            if (address.getAddress() instanceof Inet4Address) {
                return address.getAddress().getHostAddress();
            }
        }
        return null;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
