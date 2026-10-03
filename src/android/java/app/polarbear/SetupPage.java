package app.polarbear;

import android.app.Activity;
import android.app.Dialog;
import android.graphics.Color;
import android.graphics.Insets;
import android.graphics.drawable.ColorDrawable;
import android.os.Build;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

/**
 * The page the app shows while there's no desktop yet (setup, or a phone that can't run one),
 * full screen over the NativeActivity. It's a dialog rather than a PopupWindow: a popup that
 * isn't focusable gets no keyboard (the setup page asks for a user name), and a focusable one
 * closes on Back, which would leave setup without its page.
 */
public final class SetupPage {
    private SetupPage() {}

    /** Show {@code url}. Call on a thread with a Looper, and run the Looper afterwards. */
    public static void show(Activity activity, String url) {
        WebView webView = new WebView(activity);
        webView.getSettings().setJavaScriptEnabled(true);
        // Links open in the page rather than in a browser.
        webView.setWebViewClient(new WebViewClient());
        webView.loadUrl(url);

        FrameLayout container = new FrameLayout(activity);
        container.setBackgroundColor(Color.BLACK);
        container.addView(webView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));

        Dialog dialog = new Dialog(activity, android.R.style.Theme_DeviceDefault_NoActionBar);
        dialog.setCancelable(false);
        dialog.setContentView(container);
        Window window = dialog.getWindow();
        window.setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT);
        window.setBackgroundDrawable(new ColorDrawable(Color.BLACK));
        // The keyboard shrinks the page instead of covering the form.
        window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
        if (Build.VERSION.SDK_INT >= 30) {
            // Apps targeting Android 15 draw behind the system bars and the keyboard, which no
            // longer resizes the window: keep the page clear of both, like WebPageActivity.
            container.setOnApplyWindowInsetsListener(new View.OnApplyWindowInsetsListener() {
                @Override
                public WindowInsets onApplyWindowInsets(View view, WindowInsets insets) {
                    Insets bars = insets.getInsets(WindowInsets.Type.systemBars()
                            | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
                    view.setPadding(bars.left, bars.top, bars.right, bars.bottom);
                    return WindowInsets.CONSUMED;
                }
            });
        }
        dialog.show();
    }
}
