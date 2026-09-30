package io.trafficpolice;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.net.Uri;
import android.util.Log;

/**
 * Starts capture when the app's main process starts (ARCHITECTURE.md §4.6). A plain
 * ContentProvider (no androidx). Android creates manifest providers only in the process they are
 * declared for, so secondary processes call {@link TrafficPolice#start} themselves.
 */
public final class TrafficPoliceInitProvider extends ContentProvider {
    @Override
    public boolean onCreate() {
        try {
            TrafficPolice.start(getContext());
        } catch (Throwable t) {
            Log.w("TrafficPolice", "could not start capture", t);
        }
        return true;
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] selectionArgs, String sortOrder) {
        return null;
    }

    @Override
    public String getType(Uri uri) {
        return null;
    }

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
        return 0;
    }
}
