package io.ntrack.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

import java.io.File;

/**
 * Foreground service shown while a live share is running. It keeps the process
 * and location access alive with the screen off, and ensures the user always
 * sees that sharing is active.
 *
 * Three roles:
 *  - While the app is open it is a keep-alive shell: location fixes are
 *    delivered to {@link LocationBridge}'s listener in this same process and the
 *    engine runs inside the activity. When the activity is torn down (the app is
 *    swiped away from recents) the Rust side hands the engine over to a UI-less
 *    one hosted by this still-running service, so publishing continues.
 *  - On boot (started by {@link BootReceiver} with {@link #EXTRA_FROM_BOOT})
 *    there is no activity, so it loads the native library and starts a UI-less
 *    engine ({@code nativeServiceStart}) that resumes the share. It also exposes
 *    itself (via {@link #current}) as the Context {@link LocationBridge} uses
 *    for location when no activity exists.
 *  - After the OS kills the process while sharing, the {@code START_STICKY}
 *    restart (null intent) resumes the share headlessly exactly like the boot
 *    path — gated on the same persisted sentinels, so a restart with nothing to
 *    do stops itself instead of lingering as an idle notification.
 */
public class LocationService extends Service {
    private static final String TAG = "ntrack";
    private static final String CHANNEL_ID = "ntrack.sharing";
    private static final int NOTIFICATION_ID = 1;

    /** Set by {@link BootReceiver} so onStartCommand knows to resume headlessly. */
    public static final String EXTRA_FROM_BOOT = "from_boot";

    static {
        // The boot path runs without the NativeActivity that normally loads
        // this library, so load it here. Idempotent if already loaded.
        System.loadLibrary("ntrack_app");
    }

    /** Resume a share headlessly; reads config from {@code dataDir}. */
    private static native void nativeServiceStart(String dataDir);

    /** Tear the headless engine down. */
    private static native void nativeServiceStop();

    private static volatile LocationService sInstance;
    private boolean headlessStarted;

    /** The live service instance, or null when not running. Used by
     * {@link LocationBridge} to drive location with no activity present. */
    static LocationService current() {
        return sInstance;
    }

    /**
     * Whether the core's persisted sentinels say there is work for a headless
     * engine: a share to resume ({@code resume.flag}) or an armed check-in to
     * evaluate ({@code checkin.flag}). The same gate {@link BootReceiver} uses;
     * the config file itself is never parsed from Java — it holds group secrets.
     */
    static boolean shareOrCheckinArmed(Context context) {
        File files = context.getFilesDir();
        return new File(files, "resume.flag").exists()
                || new File(files, "checkin.flag").exists();
    }

    @Override
    public void onCreate() {
        super.onCreate();
        sInstance = this;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        // A null intent is a START_STICKY restart: the OS killed the process
        // while sharing (activity and engine died with it) and has now brought
        // the service back. With nothing armed there is nothing to host — stop
        // before ever going foreground so no notification flashes. (A restart
        // is not a startForegroundService() start, so skipping startForeground
        // here does not violate the 5-second contract.)
        boolean restarted = intent == null;
        if (restarted && !shareOrCheckinArmed(this)) {
            stopSelf(startId);
            return START_NOT_STICKY;
        }

        NotificationManager nm = getSystemService(NotificationManager.class);
        NotificationChannel channel = new NotificationChannel(
                CHANNEL_ID, "Live location sharing", NotificationManager.IMPORTANCE_LOW);
        channel.setDescription("Shown while ntrack is sharing your location");
        nm.createNotificationChannel(channel);

        Intent open = new Intent(this, MainActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_IMMUTABLE);

        Notification notification = new Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Sharing live location")
                .setContentText("Your encrypted location is being broadcast to your groups.")
                .setSmallIcon(android.R.drawable.ic_menu_mylocation)
                .setOngoing(true)
                .setContentIntent(tap)
                .build();

        try {
            if (Build.VERSION.SDK_INT >= 29) {
                startForeground(NOTIFICATION_ID, notification,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION);
            } else {
                startForeground(NOTIFICATION_ID, notification);
            }
        } catch (Exception e) {
            // Android 14+ throws SecurityException from startForeground with a
            // location type when the location permission was revoked while we
            // were down, and 12-14 can deny a background foreground-start
            // outright. Crashing here would make the sticky restart crash-loop;
            // stay down instead — the share resumes on the next app launch,
            // where the permission flow can run.
            Log.e(TAG, "startForeground failed; not resuming", e);
            stopSelf(startId);
            return START_NOT_STICKY;
        }

        // No-activity paths — boot, or a sticky restart after a process kill —
        // host the engine here. The headless engine resumes the share and
        // drives location through LocationBridge (which resolves this service
        // via current()). Guard so a redelivery can't start a second engine;
        // the native side additionally refuses (and leaves the live engine's
        // event routing untouched) once a UI owns the engine — a restart
        // racing the user reopening the app.
        boolean fromBoot = intent != null && intent.getBooleanExtra(EXTRA_FROM_BOOT, false);
        if ((fromBoot || restarted) && !headlessStarted) {
            headlessStarted = true;
            try {
                nativeServiceStart(getFilesDir().getAbsolutePath());
            } catch (Throwable t) {
                Log.e(TAG, "headless resume failed to start", t);
            }
        }
        // Sticky, so a share survives its process being killed: the restart
        // re-enters this method with a null intent and resumes headlessly.
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        // Tear down whichever headless engine this process hosts, regardless
        // of which path created it: nativeServiceStart here (boot / sticky
        // restart, tracked by headlessStarted) or the Rust-side swipe-away
        // handoff, which this service never observes. Unconditional on
        // purpose — headless::stop() is a safe no-op when no engine runs or
        // the UI owns it, and gating on headlessStarted would strand a
        // handoff engine (tokio runtime + relay pool) in the cached process
        // whenever the service stops under it.
        try {
            nativeServiceStop();
        } catch (Throwable t) {
            Log.e(TAG, "headless stop failed", t);
        }
        headlessStarted = false;
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
