package io.ntrack.app;

import android.Manifest;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Build;
import android.util.Log;

import java.io.File;

/**
 * After a reboot (or a low-battery shutdown), if a share was still active when
 * the device went down, offer to resume it.
 *
 * Android forbids a background broadcast receiver from launching an Activity,
 * and a boot-started {@code location} foreground service would only receive
 * GPS with the intrusive "Allow all the time" background-location permission.
 * So instead of resuming silently we post a notification; tapping it launches
 * {@link MainActivity} (an allowed activity start) with a {@code resume_sharing}
 * extra, and the Rust core continues the previous share through the normal,
 * foreground path. The tap is also the user's explicit consent to start
 * broadcasting their location again.
 *
 * Whether a share was active is read from a tiny non-secret sentinel file the
 * core maintains next to its config ({@code resume.flag} in {@code getFilesDir()},
 * which equals the NativeActivity {@code internalDataPath} the core uses). We
 * never parse the config file from Java — it holds group secrets.
 */
public final class BootReceiver extends BroadcastReceiver {
    private static final String TAG = "ntrack";
    private static final String CHANNEL_ID = "ntrack.resume";
    // Distinct from LocationService's ongoing notification (id 1).
    private static final int NOTIFICATION_ID = 2;

    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent == null ? null : intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action)
                && !"android.intent.action.QUICKBOOT_POWERON".equals(action)) {
            return;
        }
        // Only offer to resume if we were sharing when we went down.
        if (!new File(context.getFilesDir(), "resume.flag").exists()) return;

        // Posting requires the runtime notification permission on Android 13+.
        if (Build.VERSION.SDK_INT >= 33
                && context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                        != PackageManager.PERMISSION_GRANTED) {
            Log.i(TAG, "boot: share was active but POST_NOTIFICATIONS not granted");
            return;
        }

        NotificationManager nm = context.getSystemService(NotificationManager.class);
        if (nm == null) return;
        NotificationChannel channel = new NotificationChannel(
                CHANNEL_ID, "Resume sharing", NotificationManager.IMPORTANCE_HIGH);
        channel.setDescription("Offers to resume location sharing after a restart");
        nm.createNotificationChannel(channel);

        Intent open = new Intent(context, MainActivity.class)
                .putExtra("resume_sharing", true)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        PendingIntent tap = PendingIntent.getActivity(
                context, 0, open,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);

        Notification notification = new Notification.Builder(context, CHANNEL_ID)
                .setContentTitle("Resume location sharing?")
                .setContentText("You were sharing your location before the restart. Tap to continue.")
                .setSmallIcon(android.R.drawable.ic_menu_mylocation)
                .setAutoCancel(true)
                .setContentIntent(tap)
                .build();
        nm.notify(NOTIFICATION_ID, notification);
    }
}
