//! Desktop development platform: simulated location source and logged
//! stand-ins for the Android intents. Lets the full app (UI ↔ engine ↔
//! relays) run on a workstation: `cargo run -p ntrack-app --features desktop`.

use std::path::PathBuf;
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
        // A share left armed in the persisted config resumes automatically at
        // startup (see `Controller::resume_if_armed`), exactly like Android:
        // launch the desktop build against a config that was left mid-share to
        // exercise the resume path — no special signalling needed here.
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

    fn notify_alert(&self, title: &str, body: &str) {
        // The desktop build has no notification surface; log it loudly so the
        // alert/check-in flows can still be exercised on a workstation.
        log::warn!("sim: 🔔 ALERT NOTIFICATION — {title}: {body}");
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

    fn share_file(&self, filename: &str, mime: &str, content: &[u8], prefer_view: bool) {
        // Write the file under $NTRACK_DATA (else the temp dir) so the desktop
        // demo can open the exported GPX in any viewer. Sanitize to a bare file
        // name so a crafted filename can't escape the directory.
        let dir = std::env::var_os("NTRACK_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let name = std::path::Path::new(filename)
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("export.gpx"));
        let path = dir.join(name);
        match std::fs::write(&path, content) {
            Ok(()) => log::info!(
                "sim: share_file -> {} ({} bytes, {mime}, prefer_view={prefer_view})",
                path.display(),
                content.len()
            ),
            Err(e) => log::error!("sim: share_file failed for {}: {e}", path.display()),
        }
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
