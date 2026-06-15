//! Android platform implementation over JNI.
//!
//! The Java side (`io.ntrack.app.LocationBridge`) owns the Android
//! LocationManager, the runtime-permission flow and the foreground service;
//! this module registers the native callbacks it invokes and forwards calls
//! from the [`Platform`] trait into its static methods.
//!
//! Important: the context published by the android-activity glue
//! (`ndk_context::android_context().context()`) is the **Application**, not
//! the Activity — passing it where Java expects an `Activity` aborts under
//! CheckJNI. We therefore never pass a context across JNI: the bridge
//! methods take only primitives/strings and resolve the live activity on
//! the Java side (`MainActivity.current()`). The Application context is
//! used here once, at init, to reach the app class loader.
//!
//! The module compiles on every platform (so host `cargo check`/`clippy`
//! cover it) but can only be *constructed* on Android, where `ndk-context`
//! is initialized by the android-activity glue.

use std::ffi::c_void;
use std::sync::OnceLock;

use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jdouble, jfloat, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM, NativeMethod};
use ntrack_core::engine::LocationSample;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

const BRIDGE_CLASS: &str = "io.ntrack.app.LocationBridge";

/// Sink for events arriving from Java callbacks. One per process.
static PLATFORM_TX: OnceLock<mpsc::UnboundedSender<PlatformEvent>> = OnceLock::new();

pub struct AndroidPlatform {
    vm: JavaVM,
    bridge: GlobalRef,
}

impl AndroidPlatform {
    /// Build the platform from the ambient Android context provided by the
    /// android-activity glue, register native methods on the bridge class
    /// and store `tx` as the destination for Java→Rust events.
    pub fn new(tx: mpsc::UnboundedSender<PlatformEvent>) -> Result<Self, String> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| format!("JavaVM::from_raw: {e}"))?;

        let bridge = {
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| format!("attach: {e}"))?;

            // App classes are invisible to FindClass on native threads; go
            // through the app context's class loader instead. (`context()`
            // is the Application object — fine for getClassLoader.)
            let context_obj = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
            let loader = env
                .call_method(
                    &context_obj,
                    "getClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                )
                .and_then(|v| v.l())
                .map_err(|e| format!("getClassLoader: {e}"))?;
            let class_name = env
                .new_string(BRIDGE_CLASS)
                .map_err(|e| format!("new_string: {e}"))?;
            let bridge_obj = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name)],
                )
                .and_then(|v| v.l())
                .map_err(|e| format!("loadClass {BRIDGE_CLASS}: {e}"))?;
            let bridge = env
                .new_global_ref(&bridge_obj)
                .map_err(|e| format!("global ref class: {e}"))?;

            let bridge_class: &JClass = (&bridge_obj).into();
            env.register_native_methods(
                bridge_class,
                &[
                    NativeMethod {
                        name: "nativeOnLocation".into(),
                        sig: "(DDFJ)V".into(),
                        fn_ptr: native_on_location as *mut c_void,
                    },
                    NativeMethod {
                        name: "nativeOnPermission".into(),
                        sig: "(Z)V".into(),
                        fn_ptr: native_on_permission as *mut c_void,
                    },
                    NativeMethod {
                        name: "nativeDecodeQr".into(),
                        sig: "([BIII)Ljava/lang/String;".into(),
                        fn_ptr: native_decode_qr as *mut c_void,
                    },
                    NativeMethod {
                        name: "nativeOnQrResult".into(),
                        sig: "(Ljava/lang/String;)V".into(),
                        fn_ptr: native_on_qr_result as *mut c_void,
                    },
                    NativeMethod {
                        name: "nativeOnDeepLink".into(),
                        sig: "(Ljava/lang/String;)V".into(),
                        fn_ptr: native_on_deep_link as *mut c_void,
                    },
                    NativeMethod {
                        name: "nativeOnResume".into(),
                        sig: "()V".into(),
                        fn_ptr: native_on_resume as *mut c_void,
                    },
                ],
            )
            .map_err(|e| format!("register natives: {e}"))?;
            bridge
        };

        let _ = PLATFORM_TX.set(tx);
        let me = Self { vm, bridge };
        // Native callbacks are now registered, so any deep link or resume
        // request that arrived before this Rust side was ready (e.g. a boot
        // notification tap during cold start) can be flushed through.
        me.with_env("flushPendingDeepLink", |env, class| {
            env.call_static_method(class, "flushPendingDeepLink", "()V", &[])
                .map(|_| ())
        });
        me.with_env("flushPendingResume", |env, class| {
            env.call_static_method(class, "flushPendingResume", "()V", &[])
                .map(|_| ())
        });
        Ok(me)
    }

    /// Attach (if needed) and run `f` with the env and the bridge class.
    /// JNI errors are logged and swallowed: platform calls are
    /// fire-and-forget from the app's perspective.
    fn with_env<R>(
        &self,
        what: &str,
        f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<R>,
    ) -> Option<R> {
        let mut guard = match self.vm.attach_current_thread() {
            Ok(g) => g,
            Err(e) => {
                log::error!("jni attach failed for {what}: {e}");
                return None;
            }
        };
        let env: &mut JNIEnv = &mut guard;
        let bridge_obj = self.bridge.as_obj();
        let class: &JClass = bridge_obj.into();
        match f(env, class) {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("jni call {what} failed: {e}");
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
                None
            }
        }
    }
}

impl Platform for AndroidPlatform {
    fn has_location_permission(&self) -> bool {
        self.with_env("hasLocationPermission", |env, class| {
            env.call_static_method(class, "hasLocationPermission", "()Z", &[])
                .and_then(|v| v.z())
        })
        .unwrap_or(false)
    }

