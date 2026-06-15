//! ntrack app: Slint UI + platform glue around `ntrack-core`.
//!
//! Entry points:
//! * Android: [`android_main`] (cdylib, loaded by `MainActivity`)
//! * Desktop dev build: `src/main.rs` (`--features desktop`)

slint::include_modules!();

pub mod controller;
pub mod glue;
pub mod headless;
pub mod map;
pub mod platform;
pub mod sim;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use tokio::sync::mpsc;

use crate::controller::Controller;
use crate::platform::{Platform, PlatformEvent};

/// Create the window, controller and timers, then run the event loop until
/// the window closes. Shared by the Android and desktop entry points.
pub fn run_app(
    data_dir: PathBuf,
    platform: Arc<dyn Platform>,
    platform_rx: mpsc::UnboundedReceiver<PlatformEvent>,
) {
    let ui = MainWindow::new().expect("failed to create main window");
    let controller = Controller::new(data_dir, platform, ui.as_weak());
    controller.attach(&ui);
    controller.spawn_platform_forwarder(platform_rx);
    // Continue a share that was still active when the process last died. On
    // Android the boot foreground service already resumed it headlessly; this
    // hands it over to the now-live UI engine. A normal launch no-ops.
    controller.resume_if_armed();

    // Re-render once per second: relative timestamps, live-share timeouts,
    // toast expiry, relay status. Cheap (small models) and keeps render logic
    // in one place.
    let render_timer = slint::Timer::default();
    {
        let ctrl = controller.clone();
        let weak = ui.as_weak();
        render_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(ui) = weak.upgrade() {
                    ctrl.render(&ui);
                }
            },
        );
    }

    ui.run().expect("event loop failed");
    controller.shutdown();
}

#[cfg(all(target_os = "android", not(feature = "android")))]
compile_error!(
    "Android builds need the android-activity backend: pass `--no-default-features --features android` (scripts/build-apk.sh does this)"
);

/// Android entry point, invoked by the android-activity glue after
/// `MainActivity` loads this library.
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("ntrack"),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));
    log::info!("ntrack starting");

    slint::android::init(app.clone()).expect("slint android init failed");

    // Claim the engine for this UI process and tear down any headless boot
    // engine running inside the foreground service: there must be exactly one
    // engine writing the config and publishing. The headless engine leaves the
    // resume flag armed on shutdown, so `run_app`'s `resume_if_armed` below
    // seamlessly continues the share under the UI engine.
    headless::claim_for_ui();

    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/ntrack"));

    let (tx, rx) = mpsc::unbounded_channel();
    let platform = match glue::AndroidPlatform::new(tx) {
        Ok(p) => p,
        Err(e) => {
            log::error!("failed to initialize android platform: {e}");
            return;
        }
    };
    run_app(data_dir, Arc::new(platform), rx);
}
