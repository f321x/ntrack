package io.ntrack.app;

import android.app.NativeActivity;
import android.graphics.Insets;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.WindowInsets;
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
    }

    /**
     * Lay the window out edge-to-edge and forward the system-bar and
     * display-cutout insets to native code, which pads the Slint UI so it is
     * not drawn under the status bar or the gesture/navigation bar.
     *
     * A NativeActivity's surface always fills the whole window, so it has
     * always extended under the system bars; that overlap used to be masked by
     * the opaque status/navigation bar colors set in the theme. Android 15+
     * ignores those colors and forces transparent, edge-to-edge bars, exposing
     * the overlap — hence reading the real insets and padding for them.
     */
    private void setupEdgeToEdge() {
        final View decor = getWindow().getDecorView();
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
        } else {
            decor.setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
        }
        decor.setOnApplyWindowInsetsListener((v, insets) -> {
            int top, bottom, left, right;
            if (Build.VERSION.SDK_INT >= 30) {
                Insets bars = insets.getInsets(
                        WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
                top = bars.top;
                bottom = bars.bottom;
                left = bars.left;
                right = bars.right;
            } else {
                top = insets.getSystemWindowInsetTop();
                bottom = insets.getSystemWindowInsetBottom();
                left = insets.getSystemWindowInsetLeft();
                right = insets.getSystemWindowInsetRight();
            }
            LocationBridge.dispatchInsets(top, bottom, left, right);
            return insets;
        });
        // Force a dispatch once the view is attached so we get current values.
        decor.requestApplyInsets();
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
