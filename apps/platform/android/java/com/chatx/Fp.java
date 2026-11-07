package com.chatx;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.util.Log;

import java.io.File;
import java.io.FileNotFoundException;
import java.util.Locale;

/**
 * ContentProvider that exposes a fixed root of the app's private file tree as
 * {@code content://com.chatx.fp/<relative/path>} URIs.
 *
 * <p>Used by {@link NativeBridge#reveal(String)} to share a file with an
 * external viewer (ACTION_VIEW / chooser) without requiring
 * READ_EXTERNAL_STORAGE.
 *
 * <p>Layout: {@link Shell#activity()}.{@code getFilesDir()} is the root; the
 * URI's path segment (everything after the leading {@code /}) maps 1:1 to
 * a relative path under it. Traversal outside that root is refused.
 *
 * <p>Declared in {@code AndroidManifest.xml}:
 * <pre>
 *   &lt;provider
 *       android:name="com.chatx.Fp"
 *       android:authorities="com.chatx.fp"
 *       android:grantUriPermissions="true"
 *       android:exported="false" /&gt;
 * </pre>
 */
public final class Fp extends ContentProvider {
    private static final String TAG = "chatx.fp";
    public static final String AUTHORITY = "com.chatx.fp";

    @Override public boolean onCreate() { return true; }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection,
            String[] selectionArgs, String sortOrder) {
        return null; // we serve one file per URI via openFile
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException();
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        throw new UnsupportedOperationException();
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) {
        throw new UnsupportedOperationException();
    }

    @Override
    public String getType(Uri uri) {
        return mimeFor(uri.getLastPathSegment());
    }

    /** Open the file at {@code uri} for reading. Returns a ParcelFileDescriptor
     *  (framework-37 signature for {@link ContentProvider#openFile}). */
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        File f = resolve(uri);
        if (f == null || !f.isFile()) {
            throw new FileNotFoundException("not a file: " + uri);
        }
        try {
            return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY);
        } catch (java.io.IOException e) {
            throw new FileNotFoundException("openFile failed: " + e);
        }
    }

    /** Build the content:// URI for an absolute fs path under the app's private
     *  files dir (i.e. {@code <filesDir>/<rel>}). Returns {@code null} if
     *  {@code path} is not under the files dir. */
    public static Uri uriFor(android.app.Activity a, File file) {
        if (a == null || file == null) return null;
        File base = a.getFilesDir();
        return uriFor(base, file);
    }

    /** Same as {@link #uriFor(android.app.Activity, File)} but given the
     *  explicit base directory. */
    public static Uri uriFor(File baseDir, File file) {
        try {
            String baseAbs = baseDir.getCanonicalPath();
            String fileAbs = file.getCanonicalPath();
            if (!fileAbs.startsWith(baseAbs + File.separator)) return null;
            String rel = fileAbs.substring(baseAbs.length() + 1);
            Uri.Builder b = Uri.parse("content://" + AUTHORITY).buildUpon();
            for (String seg : rel.split(java.io.File.separator)) {
                if (seg.isEmpty()) continue;
                b.appendPath(seg);
            }
            Uri u = b.build();
            Log.i(TAG, "uriFor " + fileAbs + " -> " + u);
            return u;
        } catch (Throwable t) {
            Log.w(TAG, "uriFor failed: " + t);
            return null;
        }
    }

    /** Map {@code content://com.chatx.fp/<rel>} → the activity's
     *  {@code getFilesDir()/<rel>}. Returns {@code null} if the path escapes
     *  the private root. */
    static File resolve(Uri uri) {
        if (uri == null) return null;
        String path = uri.getPath();
        if (path == null || path.isEmpty()) return null;
        if (path.startsWith("/")) path = path.substring(1);
        if (path.isEmpty()) return null;
        android.app.Activity a = Shell.activity();
        if (a == null) return null;
        File base = a.getFilesDir();
        File candidate = new File(base, path);
        try {
            if (!candidate.getCanonicalPath().startsWith(base.getCanonicalPath() + File.separator)) {
                return null;
            }
        } catch (Throwable ignored) {
            return null;
        }
        return candidate;
    }

    /** Very small extension → MIME map, enough for the ACTION_VIEW chooser.
     *  Defaults to {@code application/octet-stream}. */
    public static String mimeFor(String name) {
        if (name == null) return "application/octet-stream";
        int dot = name.lastIndexOf('.');
        if (dot < 0 || dot == name.length() - 1) return "application/octet-stream";
        String ext = name.substring(dot + 1).toLowerCase(Locale.ROOT);
        switch (ext) {
            case "pdf":  return "application/pdf";
            case "txt":  return "text/plain";
            case "html": case "htm": return "text/html";
            case "csv":  return "text/csv";
            case "json": return "application/json";
            case "xml":  return "application/xml";
            case "zip":  return "application/zip";
            case "doc":  return "application/msword";
            case "docx": return "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
            case "xls":  return "application/vnd.ms-excel";
            case "xlsx": return "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
            case "ppt":  return "application/vnd.ms-powerpoint";
            case "pptx": return "application/vnd.openxmlformats-officedocument.presentationml.presentation";
            case "jpg": case "jpeg": return "image/" + ext;
            case "png":  return "image/png";
            case "gif":  return "image/gif";
            case "webp": return "image/webp";
            case "mp3":  return "audio/mpeg";
            case "wav":  return "audio/wav";
            case "mp4":  return "video/mp4";
            default:     return "application/octet-stream";
        }
    }
}
