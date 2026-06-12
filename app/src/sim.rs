//! Desktop development platform: simulated location source and logged
//! stand-ins for the Android intents. Lets the full app (UI ↔ engine ↔
//! relays) run on a workstation: `cargo run -p ntrack-app --features desktop`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ntrack_core::engine::LocationSample;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

pub struct SimPlatform {
    tx: mpsc::UnboundedSender<PlatformEvent>,
    running: Arc<AtomicBool>,
    step: Arc<AtomicU64>,
}

impl SimPlatform {
    pub fn new(tx: mpsc::UnboundedSender<PlatformEvent>) -> Self {
        Self {
            tx,
            running: Arc::new(AtomicBool::new(false)),
            step: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Platform for SimPlatform {
    fn has_location_permission(&self) -> bool {
        true
    }

    fn request_location_permission(&self) {
        let _ = self.tx.send(PlatformEvent::PermissionResult(true));
    }

    fn start_location(&self, _interval_ms: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("sim: location updates started");
        let tx = self.tx.clone();
        let running = self.running.clone();
        let step = self.step.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let i = step.fetch_add(1, Ordering::SeqCst) as f64;
                // slow walk in a circle around Munich Marienplatz
                let (lat0, lng0, r) = (48.13743, 11.57549, 0.002);
                let sample = LocationSample {
                    lat: lat0 + r * (i / 20.0).sin(),
                    lng: lng0 + r * (i / 20.0).cos(),
                    accuracy_m: 8.0,
                    ts_millis: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                };
                if tx.send(PlatformEvent::Location(sample)).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            log::info!("sim: location updates stopped");
        });
    }

    fn stop_location(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn open_map(&self, lat: f64, lng: f64, label: &str) {
        log::info!(
            "sim: open map for {label}: https://www.openstreetmap.org/?mlat={lat}&mlon={lng}#map=16/{lat}/{lng}"
        );
    }

    fn copy_text(&self, text: &str) {
        log::info!("sim: copy to clipboard ({} chars)", text.len());
    }

    fn share_text(&self, text: &str) {
        log::info!("sim: share sheet ({} chars)", text.len());
    }
}
