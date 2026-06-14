package io.ntrack.app;

import android.Manifest;
import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.location.Location;
import android.location.LocationListener;
import android.location.LocationManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Looper;
import android.util.Log;

import java.util.ArrayList;
import java.util.List;

/**
 * Static bridge between the Rust core and the Android platform.
 *
 * Rust calls the public static methods (from arbitrary threads — every
 * method hops to the main looper internally where required) and receives
 * results through the two native callbacks, which Rust registers at startup
 * via JNI RegisterNatives:
 *
 *   nativeOnLocation(lat, lng, accuracy, timeMillis)  — each location fix
 *   nativeOnPermission(granted)                        — permission outcome
 *
 * The bridge deliberately takes no Context/Activity parameters: the
 * android-activity glue only exposes the Application context to native
 * code, and passing it where an Activity is expected trips CheckJNI. The
 * live activity is always resolved via {@link MainActivity#current()}.
 */
public final class LocationBridge {
    private static final String TAG = "ntrack";
    private static final int REQ_LOCATION = 4242;

    private LocationBridge() {}

    static native void nativeOnLocation(double lat, double lng, float accuracy, long timeMillis);
    static native void nativeOnPermission(boolean granted);
    static native void nativeOnInsets(int top, int bottom, int left, int right);

    private static LocationListener listener;

    // ---- window insets (edge-to-edge) ------------------------------------

    // Cached so a late-registering native side can be re-flushed via
    // requestInsets(): the activity's inset listener may fire before Rust has
    // called RegisterNatives.
    private static volatile boolean haveInsets;
    private static volatile int insetTop, insetBottom, insetLeft, insetRight;

    /**
     * Forward window insets (physical pixels) to native code. Called by
     * {@link MainActivity}'s OnApplyWindowInsetsListener.
     */
    public static void dispatchInsets(int top, int bottom, int left, int right) {
        insetTop = top;
        insetBottom = bottom;
        insetLeft = left;
        insetRight = right;
        haveInsets = true;
        try {
            nativeOnInsets(top, bottom, left, right);
        } catch (UnsatisfiedLinkError e) {
            // Native callback not registered yet — requestInsets() re-flushes.
        }
    }

    /**
     * Re-emit the last known insets and force a fresh dispatch. Rust calls this
     * once it has registered the native callbacks, covering the startup race
     * where the first inset dispatch arrived before RegisterNatives.
     */
    public static void requestInsets() {
        if (haveInsets) {
            try {
                nativeOnInsets(insetTop, insetBottom, insetLeft, insetRight);
            } catch (UnsatisfiedLinkError e) {
                Log.e(TAG, "nativeOnInsets unavailable in requestInsets", e);
            }
        }
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(() ->
                activity.getWindow().getDecorView().requestApplyInsets());
    }

    // ---- permissions -----------------------------------------------------

