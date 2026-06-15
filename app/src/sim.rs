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
        // Mimic the Android boot "resume" notification tap on desktop so the
        // resume path can be exercised end-to-end: set NTRACK_SIM_RESUME=1 and
        // run with a config that was left mid-share (resume_share armed). The
        // unbounded channel buffers this until the controller's forwarder
        // starts consuming.
        if std::env::var_os("NTRACK_SIM_RESUME").is_some() {
            log::info!("sim: emitting resume-share request (NTRACK_SIM_RESUME)");
            let _ = tx.send(PlatformEvent::ResumeShareRequest);
        }
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

    fn paste_text(&self) -> String {
        // Desktop has no clipboard wiring; synthesize an invite so the Paste →
        // import pre-fill flow can be exercised on the workstation, mirroring
        // the synthetic [`scan_qr`].
        let k = ntrack_core::keys::generate();
        let nsec = ntrack_core::keys::nsec(&k);
        let invite = ntrack_core::invite::build_invite("Pasted Demo", nsec.expose(), &[]);
        log::info!("sim: paste_text -> synthetic invite");
        invite
    }

    fn share_text(&self, text: &str) {
        log::info!("sim: share sheet ({} chars)", text.len());
    }

    fn scan_qr(&self) {
        // Desktop has no camera; synthesize a scanned invite so the import
        // pre-fill flow can be exercised on the workstation.
        let k = ntrack_core::keys::generate();
        let nsec = ntrack_core::keys::nsec(&k);
        // Include a non-default relay so the desktop demo also exercises the
        // relay-import path (it should get auto-added on import).
        let invite = ntrack_core::invite::build_invite(
            "Scanned Demo",
            nsec.expose(),
            &["wss://relay.sim.example".to_string()],
        );
        log::info!("sim: scan_qr -> synthetic invite");
        let _ = self.tx.send(PlatformEvent::IncomingInvite(invite));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test (not two) so the shared-process env var can't race a sibling.
    #[test]
    fn resume_request_emitted_only_with_env() {
        // Absent the opt-in env var, construction emits nothing — the desktop
        // app starts idle, exactly like a normal launch.
        std::env::remove_var("NTRACK_SIM_RESUME");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _idle = SimPlatform::new(tx);
        assert!(rx.try_recv().is_err(), "no resume request without the env var");

        // With it set, construction buffers a resume request — the desktop
        // stand-in for the post-reboot notification tap.
        std::env::set_var("NTRACK_SIM_RESUME", "1");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _resuming = SimPlatform::new(tx);
        std::env::remove_var("NTRACK_SIM_RESUME");
        assert!(
            matches!(rx.try_recv(), Ok(PlatformEvent::ResumeShareRequest)),
            "resume request emitted when NTRACK_SIM_RESUME is set"
        );
    }
}
