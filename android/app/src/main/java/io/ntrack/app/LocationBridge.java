package io.ntrack.app;

import android.Manifest;
import android.app.Activity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
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
import android.os.Handler;
import android.os.Looper;
import android.util.Log;

import java.io.File;
import java.io.FileOutputStream;
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
    private static final int REQ_BACKGROUND = 4243;
    /** High-importance channel for duress alerts and check-in escalations,
     * separate from the low-importance ongoing-sharing channel. */
    private static final String ALERT_CHANNEL_ID = "ntrack.alert";
    /** Rolling id so successive alerts stack rather than overwrite. */
    private static int alertNotificationId = 100;

    private LocationBridge() {}

    static native void nativeOnLocation(double lat, double lng, float accuracy, long timeMillis);
    static native void nativeOnPermission(boolean granted);
    /** Decode one camera frame's luminance plane; returns the payload or null. */
    static native String nativeDecodeQr(byte[] luma, int width, int height, int rowStride);
    /** A QR code was scanned. */
    static native void nativeOnQrResult(String text);
    /** An {@code ntrack://join} deep link launched or resumed the app. */
    static native void nativeOnDeepLink(String uri);

    private static LocationListener listener;

    /** Whether the current session has delivered at least one fix. Until it has,
     * we keep the GPS radio continuously powered (see {@link #ACQUIRE_INTERVAL_MS})
     * so a cold start can complete: a long minTime lets Android rest the radio
     * between sparse windows, which stalls the first fix and flickers the OS
     * location-access indicator off. Reset at the start/stop of each session. */
    private static boolean acquiredFix;
    /** GPS sampling interval (ms) while acquiring the first fix — short enough to
     * keep the radio powered (≈continuous tracking) rather than duty-cycling. */
    private static final long ACQUIRE_INTERVAL_MS = 1000L;

    // Deep links can arrive (in MainActivity.onCreate) before the Rust side has
    // registered its native callbacks, so buffer the latest one until Rust is
    // ready and flushes it.
    private static String pendingDeepLink;
    private static boolean nativeReady;

    // ---- context resolution ----------------------------------------------

    /**
     * The Context used to drive location and check permission: the live
     * activity while the app is open, else the boot foreground service. Both are
     * Contexts; only an activity can host a permission dialog (see
     * {@link #requestLocationPermission}).
     */
    private static Context locationContext() {
        Activity activity = MainActivity.current();
        if (activity != null) return activity;
        return LocationService.current();
    }

    /** Post onto the main looper — works from any context, unlike an activity's
     * runOnUiThread. */
    private static void post(Runnable r) {
        new Handler(Looper.getMainLooper()).post(r);
    }

    // ---- permissions -----------------------------------------------------

    private static boolean hasForeground(Context ctx) {
        // Android 12+ lets users grant approximate location only; coarse fixes
        // are still useful for live sharing.
        return ctx.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                        == PackageManager.PERMISSION_GRANTED
                || ctx.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION)
                        == PackageManager.PERMISSION_GRANTED;
    }

    private static boolean hasBackground(Context ctx) {
        // Before Android 10 there is no separate background-location permission;
        // a granted foreground permission already works in the background.
        if (Build.VERSION.SDK_INT < 29) return true;
        return ctx.checkSelfPermission(Manifest.permission.ACCESS_BACKGROUND_LOCATION)
                == PackageManager.PERMISSION_GRANTED;
    }

    /**
     * Whether we may share. Sharing keeps publishing with the app backgrounded
     * and resumes after a reboot, both of which require "Allow all the time" —
     * so background location is mandatory, not optional. There is deliberately
     * no degraded foreground-only mode.
     */
    public static boolean hasLocationPermission() {
        Context ctx = locationContext();
        return ctx != null && hasForeground(ctx) && hasBackground(ctx);
    }

    /**
     * Request the permissions sharing needs. Foreground (fine/coarse, plus
     * notifications so the service is visible) comes first; once granted,
     * {@link #handlePermissionResult} escalates to background location, which
     * Android 11+ requires as a separate step (it opens Settings to pick
     * "Allow all the time"). Only an activity can show these prompts.
     */
    public static void requestLocationPermission() {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "requestLocationPermission: no live activity");
            reportPermission(false);
            return;
        }
        activity.runOnUiThread(() -> {
            if (hasForeground(activity)) {
                // Foreground already held: go straight to background.
                requestBackground(activity);
                return;
            }
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

    /** Ask for background location, or report success when already held / N/A. */
    private static void requestBackground(Activity activity) {
        if (hasBackground(activity)) {
            reportPermission(true);
        } else {
            activity.requestPermissions(
                    new String[]{Manifest.permission.ACCESS_BACKGROUND_LOCATION}, REQ_BACKGROUND);
        }
    }

    /** Called by MainActivity with a permission dialog outcome. */
    public static void handlePermissionResult(int requestCode, String[] permissions, int[] results) {
        Activity activity = MainActivity.current();
        if (requestCode == REQ_LOCATION) {
            if (activity != null && hasForeground(activity)) {
                // Foreground granted; now require "Allow all the time".
                requestBackground(activity);
            } else {
                reportPermission(false);
            }
        } else if (requestCode == REQ_BACKGROUND) {
            // Re-check rather than trust the results array: on Android 11+ the
            // grant happens on the Settings screen, not in this dialog.
            reportPermission(hasLocationPermission());
        }
    }

    private static void reportPermission(boolean granted) {
        try {
            nativeOnPermission(granted);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native callbacks not registered yet", e);
        }
    }

    // ---- location updates -------------------------------------------------

    /**
     * Subscribe to location updates (and, from the UI, start the foreground
     * service that keeps the process and GPS alive while backgrounded). Driven
     * either by the activity or, on the boot path, by the foreground service
     * itself — whichever {@link #locationContext} resolves.
     */
    public static void startLocation(final long intervalMs) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "startLocation: no context");
            reportPermission(false);
            return;
        }
        final boolean fromActivity = ctx instanceof Activity;
        post(() -> {
            if (!hasLocationPermission()) {
                Log.w(TAG, "startLocation without permission");
                reportPermission(false);
                return;
            }
            // The UI path brings the keep-alive service up; the boot path is
            // already running inside it.
            if (fromActivity) {
                try {
                    ctx.startForegroundService(new Intent(ctx, LocationService.class));
                } catch (Exception e) {
                    // The app can still share while in the foreground.
                    Log.e(TAG, "failed to start foreground service", e);
                }
            }
            // Fresh session: re-acquire from scratch (keep the radio hot until
            // the first fix), ignoring any prior session's acquired state.
            acquiredFix = false;
            subscribe(ctx, intervalMs);
        });
    }

    /**
     * Change the GPS sampling cadence of the already-running session in place
     * (a duress alert boosts the rate; clearing it relaxes it again), leaving
     * the foreground service untouched.
     *
     * Crucially this is NOT a stopLocation()/startLocation() pair: calling
     * stopService() immediately after startForegroundService() races Android's
     * "call startForeground() within a few seconds" contract and crashes the
     * process with ForegroundServiceDidNotStartInTimeException — and on the boot
     * path it would tear down the very service that hosts the headless engine.
     * Re-tuning in place also keeps the OS location-access indicator lit
     * continuously instead of flickering off between a stop and the next start.
     *
     * No-op when no session is running (it is only ever called while sharing).
     */
    public static void setLocationInterval(final long intervalMs) {
        final Context ctx = locationContext();
        if (ctx == null) return;
        post(() -> {
            if (listener == null) return; // nothing running to re-tune
            if (!hasLocationPermission()) {
                reportPermission(false);
                return;
            }
            subscribe(ctx, intervalMs);
        });
    }

    /**
     * (Re)subscribe the location listener at {@code intervalMs}, replacing any
     * existing subscription. Main-thread only — it mutates {@link #listener},
     * so every caller posts here first.
     */
    private static void subscribe(final Context ctx, final long intervalMs) {
        try {
            LocationManager lm =
                    (LocationManager) ctx.getSystemService(Context.LOCATION_SERVICE);
            stopListening(ctx);
            listener = new LocationListener() {
                @Override
                public void onLocationChanged(Location location) {
                    nativeOnLocation(
                            location.getLatitude(),
                            location.getLongitude(),
                            location.getAccuracy(),
                            location.getTime() > 0 ? location.getTime()
                                    : System.currentTimeMillis());
                    // First fix of the session: drop from the continuous
                    // acquisition rate to the requested broadcast cadence so the
                    // GPS can duty-cycle and save power. Re-subscribing from a
                    // location callback must hop to the looper (it mutates
                    // `listener`); the !acquiredFix guard makes this fire once.
                    if (!acquiredFix) {
                        acquiredFix = true;
                        if (Math.max(intervalMs, 1000L) > ACQUIRE_INTERVAL_MS) {
                            post(() -> {
                                if (listener != null) subscribe(ctx, intervalMs);
                            });
                        }
                    }
                }
                @Override public void onStatusChanged(String provider, int status, Bundle extras) {}
                @Override public void onProviderEnabled(String provider) {}
                @Override public void onProviderDisabled(String provider) {}
            };
            boolean any = false;
            // Until the first fix lands, sample fast enough to keep the radio
            // continuously powered (ACQUIRE_INTERVAL_MS): a long minTime lets
            // Android rest the GPS between windows, stalling a cold start and
            // flickering the OS location-access indicator off before we ever get
            // a fix. Once acquired we relax to the broadcast cadence — ntrack only
            // sends the latest fix once per interval, so sampling faster would
            // spin the radio for positions we'd immediately discard (1 s floor
            // guards a pathologically small interval).
            long minTimeMs = acquiredFix ? Math.max(intervalMs, 1000L) : ACQUIRE_INTERVAL_MS;
            for (String provider : pickProviders(lm)) {
                lm.requestLocationUpdates(provider, minTimeMs, 0f,
                        listener, Looper.getMainLooper());
                any = true;
                Log.i(TAG, "location updates from " + provider + " every " + minTimeMs + "ms");
                // Seed an immediate fix from the OS cache ONLY while still
                // acquiring (before the first real fix of the session). A re-tune
                // (setLocationInterval, e.g. when the engine relaxes the cadence)
                // re-runs subscribe() with a fix already in hand; re-delivering
                // getLastKnownLocation there hands the engine a fix bearing the
                // SAME timestamp it just processed, which its adaptive cadence
                // reads as "can't measure speed" and snaps back to the minimum
                // interval — pinning updates to the maximum rate even when
                // stationary. The seed only exists to bootstrap a cold start.
                if (!acquiredFix) {
                    Location last = lm.getLastKnownLocation(provider);
                    if (last != null
                            && System.currentTimeMillis() - last.getTime() < 60_000) {
                        listener.onLocationChanged(last);
                    }
                }
            }
            if (!any) {
                Log.w(TAG, "no usable location provider");
            }
        } catch (SecurityException e) {
            Log.e(TAG, "location permission lost", e);
            reportPermission(false);
        }
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
        final Context ctx = locationContext();
        if (ctx == null) return;
        post(() -> {
            stopListening(ctx);
            acquiredFix = false;
            ctx.stopService(new Intent(ctx, LocationService.class));
        });
    }

    private static void stopListening(Context ctx) {
        if (listener != null) {
            LocationManager lm =
                    (LocationManager) ctx.getSystemService(Context.LOCATION_SERVICE);
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

    /**
     * Read the current clipboard text, or "" when empty/unreadable. Called
     * synchronously from the UI thread (where Rust invokes it), so no looper
     * hop is needed; coerceToText turns non-plain clips into their text form.
     * Android 10+ only returns clipboard contents while the app has focus,
     * which it does when the user taps the in-app Paste button.
     */
    public static String getClipboardText() {
        final Activity activity = MainActivity.current();
        if (activity == null) return "";
        try {
            ClipboardManager cm =
                    (ClipboardManager) activity.getSystemService(Context.CLIPBOARD_SERVICE);
            if (cm == null || !cm.hasPrimaryClip()) return "";
            ClipData clip = cm.getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) return "";
            CharSequence text = clip.getItemAt(0).coerceToText(activity);
            return text == null ? "" : text.toString();
        } catch (Exception e) {
            Log.e(TAG, "getClipboardText failed", e);
            return "";
        }
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

    /**
     * Write {@code content} to a single rotating temp file under
     * {@code cacheDir/shared/}, expose it via {@link FileBridgeProvider}, and
     * hand it to the OS. When {@code preferView} is set we first try to open it
     * directly in a capable app ({@code ACTION_VIEW}, e.g. a GPX/track viewer);
     * otherwise, and as a fallback when nothing can view it, we offer the
     * system share sheet ({@code ACTION_SEND}). All work hops to the UI thread.
     */
    public static void shareFile(final byte[] content, final String filename,
                                 final String mime, final boolean preferView) {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "shareFile: no live activity");
            return;
        }
        activity.runOnUiThread(() -> {
            try {
                File dir = new File(activity.getCacheDir(), FileBridgeProvider.SHARED_DIR);
                if (!dir.exists() && !dir.mkdirs()) {
                    Log.e(TAG, "shareFile: could not create shared dir");
                    return;
                }
                // Single rotating temp file: clear previous exports first so the
                // cache never accumulates and a stale URI can't be re-resolved.
                File[] old = dir.listFiles();
                if (old != null) {
                    for (File f : old) {
                        // noinspection ResultOfMethodCallIgnored
                        f.delete();
                    }
                }
                // Sanitize to a bare file name (defence in depth; the Rust side
                // already produces a safe name).
                String safe = new File(filename == null ? "track.gpx" : filename).getName();
                if (safe.isEmpty()) safe = "track.gpx";
                File out = new File(dir, safe);
                try (FileOutputStream fos = new FileOutputStream(out)) {
                    fos.write(content);
                }

                Uri uri = Uri.parse("content://" + FileBridgeProvider.AUTHORITY
                        + "/" + Uri.encode(safe));

                if (preferView) {
                    Intent view = new Intent(Intent.ACTION_VIEW);
                    view.setDataAndType(uri, mime);
                    view.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                    view.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                    if (view.resolveActivity(activity.getPackageManager()) != null) {
                        activity.startActivity(view);
                        return;
                    }
                }

                // Fallback (or pure-share request): the system chooser.
                Intent send = new Intent(Intent.ACTION_SEND);
                send.setType(mime);
                send.putExtra(Intent.EXTRA_STREAM, uri);
                send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                Intent chooser = Intent.createChooser(send, "Export track");
                chooser.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                try {
                    activity.startActivity(chooser);
                } catch (ActivityNotFoundException e) {
                    Log.e(TAG, "no app can open or share the exported file");
                }
            } catch (Exception e) {
                Log.e(TAG, "shareFile failed", e);
            }
        });
    }

    // ---- alert / check-in notifications -----------------------------------

    /**
     * Raise a high-urgency notification (sound/vibration, Do-Not-Disturb bypass
     * where the user has allowed it) for an incoming peer alert or a check-in
     * grace/escalation — visible even when the app is backgrounded. Tapping it
     * opens the app. Works from either the live activity or the boot foreground
     * service, whichever {@link #locationContext} resolves.
     */
    public static void notifyAlert(final String title, final String body) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "notifyAlert: no context");
            return;
        }
        post(() -> {
            try {
                NotificationManager nm = ctx.getSystemService(NotificationManager.class);
                if (nm == null) return;
                NotificationChannel channel = new NotificationChannel(
                        ALERT_CHANNEL_ID, "Alerts & check-ins",
                        NotificationManager.IMPORTANCE_HIGH);
                channel.setDescription(
                        "Urgent location alerts from your groups and check-in reminders");
                channel.enableVibration(true);
                // Best-effort DnD bypass; only takes effect if the user grants
                // notification-policy access, otherwise silently ignored.
                channel.setBypassDnd(true);
                nm.createNotificationChannel(channel);

                Intent open = new Intent(ctx, MainActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                PendingIntent tap = PendingIntent.getActivity(
                        ctx, 0, open,
                        PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);

                Notification n = new Notification.Builder(ctx, ALERT_CHANNEL_ID)
                        .setContentTitle(title)
                        .setContentText(body)
                        .setStyle(new Notification.BigTextStyle().bigText(body))
                        .setSmallIcon(android.R.drawable.ic_dialog_alert)
                        .setCategory(Notification.CATEGORY_ALARM)
                        .setAutoCancel(true)
                        .setContentIntent(tap)
                        .build();
                nm.notify(nextAlertId(), n);
            } catch (Exception e) {
                Log.e(TAG, "notifyAlert failed", e);
            }
        });
    }

    private static synchronized int nextAlertId() {
        return alertNotificationId++;
    }

    // ---- QR scanner -------------------------------------------------------

    /** Open the camera scanner (a separate {@link ScanActivity}). */
    public static void scanQr() {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "scanQr: no live activity");
            return;
        }
        activity.runOnUiThread(() -> {
            try {
                activity.startActivity(new Intent(activity, ScanActivity.class));
            } catch (Exception e) {
                Log.e(TAG, "failed to open scanner", e);
            }
        });
    }

    /** Called by {@link ScanActivity} per frame; returns the payload or null. */
    static String decodeQr(byte[] luma, int width, int height, int rowStride) {
        try {
            return nativeDecodeQr(luma, width, height, rowStride);
        } catch (UnsatisfiedLinkError e) {
            return null;
        }
    }

    /** Called by {@link ScanActivity} once a QR code has been decoded. */
    static void deliverScan(String text) {
        try {
            nativeOnQrResult(text);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native not ready for scan result", e);
        }
    }

    // ---- deep links -------------------------------------------------------

    /** Called by {@link MainActivity} for an {@code ntrack://join} VIEW intent. */
    public static synchronized void onDeepLinkIntent(String uri) {
        if (uri == null || uri.isEmpty()) return;
        pendingDeepLink = uri;
        if (nativeReady) {
            flushPendingDeepLink();
        }
    }

    /**
     * Deliver any buffered deep link. Called by Rust once the native callbacks
     * are registered (marking the bridge ready), and re-entrantly from
     * {@link #onDeepLinkIntent} for links that arrive afterwards.
     */
    public static synchronized void flushPendingDeepLink() {
        nativeReady = true;
        if (pendingDeepLink == null) return;
        String uri = pendingDeepLink;
        pendingDeepLink = null;
        try {
            nativeOnDeepLink(uri);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native not ready for deep link", e);
            pendingDeepLink = uri; // try again on a later flush
        }
    }
}
