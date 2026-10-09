package app.polarbear;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.ClipData;
import android.content.ContentResolver;
import android.content.Context;
import android.content.DialogInterface;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Parcelable;
import android.provider.OpenableColumns;
import android.view.ContextThemeWrapper;
import android.webkit.MimeTypeMap;
import android.widget.ProgressBar;
import android.widget.Toast;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * What other apps hand the desktop: Android's share sheet and "Open with". Each share becomes a
 * request in the app's storage, files/shared/<request>/, with the files in it, or a link in its
 * ".link" file; the guest link moves the files into the desktop user's Downloads folder and opens
 * them there, or the link in the desktop's browser (src/android/guest/shared.rs). The folder is
 * ".<request>" until everything is in it.
 *
 * The activity is see-through and shows its progress only when copying takes a while. It stays
 * until the copy is done: the right to read what another app shares ends with it.
 */
public final class ShareActivity extends Activity {
    /** Copying shows its progress once it has taken this long, in ms. */
    private static final long QUIET_MS = 500;
    /** ext4 takes names of up to 255 bytes; this leaves room for " (2)" and the like. */
    private static final int NAME_BYTES = 200;

    private volatile boolean cancelled;
    private volatile long total;
    private volatile long copied;
    private boolean finished;
    private AlertDialog dialog;
    private ProgressBar progress;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        final Intent intent = getIntent();
        final File requests = new File(getFilesDir(), "shared");
        new Thread(new Runnable() {
            @Override
            public void run() {
                copy(intent, requests);
            }
        }, "share").start();
        new Handler(Looper.getMainLooper()).postDelayed(new Runnable() {
            @Override
            public void run() {
                showProgress();
            }
        }, QUIET_MS);
    }

    @Override
    protected void onDestroy() {
        // Android took the activity away before the copy was done, and the right to read with it.
        cancelled = true;
        if (dialog != null) {
            dialog.dismiss();
        }
        super.onDestroy();
    }

    /** Copy what the intent shares into a new request, then hand it to the desktop. */
    private void copy(Intent intent, File requests) {
        String id = String.format(Locale.ROOT, "%013d", System.currentTimeMillis());
        for (int n = 2; new File(requests, id).exists() || new File(requests, "." + id).exists(); n++) {
            id = String.format(Locale.ROOT, "%013d-%d", System.currentTimeMillis(), n);
        }
        File partial = new File(requests, "." + id);
        String what = "what was shared";
        try {
            if (!partial.mkdirs()) {
                throw new IOException("can't make " + partial);
            }
            ContentResolver resolver = getContentResolver();
            List<Uri> uris = sharedFiles(intent);
            if (uris.isEmpty()) {
                CharSequence text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT);
                if (text == null) {
                    throw new IOException("nothing came with it");
                }
                writeText(partial, text.toString(), intent.getStringExtra(Intent.EXTRA_SUBJECT));
            } else {
                long sum = 0;
                for (Uri uri : uris) {
                    long size = size(resolver, uri);
                    sum = size < 0 || sum < 0 ? -1 : sum + size;
                }
                total = sum;
                Set<String> names = new HashSet<>();
                for (Uri uri : uris) {
                    String name = unique(names, fileName(resolver, uri));
                    what = name;
                    if (cancelled) {
                        break;
                    }
                    copyOne(resolver, uri, new File(partial, name));
                }
            }
            if (cancelled) {
                delete(partial);
                finishQuietly();
                return;
            }
            if (!partial.renameTo(new File(requests, id))) {
                throw new IOException("can't finish " + partial);
            }
            handOver();
        } catch (IOException | RuntimeException e) {
            delete(partial);
            fail(what, e);
        }
    }

    /** The files an intent shares or opens. */
    private static List<Uri> sharedFiles(Intent intent) {
        List<Uri> uris = new ArrayList<>();
        String action = intent.getAction();
        if (Intent.ACTION_VIEW.equals(action)) {
            if (intent.getData() != null) {
                uris.add(intent.getData());
            }
        } else if (Intent.ACTION_SEND_MULTIPLE.equals(action)) {
            ArrayList<Parcelable> streams = intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
            if (streams != null) {
                for (Parcelable stream : streams) {
                    if (stream instanceof Uri) {
                        uris.add((Uri) stream);
                    }
                }
            }
        } else {
            Parcelable stream = intent.getParcelableExtra(Intent.EXTRA_STREAM);
            if (stream instanceof Uri) {
                uris.add((Uri) stream);
            }
        }
        // Some apps put what they share in the clip data alone.
        ClipData clip = intent.getClipData();
        if (uris.isEmpty() && clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                Uri uri = clip.getItemAt(i).getUri();
                if (uri != null) {
                    uris.add(uri);
                }
            }
        }
        return uris;
    }

    private void copyOne(ContentResolver resolver, Uri uri, File target) throws IOException {
        if ("file".equals(uri.getScheme())) {
            // Not the app's own files: those aren't another app's to hand out.
            String path = new File(String.valueOf(uri.getPath())).getCanonicalPath();
            String own = new File(getApplicationInfo().dataDir).getCanonicalPath();
            if (path.equals(own) || path.startsWith(own + "/")) {
                throw new IOException("that's Local Desktop's own file");
            }
        }
        try (InputStream in = resolver.openInputStream(uri);
                OutputStream out = new FileOutputStream(target)) {
            if (in == null) {
                throw new IOException("Android gave nothing to read");
            }
            byte[] buffer = new byte[1 << 16];
            int read;
            while ((read = in.read(buffer)) > 0 && !cancelled) {
                out.write(buffer, 0, read);
                copied += read;
            }
        }
    }

    /** A link opens in the desktop's browser; other text goes in a text file. */
    private static void writeText(File request, String text, String subject) throws IOException {
        String trimmed = text.trim();
        Uri link = Uri.parse(trimmed);
        boolean web = "http".equalsIgnoreCase(link.getScheme())
                || "https".equalsIgnoreCase(link.getScheme());
        if (web && !trimmed.matches("(?s).*\\s.*")) {
            write(new File(request, ".link"), trimmed);
            return;
        }
        String name = subject == null || subject.trim().isEmpty() ? "Shared text" : subject;
        write(new File(request, shorten(clean(name), 120) + ".txt"), text);
    }

    private static void write(File file, String text) throws IOException {
        try (OutputStream out = new FileOutputStream(file)) {
            out.write(text.getBytes(StandardCharsets.UTF_8));
        }
    }

    /** The size the provider tells, -1 if it doesn't. */
    private static long size(ContentResolver resolver, Uri uri) {
        if ("file".equals(uri.getScheme())) {
            File file = new File(String.valueOf(uri.getPath()));
            return file.isFile() ? file.length() : -1;
        }
        try (Cursor cursor = resolver.query(uri, new String[] {OpenableColumns.SIZE}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst() && !cursor.isNull(0)) {
                return cursor.getLong(0);
            }
        } catch (RuntimeException e) {
            // Not every provider answers queries; the size only drives the progress bar.
        }
        return -1;
    }

    /** The name the file had, as the desktop takes it, with an extension for its type. */
    private static String fileName(ContentResolver resolver, Uri uri) {
        String name = null;
        if ("file".equals(uri.getScheme())) {
            name = new File(String.valueOf(uri.getPath())).getName();
        } else {
            String[] columns = {OpenableColumns.DISPLAY_NAME};
            try (Cursor cursor = resolver.query(uri, columns, null, null, null)) {
                if (cursor != null && cursor.moveToFirst() && !cursor.isNull(0)) {
                    name = cursor.getString(0);
                }
            } catch (RuntimeException e) {
                // Not every provider answers queries.
            }
        }
        if (name == null || name.isEmpty()) {
            name = uri.getLastPathSegment();
        }
        name = clean(name == null ? "" : name);
        if (name.lastIndexOf('.') <= 0) {
            String type = resolver.getType(uri);
            String extension =
                    type == null ? null : MimeTypeMap.getSingleton().getExtensionFromMimeType(type);
            if (extension != null) {
                name += "." + extension;
            }
        }
        return shorten(name, NAME_BYTES);
    }

    /** No path, no control characters, not hidden. */
    static String clean(String name) {
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < name.length(); i++) {
            char c = name.charAt(i);
            out.append(c == '/' || c < 0x20 || c == 0x7f ? '_' : c);
        }
        String cleaned = out.toString().trim();
        while (cleaned.startsWith(".")) {
            cleaned = cleaned.substring(1);
        }
        return cleaned.isEmpty() ? "Shared file" : cleaned;
    }

    /** At most `bytes` bytes of UTF-8, keeping the extension. */
    static String shorten(String name, int bytes) {
        if (name.getBytes(StandardCharsets.UTF_8).length <= bytes) {
            return name;
        }
        int dot = name.lastIndexOf('.');
        String extension = dot > 0 && name.length() - dot <= 16 ? name.substring(dot) : "";
        String stem = name.substring(0, name.length() - extension.length());
        int budget = bytes - extension.getBytes(StandardCharsets.UTF_8).length;
        StringBuilder out = new StringBuilder();
        int used = 0;
        for (int i = 0; i < stem.length(); ) {
            int point = stem.codePointAt(i);
            int size = new String(Character.toChars(point)).getBytes(StandardCharsets.UTF_8).length;
            if (used + size > budget) {
                break;
            }
            out.appendCodePoint(point);
            used += size;
            i += Character.charCount(point);
        }
        return out + extension;
    }

    /** `name`, or with " (2)", " (3)" and so on if the request has one by that name. */
    private static String unique(Set<String> taken, String name) {
        int dot = name.lastIndexOf('.');
        String stem = dot > 0 ? name.substring(0, dot) : name;
        String extension = dot > 0 ? name.substring(dot) : "";
        String candidate = name;
        for (int n = 2; !taken.add(candidate); n++) {
            candidate = stem + " (" + n + ")" + extension;
        }
        return candidate;
    }

    private static void delete(File file) {
        File[] children = file.listFiles();
        if (children != null) {
            for (File child : children) {
                delete(child);
            }
        }
        file.delete();
    }

    private void showProgress() {
        if (finished || isFinishing()) {
            return;
        }
        int theme = Build.VERSION.SDK_INT >= 22
                ? android.R.style.Theme_DeviceDefault_Dialog_Alert
                : android.R.style.Theme_Material_Dialog_Alert;
        // In the dialog's style: the activity's own theme is the see-through one from before Material.
        Context styled = new ContextThemeWrapper(this, theme);
        progress = new ProgressBar(styled, null, android.R.attr.progressBarStyleHorizontal);
        progress.setMax(1000);
        progress.setIndeterminate(total <= 0);
        int padding = (int) (24 * getResources().getDisplayMetrics().density);
        progress.setPadding(padding, padding / 2, padding, 0);
        dialog = new AlertDialog.Builder(this, theme)
                .setTitle("Sending to the desktop")
                .setView(progress)
                .setCancelable(false)
                .setNegativeButton(android.R.string.cancel, new DialogInterface.OnClickListener() {
                    @Override
                    public void onClick(DialogInterface ignored, int which) {
                        cancelled = true;
                    }
                })
                .show();
        tick();
    }

    private void tick() {
        if (finished || dialog == null || !dialog.isShowing()) {
            return;
        }
        if (total > 0) {
            progress.setProgress((int) Math.min(1000, copied * 1000 / total));
        }
        progress.postDelayed(new Runnable() {
            @Override
            public void run() {
                tick();
            }
        }, 200);
    }

    /** The request is complete: tell the desktop and bring it to the front. */
    private void handOver() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    Native.onShared();
                } catch (UnsatisfiedLinkError e) {
                    // The desktop isn't running yet; it looks for requests when it starts.
                }
                Intent desktop = getPackageManager().getLaunchIntentForPackage(getPackageName());
                if (desktop != null) {
                    startActivity(desktop);
                }
                finishQuietly();
            }
        });
    }

    private void fail(final String what, final Exception error) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                String why = error.getMessage() != null ? error.getMessage() : error.toString();
                String message = "Local Desktop couldn't take " + what + ": " + why;
                Toast.makeText(ShareActivity.this, message, Toast.LENGTH_LONG).show();
                finishQuietly();
            }
        });
    }

    private void finishQuietly() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                finished = true;
                if (dialog != null) {
                    dialog.dismiss();
                }
                finish();
            }
        });
    }
}
