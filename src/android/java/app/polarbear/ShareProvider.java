package app.polarbear;

import android.content.Context;
import android.net.Uri;
import android.os.Environment;
import android.webkit.MimeTypeMap;
import java.io.File;
import java.io.IOException;
import java.util.List;
import java.util.Locale;

/**
 * Files the desktop shares with Android's apps (`localdesktop share`, Commands.share()), where
 * they are: in the Linux system (rootfs/…) or in Android's shared storage (storage/…), as
 * src/core/control.rs names the two. The share sheet lets the app the user picks read them.
 */
public final class ShareProvider extends ReadOnlyFileProvider {
    /** The URI of a file at `path` in `root`, as the manifest names the provider. */
    static Uri uri(Context context, String root, String path) {
        Uri.Builder builder = new Uri.Builder()
                .scheme("content")
                .authority(context.getPackageName() + ".share")
                .appendPath(root);
        for (String segment : path.split("/")) {
            if (!segment.isEmpty()) {
                builder.appendPath(segment);
            }
        }
        return builder.build();
    }

    private static File root(Context context, String name) {
        if ("rootfs".equals(name)) {
            return new File(context.getFilesDir(), "arch");
        }
        if ("storage".equals(name)) {
            return Environment.getExternalStorageDirectory();
        }
        return null;
    }

    /**
     * What a URI stands for, if it is in its root once links are followed (the Linux system's
     * hard links are links into rootfs/.l2s); null otherwise.
     */
    static File resolve(Context context, Uri uri) {
        List<String> segments = uri.getPathSegments();
        File root = segments.size() < 2 ? null : root(context, segments.get(0));
        if (root == null) {
            return null;
        }
        File file = root;
        for (String segment : segments.subList(1, segments.size())) {
            file = new File(file, segment);
        }
        try {
            String inside = root.getCanonicalPath() + File.separator;
            return file.getCanonicalPath().startsWith(inside) ? file : null;
        } catch (IOException e) {
            return null;
        }
    }

    /** The file a URI stands for, or null (see resolve()). */
    static File file(Context context, Uri uri) {
        File file = resolve(context, uri);
        return file != null && file.isFile() ? file : null;
    }

    /** By the extension, as Android knows them. */
    static String typeOf(String name) {
        int dot = name.lastIndexOf('.');
        String type = dot < 0
                ? null
                : MimeTypeMap.getSingleton()
                        .getMimeTypeFromExtension(name.substring(dot + 1).toLowerCase(Locale.ROOT));
        return type != null ? type : "application/octet-stream";
    }

    @Override
    File file(Uri uri) {
        return file(getContext(), uri);
    }

    @Override
    public String getType(Uri uri) {
        return typeOf(String.valueOf(uri.getLastPathSegment()));
    }
}
