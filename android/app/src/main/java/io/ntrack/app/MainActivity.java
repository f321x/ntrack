package io.ntrack.app;

import android.app.NativeActivity;
import android.os.Bundle;
import android.view.WindowManager;

/**
 * Thin NativeActivity subclass. All UI lives in Rust (Slint); this class
 * exists to forward runtime-permission results to native code (plain
 * NativeActivity cannot) and to expose the current activity instance to
 * {@link LocationBridge}.
 *
 * Note: the Rust side must never pass a context across JNI itself — the
 * android-activity glue only publishes the *Application* context, which is
 * not an Activity. The bridge always resolves the live activity here.
 */
public class MainActivity extends NativeActivity {

    private static volatile MainActivity sInstance;

    /** The currently alive activity, or null between destroy and recreate. */
    static MainActivity current() {
        return sInstance;
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        sInstance = this;
        super.onCreate(savedInstanceState);
        // Keep the screen on while the app is in the foreground: location
        // sharing sessions are typically glanced at, not interacted with.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
    }

    @Override
    protected void onDestroy() {
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        LocationBridge.handlePermissionResult(requestCode, permissions, grantResults);
    }
}