    fn request_location_permission(&self) {
        self.with_env("requestLocationPermission", |env, class| {
            env.call_static_method(class, "requestLocationPermission", "()V", &[])
                .map(|_| ())
        });
    }

    fn start_location(&self, interval_ms: u64) {
        self.with_env("startLocation", |env, class| {
            env.call_static_method(
                class,
                "startLocation",
                "(J)V",
                &[JValue::Long(interval_ms as i64)],
            )
            .map(|_| ())
        });
    }

    fn stop_location(&self) {
        self.with_env("stopLocation", |env, class| {
            env.call_static_method(class, "stopLocation", "()V", &[])
                .map(|_| ())
        });
    }

    fn open_map(&self, lat: f64, lng: f64, label: &str) {
        self.with_env("openMap", |env, class| {
            let jlabel = env.new_string(label)?;
            env.call_static_method(
                class,
                "openMap",
                "(DDLjava/lang/String;)V",
                &[
                    JValue::Double(lat),
                    JValue::Double(lng),
                    JValue::Object(&jlabel),
                ],
            )
            .map(|_| ())
        });
    }

    fn copy_text(&self, text: &str) {
        self.with_env("copyText", |env, class| {
            let jtext = env.new_string(text)?;
            env.call_static_method(
                class,
                "copyText",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&jtext)],
            )
            .map(|_| ())
        });
    }

    fn paste_text(&self) -> String {
        self.with_env("getClipboardText", |env, class| {
            let obj = env
                .call_static_method(class, "getClipboardText", "()Ljava/lang/String;", &[])
                .and_then(|v| v.l())?;
            // Java always returns a (possibly empty) String, never null.
            let jstr = JString::from(obj);
            let java_str = env.get_string(&jstr)?;
            Ok(String::from(java_str))
        })
        .unwrap_or_default()
    }

    fn share_text(&self, text: &str) {
        self.with_env("shareText", |env, class| {
            let jtext = env.new_string(text)?;
            env.call_static_method(
                class,
                "shareText",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&jtext)],
            )
            .map(|_| ())
        });
    }

    fn scan_qr(&self) {
        self.with_env("scanQr", |env, class| {
            env.call_static_method(class, "scanQr", "()V", &[]).map(|_| ())
        });
    }
}

/// Forward a Java string (deep-link URI or decoded QR payload) to the
/// controller as an [`PlatformEvent::IncomingInvite`].
fn deliver_invite(env: &mut JNIEnv, s: &JString) {
    let Ok(java_str) = env.get_string(s) else {
        return;
    };
    let raw: String = java_str.into();
    if raw.is_empty() {
        return;
    }
    if let Some(tx) = PLATFORM_TX.get() {
        let _ = tx.send(PlatformEvent::IncomingInvite(raw));
    }
}

/// `static native void nativeOnLocation(double, double, float, long)` —
/// called by Java on the main looper for every location fix.
extern "system" fn native_on_location(
    _env: JNIEnv,
    _class: JClass,
    lat: jdouble,
    lng: jdouble,
    accuracy: jfloat,
    ts_millis: jlong,
) {
    if let Some(tx) = PLATFORM_TX.get() {
        let _ = tx.send(PlatformEvent::Location(LocationSample {
            lat,
            lng,
            accuracy_m: accuracy,
            ts_millis: ts_millis.max(0) as u64,
        }));
    }
}

/// `static native void nativeOnPermission(boolean)` — result of the runtime
/// permission request.
extern "system" fn native_on_permission(_env: JNIEnv, _class: JClass, granted: jboolean) {
    if let Some(tx) = PLATFORM_TX.get() {
        let _ = tx.send(PlatformEvent::PermissionResult(granted != 0));
    }
}

/// `static native String nativeDecodeQr(byte[] y, int width, int height, int rowStride)`
/// — decode one camera frame's luminance plane. Returns the decoded payload or
/// `null` when no QR code is present, so the Java scanner keeps trying.
extern "system" fn native_decode_qr(
    env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    width: jint,
    height: jint,
    row_stride: jint,
) -> jstring {
    let null = std::ptr::null_mut();
    let len = match env.get_array_length(&data) {
        Ok(l) if l >= 0 => l as usize,
        _ => return null,
    };
    let mut signed = vec![0i8; len];
    if env.get_byte_array_region(&data, 0, &mut signed).is_err() {
        return null;
    }
    // Reinterpret the JNI-signed bytes as the unsigned luminance samples they
    // really are (same bit pattern, no copy).
    let luma: &[u8] = unsafe { std::slice::from_raw_parts(signed.as_ptr() as *const u8, len) };
    match ntrack_core::qr::decode_luma(
        width.max(0) as usize,
        height.max(0) as usize,
        row_stride.max(0) as usize,
        luma,
    ) {
        Some(text) => env
            .new_string(text)
            .map(|s| s.into_raw())
            .unwrap_or(null),
        None => null,
    }
}

/// `static native void nativeOnQrResult(String)` — the scanner activity
/// delivered a decoded payload.
extern "system" fn native_on_qr_result(mut env: JNIEnv, _class: JClass, text: JString) {
    deliver_invite(&mut env, &text);
}

/// `static native void nativeOnDeepLink(String)` — an `ntrack://join` URI that
/// launched or resumed the activity.
extern "system" fn native_on_deep_link(mut env: JNIEnv, _class: JClass, uri: JString) {
    deliver_invite(&mut env, &uri);
}

/// `static native void nativeOnResume()` — the user tapped the post-reboot
/// "resume sharing" notification (delivered via `MainActivity`).
extern "system" fn native_on_resume(_env: JNIEnv, _class: JClass) {
    if let Some(tx) = PLATFORM_TX.get() {
        let _ = tx.send(PlatformEvent::ResumeShareRequest);
    }
}
