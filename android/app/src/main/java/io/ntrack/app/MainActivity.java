package io.ntrack.app;

import android.app.NativeActivity;
import android.content.Intent;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
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
        setupEdgeToEdge();
        // A cold-start invite link arrives here before the Rust side has
        // registered its callbacks; LocationBridge buffers it until ready.
        handleDeepLink(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleDeepLink(intent);
    }

    /** Forward an {@code ntrack://join} VIEW intent to the bridge. */
    private void handleDeepLink(Intent intent) {
        if (intent == null || !Intent.ACTION_VIEW.equals(intent.getAction())) return;
        Uri data = intent.getData();
        if (data != null) {
            LocationBridge.onDeepLinkIntent(data.toString());
        }
        // Mark the launch intent consumed: an unhandled config change (locale,
        // font scale, …) recreates the activity, and onCreate would otherwise
        // re-read this same VIEW intent and re-deliver the invite.
        intent.setAction(null);
        intent.setData(null);
        setIntent(intent);
    }

    /**
     * Lay the window out edge-to-edge so the Slint UI owns the full window,
     * including the area behind the status and gesture/navigation bars.
     *
     * A NativeActivity's surface always fills the whole window, so it has
     * always extended under the system bars; that overlap used to be masked by
     * the opaque status/navigation bar colors set in the theme. Android 15+
     * ignores those colors and forces transparent, edge-to-edge bars, exposing
     * the overlap. The Slint Android backend reads the window insets itself and
     * exposes them as the Window's `safe-area-insets` (logical pixels, updated
     * on configuration changes), which the UI uses to pad around the bars — so
     * no manual inset plumbing is needed here, only edge-to-edge layout.
     */
    private void setupEdgeToEdge() {
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
        }
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
