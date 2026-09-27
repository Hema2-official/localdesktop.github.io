package app.polarbear;

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.graphics.Color;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.text.InputType;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

/** Shows one of the app's own pages (the terminal, settings) full screen. */
public class WebPageActivity extends Activity {
    static final String EXTRA_URL = "url";
    private static final String PAGE_PREFIX = "file:///android_asset/";
    private static final String TERMINAL_PAGE = PAGE_PREFIX + "terminal.html";
    /** A link pages can use to open Android's Developer options. */
    private static final String DEVELOPER_OPTIONS = "localdesktop:developer-options";

    private WebView webView;
    /** Keystrokes as typed, no autocorrect or word suggestions: what a terminal needs. */
    private boolean rawKeyboard;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        webView = new WebView(this) {
            @Override
            public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
                InputConnection connection = super.onCreateInputConnection(outAttrs);
                if (rawKeyboard) {
                    // Keyboards ignore the page's autocorrect="off" but not this (as in Termux).
                    outAttrs.inputType = InputType.TYPE_CLASS_TEXT
                            | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                            | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
                }
                return connection;
            }
        };
        webView.getSettings().setJavaScriptEnabled(true);
        webView.setWebViewClient(new WebViewClient() {
            @Override
            public boolean shouldOverrideUrlLoading(WebView view, String url) {
                if (url.startsWith(PAGE_PREFIX)) {
                    return false;
                }
                // Links out of the app's pages go to other apps.
                Intent intent = DEVELOPER_OPTIONS.equals(url)
                        ? new Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS)
                        : new Intent(Intent.ACTION_VIEW, Uri.parse(url));
                try {
                    startActivity(intent);
                } catch (ActivityNotFoundException e) {
                    // Developer options aren't turned on yet: the main settings screen instead.
                    startActivity(new Intent(Settings.ACTION_SETTINGS));
                }
                return true;
            }
        });

        FrameLayout container = new FrameLayout(this);
        container.setBackgroundColor(Color.rgb(0x1c, 0x1c, 0x1c));
        container.addView(webView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        setContentView(container);
        if (Build.VERSION.SDK_INT >= 30) {
            // Apps targeting Android 15 draw behind the system bars and the keyboard, which no
            // longer resizes the window: keep the page clear of both.
            container.setOnApplyWindowInsetsListener(new View.OnApplyWindowInsetsListener() {
                @Override
                public WindowInsets onApplyWindowInsets(View view, WindowInsets insets) {
                    Insets bars = insets.getInsets(WindowInsets.Type.systemBars()
                            | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
                    view.setPadding(bars.left, bars.top, bars.right, bars.bottom);
                    return WindowInsets.CONSUMED;
                }
            });
            // Light status and navigation bar icons on the dark background.
            getWindow().getInsetsController().setSystemBarsAppearance(0,
                    WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                            | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS);
        }
        load(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        load(intent);
    }

    private void load(Intent intent) {
        String url = intent.getStringExtra(EXTRA_URL);
        if (url == null || !url.startsWith(PAGE_PREFIX)) {
            finish();
            return;
        }
        rawKeyboard = url.startsWith(TERMINAL_PAGE);
        if (!url.equals(webView.getUrl())) {
            webView.loadUrl(url);
        }
    }

    @Override
    public void onBackPressed() {
        if (webView.canGoBack()) {
            webView.goBack();
        } else {
            finish();
        }
    }

    @Override
    protected void onDestroy() {
        webView.destroy();
        super.onDestroy();
    }
}
