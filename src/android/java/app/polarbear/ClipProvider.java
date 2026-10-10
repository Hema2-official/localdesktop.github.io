package app.polarbear;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import java.io.File;
import java.io.FileNotFoundException;
import java.util.Locale;

/**
 * The images copied on the desktop, for the apps that paste them. The link leaves them in
 * files/clipboard/ (src/android/guest/clipboard.rs), Clipboard.writeImage() puts one on Android's
 * clipboard, and the clipboard lets each app that reads the clip read the image. Read only.
 */
public final class ClipProvider extends ContentProvider {
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
    public boolean onCreate() {
        return true;
    }

    /** The image a URI stands for, or null. */
    private File file(Uri uri) {
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

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        File file = file(uri);
        if (file == null || !"r".equals(mode)) {
            throw new FileNotFoundException(uri + " (" + mode + ")");
        }
        return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY);
    }

    /** The name and size, which apps ask for before they read a file they are given. */
    @Override
    public Cursor query(
            Uri uri, String[] projection, String selection, String[] arguments, String order) {
        File file = file(uri);
        if (file == null) {
            return null;
        }
        String[] columns = projection != null
                ? projection
                : new String[] {OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE};
        Object[] row = new Object[columns.length];
        for (int i = 0; i < columns.length; i++) {
            if (OpenableColumns.DISPLAY_NAME.equals(columns[i])) {
                row[i] = file.getName();
            } else if (OpenableColumns.SIZE.equals(columns[i])) {
                row[i] = file.length();
            }
        }
        MatrixCursor cursor = new MatrixCursor(columns, 1);
        cursor.addRow(row);
        return cursor;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("read only");
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] arguments) {
        throw new UnsupportedOperationException("read only");
    }

    @Override
    public int delete(Uri uri, String selection, String[] arguments) {
        throw new UnsupportedOperationException("read only");
    }
}
