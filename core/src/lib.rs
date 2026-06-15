//! ntrack-core — the ntrack live-location-sharing protocol (Nostr kind:3434),
//! plus the relay plumbing and engines used by the ntrack app.
//!
//! The wire protocol is specified in `docs/PROTOCOL.md`.
//!
//! Layering (everything here is UI-free and runs on any host, which keeps
//! the protocol fully unit-testable off-device):
//!
//! * [`protocol`] — kind:3434 event construction, parsing and validation
//! * [`keys`] — key parsing/generation helpers and secret redaction
//! * [`dedup`] — replay protection (processed event-id tracking)
//! * [`config`] — persisted app configuration (groups, relays, sender key)
//! * [`relay`] — minimal Nostr relay pool (publish / subscribe / reconnect)
//! * [`engine`] — share & track engines orchestrating the above
//! * [`gpx`] — GPX 1.1 serialization for exporting a received track

pub mod config;
pub mod dedup;
pub mod engine;
pub mod error;
pub mod gpx;
pub mod invite;
pub mod keys;
pub mod protocol;
pub mod qr;
pub mod relay;

pub use error::Error;

/// Re-export of the underlying Nostr types used in our public API.
pub use nostr;
