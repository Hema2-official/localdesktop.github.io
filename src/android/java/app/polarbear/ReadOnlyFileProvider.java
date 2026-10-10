package app.polarbear;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import java.io.File;
import java.io.FileNotFoundException;

/**
 * Files the app hands other apps to read, through Android's grants for single URIs: the
 * provider isn't exported, so an app reads only the URIs it was given. A subclass maps its URIs
 * to files.
 */
abstract class ReadOnlyFileProvider extends ContentProvider {
    /** The file a URI stands for, or null for one there's nothing at. */
    abstract File file(Uri uri);

    @Override
    public boolean onCreate() {
        return true;
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
                // The name it has where the user picked it, not where links lead.
                row[i] = uri.getLastPathSegment();
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