    public static boolean hasLocationPermission() {
        Activity activity = MainActivity.current();
        if (activity == null) return false;
        // Android 12+ lets users grant approximate location only; coarse
        // fixes are still useful for live sharing.
        return activity.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                == PackageManager.PERMISSION_GRANTED
                || activity.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION)
                == PackageManager.PERMISSION_GRANTED;
    }

    public static void requestLocationPermission() {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "requestLocationPermission: no live activity");
            nativeOnPermission(false);
            return;
        }
        activity.runOnUiThread(() -> {
            List<String> perms = new ArrayList<>();
            perms.add(Manifest.permission.ACCESS_FINE_LOCATION);
            perms.add(Manifest.permission.ACCESS_COARSE_LOCATION);
            if (Build.VERSION.SDK_INT >= 33) {
                // Needed so the foreground-service notification is visible.
                perms.add(Manifest.permission.POST_NOTIFICATIONS);
            }
            activity.requestPermissions(perms.toArray(new String[0]), REQ_LOCATION);
        });
    }

    /** Called by MainActivity with the system permission dialog outcome. */
    public static void handlePermissionResult(int requestCode, String[] permissions, int[] results) {
        if (requestCode != REQ_LOCATION) return;
        boolean granted = false;
        for (int i = 0; i < permissions.length && i < results.length; i++) {
            boolean isLocation =
                    Manifest.permission.ACCESS_FINE_LOCATION.equals(permissions[i])
                            || Manifest.permission.ACCESS_COARSE_LOCATION.equals(permissions[i]);
            if (isLocation && results[i] == PackageManager.PERMISSION_GRANTED) {
                granted = true;
            }
        }
        try {
            nativeOnPermission(granted);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native callbacks not registered yet", e);
        }
    }

    // ---- location updates -------------------------------------------------

    /**
     * Start the foreground service (keeps the process and GPS alive while
     * the app is backgrounded) and subscribe to location updates.
     */
    public static void startLocation(final long intervalMs) {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "startLocation: no live activity");
            nativeOnPermission(false);
            return;
        }
        activity.runOnUiThread(() -> {
            if (!hasLocationPermission()) {
                Log.w(TAG, "startLocation without permission");
                nativeOnPermission(false);
                return;
            }
            try {
                Intent svc = new Intent(activity, LocationService.class);
                activity.startForegroundService(svc);
            } catch (Exception e) {
                // The app can still share while in the foreground.
                Log.e(TAG, "failed to start foreground service", e);
            }
            try {
                LocationManager lm =
                        (LocationManager) activity.getSystemService(Context.LOCATION_SERVICE);
                stopListening(activity);
                listener = new LocationListener() {
                    @Override
                    public void onLocationChanged(Location location) {
                        nativeOnLocation(
                                location.getLatitude(),
                                location.getLongitude(),
                                location.getAccuracy(),
                                location.getTime() > 0 ? location.getTime()
                                        : System.currentTimeMillis());
                    }
                    @Override public void onStatusChanged(String provider, int status, Bundle extras) {}
                    @Override public void onProviderEnabled(String provider) {}
                    @Override public void onProviderDisabled(String provider) {}
                };
                boolean any = false;
                for (String provider : pickProviders(lm)) {
                    lm.requestLocationUpdates(provider, Math.max(intervalMs / 2, 1000L), 0f,
                            listener, Looper.getMainLooper());
                    any = true;
                    Log.i(TAG, "location updates from " + provider);
                    Location last = lm.getLastKnownLocation(provider);
                    if (last != null
                            && System.currentTimeMillis() - last.getTime() < 60_000) {
                        listener.onLocationChanged(last);
                    }
                }
                if (!any) {
                    Log.w(TAG, "no usable location provider");
                }
            } catch (SecurityException e) {
                Log.e(TAG, "location permission lost", e);
                nativeOnPermission(false);
            }
        });
    }

    private static List<String> pickProviders(LocationManager lm) {
        List<String> out = new ArrayList<>();
        List<String> all = lm.getAllProviders();
        if (Build.VERSION.SDK_INT >= 31 && all.contains(LocationManager.FUSED_PROVIDER)) {
            out.add(LocationManager.FUSED_PROVIDER);
            return out;
        }
        if (all.contains(LocationManager.GPS_PROVIDER)) out.add(LocationManager.GPS_PROVIDER);
        if (all.contains(LocationManager.NETWORK_PROVIDER)) out.add(LocationManager.NETWORK_PROVIDER);
        return out;
    }

    public static void stopLocation() {
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            stopListening(activity);
            activity.stopService(new Intent(activity, LocationService.class));
        });
    }

    private static void stopListening(Activity activity) {
        if (listener != null) {
            LocationManager lm =
                    (LocationManager) activity.getSystemService(Context.LOCATION_SERVICE);
            try {
                lm.removeUpdates(listener);
            } catch (Exception e) {
                Log.w(TAG, "removeUpdates failed", e);
            }
            listener = null;
        }
    }

    // ---- misc platform actions --------------------------------------------

    public static void openMap(final double lat, final double lng, final String label) {
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            String coords = lat + "," + lng;
            Uri geo = Uri.parse("geo:" + coords + "?q=" + coords
                    + "(" + Uri.encode(label == null ? "shared location" : label) + ")");
            try {
                activity.startActivity(new Intent(Intent.ACTION_VIEW, geo));
            } catch (ActivityNotFoundException e) {
                Uri web = Uri.parse("https://www.openstreetmap.org/?mlat=" + lat
                        + "&mlon=" + lng + "#map=16/" + lat + "/" + lng);
                try {
                    activity.startActivity(new Intent(Intent.ACTION_VIEW, web));
                } catch (ActivityNotFoundException e2) {
                    Log.e(TAG, "no app can open a map or browser");
                }
            }
        });
    }

    public static void copyText(final String text) {
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            ClipboardManager cm =
                    (ClipboardManager) activity.getSystemService(Context.CLIPBOARD_SERVICE);
            ClipData clip = ClipData.newPlainText("ntrack", text);
            if (Build.VERSION.SDK_INT >= 33) {
                // Mark sensitive so launchers don't preview group secrets.
                android.os.PersistableBundle extras = new android.os.PersistableBundle();
                extras.putBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE, true);
                clip.getDescription().setExtras(extras);
            }
            cm.setPrimaryClip(clip);
        });
    }

    public static void shareText(final String text) {
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(() -> {
            Intent send = new Intent(Intent.ACTION_SEND);
            send.setType("text/plain");
            send.putExtra(Intent.EXTRA_TEXT, text);
            try {
                activity.startActivity(Intent.createChooser(send, "Share group key"));
            } catch (ActivityNotFoundException e) {
                Log.e(TAG, "no app can share text");
            }
        });
    }
}
