package app.polarbear;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.ImageDecoder;
import android.net.Uri;
import android.os.Build;
import android.os.ParcelFileDescriptor;
import android.text.Html;
import android.text.TextUtils;
import java.io.ByteArrayOutputStream;
import java.io.Closeable;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.ByteBuffer;

/**
 * Android's clipboard, for the one shared with the desktop (src/android/clipboard.rs).
 *
 * Since Android 10 an app only gets to see the clipboard while one of its windows has focus;
 * writing to it is always allowed. Reading a clip (not describing it) makes Android 12+ tell the
 * user that the app pasted from the clipboard.
 *
 * An image is a content URI on the clipboard, which the clipboard lets each app that reads the
 * clip read. The desktop's images are files of the app's, see ClipProvider.
 */
final class Clipboard {
    /** On the clips made of the desktop's selections, to tell them from everybody else's. */
    private static final String LABEL = "Local Desktop";
    /** How large an image may be, as core::clipboard::IMAGE_LIMIT. */
    private static final int IMAGE_LIMIT = 64 << 20;

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
     * here, "text" and "html" for what it has, and the type of its image. Null without a clip or
     * without access to it.
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
        String image = imageType(description);
        return new String[] {
            Long.toString(stamp),
            own ? "own" : "",
            text ? "text" : "",
            html ? "html" : "",
            image != null ? image : ""
        };
    }

    /** The type of a clip's image, or null if it has none. */
    private static String imageType(ClipDescription description) {
        for (int i = 0; i < description.getMimeTypeCount(); i++) {
            String type = description.getMimeType(i);
            if (type != null && type.regionMatches(true, 0, "image/", 0, 6)) {
                return type;
            }
        }
        return null;
    }

    /** Why Android gave no clip: it doesn't say whether it keeps the clipboard from the app. */
    private static String unread(Context context) {
        boolean focused = !(context instanceof Activity) || ((Activity) context).hasWindowFocus();
        return focused ? "gone" : "unfocused";
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
            return new String[] {unread(context)};
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
        return set(context, clip);
    }

    /** Make a clip of the image `name` in ClipProvider's directory. Whether Android took it. */
    static boolean writeImage(Context context, String name, String type) {
        ClipData clip = new ClipData(
                new ClipDescription(LABEL, new String[] {type}),
                new ClipData.Item(ClipProvider.uri(context, name)));
        return set(context, clip);
    }

    private static boolean set(Context context, ClipData clip) {
        try {
            manager(context).setPrimaryClip(clip);
            return true;
        } catch (RuntimeException e) {
            // Too large for Binder, or a device that keeps the clipboard to itself.
            return false;
        }
    }

    /**
     * Start writing the clip's image into the pipe `fd`, or rather into a copy of it, which is
     * closed at the end: as it is if it has type `type`, else as PNG made of it. "image" once
     * that is under way, or why there is nothing to read, as read() tells it.
     */
    static String[] readImage(Context context, int fd, String type) {
        ClipData clip;
        try {
            clip = manager(context).getPrimaryClip();
        } catch (RuntimeException e) {
            return new String[] {"failed", e.getClass().getName()};
        }
        if (clip == null) {
            return new String[] {unread(context)};
        }
        Uri uri = null;
        for (int i = 0; i < clip.getItemCount() && uri == null; i++) {
            uri = clip.getItemAt(i).getUri();
        }
        String described = imageType(clip.getDescription());
        if (uri == null || described == null) {
            return new String[] {"empty"};
        }
        final InputStream image;
        final ParcelFileDescriptor pipe;
        try {
            // Allowed by the clipboard until the clip is replaced.
            image = context.getContentResolver().openInputStream(uri);
            if (image == null) {
                return new String[] {"failed", "no stream"};
            }
        } catch (IOException | RuntimeException e) {
            return new String[] {"failed", e.getClass().getName()};
        }
        try {
            pipe = ParcelFileDescriptor.fromFd(fd);
        } catch (IOException e) {
            close(image);
            return new String[] {"failed", e.getClass().getName()};
        }
        final boolean asItIs = type.equalsIgnoreCase(described);
        // Making a PNG of a photo takes seconds, which the link has no time for.
        new Thread(new Runnable() {
            @Override
            public void run() {
                OutputStream out = new ParcelFileDescriptor.AutoCloseOutputStream(pipe);
                try {
                    if (asItIs) {
                        copy(image, out);
                    } else {
                        png(image, out);
                    }
                } catch (IOException | RuntimeException | OutOfMemoryError e) {
                    // The link sees the pipe end early, and what that means. It also ends this
                    // when it has had enough.
                } finally {
                    close(image);
                    close(out);
                }
            }
        }, "clipboard-image").start();
        return new String[] {"image"};
    }

    private static void copy(InputStream in, OutputStream out) throws IOException {
        byte[] buffer = new byte[65536];
        int read;
        while ((read = in.read(buffer)) != -1) {
            out.write(buffer, 0, read);
        }
    }

    /** Make a PNG of an image of any type Android knows. */
    private static void png(InputStream in, OutputStream out) throws IOException {
        Bitmap bitmap;
        if (Build.VERSION.SDK_INT >= 28) {
            // Turned as a photo's EXIF data says, which a PNG has no place for.
            ImageDecoder.Source encoded = ImageDecoder.createSource(ByteBuffer.wrap(readAll(in)));
            bitmap = ImageDecoder.decodeBitmap(encoded, new ImageDecoder.OnHeaderDecodedListener() {
                @Override
                public void onHeaderDecoded(ImageDecoder decoder, ImageDecoder.ImageInfo info,
                        ImageDecoder.Source source) {
                    // compress() needs the pixels, which a hardware bitmap keeps on the GPU.
                    decoder.setAllocator(ImageDecoder.ALLOCATOR_SOFTWARE);
                }
            });
        } else {
            bitmap = BitmapFactory.decodeStream(in);
        }
        if (bitmap == null) {
            throw new IOException("not an image");
        }
        try {
            // Written while it is made, so programs on the desktop don't give up waiting.
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out);
        } finally {
            bitmap.recycle();
        }
    }

    private static byte[] readAll(InputStream in) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        byte[] buffer = new byte[65536];
        int read;
        while ((read = in.read(buffer)) != -1) {
            bytes.write(buffer, 0, read);
            if (bytes.size() > IMAGE_LIMIT) {
                throw new IOException("too large");
            }
        }
        return bytes.toByteArray();
    }

    private static void close(Closeable closeable) {
        try {
            closeable.close();
        } catch (IOException e) {
            // Nothing left to do with it.
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
