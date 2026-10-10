package app.polarbear;

import android.content.Context;
import android.net.Uri;
import java.io.File;
import java.util.Locale;

/**
 * The images copied on the desktop, for the apps that paste them. The link leaves them in
 * files/clipboard/ (src/android/guest/clipboard.rs), Clipboard.writeImage() puts one on Android's
 * clipboard, and the clipboard lets each app that reads the clip read the image.
 */
public final class ClipProvider extends ReadOnlyFileProvider {
    static File directory(Context context) {
        return new File(context.getFilesDir(), "clipboard");
    }

    /** The URI of an image in directory(), as the manifest names the provider. */
    static Uri uri(Context context, String name) {
        return new Uri.Builder()
                .scheme("content")
                .authority(context.getPackageName() + ".clipboard")
                .appendPath(name)
                .build();
    }

    @Override
    File file(Uri uri) {
        String name = uri.getLastPathSegment();
        if (uri.getPathSegments().size() != 1 || name == null || name.startsWith(".")) {
            return null;
        }
        File file = new File(directory(getContext()), name);
        return file.isFile() ? file : null;
    }

    /** By the extension, which the link gives each image (core::clipboard::IMAGE_TYPES). */
    @Override
    public String getType(Uri uri) {
        String name = String.valueOf(uri.getLastPathSegment()).toLowerCase(Locale.ROOT);
        String extension = name.substring(name.lastIndexOf('.') + 1);
        switch (extension) {
            case "png":
                return "image/png";
            case "jpg":
                return "image/jpeg";
            case "webp":
                return "image/webp";
            case "gif":
                return "image/gif";
            case "bmp":
                return "image/bmp";
            default:
                return null;
        }
    }
}
