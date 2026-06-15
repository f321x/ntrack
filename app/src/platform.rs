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
    /// A group invite arrived from outside the UI: a scanned QR code
    /// ([`Platform::scan_qr`]) or a tapped `ntrack://join` deep link. The
    /// payload is the raw string; the controller parses it with
    /// [`ntrack_core::invite::parse_shared`] and pre-fills the import form.
    IncomingInvite(String),
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
    /// Read the current clipboard text (empty string if the clipboard is empty
    /// or unreadable). Called synchronously from the UI thread to fill the
    /// import form from a copied invite/key.
    fn paste_text(&self) -> String;
    fn share_text(&self, text: &str);
    /// Hand a generated file (e.g. an exported GPX track) to the OS. When
    /// `prefer_view` is set the platform first tries to open it directly in a
    /// capable app (Android `ACTION_VIEW`) and falls back to the system share
    /// sheet; the view-vs-share decision is made platform-side, where the
    /// intent resolver lives. The bytes are owned by the OS afterwards (Android
    /// writes them to a content-provider–served cache file).
    fn share_file(&self, filename: &str, mime: &str, content: &[u8], prefer_view: bool);
    /// Open the camera QR scanner. The decoded string arrives asynchronously
    /// as [`PlatformEvent::IncomingInvite`].
    fn scan_qr(&self);
}
