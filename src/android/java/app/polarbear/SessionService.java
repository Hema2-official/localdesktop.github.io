package app.polarbear;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.os.Build;
import android.os.IBinder;
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
    private static final String ACTION_RESTART = "app.polarbear.action.RESTART_DESKTOP";
    private static final String ACTION_QUIT = "app.polarbear.action.QUIT";
    private static final String CHANNEL_ID = "session";
    private static final int NOTIFICATION_ID = 1;
    /** ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, which android-33's jar doesn't have. */
    private static final int FOREGROUND_SERVICE_TYPE_SPECIAL_USE = 1 << 30;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_RESTART.equals(action) || ACTION_QUIT.equals(action)) {
            try {
                Native.onAction(ACTION_RESTART.equals(action) ? "restart" : "quit");
            } catch (UnsatisfiedLinkError e) {
                // The app isn't running any more; nothing left to manage.
                stopSelf();
            }
            return START_NOT_STICKY;
        }
        if (intent == null) {
            stopSelf();
            return START_NOT_STICKY;
        }

        Notification notification = buildNotification(intent);
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION_ID, notification, FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }
        return START_NOT_STICKY;
    }

    private Notification buildNotification(Intent intent) {
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            NotificationManager manager = getSystemService(NotificationManager.class);
            manager.createNotificationChannel(new NotificationChannel(
                    CHANNEL_ID, "Desktop session", NotificationManager.IMPORTANCE_LOW));
            builder = new Notification.Builder(this, CHANNEL_ID);
        } else {
            builder = new Notification.Builder(this);
        }

        int flags = PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE;
        Intent open = getPackageManager().getLaunchIntentForPackage(getPackageName());
        builder.setSmallIcon(getApplicationInfo().icon)
                .setContentTitle("Local Desktop is running")
                .setContentText(details(intent))
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0, open, flags));

        String terminalUrl = intent.getStringExtra(EXTRA_TERMINAL_URL);
        if (terminalUrl != null) {
            Intent terminal = new Intent(this, WebPageActivity.class)
                    .putExtra(WebPageActivity.EXTRA_URL, terminalUrl)
                    .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
            builder.addAction(0, "Terminal", PendingIntent.getActivity(this, 1, terminal, flags));
        }
        builder.addAction(0, "Restart desktop", PendingIntent.getService(
                this, 2, new Intent(this, SessionService.class).setAction(ACTION_RESTART), flags));
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
