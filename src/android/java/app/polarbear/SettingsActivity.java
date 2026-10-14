package app.polarbear;

import android.content.Intent;
import android.net.Uri;
import android.webkit.WebView;

/**
 * Local Desktop's settings (assets/settings.html), which need no desktop: the launcher icon's
 * long-press menu (Shortcuts), Android's App info (its gear) and the desktop's menu open them.
 * They run in a task of their own, so that opening them never touches the desktop's.
 */
public class SettingsActivity extends WebPageActivity {
    static final String PAGE = "file:///android_asset/settings.html";
    /** Android's file picker, for where a backup goes and which one to restore. */
    static final int CHOOSE_BACKUP = 1;
    static final int CHOOSE_RESTORE = 2;

    private WebView page;
    private SettingsBridge bridge;

    @Override
    protected String pageUrl(Intent intent) {
        return PAGE;
    }

    @Override
    protected void prepare(WebView webView) {
        page = webView;
        bridge = new SettingsBridge(this);
        webView.addJavascriptInterface(bridge, "localdesktop");
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != CHOOSE_BACKUP && request != CHOOSE_RESTORE) {
            return;
        }
        Uri uri = result == RESULT_OK && data != null ? data.getData() : null;
        bridge.chosen(request, uri);
    }

    /** Hand the page the answer to one of its requests, a JSON value (or null). */
    void answer(final String function, final String json) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                page.evaluateJavascript(
                        "window." + function + " && " + function + "(" + json + ")", null);
            }
        });
    }
}
