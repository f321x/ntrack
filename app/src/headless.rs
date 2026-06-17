//! UI-less engine host for the Android boot path.
//!
//! After a reboot there is no Activity — and therefore no Slint UI and no
//! [`Controller`](crate::controller::Controller) — yet we still want a share
//! that was active before the restart to resume with no user interaction. The
//! `BootReceiver` starts the `LocationService` foreground service, which loads
//! this library and calls in (via [`crate::glue`]) to spin up the same UI-free
//! `ntrack_core` engine the controller normally drives. We wire it straight to
//! the platform and drop the `UiEvent`s that have no headless consumer — only
//! [`UiEvent::NeedLocation`] matters here.
//!
//! Exactly one engine may own the persisted config and publish at a time. When
//! the user later opens the app, `android_main` calls [`claim_for_ui`] to tear
//! this host down before constructing the UI engine; the share is handed over
//! through the persisted resume flag, which the engine leaves armed across a
//! shutdown.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ntrack_core::config::ConfigStore;
use ntrack_core::engine::{Engine, EngineCmd, UiEvent};
use ntrack_core::relay::RelayPool;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

/// GPS sampling cadence used until the engine's own interval is known. The
/// engine still only publishes once per configured interval; this just bounds
/// how often the OS hands us a fix.
const FALLBACK_INTERVAL_MS: u64 = 30_000;
/// How long [`stop`] waits for the engine to flush its dedup tail (and emit a
/// best-effort STOP) before aborting the runtime.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// The single headless host, if one is running. The mutex serialises a boot
/// service `start` against the UI thread's `claim_for_ui`.
static HOST: Mutex<Option<HeadlessHost>> = Mutex::new(None);
/// Set once the UI process has claimed the engine. A headless `start` that
/// arrives afterwards (a boot service start racing the user opening the app)
/// then no-ops, so the UI engine stays the sole owner.
static UI_ACTIVE: AtomicBool = AtomicBool::new(false);

struct HeadlessHost {
    rt: tokio::runtime::Runtime,
    cmd_tx: mpsc::UnboundedSender<EngineCmd>,
    platform: Arc<dyn Platform>,
}

/// Start a UI-less engine that resumes any armed share. Idempotent, and a no-op
/// once the UI owns the engine. Takes ownership of the platform and its event
/// receiver — location fixes flow in through `platform_rx`.
pub fn start(
    data_dir: PathBuf,
    platform: Arc<dyn Platform>,
    mut platform_rx: mpsc::UnboundedReceiver<PlatformEvent>,
) {
    if UI_ACTIVE.load(Ordering::SeqCst) {
        log::info!("headless: UI already owns the engine; ignoring service start");
        return;
    }
    let mut slot = HOST.lock().unwrap();
    if slot.is_some() {
        log::info!("headless: engine already running; ignoring service start");
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let store = ConfigStore::new(&data_dir);
    // Read the publish interval up front for the GPS sampling cadence; the
    // engine loads its own copy of the config when constructed.
    let interval_ms = store
        .load()
        .ok()
        .map(|c| c.cadence_mode.params().min_secs.saturating_mul(1000))
        .filter(|ms| *ms > 0)
        .unwrap_or(FALLBACK_INTERVAL_MS);

    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    rt.block_on(async {
        let pool = RelayPool::new(pool_tx);
        let engine = Engine::new(store, pool, ui_tx);
        tokio::spawn(engine.run(cmd_rx));
    });

    // pool events -> engine
    let cmd_pool = cmd_tx.clone();
    rt.spawn(async move {
        while let Some(ev) = pool_rx.recv().await {
            if cmd_pool.send(EngineCmd::Pool(ev)).is_err() {
                break;
            }
        }
    });

    // platform location fixes -> engine. Permission results are ignored on
    // purpose: there is no UI to prompt, and tearing the share down here would
    // forfeit the resume flag a later (permitted) boot could still act on.
    let cmd_loc = cmd_tx.clone();
    rt.spawn(async move {
        while let Some(ev) = platform_rx.recv().await {
            if let PlatformEvent::Location(sample) = ev {
                if cmd_loc.send(EngineCmd::Location(sample)).is_err() {
                    break;
                }
            }
        }
    });

    // engine -> location control + alert notifications. The engine asks for
    // fixes via NeedLocation, adjusts the GPS cadence via SetLocationInterval
    // (e.g. when a check-in escalates to an alert headlessly), and surfaces
    // emergency notifications via Notify. Every other UiEvent (snapshots,
    // toasts, dialogs) has no headless consumer.
    let platform_loc = platform.clone();
    rt.spawn(async move {
        while let Some(ev) = ui_rx.recv().await {
            match ev {
                UiEvent::NeedLocation(on) => {
                    if on {
                        platform_loc.start_location(interval_ms);
                    } else {
                        platform_loc.stop_location();
                    }
                }
                UiEvent::SetLocationInterval(ms) => {
                    // Only ever emitted while a share is active, so location is
                    // already running; re-tune it in place. A stop+start here
                    // would tear down the very foreground service hosting this
                    // engine (stop_location stops LocationService).
                    platform_loc.set_location_interval(ms);
                }
                UiEvent::Notify { title, body, .. } => {
                    platform_loc.notify_alert(&title, &body);
                }
                _ => {}
            }
        }
    });

    let _ = cmd_tx.send(EngineCmd::ResumeShareIfArmed);
    log::info!("headless: engine started; resuming any armed share");
    *slot = Some(HeadlessHost {
        rt,
        cmd_tx,
        platform,
    });
}

/// Stop the headless engine if one is running. Asks the engine to shut down
/// (flushing its dedup tail and emitting a best-effort STOP), stops location,
/// then drains the runtime. The resume flag was persisted when the share began,
/// so a UI engine started afterwards resumes from the config; an in-flight STOP
/// that does not finish flushing is harmless — we are in fact still sharing.
pub fn stop() {
    let host = HOST.lock().unwrap().take();
    if let Some(host) = host {
        log::info!("headless: stopping engine");
        let _ = host.cmd_tx.send(EngineCmd::Shutdown);
        host.platform.stop_location();
        host.rt.shutdown_timeout(SHUTDOWN_GRACE);
    }
}

/// Mark this process's UI as the sole engine owner and tear down any headless
/// boot engine. Called from `android_main` before the UI engine is built.
pub fn claim_for_ui() {
    UI_ACTIVE.store(true, Ordering::SeqCst);
    stop();
}
