package io.ntrack.app;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import android.util.Log;

import java.io.File;
import java.io.FileNotFoundException;

/**
 * Minimal, read-only {@link ContentProvider} that serves the single rotating
 * export file written under {@code cacheDir/shared/} (see
 * {@link LocationBridge#shareFile}). It exists so an exported GPX track can be
 * handed to another app via a {@code content://} URI: {@code file://} URIs are
 * illegal across app boundaries on API 24+, and androidx {@code FileProvider}
 * is deliberately avoided to keep the app dependency-free.
 *
 * <p>Only the framework {@code android.*}/{@code java.io.*} APIs are used. The
 * provider is not exported (see the manifest); access is granted per-intent
 * via {@code FLAG_GRANT_READ_URI_PERMISSION}.
 */
public final class FileBridgeProvider extends ContentProvider {
    private static final String TAG = "ntrack";
    /** Must match the {@code android:authorities} in AndroidManifest.xml. */
    static final String AUTHORITY = "io.ntrack.app.files";
    /** Subdirectory of {@code cacheDir} the shared file lives in. */
    static final String SHARED_DIR = "shared";
    private static final String MIME = "application/gpx+xml";

    @Override
    public boolean onCreate() {
        return true;
    }

    /**
     * Map a content URI to a file under {@code cacheDir/shared}, stripping any
     * path components from the last segment as a traversal guard. Returns
     * {@code null} for an unusable URI or a missing context.
     */
    private File resolve(Uri uri) {
        if (getContext() == null) return null;
        String last = uri.getLastPathSegment();
        if (last == null) return null;
        // getName() discards anything up to the final separator, so a crafted
        // "../../foo" can never escape the shared directory.
        String name = new File(last).getName();
        if (name.isEmpty()) return null;
        File dir = new File(getContext().getCacheDir(), SHARED_DIR);
        return new File(dir, name);
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        File f = resolve(uri);
        if (f == null || !f.exists()) {
            throw new FileNotFoundException(String.valueOf(uri));
        }
        // Read-only: this provider never accepts writes from other apps.
        return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY);
    }

    /**
     * Answer the {@link OpenableColumns} a chooser/receiver queries for so it
     * can show a sensible file name and size. Any other requested column is
     * returned as null.
     */
    @Override
    public Cursor query(Uri uri, String[] projection, String selection,
                        String[] selectionArgs, String sortOrder) {
        File f = resolve(uri);
        if (f == null || !f.exists()) return null;
        String[] cols = (projection != null) ? projection
                : new String[] { OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE };
        Object[] row = new Object[cols.length];
        for (int i = 0; i < cols.length; i++) {
            if (OpenableColumns.DISPLAY_NAME.equals(cols[i])) {
                row[i] = f.getName();
            } else if (OpenableColumns.SIZE.equals(cols[i])) {
                row[i] = f.length();
            } else {
                row[i] = null;
            }
        }
        MatrixCursor cursor = new MatrixCursor(cols, 1);
        cursor.addRow(row);
        return cursor;
    }

    @Override
    public String getType(Uri uri) {
        return MIME;
    }

    // Read-only provider: mutation operations are unsupported no-ops.

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        return null;
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        return 0;
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) {
        Log.w(TAG, "FileBridgeProvider is read-only; update ignored");
        return 0;
    }
}
