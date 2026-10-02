package app.polarbear;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.os.Build;
import android.text.Html;
import android.text.TextUtils;

/**
 * Android's clipboard, for the one shared with the desktop (src/android/clipboard.rs).
 *
 * Since Android 10 an app only gets to see the clipboard while one of its windows has focus;
 * writing to it is always allowed. Reading a clip (not describing it) makes Android 12+ tell the
 * user that the app pasted from the clipboard.
 */
final class Clipboard {
    /** On the clips made of the desktop's selections, to tell them from everybody else's. */
    private static final String LABEL = "Local Desktop";

    private static ClipboardManager.OnPrimaryClipChangedListener listener;
    /** Stands in for the clips' timestamps, which Android has since 8.0. */
    private static long changes;

    private Clipboard() {}

    private static ClipboardManager manager(Context context) {
        return (ClipboardManager)
                context.getApplicationContext().getSystemService(Context.CLIPBOARD_SERVICE);
    }

    /**
     * What is on the clipboard, without reading it: its timestamp, "own" for a clip written
     * here, "text" and "html" for what it has. Null without a clip or without access to it.
     */
    @SuppressWarnings("deprecation")
    static String[] describe(Context context) {
        ClipboardManager manager = manager(context);
        boolean hasText;
        ClipDescription description;
        try {
            // A text clip's type doesn't tell whether it has any: KDE Connect makes empty ones of
            // a PC clipboard without text. hasText() looks at the first item without reading the
            // clip. Asked first: if the access ends in between, that is no clip, not an empty one.
            hasText = manager.hasText();
            description = manager.getPrimaryClipDescription();
        } catch (RuntimeException e) {
            return null;
        }
        if (description == null) {
            return null;
        }
        long stamp = Build.VERSION.SDK_INT >= 26 ? description.getTimestamp() : changes;
        boolean own = LABEL.contentEquals(String.valueOf(description.getLabel()));
        boolean html = description.hasMimeType(ClipDescription.MIMETYPE_TEXT_HTML);
        boolean text = html
                || (hasText && description.hasMimeType(ClipDescription.MIMETYPE_TEXT_PLAIN));
        return new String[] {
            Long.toString(stamp), own ? "own" : "", text ? "text" : "", html ? "html" : ""
        };
    }

    /**
     * "text", the clip's text and its HTML (or null). Or why there is nothing to read: "empty"
     * for a clip without text, "gone" if Android gives none although the window has focus,
     * "unfocused" if it doesn't have it, or "failed" and the exception.
     */
    static String[] read(Context context) {
        ClipData clip;
        try {
            clip = manager(context).getPrimaryClip();
        } catch (RuntimeException e) {
            return new String[] {"failed", e.getClass().getName()};
        }
        if (clip == null) {
            // Android doesn't say whether it keeps the clipboard from the app.
            boolean focused =
                    !(context instanceof Activity) || ((Activity) context).hasWindowFocus();
            return new String[] {focused ? "gone" : "unfocused"};
        }
        // Several items are several things copied at once: one per line, as Android pastes them.
        StringBuilder text = new StringBuilder();
        StringBuilder html = new StringBuilder();
        boolean hasHtml = false;
        for (int i = 0; i < clip.getItemCount(); i++) {
            ClipData.Item item = clip.getItemAt(i);
            CharSequence itemText = item.getText();
            String itemHtml = item.getHtmlText();
            if (itemText == null && itemHtml == null) {
                continue;
            }
            if (text.length() > 0) {
                text.append('\n');
                html.append("<br>");
            }
            if (itemText != null) {
                text.append(itemText);
            }
            if (itemHtml != null) {
                hasHtml = true;
                html.append(itemHtml);
            } else if (itemText != null) {
                html.append(TextUtils.htmlEncode(itemText.toString()));
            }
        }
        if (text.length() == 0 && !hasHtml) {
            return new String[] {"empty"};
        }
        return new String[] {"text", text.toString(), hasHtml ? html.toString() : null};
    }

    /** Make a clip of the text, and its HTML if not null. Whether Android took it. */
    @SuppressWarnings("deprecation")
    static boolean write(Context context, String text, String html) {
        if (html != null && text.isEmpty()) {
            // Android wants a text next to every HTML.
            text = (Build.VERSION.SDK_INT >= 24 ? Html.fromHtml(html, 0) : Html.fromHtml(html))
                    .toString();
        }
        ClipData clip = html != null
                ? ClipData.newHtmlText(LABEL, text, html)
                : ClipData.newPlainText(LABEL, text);
        try {
            manager(context).setPrimaryClip(clip);
            return true;
        } catch (RuntimeException e) {
            // Too large for Binder, or a device that keeps the clipboard to itself.
            return false;
        }
    }

    /** Start or stop reporting changes of the clipboard with Native.onClipboardChanged(). */
    static synchronized void watch(Context context, boolean on) {
        if (on == (listener != null)) {
            return;
        }
        if (!on) {
            manager(context).removePrimaryClipChangedListener(listener);
            listener = null;
            return;
        }
        listener = new ClipboardManager.OnPrimaryClipChangedListener() {
            @Override
            public void onPrimaryClipChanged() {
                changes++;
                Native.onClipboardChanged();
            }
        };
        manager(context).addPrimaryClipChangedListener(listener);
    }
}
