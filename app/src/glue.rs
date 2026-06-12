//! Android platform implementation over JNI.
//!
//! The Java side (`io.ntrack.app.LocationBridge`) owns the Android
//! LocationManager, the runtime-permission flow and the foreground service;
//! this module registers the native callbacks it invokes and forwards calls
//! from the [`Platform`] trait into its static methods.
//!
//! The module compiles on every platform (so host `cargo check`/`clippy`
//! cover it) but can only be *constructed* on Android, where
//! `ndk-context` is initialized by the android-activity glue.

use std::ffi::c_void;
use std::sync::OnceLock;

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::{jboolean, jdouble, jfloat, jlong};
use jni::{JNIEnv, JavaVM, NativeMethod};
use ntrack_core::engine::LocationSample;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

const BRIDGE_CLASS: &str = "io.ntrack.app.LocationBridge";

/// Sink for events arriving from Java callbacks. One per process.
static PLATFORM_TX: OnceLock<mpsc::UnboundedSender<PlatformEvent>> = OnceLock::new();

pub struct AndroidPlatform {
    vm: JavaVM,
    activity: GlobalRef,
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

        let (activity, bridge) = {
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| format!("attach: {e}"))?;

            let activity_obj = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
            let activity = env
                .new_global_ref(&activity_obj)
                .map_err(|e| format!("global ref activity: {e}"))?;

            // App classes are invisible to FindClass on native threads; go
            // through the activity's class loader instead.
            let loader = env
                .call_method(
                    &activity_obj,
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
                ],
            )
            .map_err(|e| format!("register natives: {e}"))?;
            (activity, bridge)
        };

        let _ = PLATFORM_TX.set(tx);
        Ok(Self { vm, activity, bridge })
    }

    /// Attach (if needed) and run `f` with the env, activity and bridge
    /// class. JNI errors are logged and swallowed: platform calls are
    /// fire-and-forget from the app's perspective.
    fn with_env<R>(
        &self,
        what: &str,
        f: impl FnOnce(&mut JNIEnv, &JObject, &JClass) -> jni::errors::Result<R>,
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
        match f(env, self.activity.as_obj(), class) {
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
        self.with_env("hasLocationPermission", |env, activity, class| {
            env.call_static_method(
                class,
                "hasLocationPermission",
                "(Landroid/app/Activity;)Z",
                &[JValue::Object(activity)],
            )
            .and_then(|v| v.z())
        })
        .unwrap_or(false)
    }

    fn request_location_permission(&self) {
        self.with_env("requestLocationPermission", |env, activity, class| {
            env.call_static_method(
                class,
                "requestLocationPermission",
                "(Landroid/app/Activity;)V",
                &[JValue::Object(activity)],
            )
            .map(|_| ())
        });
    }

    fn start_location(&self, interval_ms: u64) {
        self.with_env("startLocation", |env, activity, class| {
            env.call_static_method(
                class,
                "startLocation",
                "(Landroid/app/Activity;J)V",
                &[JValue::Object(activity), JValue::Long(interval_ms as i64)],
            )
            .map(|_| ())
        });
    }

    fn stop_location(&self) {
        self.with_env("stopLocation", |env, activity, class| {
            env.call_static_method(
                class,
                "stopLocation",
                "(Landroid/app/Activity;)V",
                &[JValue::Object(activity)],
            )
            .map(|_| ())
        });
    }

    fn open_map(&self, lat: f64, lng: f64, label: &str) {
        self.with_env("openMap", |env, activity, class| {
            let jlabel = env.new_string(label)?;
            env.call_static_method(
                class,
                "openMap",
                "(Landroid/app/Activity;DDLjava/lang/String;)V",
                &[
                    JValue::Object(activity),
                    JValue::Double(lat),
                    JValue::Double(lng),
                    JValue::Object(&jlabel),
                ],
            )
            .map(|_| ())
        });
    }

    fn copy_text(&self, text: &str) {
        self.with_env("copyText", |env, activity, class| {
            let jtext = env.new_string(text)?;
            env.call_static_method(
                class,
                "copyText",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&jtext)],
            )
            .map(|_| ())
        });
    }

    fn share_text(&self, text: &str) {
        self.with_env("shareText", |env, activity, class| {
            let jtext = env.new_string(text)?;
            env.call_static_method(
                class,
                "shareText",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&jtext)],
            )
            .map(|_| ())
        });
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
