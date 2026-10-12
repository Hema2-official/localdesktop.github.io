package app.polarbear;

import android.content.Intent;
import android.webkit.WebView;

/**
 * Local Desktop's settings (assets/settings.html), which need no desktop: the launcher icon's
 * long-press menu (Shortcuts), Android's App info (its gear) and the desktop's menu open them.
 * They run in a task of their own, so that opening them never touches the desktop's.
 */
public class SettingsActivity extends WebPageActivity {
    static final String PAGE = "file:///android_asset/settings.html";

    @Override
    protected String pageUrl(Intent intent) {
        return PAGE;
    }

    @Override
    protected void prepare(WebView webView) {
        webView.addJavascriptInterface(new SettingsBridge(this), "localdesktop");
    }
}
