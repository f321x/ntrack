package io.ntrack.app;

import android.Manifest;
import android.app.Activity;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.graphics.SurfaceTexture;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.util.Log;
import android.util.Size;
import android.view.Gravity;
import android.view.Surface;
import android.view.TextureView;
import android.view.ViewGroup;
import android.widget.FrameLayout;
import android.widget.TextView;

import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Dependency-free QR scanner: a framework Camera2 preview whose YUV frames are
 * decoded in Rust (the luminance plane is handed to {@code nativeDecodeQr},
 * which calls {@code ntrack_core::qr::decode_luma}). The first decoded payload
 * is delivered to native code via {@link LocationBridge#deliverScan} and the
 * activity finishes.
 *
 * QR decoding is rotation-invariant, so the (uncorrected) preview orientation
 * does not affect scanning — the user only needs to roughly frame the code.
 *
 * Threading: camera *control* callbacks (open / session) are dispatched on the
 * main looper so they are serialized with {@link #closeCamera()} (also on the
 * UI thread), which avoids the classic open-after-close camera leak. Only the
 * per-frame decode runs on the background thread.
 */
public final class ScanActivity extends Activity {
    private static final String TAG = "ntrack";
    private static final int REQ_CAMERA = 7000;

    private TextureView textureView;
    private CameraDevice camera;
    private CameraCaptureSession session;
    private ImageReader reader;
    private HandlerThread bgThread;
    private Handler bgHandler;
    private Handler mainHandler;
    private Size frameSize = new Size(1280, 720);
    private boolean starting;
    private boolean closed;
    private final AtomicBoolean delivered = new AtomicBoolean(false);

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        mainHandler = new Handler(Looper.getMainLooper());
        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(0xFF000000);
        textureView = new TextureView(this);
        root.addView(textureView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        TextView hint = new TextView(this);
        hint.setText("Point the camera at an ntrack QR code");
        hint.setTextColor(0xFFFFFFFF);
        hint.setPadding(48, 48, 48, 96);
        FrameLayout.LayoutParams hp = new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        hp.gravity = Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL;
        root.addView(hint, hp);
        setContentView(root);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            tryStart();
        } else {
            requestPermissions(new String[]{Manifest.permission.CAMERA}, REQ_CAMERA);
        }
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] results) {
        if (requestCode != REQ_CAMERA) return;
        if (results.length > 0 && results[0] == PackageManager.PERMISSION_GRANTED) {
            tryStart();
        } else {
            Log.w(TAG, "camera permission denied");
            finish();
        }
    }

    @Override
    protected void onPause() {
        closeCamera();
        super.onPause();
    }

    private void tryStart() {
        if (starting || camera != null) return;
        starting = true;
        closed = false;
        bgThread = new HandlerThread("ntrack-scan");
        bgThread.start();
        bgHandler = new Handler(bgThread.getLooper());
        if (textureView.isAvailable()) {
            openCamera();
        } else {
            textureView.setSurfaceTextureListener(new TextureView.SurfaceTextureListener() {
                @Override public void onSurfaceTextureAvailable(SurfaceTexture s, int w, int h) { openCamera(); }
                @Override public void onSurfaceTextureSizeChanged(SurfaceTexture s, int w, int h) {}
                @Override public boolean onSurfaceTextureDestroyed(SurfaceTexture s) { return true; }
                @Override public void onSurfaceTextureUpdated(SurfaceTexture s) {}
            });
        }
    }

    @SuppressWarnings("deprecation") // createCaptureSession(List,...) covers minSdk 26
    private void openCamera() {
        if (closed) return;
        CameraManager cm = (CameraManager) getSystemService(CAMERA_SERVICE);
        try {
            String camId = pickBackCamera(cm);
            if (camId == null) {
                Log.w(TAG, "no camera available");
                finish();
                return;
            }
            CameraCharacteristics ch = cm.getCameraCharacteristics(camId);
            StreamConfigurationMap map = ch.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
            if (map != null) {
                Size[] sizes = map.getOutputSizes(ImageFormat.YUV_420_888);
                if (sizes != null && sizes.length > 0) {
                    frameSize = chooseSize(sizes);
                }
            }
            reader = ImageReader.newInstance(frameSize.getWidth(), frameSize.getHeight(),
                    ImageFormat.YUV_420_888, 2);
            reader.setOnImageAvailableListener(this::onFrame, bgHandler);

            SurfaceTexture st = textureView.getSurfaceTexture();
            st.setDefaultBufferSize(frameSize.getWidth(), frameSize.getHeight());
            final Surface previewSurface = new Surface(st);
            final Surface readerSurface = reader.getSurface();

            // Camera control callbacks run on the main looper, serialized with
            // closeCamera() so a device opened after teardown is still closed.
            cm.openCamera(camId, new CameraDevice.StateCallback() {
                @Override
                public void onOpened(CameraDevice device) {
                    if (closed) {
                        device.close();
                        return;
                    }
                    camera = device;
                    try {
                        final CaptureRequest.Builder req =
                                device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW);
                        req.addTarget(previewSurface);
                        req.addTarget(readerSurface);
                        req.set(CaptureRequest.CONTROL_AF_MODE,
                                CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE);
                        device.createCaptureSession(
                                Arrays.asList(previewSurface, readerSurface),
                                new CameraCaptureSession.StateCallback() {
                                    @Override
                                    public void onConfigured(CameraCaptureSession s) {
                                        if (closed) {
                                            s.close();
                                            return;
                                        }
                                        session = s;
                                        try {
                                            s.setRepeatingRequest(req.build(), null, bgHandler);
                                        } catch (CameraAccessException e) {
                                            Log.e(TAG, "setRepeatingRequest failed", e);
                                        }
                                    }

                                    @Override
                                    public void onConfigureFailed(CameraCaptureSession s) {
                                        Log.e(TAG, "capture session configure failed");
                                        finish();
                                    }
                                }, mainHandler);
                    } catch (CameraAccessException e) {
                        Log.e(TAG, "createCaptureSession failed", e);
                        finish();
                    }
                }

                @Override
                public void onDisconnected(CameraDevice device) {
                    closeCamera();
                }

                @Override
                public void onError(CameraDevice device, int error) {
                    Log.e(TAG, "camera error " + error);
                    closeCamera();
                    finish();
                }
            }, mainHandler);
        } catch (CameraAccessException | SecurityException | IllegalArgumentException e) {
            Log.e(TAG, "openCamera failed", e);
            finish();
        }
    }

    private void onFrame(ImageReader r) {
        Image img;
        try {
            img = r.acquireLatestImage();
        } catch (IllegalStateException e) {
            // Reader closed (teardown) between dispatch and now.
            return;
        }
        if (img == null) return;
        try {
            if (delivered.get()) return;
            Image.Plane plane = img.getPlanes()[0];
            ByteBuffer buf = plane.getBuffer();
            byte[] luma = new byte[buf.remaining()];
            buf.get(luma);
            String result = LocationBridge.decodeQr(
                    luma, img.getWidth(), img.getHeight(), plane.getRowStride());
            if (result != null && delivered.compareAndSet(false, true)) {
                runOnUiThread(() -> {
                    LocationBridge.deliverScan(result);
                    finish();
                });
            }
        } finally {
            img.close();
        }
    }

    private static String pickBackCamera(CameraManager cm) throws CameraAccessException {
        String fallback = null;
        for (String id : cm.getCameraIdList()) {
            if (fallback == null) fallback = id;
            Integer facing = cm.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING);
            if (facing != null && facing == CameraCharacteristics.LENS_FACING_BACK) {
                return id;
            }
        }
        return fallback;
    }

    /** Pick a YUV size with area nearest 720p (and not absurdly large), which
     *  is plenty for QR detection while keeping per-frame decode cheap. */
    private static Size chooseSize(Size[] sizes) {
        Size best = null;
        long target = 1280L * 720L;
        long bestScore = Long.MAX_VALUE;
        for (Size s : sizes) {
            if (s.getWidth() > 1920 || s.getHeight() > 1920) continue;
            long score = Math.abs((long) s.getWidth() * s.getHeight() - target);
            if (score < bestScore) {
                bestScore = score;
                best = s;
            }
        }
        if (best == null && sizes.length > 0) best = sizes[0];
        return best != null ? best : new Size(640, 480);
    }

    private void closeCamera() {
        starting = false;
        closed = true;
        if (session != null) {
            try { session.close(); } catch (Exception ignored) {}
            session = null;
        }
        if (camera != null) {
            try { camera.close(); } catch (Exception ignored) {}
            camera = null;
        }
        if (reader != null) {
            try { reader.close(); } catch (Exception ignored) {}
            reader = null;
        }
        if (bgThread != null) {
            bgThread.quitSafely();
            bgThread = null;
            bgHandler = null;
        }
    }
}
