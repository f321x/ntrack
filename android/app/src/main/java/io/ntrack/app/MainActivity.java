package io.ntrack.app;

import android.app.NativeActivity;
import android.os.Bundle;
import android.view.WindowManager;

/**
 * Thin NativeActivity subclass. All UI lives in Rust (Slint); this class
 * exists to forward runtime-permission results to native code, which plain
 * NativeActivity cannot do.
 */
public class MainActivity extends NativeActivity {

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // Keep the screen on while the app is in the foreground: location
        // sharing sessions are typically glanced at, not interacted with.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        LocationBridge.handlePermissionResult(requestCode, permissions, grantResults);
    }
}
