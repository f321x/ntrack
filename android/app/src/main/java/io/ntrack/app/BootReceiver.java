package io.ntrack.app;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

import java.io.File;

/**
 * After a reboot (or a low-battery shutdown), if a share was still active when
 * the device went down, resume it automatically — no user interaction.
 *
 * {@code ACTION_BOOT_COMPLETED} is an exemption to the
 * "no foreground service from the background" rule, and a {@code location}
 * foreground service may still be started from a boot receiver (Android 14 only
 * blocks {@code microphone} and {@code camera} there). So we start
 * {@link LocationService} directly; it brings up a UI-less engine that keeps
 * publishing. Actually receiving GPS with no visible UI requires the
 * "Allow all the time" background-location permission — without it the service
 * runs but gets no fixes (the user must grant it; there is no fallback path).
 *
 * Whether a share was active is read from a tiny non-secret sentinel file the
 * core maintains next to its config ({@code resume.flag} in {@code getFilesDir()},
 * which equals the NativeActivity {@code internalDataPath} the core uses). We
 * never parse the config file from Java — it holds group secrets.
 */
public final class BootReceiver extends BroadcastReceiver {
    private static final String TAG = "ntrack";

    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent == null ? null : intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action)
                && !"android.intent.action.QUICKBOOT_POWERON".equals(action)) {
            return;
        }
        // Only resume if we were sharing when we went down.
        if (!new File(context.getFilesDir(), "resume.flag").exists()) return;

        Intent svc = new Intent(context, LocationService.class)
                .putExtra(LocationService.EXTRA_FROM_BOOT, true);
        try {
            context.startForegroundService(svc);
            Log.i(TAG, "boot: resuming share via foreground service");
        } catch (Exception e) {
            // Defensive: some OEMs are stricter than the documented exemption.
            Log.e(TAG, "boot: failed to start location service", e);
        }
    }
}
