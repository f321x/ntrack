package io.ntrack.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Foreground service shown while a live share is running. It keeps the process
 * and location access alive with the screen off, and ensures the user always
 * sees that sharing is active.
 *
 * Two roles:
 *  - While the app is open it is a keep-alive shell: location fixes are
 *    delivered to {@link LocationBridge}'s listener in this same process and the
 *    engine runs inside the activity.
 *  - On boot (started by {@link BootReceiver} with {@link #EXTRA_FROM_BOOT})
 *    there is no activity, so it loads the native library and starts a UI-less
 *    engine ({@code nativeServiceStart}) that resumes the share. It also exposes
 *    itself (via {@link #current}) as the Context {@link LocationBridge} uses
 *    for location when no activity exists.
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

    @Override
    public void onCreate() {
        super.onCreate();
        sInstance = this;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
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

        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }

        // Boot path: no activity exists, so host the engine here. The headless
        // engine resumes the share and drives location through LocationBridge
        // (which resolves this service via current()). Guard so a redelivery
        // can't start a second engine.
        boolean fromBoot = intent != null && intent.getBooleanExtra(EXTRA_FROM_BOOT, false);
        if (fromBoot && !headlessStarted) {
            headlessStarted = true;
            try {
                nativeServiceStart(getFilesDir().getAbsolutePath());
            } catch (Throwable t) {
                Log.e(TAG, "headless resume failed to start", t);
            }
        }
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        if (headlessStarted) {
            try {
                nativeServiceStop();
            } catch (Throwable t) {
                Log.e(TAG, "headless stop failed", t);
            }
            headlessStarted = false;
        }
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
