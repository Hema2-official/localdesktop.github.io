package app.polarbear;

import android.app.Activity;
import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.Intent;
import android.os.Bundle;

/**
 * Immersive mode on VR headsets. Meta Horizon OS runs this activity full-field because of the VR
 * category in the manifest, and the desktop's own activity as a panel. The OpenXR session runs on
 * a thread of its own in Rust (src/android/xr.rs).
 *
 * The extra "to" switches modes: "panel" leaves immersive mode for the desktop's panel in Home,
 * "overlay" opens the panel over immersive mode, which keeps running behind it.
 */
public class XrActivity extends Activity {
    static final String EXTRA_TO = "to";

    static {
        // NativeActivity loads the library without telling Java, so Java wouldn't find these
        // methods by name. Loading it again here is cheap, and works whichever activity came first.
        System.loadLibrary("localdesktop");
    }

    private static native void nativeStart(Activity activity);

    private static native void nativeStop();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        nativeStart(this);
        handle(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        handle(intent);
    }

    @Override
    protected void onDestroy() {
        nativeStop();
        super.onDestroy();
    }

    private void handle(Intent intent) {
        String to = intent == null ? null : intent.getStringExtra(EXTRA_TO);
        if ("panel".equals(to)) {
            leaveForPanel();
        } else if ("overlay".equals(to)) {
            startActivity(panelIntent());
        }
    }

    private Intent panelIntent() {
        return new Intent(this, NativeActivity.class)
                .setAction(Intent.ACTION_MAIN)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
    }

    /** Back to the desktop's panel, the way Meta's hybrid apps do it: Home opens the panel. */
    private void leaveForPanel() {
        PendingIntent panel = PendingIntent.getActivity(this, 0, panelIntent(),
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Intent home = new Intent(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_HOME)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                .putExtra("extra_launch_in_home_pending_intent", panel);
        startActivity(home);
        finishAndRemoveTask();
    }
}
