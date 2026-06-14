//! Platform abstraction: everything the app needs from the OS that is not
//! covered by the UI toolkit. Android implements this with JNI calls into
//! the Java `LocationBridge`; the desktop dev build uses a simulator.

use ntrack_core::engine::LocationSample;

/// Events flowing from the platform into the controller.
#[derive(Debug, Clone)]
pub enum PlatformEvent {
    Location(LocationSample),
    /// Result of a permission request triggered by
    /// [`Platform::request_location_permission`].
    PermissionResult(bool),
}

pub trait Platform: Send + Sync + 'static {
    fn has_location_permission(&self) -> bool;
    /// Ask the OS for location (and notification) permission. The outcome
    /// arrives asynchronously as [`PlatformEvent::PermissionResult`].
    fn request_location_permission(&self);
    /// Start platform location updates (and on Android the foreground
    /// service that keeps them alive in the background).
    fn start_location(&self, interval_ms: u64);
    fn stop_location(&self);
    fn open_map(&self, lat: f64, lng: f64, label: &str);
    fn copy_text(&self, text: &str);
    fn share_text(&self, text: &str);
}
