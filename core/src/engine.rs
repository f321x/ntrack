//! The app engine: a single task that owns the configuration, the share
//! state machine and the tracking state, decoupled from any UI.
//!
//! The UI layer sends [`EngineCmd`]s and renders [`UiEvent`] snapshots; the
//! platform layer feeds [`LocationSample`]s and reacts to
//! [`UiEvent::NeedLocation`] by starting/stopping platform location updates.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr::{EventId, Filter, Keys, PublicKey};
use tokio::sync::mpsc;

use crate::config::{Config, ConfigStore};
use crate::dedup::SeenIds;
use crate::keys;
use crate::protocol::{self, Payload, Status};
use crate::relay::{PoolEvent, Publisher};

/// NIP-40 expiration attached to outgoing events when enabled.
pub const EXPIRATION_SECS: u64 = 24 * 3600;
/// How far back the tracking subscription looks on (re)start. Matched to the
/// NIP-40 expiration window: events older than that have aged out of relays
/// anyway, while everything still alive is a peer's most recent fix. Looking
/// back the full window means a peer who last published a while ago (but within
/// the expiry) still surfaces their last-known location on startup, rather than
/// only peers broadcasting right now. Replay protection makes the overlap
/// harmless.
pub const SINCE_LOOKBACK_SECS: u64 = EXPIRATION_SECS;
/// Capacity of the processed-event-id replay window.
pub const SEEN_CAPACITY: usize = 4096;
/// Cap on a sender-declared display name once cleaned for display.
const MAX_NAME_CHARS: usize = 48;
/// Location cadence (seconds) forced while a duress alert is active, overriding
/// the configured battery-saving interval: in an emergency, being found fast
/// outweighs battery. A deliberate exception to "never sample faster than the
/// configured interval".
pub const ALERT_INTERVAL_SECS: u64 = 15;
/// Grace window (seconds) granted at startup when a check-in deadline already
/// elapsed while the app/device was down. Rather than escalate instantly (the
/// battery may simply have died and the phone was just plugged in), the user
/// gets this long to confirm they're safe — prompted by a notification — before
/// the alert fires.
pub const STARTUP_GRACE_SECS: u64 = 60;
/// Fraction of an armed check-in period at which the user is nudged once with a
/// reminder before the deadline: at 10% remaining (period / this divisor). A
/// short period scales the lead down with it; a long one gives ample warning.
const CHECKIN_REMINDER_LEAD_DIVISOR: u64 = 10;

// ---- track-history retention (for GPX export) --------------------------
//
// History is in-memory only (decision: ephemeral) and bounded by three caps,
// so the worst case is ~MAX_HISTORY_SESSIONS × MAX_POINTS_PER_SESSION points
// (≈13 MB across 64 sessions). The latest-point *display* path is unaffected:
// history is additive and kept in a separate map.

/// Cap on retained track points per (sender, group) session.
const MAX_POINTS_PER_SESSION: usize = 5_000;
/// Per-insert retention window relative to the newest point in a session.
/// Slightly over the 24 h NIP-40 expiration so a full window survives.
const HISTORY_WINDOW_SECS: u64 = 25 * 3600;
/// Cap on retained sessions; the least-recently-updated is evicted past it.
const MAX_HISTORY_SESSIONS: usize = 64;
/// Upper bound on events requested in a one-shot export backfill.
const BACKFILL_LIMIT: usize = 5_000;
/// How long to wait for backfill EOSEs before exporting whatever arrived.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocationSample {
    pub lat: f64,
    pub lng: f64,
    pub accuracy_m: f32,
    /// Unix time of the fix, milliseconds.
    pub ts_millis: u64,
}

impl LocationSample {
    fn ts_secs(&self) -> u64 {
        self.ts_millis / 1000
    }
}

/// Commands from the UI / platform layer into the engine.
pub enum EngineCmd {
    /// Mutate the configuration; the engine persists it and re-syncs
    /// relays/subscriptions afterwards.
    Mutate(Box<dyn FnOnce(&mut Config) + Send>),
    StartShare { msg: Option<String> },
    /// Resume a share that was active before the process died, but only if the
    /// persisted resume flag is still armed (i.e. the user never explicitly
    /// stopped). Driven by the Android boot "resume" notification tap.
    ResumeShareIfArmed,
    /// Update the message attached to subsequent location publishes.
    SetMessage(Option<String>),
    /// Raise (true) or clear (false) the duress alert on the live share.
    /// Raising re-broadcasts at once, marks every ACTIVE as an alert and boosts
    /// the location cadence; clearing reverts. No-op without an active share.
    SetAlert(bool),
    /// One-tap panic: force-start a share to the emergency audience (the
    /// selected groups, or — if none are selected — every group) AND raise the
    /// alert. If already sharing, just raises the alert.
    Panic,
    /// Arm a dead-man's-switch check-in: escalate to [`Panic`](Self::Panic)
    /// unless the user confirms safety within `secs`.
    ArmCheckin { secs: u64 },
    /// Confirm safety: reset the dead-man's-switch countdown to a fresh full
    /// period rather than disarming, so it keeps protecting the user until they
    /// explicitly disarm it. Also confirms out of the post-startup grace window.
    Checkin,
    /// Disarm the dead-man's-switch entirely, stopping the repeating check-in.
    DisarmCheckin,
    /// Evaluate a persisted check-in at startup (driven by `run`/the boot path,
    /// exposed for tests): resume its countdown, or — if its deadline elapsed
    /// while the app was down — open a grace window and notify rather than fire.
    EvaluateCheckinOnStart,
    StopShare,
    Location(LocationSample),
    /// Permission was denied or location turned off by the platform.
    LocationUnavailable(String),
    /// Ask the engine to emit the share dialog data for a group.
    RequestGroupShare { group_hex: String },
    /// Rotate a group's recipient pseudonym key.
    /// Emits the refreshed config plus a [`UiEvent::GroupShare`] carrying the
    /// new secret for redistribution to members.
    RotateGroup { group_hex: String },
    /// Export a received sender's track (within one group) as GPX: seed from
    /// the live history buffer, fire a one-shot relay backfill, merge, then
    /// emit [`UiEvent::TrackExport`] (or a toast when there is nothing to
    /// export). Non-blocking — backfill rides the existing select!/Tick.
    ExportTrack { sender_hex: String, group_hex: String },
    Pool(PoolEvent),
    /// Periodic flush + share tick (driven internally; exposed for tests).
    Tick,
    Shutdown,
}

/// Snapshot of one group for the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSnapshot {
    /// Recipient pseudonym pubkey, hex — used as the stable group id.
    pub id: String,
    pub name: String,
    pub npub: String,
    pub can_receive: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSnapshot {
    pub groups: Vec<GroupSnapshot>,
    pub relays: Vec<String>,
    pub interval_secs: u64,
    pub sender_npub: String,
    /// The user's configured display name (empty when unset).
    pub display_name: String,
    /// Handle derived from the sender key, shown as the placeholder/fallback
    /// while `display_name` is empty. Empty until the sender key exists.
    pub default_name: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShareSnapshot {
    pub sharing: bool,
    /// Unix seconds of the last successful publish hand-off.
    pub last_publish: Option<u64>,
    pub publish_count: u64,
    /// At least one relay acknowledged the latest event.
    pub last_acked: bool,
    pub waiting_for_fix: bool,
    /// A duress alert is currently raised on the live share.
    pub alert: bool,
    /// Unix-seconds deadline of an armed check-in (a live countdown, or the
    /// startup grace window), for the UI countdown. `None` when none is armed.
    pub checkin_deadline: Option<u64>,
    /// The armed check-in is in the post-startup grace window (its deadline
    /// elapsed while the app was down) — the UI shows a "confirm you're safe"
    /// prompt rather than an ordinary countdown.
    pub checkin_grace: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackSnapshot {
    pub sender_hex: String,
    pub sender_short: String,
    pub label: String,
    /// Effective display name for the sender: the name they broadcast, else a
    /// handle derived from their key. The receiver's own `label` still wins.
    pub name: String,
    /// Accent colour (R, G, B) derived from the sender key — the disambiguator
    /// when two senders happen to share a name.
    pub color: (u8, u8, u8),
    pub group_name: String,
    pub status: Status,
    pub live: bool,
    /// This sender is broadcasting a duress alert (their latest ACTIVE carried
    /// the alert marker). Drives the escalated card styling and pins the track.
    pub alert: bool,
    pub lat: f64,
    pub lng: f64,
    /// Location capture time (unix seconds); 0 when unknown (bare STOP).
    pub ts: u64,
    pub created_at: u64,
    pub msg: String,
    /// Recipient pseudonym (group) pubkey hex this track belongs to; pairs
    /// with `sender_hex` to identify the session for export.
    pub group_hex: String,
}

/// Data for the "share group key" dialog.
#[derive(Debug, Clone)]
pub struct GroupShare {
    pub name: String,
    pub npub: String,
    /// nsec to hand to new members; `None` for send-only groups.
    pub nsec: Option<keys::SecretString>,
    /// Oldest relays to embed in the shared invite so recipients converge on
    /// the same relays (see [`crate::config::Config::invite_relays`]).
    pub relays: Vec<String>,
}

/// Category of a [`UiEvent::Notify`], so the platform can pick the right
/// notification channel / urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    /// A group member raised a duress alert.
    PeerAlert,
    /// A check-in deadline elapsed while the app was down — confirm safety soon.
    CheckinGrace,
    /// A check-in deadline is approaching (one-shot pre-escalation reminder).
    CheckinReminder,
    /// A check-in lapsed and escalated to sharing + alert.
    CheckinEscalated,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Config(ConfigSnapshot),
    Share(ShareSnapshot),
    Tracks(Vec<TrackSnapshot>),
    Relays(Vec<(String, bool)>),
    GroupShare(GroupShare),
    /// A built GPX document for a track, ready to hand to the platform's
    /// file-share (direct-open a maps/track app, with a share-sheet fallback).
    TrackExport { suggested_filename: String, gpx_xml: String },
    /// Platform layer should start (true) / stop (false) location updates.
    NeedLocation(bool),
    /// Platform layer should change the GPS sampling cadence (milliseconds) of
    /// the *running* location session — emitted when a duress alert boosts or
    /// relaxes the interval. Only sent while a share is active.
    SetLocationInterval(u64),
    /// A high-urgency notification the platform should surface even when the app
    /// is backgrounded (sound/vibration, bypassing Do-Not-Disturb where
    /// allowed): an incoming peer alert, or a check-in grace/reminder/escalation.
    Notify { kind: NotifyKind, title: String, body: String },
    Toast(String),
}

struct ShareState {
    sender: Keys,
    recipients: Vec<PublicKey>,
    msg: Option<String>,
    /// Unix-seconds the duress alert was raised on this share; `None` when not
    /// alerting. While set, the publish cadence is forced to
    /// [`ALERT_INTERVAL_SECS`] and every ACTIVE carries the alert marker (and is
    /// re-broadcast even without a fresh fix).
    alert_since: Option<u64>,
    last_publish_at: Option<tokio::time::Instant>,
    last_sample: Option<LocationSample>,
    /// Whether `last_sample` has already been broadcast. Guards the tick
    /// against re-sending a position we've already published (which would
    /// spin the radio for nothing); only genuinely new fixes are broadcast.
    last_sample_published: bool,
    last_event_id: Option<EventId>,
    last_publish_ts: Option<u64>,
    publish_count: u64,
    last_acked: bool,
}

#[derive(Clone)]
struct TrackState {
    group: PublicKey,
    payload: Payload,
    created_at: u64,
    /// Coordinates retained from the last ACTIVE when a STOP arrives.
    last_coords: Option<(f64, f64, u64)>,
    /// Sanitized display name from the last ACTIVE, retained across a STOP
    /// (which carries none) so a sender's chosen name survives going offline.
    last_name: Option<String>,
}

/// One retained location fix for a (sender, group) session.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HistPoint {
    lat: f64,
    lng: f64,
    ts: u64,
    created_at: u64,
}

impl HistPoint {
    /// The point an ACTIVE broadcast contributes to a track, or `None` for
    /// a STOP (a boundary, not a point) or a payload missing coordinates.
    fn from_incoming(inc: &protocol::Incoming) -> Option<Self> {
        match (inc.payload.status, inc.payload.lat, inc.payload.lng, inc.payload.ts) {
            (Status::Active, Some(lat), Some(lng), Some(ts)) => {
                Some(Self { lat, lng, ts, created_at: inc.created_at })
            }
            _ => None,
        }
    }
}

/// Bounded, time-ordered point history for one (sender, group) session.
#[derive(Default)]
struct TrackHistory {
    /// Ascending by `ts`, deduplicated by `ts`.
    points: VecDeque<HistPoint>,
    /// Newest `created_at` recorded, used for global LRU eviction.
    last_seen: u64,
}

impl TrackHistory {
    /// Insert a point keeping the deque ascending and deduped by `ts` (a tie
    /// keeps the larger `created_at`), then prune by the retention window and
    /// the per-session count cap.
    fn insert(&mut self, p: HistPoint) {
        self.last_seen = self.last_seen.max(p.created_at);
        match self.points.binary_search_by(|e| e.ts.cmp(&p.ts)) {
            Ok(idx) => {
                if p.created_at > self.points[idx].created_at {
                    self.points[idx] = p;
                }
            }
            Err(idx) => self.points.insert(idx, p),
        }
        // Window prune relative to the newest point.
        if let Some(newest) = self.points.back().map(|e| e.ts) {
            let cutoff = newest.saturating_sub(HISTORY_WINDOW_SECS);
            while self.points.front().map(|e| e.ts < cutoff).unwrap_or(false) {
                self.points.pop_front();
            }
        }
        while self.points.len() > MAX_POINTS_PER_SESSION {
            self.points.pop_front();
        }
    }
}

/// Dead-man's-switch state. Escalates to a [`Panic`](EngineCmd::Panic) unless
/// the user confirms safety in time.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CheckinState {
    /// A live countdown to `deadline` (unix seconds). `period_secs` is the armed
    /// span (retained for display); `reminded` guards the one-shot reminder.
    Armed { deadline: u64, period_secs: u64, reminded: bool },
    /// A deadline that elapsed while the app/device was down, detected at
    /// startup. The user has until `until` (unix seconds) to confirm safety —
    /// the notification was already posted — before escalating.
    StartupGrace { until: u64 },
}

/// An in-flight export: a one-shot relay backfill whose results merge with the
/// live history seed once every reached relay EOSEs (or the timeout fires).
struct PendingExport {
    sender_hex: String,
    group_hex: String,
    /// GPX track name (user label or short npub).
    label: String,
    suggested_filename: String,
    /// Relays still expected to EOSE.
    relays_pending: usize,
    deadline: tokio::time::Instant,
    /// Live seed plus arrived backfill points; deduped/sorted at finish.
    collected: Vec<HistPoint>,
}

pub struct Engine<P: EnginePool> {
    store: ConfigStore,
    config: Config,
    pool: Arc<P>,
    seen: SeenIds,
    seen_dirty: bool,
    share: Option<ShareState>,
    /// Armed dead-man's-switch, if any. Independent of an active share — a
    /// check-in can be armed without sharing, and escalates into one.
    checkin: Option<CheckinState>,
    /// keyed by (sender hex, group hex)
    tracks: BTreeMap<(String, String), TrackState>,
    /// Bounded per-session point history for export, keyed identically to
    /// `tracks` but separate so the latest-point display path is untouched.
    history: BTreeMap<(String, String), TrackHistory>,
    /// In-flight exports keyed by fetch correlation id.
    pending_exports: BTreeMap<u64, PendingExport>,
    next_corr: u64,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
}

impl<P: EnginePool> Engine<P> {
    pub fn new(
        store: ConfigStore,
        pool: Arc<P>,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        let config = match store.load() {
            Ok(c) => c,
            Err(e) => {
                log::error!("config load failed ({e}); starting with defaults in memory");
                let _ = ui_tx.send(UiEvent::Toast(
                    "Config could not be read; using defaults".into(),
                ));
                Config::default()
            }
        };
        let seen = SeenIds::from_vec(SEEN_CAPACITY, &config.processed_ids);
        Self {
            store,
            config,
            pool,
            seen,
            seen_dirty: false,
            share: None,
            checkin: None,
            tracks: BTreeMap::new(),
            history: BTreeMap::new(),
            pending_exports: BTreeMap::new(),
            next_corr: 0,
            ui_tx,
        }
    }

    /// Run the engine until [`EngineCmd::Shutdown`] (or channel close).
    pub async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<EngineCmd>) {
        self.sync_pool();
        self.emit_config();
        self.emit_share();
        self.emit_tracks();
        // Evaluate any persisted check-in: resume its countdown, or — if it
        // lapsed while we were down — open a grace window and notify.
        self.handle(EngineCmd::EvaluateCheckinOnStart);

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut flush = tokio::time::interval(Duration::from_secs(60));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(EngineCmd::Shutdown) => break,
                    Some(cmd) => self.handle(cmd),
                },
                _ = tick.tick() => self.handle(EngineCmd::Tick),
                _ = flush.tick() => self.flush_seen(),
            }
        }
        self.flush_seen();
        // Best effort STOP so receivers don't show a stale "live" state.
        if self.share.is_some() {
            self.stop_share();
        }
    }

    pub fn handle(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::Mutate(f) => {
                f(&mut self.config);
                self.persist();
                self.sync_pool();
                self.emit_config();
                // Track cards derive their title (and group name) from config,
                // so a rename/relabel must refresh them immediately rather than
                // waiting for the next incoming event to re-emit tracks.
                self.emit_tracks();
            }
            EngineCmd::StartShare { msg } => self.start_share(msg),
            EngineCmd::ResumeShareIfArmed => {
                if self.config.resume_share && self.share.is_none() {
                    let msg = self.config.resume_msg.clone();
                    // Restore the alert too, so a reboot mid-emergency resumes
                    // alerting rather than a plain share.
                    let alert_since = self.config.alert_active.then(now_secs);
                    let recipients = self.selected_recipients();
                    if recipients.is_empty() {
                        self.toast("Select at least one group to share with".into());
                        self.emit_share();
                    } else {
                        self.begin_share(recipients, msg, alert_since);
                    }
                }
            }
            EngineCmd::SetMessage(msg) => {
                if let Some(s) = &mut self.share {
                    s.msg = msg.filter(|m| !m.trim().is_empty());
                }
            }
            EngineCmd::SetAlert(on) => self.set_alert(on),
            EngineCmd::Panic => self.trigger_panic(),
            EngineCmd::ArmCheckin { secs } => self.arm_checkin(secs),
            EngineCmd::Checkin => self.confirm_checkin(),
            EngineCmd::DisarmCheckin => {
                let was_armed = self.checkin.is_some();
                self.disarm_checkin();
                if was_armed {
                    self.toast("Check-in disarmed".into());
                }
            }
            EngineCmd::EvaluateCheckinOnStart => self.evaluate_checkin_on_start(),
            EngineCmd::StopShare => {
                // An explicit user stop disarms boot-resume (and the alert); a
                // process-death STOP (run()'s shutdown tail) goes through
                // stop_share() directly and deliberately leaves the flag armed.
                self.disarm_resume();
                self.stop_share();
            }
            EngineCmd::Location(sample) => self.on_location(sample),
            EngineCmd::LocationUnavailable(reason) => {
                if self.share.is_some() {
                    self.toast(format!("Location unavailable: {reason}"));
                    // Permission revoked / GPS off is a real interruption the
                    // user caused; don't try to resume into a denied state.
                    self.disarm_resume();
                    self.stop_share();
                } else {
                    self.emit_share();
                    let _ = self.ui_tx.send(UiEvent::NeedLocation(false));
                }
            }
            EngineCmd::RequestGroupShare { group_hex } => {
                if let Some(g) = self.config.groups.iter().find(|g| g.public == group_hex) {
                    let share = GroupShare {
                        name: g.name.clone(),
                        npub: g
                            .public_key()
                            .map(|pk| keys::npub(&pk))
                            .unwrap_or_else(|_| g.public.clone()),
                        nsec: g.secret.clone(),
                        relays: self.config.invite_relays(),
                    };
                    let _ = self.ui_tx.send(UiEvent::GroupShare(share));
                }
            }
            EngineCmd::RotateGroup { group_hex } => {
                let new_hex = {
                    let Some(g) = self
                        .config
                        .groups
                        .iter_mut()
                        .find(|g| g.public == group_hex)
                    else {
                        return;
                    };
                    g.rotate();
                    g.public.clone()
                };
                self.persist();
                self.sync_pool();
                self.emit_config();
                self.toast("Key rotated — distribute the new key to all members".into());
                self.handle(EngineCmd::RequestGroupShare { group_hex: new_hex });
            }
            EngineCmd::ExportTrack { sender_hex, group_hex } => {
                self.export_track(sender_hex, group_hex);
            }
            EngineCmd::Pool(ev) => self.on_pool_event(ev),
            EngineCmd::Tick => self.on_tick(),
            // run() exits on Shutdown before reaching here; tolerated as a
            // no-op so direct handle() calls in tests cannot panic.
            EngineCmd::Shutdown => {}
        }
    }

    // ---- share path ----------------------------------------------------

    fn selected_recipients(&self) -> Vec<PublicKey> {
        self.config
            .groups
            .iter()
            .filter(|g| g.selected)
            .filter_map(|g| g.public_key().ok())
            .collect()
    }

    fn start_share(&mut self, msg: Option<String>) {
        let recipients = self.selected_recipients();
        if recipients.is_empty() {
            self.toast("Select at least one group to share with".into());
            self.emit_share();
            return;
        }
        self.begin_share(recipients, msg, None);
    }

    /// One-tap panic: force-start a share to the emergency audience and raise
    /// the duress alert. If already sharing, just raises the alert.
    fn trigger_panic(&mut self) {
        if self.share.is_some() {
            self.set_alert(true);
            return;
        }
        let recipients = self.emergency_recipients();
        if recipients.is_empty() {
            self.toast("Add a group before raising an alert".into());
            return;
        }
        self.begin_share(recipients, None, Some(now_secs()));
    }

    /// Recipients an alert/panic broadcasts to: the groups selected for sharing,
    /// or — if none are selected — every group, so a panic never silently
    /// no-ops just because nothing was ticked on the Share screen.
    fn emergency_recipients(&self) -> Vec<PublicKey> {
        let selected = self.selected_recipients();
        if !selected.is_empty() {
            return selected;
        }
        self.config
            .groups
            .iter()
            .filter_map(|g| g.public_key().ok())
            .collect()
    }

    /// Shared share-start: validate the sender key, arm boot-resume (with the
    /// alert state, if any), install the share, and ask the platform for
    /// location at the effective cadence. `alert_since` set → start alerting.
    fn begin_share(
        &mut self,
        recipients: Vec<PublicKey>,
        msg: Option<String>,
        alert_since: Option<u64>,
    ) {
        if self.share.is_some() {
            // Already sharing: at most fold the alert into the running share.
            if alert_since.is_some() {
                self.set_alert(true);
            }
            return;
        }
        let sender = match self.config.sender_keys() {
            Ok(k) => k,
            Err(e) => {
                self.toast(format!("Sender key error: {e}"));
                return;
            }
        };
        let msg = msg.filter(|m| !m.trim().is_empty());
        // Arm boot-resume: persisted so a reboot/crash while sharing can
        // continue. The alert rides along so an emergency resumes as one.
        // Cleared only on an explicit stop / permission loss (see disarm_resume).
        self.config.resume_share = true;
        self.config.resume_msg = msg.clone();
        self.config.alert_active = alert_since.is_some();
        self.persist();
        self.share = Some(ShareState {
            sender,
            recipients,
            msg,
            alert_since,
            last_publish_at: None,
            last_sample: None,
            last_sample_published: false,
            last_event_id: None,
            last_publish_ts: None,
            publish_count: 0,
            last_acked: false,
        });
        let _ = self.ui_tx.send(UiEvent::NeedLocation(true));
        // A panic starts already alerting, so push the boosted cadence down to
        // the just-started location session (NeedLocation alone starts it at the
        // configured interval).
        if alert_since.is_some() {
            let _ = self
                .ui_tx
                .send(UiEvent::SetLocationInterval(self.effective_interval_secs() * 1000));
        }
        self.emit_share();
    }

    /// Raise (true) or clear (false) the duress alert on the live share:
    /// re-broadcast the current fix at once, persist the alert (so a reboot
    /// resumes it) and boost/relax the location cadence. No-op without a share.
    fn set_alert(&mut self, on: bool) {
        let Some(share) = &mut self.share else {
            if on {
                self.toast("Start sharing to raise an alert".into());
            }
            return;
        };
        if on == share.alert_since.is_some() {
            return; // already in the requested state
        }
        share.alert_since = on.then(now_secs);
        let sample = share.last_sample;
        self.config.alert_active = on;
        self.persist();
        // Re-broadcast immediately so the change reaches the group without
        // waiting for the next interval.
        if let Some(sample) = sample {
            self.publish_active(sample);
        }
        // Boost / relax the GPS cadence of the running location session.
        let _ = self
            .ui_tx
            .send(UiEvent::SetLocationInterval(self.effective_interval_secs() * 1000));
        self.emit_share();
        self.toast(if on {
            "Alert raised — your group is being notified".into()
        } else {
            "Alert cleared".into()
        });
    }

    /// The location cadence (seconds) that currently governs both publishing and
    /// GPS sampling: the fast [`ALERT_INTERVAL_SECS`] while alerting, otherwise
    /// the configured interval (floored at 5 s).
    fn effective_interval_secs(&self) -> u64 {
        let alerting = self
            .share
            .as_ref()
            .map(|s| s.alert_since.is_some())
            .unwrap_or(false);
        if alerting {
            ALERT_INTERVAL_SECS
        } else {
            self.config.interval_secs.max(5)
        }
    }

    /// Clear the persisted boot-resume flag, its message and the alert (and the
    /// sentinel). Called on an explicit user stop or a permission/GPS loss —
    /// never on the best-effort shutdown STOP, which must leave resume armed.
    fn disarm_resume(&mut self) {
        if self.config.resume_share || self.config.resume_msg.is_some() || self.config.alert_active {
            self.config.resume_share = false;
            self.config.resume_msg = None;
            self.config.alert_active = false;
            self.persist();
        }
    }

    fn stop_share(&mut self) {
        if let Some(state) = self.share.take() {
            match protocol::build_event(
                &state.sender,
                &state.recipients,
                &Payload::stop(),
                self.expiration(),
            ) {
                Ok(event) => self.pool.publish(event),
                Err(e) => log::error!("failed to build STOP event: {e}"),
            }
        }
        let _ = self.ui_tx.send(UiEvent::NeedLocation(false));
        self.emit_share();
    }

    fn on_location(&mut self, sample: LocationSample) {
        let interval = Duration::from_secs(self.effective_interval_secs());
        let due = match &self.share {
            Some(s) => match s.last_publish_at {
                None => true,
                Some(at) => at.elapsed() >= interval,
            },
            None => false,
        };
        if let Some(s) = &mut self.share {
            s.last_sample = Some(sample);
            s.last_sample_published = false;
        }
        if due {
            self.publish_active(sample);
        } else {
            self.emit_share();
        }
    }

    fn on_tick(&mut self) {
        let interval = Duration::from_secs(self.effective_interval_secs());
        // The tick only catches up a fix that arrived off-cycle (before its
        // interval elapsed); fresh fixes are published on arrival in
        // `on_location`. Normally an already-published position is never re-sent
        // — when the GPS stalls we go quiet rather than re-broadcasting a stale
        // point, saving the radio and bandwidth. While alerting we make the
        // opposite trade: re-broadcast even an already-sent fix so receivers
        // keep getting "still in danger" heartbeats and the last-known point.
        let due_sample = self.share.as_ref().and_then(|s| {
            let due = match s.last_publish_at {
                None => true,
                Some(at) => at.elapsed() >= interval,
            };
            let alerting = s.alert_since.is_some();
            if due && (alerting || !s.last_sample_published) {
                s.last_sample
            } else {
                None
            }
        });
        if let Some(sample) = due_sample {
            self.publish_active(sample);
        }
        self.tick_checkin();
        // Ship any export whose backfill window elapsed (live seed + whatever
        // backfill arrived) — this is the only thing that completes an export
        // when a relay is unreachable and never sends its EOSE.
        let now = tokio::time::Instant::now();
        let timed_out: Vec<u64> = self
            .pending_exports
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(corr, _)| *corr)
            .collect();
        for corr in timed_out {
            self.finish_export(corr);
        }
    }

    fn publish_active(&mut self, sample: LocationSample) {
        let (msg, alert) = match &self.share {
            Some(s) => (s.msg.clone(), s.alert_since),
            None => (None, None),
        };
        let name = self.outgoing_name();
        self.publish_payload(
            Payload::active(sample.lat, sample.lng, sample.ts_secs(), msg)
                .with_name(name)
                .with_alert(alert),
        );
    }

    /// The display name to stamp on outgoing ACTIVE broadcasts: the user's
    /// configured name, trimmed. `None` → omit it so receivers derive the same
    /// default handle we would (keeping the wire payload minimal).
    fn outgoing_name(&self) -> Option<String> {
        let n = self.config.display_name.trim();
        (!n.is_empty()).then(|| n.to_string())
    }

    /// Build, sign and hand the active-share payload to the relay pool,
    /// recording publish statistics on the live share.
    fn publish_payload(&mut self, payload: Payload) {
        let Some(state) = &self.share else { return };
        let sender = state.sender.clone();
        let recipients = state.recipients.clone();
        if recipients.is_empty() {
            return;
        }
        match protocol::build_event(&sender, &recipients, &payload, self.expiration()) {
            Ok(event) => {
                let id = event.id;
                self.pool.publish(event);
                if let Some(s) = &mut self.share {
                    s.last_publish_at = Some(tokio::time::Instant::now());
                    s.last_publish_ts = Some(now_secs());
                    s.publish_count += 1;
                    s.last_event_id = Some(id);
                    s.last_acked = false;
                    s.last_sample_published = true;
                }
                self.emit_share();
            }
            Err(e) => {
                log::error!("failed to build event: {e}");
                self.toast(format!("Failed to build event: {e}"));
            }
        }
    }

    fn expiration(&self) -> Option<u64> {
        self.config.use_expiration.then_some(EXPIRATION_SECS)
    }

    // ---- check-in (dead-man's switch) ----------------------------------

    /// Arm a check-in that escalates to a panic unless confirmed within `secs`.
    /// Persisted (plus a boot sentinel) so it survives a reboot.
    fn arm_checkin(&mut self, secs: u64) {
        let secs = secs.max(1);
        self.install_checkin(secs);
        self.toast(format!("Check-in armed — confirm within {}", fmt_duration(secs)));
        self.emit_share();
    }

    /// Confirm safety. Instead of disarming, the dead-man's switch automatically
    /// re-arms for another full period, so it keeps protecting the user until
    /// they explicitly disarm it (see [`Self::disarm_checkin`]). Confirms out of
    /// the startup grace window too, re-arming from the persisted period. No-op
    /// when nothing is armed.
    fn confirm_checkin(&mut self) {
        let period = match &self.checkin {
            Some(CheckinState::Armed { period_secs, .. }) => Some(*period_secs),
            // Grace leaves the persisted period untouched, so re-arm from config.
            Some(CheckinState::StartupGrace { .. }) => self.config.checkin_period_secs,
            None => return,
        };
        match period.filter(|p| *p > 0) {
            Some(period) => {
                self.install_checkin(period);
                self.emit_share();
                self.toast(format!(
                    "Checked in — next check-in within {}",
                    fmt_duration(period)
                ));
            }
            // No period to re-arm with (shouldn't happen): clear it instead.
            None => {
                let was_armed = self.checkin.is_some();
                self.disarm_checkin();
                if was_armed {
                    self.toast("Checked in — you're safe".into());
                }
            }
        }
    }

    /// (Re)install an armed check-in counting down `secs` from now, persisting
    /// the deadline/period and the boot sentinel. No toast or snapshot — callers
    /// add the user-facing message and emit.
    fn install_checkin(&mut self, secs: u64) {
        let secs = secs.max(1);
        let deadline = now_secs() + secs;
        self.checkin = Some(CheckinState::Armed { deadline, period_secs: secs, reminded: false });
        self.config.checkin_deadline = Some(deadline);
        self.config.checkin_period_secs = Some(secs);
        self.persist();
        self.store.set_checkin_flag(true);
    }

    /// Disarm any check-in (an explicit user disarm, or after an escalation
    /// fired) and clear its persisted state and boot sentinel.
    fn disarm_checkin(&mut self) {
        let had_state = self.checkin.is_some()
            || self.config.checkin_deadline.is_some()
            || self.config.checkin_period_secs.is_some();
        if !had_state {
            return;
        }
        self.checkin = None;
        self.config.checkin_deadline = None;
        self.config.checkin_period_secs = None;
        self.persist();
        self.store.set_checkin_flag(false);
        self.emit_share();
    }

    /// Evaluate a persisted check-in at startup. A deadline still in the future
    /// resumes its countdown; one that elapsed while the app/device was down
    /// opens a brief grace window (and posts a notification) rather than firing
    /// at once — a phone whose battery died and was just plugged in shouldn't
    /// trip a false alarm. The persisted deadline is left untouched in the grace
    /// case, so a second kill before the user confirms re-grants a fresh grace.
    fn evaluate_checkin_on_start(&mut self) {
        let Some(deadline) = self.config.checkin_deadline else {
            return;
        };
        let period = self.config.checkin_period_secs.unwrap_or(0);
        let now = now_secs();
        if now < deadline {
            self.checkin =
                Some(CheckinState::Armed { deadline, period_secs: period, reminded: false });
        } else {
            let until = now + STARTUP_GRACE_SECS;
            self.checkin = Some(CheckinState::StartupGrace { until });
            self.notify(
                NotifyKind::CheckinGrace,
                "Check-in lapsed while you were away".into(),
                format!(
                    "Open ntrack and tap \"I'm safe\" within {}, or an alert will be sent to your groups.",
                    fmt_duration(STARTUP_GRACE_SECS)
                ),
            );
        }
        self.emit_share();
    }

    /// Per-tick check-in driver: nudge once before the deadline, then escalate
    /// to a panic once the deadline (or startup grace window) elapses.
    fn tick_checkin(&mut self) {
        enum Action {
            None,
            Remind,
            Escalate,
        }
        let now = now_secs();
        let action = match &mut self.checkin {
            Some(CheckinState::Armed { deadline, period_secs, reminded }) => {
                // Nudge once when 10% of the armed period remains.
                let lead = *period_secs / CHECKIN_REMINDER_LEAD_DIVISOR;
                if now >= *deadline {
                    Action::Escalate
                } else if !*reminded && lead > 0 && now >= deadline.saturating_sub(lead) {
                    *reminded = true;
                    Action::Remind
                } else {
                    Action::None
                }
            }
            Some(CheckinState::StartupGrace { until }) => {
                if now >= *until {
                    Action::Escalate
                } else {
                    Action::None
                }
            }
            None => Action::None,
        };
        match action {
            Action::None => {}
            Action::Remind => self.notify(
                NotifyKind::CheckinReminder,
                "Check-in due soon".into(),
                "Open ntrack and confirm you're safe, or an alert will be sent.".into(),
            ),
            Action::Escalate => {
                // Clear the check-in first so the escalation's share-start isn't
                // immediately re-evaluated, then raise the alert.
                self.disarm_checkin();
                self.notify(
                    NotifyKind::CheckinEscalated,
                    "Check-in missed — alert sent".into(),
                    "You didn't check in, so ntrack started sharing and raised an alert.".into(),
                );
                self.trigger_panic();
            }
        }
    }

    fn notify(&self, kind: NotifyKind, title: String, body: String) {
        let _ = self.ui_tx.send(UiEvent::Notify { kind, title, body });
    }

    // ---- track path ----------------------------------------------------

    fn member_keys(&self) -> Vec<Keys> {
        self.config
            .groups
            .iter()
            .filter_map(|g| g.member_keys())
            .collect()
    }

    fn on_pool_event(&mut self, ev: PoolEvent) {
        match ev {
            PoolEvent::Incoming { event, .. } => {
                let member_keys = self.member_keys();
                match protocol::process_incoming(&event, &member_keys, &mut self.seen) {
                    Ok(incoming) => {
                        self.seen_dirty = true;
                        self.apply_incoming(incoming);
                    }
                    Err(drop) => {
                        log::debug!("dropped incoming event: {drop:?}");
                    }
                }
            }
            PoolEvent::Status { .. } => {
                let _ = self.ui_tx.send(UiEvent::Relays(self.pool_status()));
            }
            PoolEvent::PublishAck { event_id, accepted, message, url } => {
                if let Some(s) = &mut self.share {
                    if s.last_event_id == Some(event_id) && accepted {
                        s.last_acked = true;
                        self.emit_share();
                    }
                }
                if !accepted {
                    log::warn!("relay {url} rejected event {event_id}: {message}");
                }
            }
            PoolEvent::Eose { .. } => {}
            PoolEvent::FetchEvent { corr, event, .. } => self.on_fetch_event(corr, &event),
            PoolEvent::FetchEose { corr, .. } => {
                let done = match self.pending_exports.get_mut(&corr) {
                    Some(p) => {
                        p.relays_pending = p.relays_pending.saturating_sub(1);
                        p.relays_pending == 0
                    }
                    None => false,
                };
                if done {
                    self.finish_export(corr);
                }
            }
        }
    }

    fn apply_incoming(&mut self, inc: protocol::Incoming) {
        let key = (inc.sender.to_hex(), inc.group.to_hex());
        // Record into history *before* the out-of-order display guard: an
        // out-of-order ACTIVE still represents a real past point that belongs
        // in an exported track, even though it must not move the live display.
        self.record_history(&key, &inc);
        let prev = self.tracks.get(&key);
        // Out-of-order delivery: only apply if not older than current state.
        if let Some(prev) = prev {
            if inc.created_at < prev.created_at {
                return;
            }
        }
        let last_coords = match inc.payload.status {
            Status::Stop => prev.and_then(|p| {
                p.last_coords.or(match (p.payload.lat, p.payload.lng, p.payload.ts) {
                    (Some(lat), Some(lng), Some(ts)) => Some((lat, lng, ts)),
                    _ => None,
                })
            }),
            _ => None,
        };
        // A STOP carries no name, so keep the last one the sender broadcast.
        let last_name = match inc.payload.status {
            Status::Stop => prev.and_then(|p| p.last_name.clone()),
            _ => sanitize_name(inc.payload.name.as_deref()),
        };
        // Escalate only on the no-alert → alert edge (per sender), so a sticky
        // alert re-asserted on every heartbeat fires one loud notification, not
        // one per broadcast. STOP carries no alert, so it never triggers.
        let was_alerting = prev.map(|p| p.payload.alert.is_some()).unwrap_or(false);
        let alert_edge = inc.payload.alert.is_some() && !was_alerting;
        let alert_name =
            alert_edge.then(|| self.incoming_display_name(&inc, last_name.as_deref()));
        self.tracks.insert(
            key,
            TrackState {
                group: inc.group,
                payload: inc.payload,
                created_at: inc.created_at,
                last_coords,
                last_name,
            },
        );
        self.emit_tracks();
        if let Some(name) = alert_name {
            self.notify(
                NotifyKind::PeerAlert,
                format!("⚠ Alert from {name}"),
                "A group member raised an alert. Tap to see their live location.".into(),
            );
        }
    }

    /// Effective display name for an incoming sender, for a notification: the
    /// receiver's own label, else the name they broadcast this session, else a
    /// key-derived handle. Mirrors the Track-card title logic.
    fn incoming_display_name(&self, inc: &protocol::Incoming, last_name: Option<&str>) -> String {
        if let Some(l) = self.config.label_for(&inc.sender.to_hex()) {
            if !l.is_empty() {
                return l.to_string();
            }
        }
        if let Some(name) = last_name {
            return name.to_string();
        }
        keys::derive_name(&inc.sender)
    }

    // ---- track history & export -----------------------------------------

    /// Record an incoming broadcast into the per-session history buffer.
    /// ACTIVE/TEST with coordinates become points; STOP is a boundary, not a
    /// point. Additive and order-independent.
    fn record_history(&mut self, key: &(String, String), inc: &protocol::Incoming) {
        let Some(point) = HistPoint::from_incoming(inc) else {
            return;
        };
        self.history.entry(key.clone()).or_default().insert(point);
        self.prune_history_global();
    }

    /// Evict whole sessions (least-recently-updated first) past the global cap.
    fn prune_history_global(&mut self) {
        while self.history.len() > MAX_HISTORY_SESSIONS {
            let Some(victim) = self
                .history
                .iter()
                .min_by_key(|(_, h)| h.last_seen)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.history.remove(&victim);
        }
    }

    /// Begin an export: seed from the live history buffer, fire a one-shot
    /// relay backfill, and either emit immediately (no connected relays) or
    /// register a pending export resolved by FetchEose / the tick timeout.
    /// Fully non-blocking — `pool.fetch` only does non-awaiting channel sends.
    fn export_track(&mut self, sender_hex: String, group_hex: String) {
        let (Ok(group), Ok(sender)) =
            (keys::parse_public(&group_hex), keys::parse_public(&sender_hex))
        else {
            self.toast("Cannot export this track".into());
            return;
        };
        let label = self.export_label(&sender_hex, &group_hex);
        let suggested_filename = export_filename(&label);

        // Seed from the in-memory history for this session.
        let key = (sender_hex.clone(), group_hex.clone());
        let collected: Vec<HistPoint> = self
            .history
            .get(&key)
            .map(|h| h.points.iter().copied().collect())
            .unwrap_or_default();

        let filter = protocol::backfill_filter(group, sender, HISTORY_WINDOW_SECS, BACKFILL_LIMIT);
        let corr = self.next_corr;
        self.next_corr += 1;
        let relays_pending = self.pool.fetch(corr, filter);
        self.pending_exports.insert(
            corr,
            PendingExport {
                sender_hex,
                group_hex,
                label,
                suggested_filename,
                relays_pending,
                deadline: tokio::time::Instant::now() + FETCH_TIMEOUT,
                collected,
            },
        );
        // No connected relays → nothing will EOSE; ship the live buffer now.
        if relays_pending == 0 {
            self.finish_export(corr);
        }
    }

    /// One backfill event arrived for an in-flight export. Decrypt it
    /// specifically for the export's target group (so a sender broadcasting to
    /// several of our groups can't smear points across exports) via
    /// [`protocol::process_for_export`] — NO replay dedup, so events already
    /// seen live are recovered rather than dropped.
    fn on_fetch_event(&mut self, corr: u64, event: &nostr::Event) {
        let Some((group_hex, sender_hex)) = self
            .pending_exports
            .get(&corr)
            .map(|p| (p.group_hex.clone(), p.sender_hex.clone()))
        else {
            return;
        };
        let group_keys: Vec<Keys> = self
            .config
            .groups
            .iter()
            .filter(|g| g.public == group_hex)
            .filter_map(|g| g.member_keys())
            .collect();
        let Ok(inc) = protocol::process_for_export(event, &group_keys) else {
            return;
        };
        if inc.sender.to_hex() != sender_hex {
            return;
        }
        if let Some(point) = HistPoint::from_incoming(&inc) {
            if let Some(p) = self.pending_exports.get_mut(&corr) {
                p.collected.push(point);
            }
        }
    }

    /// Assemble and emit a finished export: merge + dedup by `ts` (larger
    /// `created_at` wins) + sort ascending → GPX. Empty → a toast.
    fn finish_export(&mut self, corr: u64) {
        let Some(pending) = self.pending_exports.remove(&corr) else {
            return;
        };
        let mut by_ts: BTreeMap<u64, HistPoint> = BTreeMap::new();
        for p in pending.collected {
            by_ts
                .entry(p.ts)
                .and_modify(|e| {
                    if p.created_at > e.created_at {
                        *e = p;
                    }
                })
                .or_insert(p);
        }
        let points: Vec<(f64, f64, u64)> =
            by_ts.into_values().map(|p| (p.lat, p.lng, p.ts)).collect();
        if points.is_empty() {
            self.toast("No track points to export".into());
            return;
        }
        let gpx_xml = crate::gpx::build_gpx(&pending.label, &points);
        let _ = self.ui_tx.send(UiEvent::TrackExport {
            suggested_filename: pending.suggested_filename,
            gpx_xml,
        });
    }

    /// Human label for a track export: the receiver's own label, else the name
    /// the sender broadcast for this session, else a key-derived handle. Mirrors
    /// the Track tab title so the GPX name matches what the user saw.
    fn export_label(&self, sender_hex: &str, group_hex: &str) -> String {
        if let Some(l) = self.config.label_for(sender_hex) {
            if !l.is_empty() {
                return l.to_string();
            }
        }
        if let Some(name) = self
            .tracks
            .get(&(sender_hex.to_string(), group_hex.to_string()))
            .and_then(|t| t.last_name.clone())
        {
            return name;
        }
        keys::parse_public(sender_hex)
            .map(|pk| keys::derive_name(&pk))
            .unwrap_or_else(|_| sender_hex.to_string())
    }

    // ---- snapshots & plumbing -------------------------------------------

    fn sync_pool(&mut self) {
        self.pool.set_relays_list(&self.config.relays);
        let member_pks: Vec<PublicKey> = self
            .member_keys()
            .iter()
            .map(|k| k.public_key())
            .collect();
        let filter = (!member_pks.is_empty())
            .then(|| protocol::subscription_filter(&member_pks, SINCE_LOOKBACK_SECS));
        self.pool.set_subscription(filter);
        let _ = self.ui_tx.send(UiEvent::Relays(self.pool_status()));
    }

    fn pool_status(&self) -> Vec<(String, bool)> {
        self.pool.relay_status_list()
    }

    fn persist(&mut self) {
        self.config.processed_ids = self.seen.to_vec();
        if let Err(e) = self.store.save(&self.config) {
            log::error!("config save failed: {e}");
            self.toast("Failed to save settings".into());
        }
        // Keep the boot-resume sentinel in lockstep with the persisted flag,
        // wherever config is saved.
        self.store.set_resume_flag(self.config.resume_share);
    }

    fn flush_seen(&mut self) {
        if self.seen_dirty {
            self.seen_dirty = false;
            self.persist();
        }
    }

    fn toast(&self, msg: String) {
        let _ = self.ui_tx.send(UiEvent::Toast(msg));
    }

    fn emit_config(&mut self) {
        let sender_pk = self
            .config
            .sender_secret
            .as_ref()
            .and_then(|s| keys::parse_secret(s.expose()).ok())
            .map(|sk| Keys::new(sk).public_key());
        let sender_npub = sender_pk.map(|pk| keys::npub(&pk)).unwrap_or_default();
        // The handle a receiver derives from our sender key — shown as the
        // settings placeholder/fallback while no custom name is set.
        let default_name = sender_pk.map(|pk| keys::derive_name(&pk)).unwrap_or_default();
        let groups = self
            .config
            .groups
            .iter()
            .map(|g| GroupSnapshot {
                id: g.public.clone(),
                name: g.name.clone(),
                npub: g
                    .public_key()
                    .map(|pk| keys::npub(&pk))
                    .unwrap_or_else(|_| g.public.clone()),
                can_receive: g.secret.is_some(),
                selected: g.selected,
            })
            .collect();
        let _ = self.ui_tx.send(UiEvent::Config(ConfigSnapshot {
            groups,
            relays: self.config.relays.clone(),
            interval_secs: self.config.interval_secs,
            sender_npub,
            display_name: self.config.display_name.clone(),
            default_name,
        }));
    }

    fn emit_share(&self) {
        let base = match &self.share {
            Some(s) => ShareSnapshot {
                sharing: true,
                last_publish: s.last_publish_ts,
                publish_count: s.publish_count,
                last_acked: s.last_acked,
                waiting_for_fix: s.last_sample.is_none(),
                alert: s.alert_since.is_some(),
                ..Default::default()
            },
            None => ShareSnapshot::default(),
        };
        // The check-in is independent of an active share, so it's always folded
        // into the snapshot.
        let (checkin_deadline, checkin_grace) = match &self.checkin {
            Some(CheckinState::Armed { deadline, .. }) => (Some(*deadline), false),
            Some(CheckinState::StartupGrace { until }) => (Some(*until), true),
            None => (None, false),
        };
        let _ = self.ui_tx.send(UiEvent::Share(ShareSnapshot {
            checkin_deadline,
            checkin_grace,
            ..base
        }));
    }

    fn emit_tracks(&self) {
        let mut out = Vec::new();
        for ((sender_hex, group_hex), t) in &self.tracks {
            let group_name = self
                .config
                .groups
                .iter()
                .find(|g| &g.public == group_hex)
                .map(|g| g.name.clone())
                .unwrap_or_else(|| keys::short_npub(&t.group));
            let (lat, lng, ts) = match t.payload.status {
                Status::Stop => t.last_coords.unwrap_or((0.0, 0.0, 0)),
                _ => (
                    t.payload.lat.unwrap_or(0.0),
                    t.payload.lng.unwrap_or(0.0),
                    t.payload.ts.unwrap_or(0),
                ),
            };
            let sender_pk = keys::parse_public(sender_hex).ok();
            let sender_short = sender_pk
                .as_ref()
                .map(keys::short_npub)
                .unwrap_or_else(|| sender_hex.clone());
            // Effective name: what the sender broadcast, else a handle derived
            // from their key. A receiver-set label overrides this in the UI.
            let name = t
                .last_name
                .clone()
                .or_else(|| sender_pk.as_ref().map(keys::derive_name))
                .unwrap_or_else(|| sender_short.clone());
            let color = sender_pk.as_ref().map(keys::display_color).unwrap_or((140, 140, 140));
            out.push(TrackSnapshot {
                sender_hex: sender_hex.clone(),
                sender_short,
                label: self
                    .config
                    .label_for(sender_hex)
                    .unwrap_or_default()
                    .to_string(),
                name,
                color,
                group_name,
                status: t.payload.status,
                live: t.payload.status == Status::Active,
                alert: t.payload.alert.is_some(),
                lat,
                lng,
                ts,
                created_at: t.created_at,
                msg: t.payload.msg.clone().unwrap_or_default(),
                group_hex: group_hex.clone(),
            });
        }
        // Alerting senders pinned to the top, then most-recently-updated first.
        out.sort_by_key(|t| (std::cmp::Reverse(t.alert), std::cmp::Reverse(t.created_at)));
        let _ = self.ui_tx.send(UiEvent::Tracks(out));
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Compact human duration for toasts/notifications: "45 s", "20 min", "2 h".
fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{} h", secs / 3600)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// Clean a sender-declared display name for safe display: control characters
/// become spaces, runs of whitespace collapse, ends are trimmed and the result
/// is capped at [`MAX_NAME_CHARS`]. `None` when nothing usable remains (the
/// caller then falls back to a key-derived handle).
fn sanitize_name(name: Option<&str>) -> Option<String> {
    let collapsed: String = name?
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(MAX_NAME_CHARS).collect())
}

// The Publisher trait lives in relay.rs but the engine needs more pool
// operations; extend via a sub-trait so tests can mock everything at once.
pub trait EnginePool: Publisher {
    fn set_relays_list(&self, relays: &[String]);
    fn relay_status_list(&self) -> Vec<(String, bool)>;
    /// Dispatch a one-shot backfill REQ; returns the number of relays it
    /// reached (i.e. how many `FetchEose` events to expect).
    fn fetch(&self, corr: u64, filter: Filter) -> usize;
}

impl EnginePool for crate::relay::RelayPool {
    fn set_relays_list(&self, relays: &[String]) {
        self.set_relays(relays);
    }
    fn relay_status_list(&self) -> Vec<(String, bool)> {
        self.relay_status()
    }
    fn fetch(&self, corr: u64, filter: Filter) -> usize {
        // Fully-qualified so this dispatches to the inherent method, never
        // back into this trait method.
        crate::relay::RelayPool::fetch(self, corr, filter)
    }
}

/// Build a sanitized export filename: `ntrack-<label>-YYYYMMDD.gpx`.
fn export_filename(label: &str) -> String {
    format!(
        "ntrack-{}-{}.gpx",
        slugify(label),
        crate::gpx::yyyymmdd_utc(now_secs())
    )
}

/// Reduce a label to a filesystem-safe slug: ASCII alphanumerics kept, every
/// other run collapsed to a single `-`, ends trimmed. Empty → `track`.
fn slugify(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "track".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Group;
    use nostr::nips::nip44;
    use nostr::{Event, EventBuilder, Filter, Kind, Tag, Timestamp};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockPool {
        published: Mutex<Vec<Event>>,
        subscription: Mutex<Option<Filter>>,
        relays: Mutex<Vec<String>>,
        /// Recorded (corr, filter) of every `fetch`.
        fetches: Mutex<Vec<(u64, Filter)>>,
        /// Canned number of relays a fetch "reaches"; tests set this to control
        /// how many `FetchEose` they must deliver. Defaults to 0 (none).
        fetch_relays: Mutex<usize>,
    }

    impl MockPool {
        fn set_fetch_relays(&self, n: usize) {
            *self.fetch_relays.lock().unwrap() = n;
        }
        fn last_fetch_filter(&self) -> Option<Filter> {
            self.fetches.lock().unwrap().last().map(|(_, f)| f.clone())
        }
    }

    impl Publisher for MockPool {
        fn publish(&self, event: Event) {
            self.published.lock().unwrap().push(event);
        }
        fn set_subscription(&self, filter: Option<Filter>) {
            *self.subscription.lock().unwrap() = filter;
        }
    }

    impl EnginePool for MockPool {
        fn set_relays_list(&self, relays: &[String]) {
            *self.relays.lock().unwrap() = relays.to_vec();
        }
        fn relay_status_list(&self) -> Vec<(String, bool)> {
            self.relays
                .lock()
                .unwrap()
                .iter()
                .map(|u| (u.clone(), true))
                .collect()
        }
        fn fetch(&self, corr: u64, filter: Filter) -> usize {
            self.fetches.lock().unwrap().push((corr, filter));
            *self.fetch_relays.lock().unwrap()
        }
    }

    struct Fixture {
        engine: Engine<MockPool>,
        pool: Arc<MockPool>,
        ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn fixture() -> Fixture {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ntrack-engine-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::new(&dir);
        let pool = Arc::new(MockPool::default());
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let engine = Engine::new(store, pool.clone(), ui_tx);
        Fixture { engine, pool, ui_rx, dir }
    }

    use std::sync::atomic::Ordering;

    fn add_member_group(f: &mut Fixture, name: &str) -> Group {
        let g = Group::new_member(name.into()).unwrap();
        let g2 = g.clone();
        f.engine.handle(EngineCmd::Mutate(Box::new(move |c| c.groups.push(g2))));
        g
    }

    fn sample(ts_millis: u64) -> LocationSample {
        LocationSample { lat: 48.1, lng: 11.5, accuracy_m: 5.0, ts_millis }
    }

    /// Decrypt one published event back to its payload for `group`, using a
    /// fresh replay window (each test only decrypts a handful of distinct
    /// events, so dedup is irrelevant here).
    fn decrypt_for(ev: &Event, group: &Group) -> protocol::Incoming {
        let mut seen = SeenIds::new(16);
        protocol::process_incoming(ev, &[group.member_keys().unwrap()], &mut seen)
            .expect("published event must decrypt for the group")
    }

    fn drain(f: &mut Fixture) -> Vec<UiEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = f.ui_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn share_lifecycle_publishes_active_then_stop() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "Family");
        drain(&mut f);

        f.engine.handle(EngineCmd::StartShare { msg: Some("on my way".into()) });
        let evs = drain(&mut f);
        assert!(
            evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))),
            "engine must request platform location"
        );

        // First fix publishes immediately.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        let mut seen = SeenIds::new(16);
        let keys = group.member_keys().unwrap();
        let inc = protocol::process_incoming(&published[0], &[keys], &mut seen).unwrap();
        assert_eq!(inc.payload.status, Status::Active);
        assert_eq!(inc.payload.lat, Some(48.1));
        assert_eq!(inc.payload.msg.as_deref(), Some("on my way"));

        // Second fix within the interval does NOT publish.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000 + 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);

        // After the interval elapses, the tick publishes the latest fix that
        // arrived off-cycle (the second one), without re-sending the first.
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Tick);
        assert_eq!(f.pool.published.lock().unwrap().len(), 2);

        // Stop publishes a STOP payload.
        f.engine.handle(EngineCmd::StopShare);
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 3);
        let keys = group.member_keys().unwrap();
        let mut seen = SeenIds::new(16);
        let inc = protocol::process_incoming(&published[2], &[keys], &mut seen).unwrap();
        assert_eq!(inc.payload.status, Status::Stop);
        assert_eq!(inc.payload.lat, None);

        let evs = drain(&mut f);
        assert!(evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(false))));
        // All events in this session share one sender key, distinct from any group key.
        let senders: std::collections::HashSet<_> =
            published.iter().map(|e| e.pubkey).collect();
        assert_eq!(senders.len(), 1);
        assert_ne!(published[0].pubkey, group.public_key().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn tick_does_not_rebroadcast_an_already_sent_fix() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });

        // First fix publishes immediately.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);

        // GPS stalls: no new fix arrives. Ticking past several intervals must
        // NOT re-broadcast the same position — re-sending a fix we already
        // sent would just spin the relay/radio for nothing.
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Tick);
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Tick);
        assert_eq!(
            f.pool.published.lock().unwrap().len(),
            1,
            "an already-broadcast fix is never re-sent"
        );

        // A genuinely new fix resumes broadcasting.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn interval_change_while_sharing_adjusts_cadence_without_restart() {
        // The publish cadence is read from config on every fix/tick, never
        // cached in the share state, so changing "Update every" mid-share
        // governs the running session immediately — no stop/start required.
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);

        // Default 30 s interval. Start sharing; the first fix publishes at once.
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);

        // Lengthen to 5 min mid-share. A fix at +31 s — which WOULD publish
        // under the 30 s default — is now held back, proving the longer
        // interval already governs the ongoing share.
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| c.interval_secs = 300)));
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(
            f.pool.published.lock().unwrap().len(),
            1,
            "a lengthened interval immediately governs the running share"
        );

        // Shorten to 15 s. The previous publish was ~31 s ago, so the next fix
        // is due again and broadcasts — once more without a restart.
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| c.interval_secs = 15)));
        tokio::time::advance(Duration::from_secs(1)).await;
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(
            f.pool.published.lock().unwrap().len(),
            2,
            "a shortened interval takes effect on the running share"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn message_change_while_sharing_applies_on_next_broadcast() {
        // SetMessage mutates the live share's message; the next ACTIVE publish
        // reads it fresh, so an edited message rides the ongoing session
        // without stopping and re-starting the share.
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);

        // Share starts carrying an initial message.
        f.engine.handle(EngineCmd::StartShare { msg: Some("first".into()) });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let ev = f.pool.published.lock().unwrap()[0].clone();
        assert_eq!(decrypt_for(&ev, &group).payload.msg.as_deref(), Some("first"));

        // Edit the message mid-share; the next due broadcast must carry it.
        f.engine.handle(EngineCmd::SetMessage(Some("updated".into())));
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 2);
        assert_eq!(
            decrypt_for(&published[1], &group).payload.msg.as_deref(),
            Some("updated"),
            "the running share adopts the edited message on its next broadcast"
        );

        // Clearing it (blank collapses to None) omits the field next broadcast.
        f.engine.handle(EngineCmd::SetMessage(Some("   ".into())));
        tokio::time::advance(Duration::from_secs(31)).await;
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 3);
        assert_eq!(
            decrypt_for(&published[2], &group).payload.msg,
            None,
            "a blank message clears it on the running share"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn start_without_groups_is_rejected() {
        let mut f = fixture();
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        let evs = drain(&mut f);
        assert!(evs.iter().any(|e| matches!(e, UiEvent::Toast(_))));
        assert!(f.pool.published.lock().unwrap().is_empty());
        assert!(!evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))));
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_event_updates_tracks_and_stop_keeps_coords() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "Friends");
        drain(&mut f);

        let sender = keys::generate();
        let ev1 = protocol::build_event(
            &sender,
            &[group.public_key().unwrap()],
            &Payload::active(10.0, 20.0, 1000, Some("hi".into())),
            None,
        )
        .unwrap();
        f.engine.handle(EngineCmd::Pool(PoolEvent::Incoming {
            url: "wss://r".into(),
            event: Box::new(ev1.clone()),
        }));
        let tracks = last_tracks(drain(&mut f)).expect("tracks emitted");
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].live);
        assert_eq!(tracks[0].lat, 10.0);
        assert_eq!(tracks[0].group_name, "Friends");
        assert_eq!(tracks[0].msg, "hi");

        // Replay of the same event: no duplicate processing.
        f.engine.handle(EngineCmd::Pool(PoolEvent::Incoming {
            url: "wss://r2".into(),
            event: Box::new(ev1),
        }));
        assert!(last_tracks(drain(&mut f)).is_none(), "duplicate emits nothing");

        // STOP retains last coordinates but marks the track ended.
        let stop = protocol::build_event(
            &sender,
            &[group.public_key().unwrap()],
            &Payload::stop(),
            None,
        )
        .unwrap();
        f.engine.handle(EngineCmd::Pool(PoolEvent::Incoming {
            url: "wss://r".into(),
            event: Box::new(stop),
        }));
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks.len(), 1);
        assert!(!tracks[0].live);
        assert_eq!(tracks[0].status, Status::Stop);
        assert_eq!(tracks[0].lat, 10.0, "coords retained after STOP");
    }

    fn last_tracks(evs: Vec<UiEvent>) -> Option<Vec<TrackSnapshot>> {
        evs.into_iter().rev().find_map(|e| match e {
            UiEvent::Tracks(t) => Some(t),
            _ => None,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn renaming_a_sender_refreshes_tracks_immediately() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "Friends");
        drain(&mut f);

        // A sender shows up with no label yet → titled by its short npub.
        let sender = keys::generate();
        let sender_hex = sender.public_key().to_hex();
        let ev = protocol::build_event(
            &sender,
            &[group.public_key().unwrap()],
            &Payload::active(10.0, 20.0, 1000, None),
            None,
        )
        .unwrap();
        f.engine.handle(EngineCmd::Pool(PoolEvent::Incoming {
            url: "wss://r".into(),
            event: Box::new(ev),
        }));
        let tracks = last_tracks(drain(&mut f)).expect("tracks emitted");
        assert_eq!(tracks[0].label, "", "no label before renaming");

        // Renaming is a plain config mutation; it must re-emit tracks so the
        // card title updates immediately instead of waiting for the next event.
        let hex = sender_hex.clone();
        f.engine
            .handle(EngineCmd::Mutate(Box::new(move |c| c.set_label(&hex, "Anna"))));
        let tracks = last_tracks(drain(&mut f)).expect("mutate re-emits tracks");
        assert_eq!(tracks[0].sender_hex, sender_hex);
        assert_eq!(tracks[0].label, "Anna");
    }

    #[tokio::test(start_paused = true)]
    async fn outgoing_active_carries_configured_name_trimmed() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        f.engine
            .handle(EngineCmd::Mutate(Box::new(|c| c.display_name = "  Anna  ".into())));
        drain(&mut f);

        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        let mut seen = SeenIds::new(16);
        let inc =
            protocol::process_incoming(&published[0], &[group.member_keys().unwrap()], &mut seen)
                .unwrap();
        assert_eq!(inc.payload.name.as_deref(), Some("Anna"));
    }

    #[tokio::test(start_paused = true)]
    async fn outgoing_active_omits_blank_name() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        let mut seen = SeenIds::new(16);
        let inc =
            protocol::process_incoming(&published[0], &[group.member_keys().unwrap()], &mut seen)
                .unwrap();
        // No configured name → omitted, so receivers derive the same default.
        assert_eq!(inc.payload.name, None);
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_declared_name_and_color_surface_in_snapshot() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        let sender = keys::generate();
        let ev = protocol::build_event(
            &sender,
            &[group.public_key().unwrap()],
            &Payload::active(1.0, 2.0, 1000, None).with_name(Some("Bea".into())),
            None,
        )
        .unwrap();
        feed(&mut f, ev);
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks[0].name, "Bea");
        // Colour is derived from the sender key and stays visible.
        assert_eq!(tracks[0].color, keys::display_color(&sender.public_key()));
        let c = tracks[0].color;
        assert!(c.0.max(c.1).max(c.2) >= 140);
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_without_name_falls_back_to_derived_handle() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        let sender = keys::generate();
        feed(&mut f, active_event(&sender, &group, 1.0, 2.0, 1000, 1000));
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks[0].name, keys::derive_name(&sender.public_key()));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_retains_last_declared_name() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        let sender = keys::generate();
        feed(
            &mut f,
            event_with(
                &sender,
                &group,
                Payload::active(1.0, 2.0, 1000, None).with_name(Some("Cleo".into())),
                1000,
            ),
        );
        feed(&mut f, event_with(&sender, &group, Payload::stop(), 1100));
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks[0].status, Status::Stop);
        assert_eq!(tracks[0].name, "Cleo", "the name survives a STOP that carries none");
    }

    #[test]
    fn sanitize_name_cleans_and_bounds() {
        assert_eq!(sanitize_name(None), None);
        assert_eq!(sanitize_name(Some("   ")), None);
        assert_eq!(sanitize_name(Some("  Anna\nB  ")).as_deref(), Some("Anna B"));
        assert_eq!(sanitize_name(Some("a\t\tb")).as_deref(), Some("a b"));
        let long = "x".repeat(100);
        assert_eq!(sanitize_name(Some(&long)).unwrap().chars().count(), MAX_NAME_CHARS);
    }

    #[tokio::test(start_paused = true)]
    async fn subscription_follows_member_groups() {
        let mut f = fixture();
        drain(&mut f);
        assert!(f.pool.subscription.lock().unwrap().is_none());

        let g = add_member_group(&mut f, "A");
        let filter = f.pool.subscription.lock().unwrap().clone().unwrap();
        let json = serde_json::to_value(&filter).unwrap();
        assert_eq!(json["kinds"], serde_json::json!([3434]));
        assert_eq!(json["#p"][0], g.public);

        // Removing the group clears the subscription.
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| c.groups.clear())));
        assert!(f.pool.subscription.lock().unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn location_unavailable_stops_share_with_stop_event() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);

        f.engine.handle(EngineCmd::LocationUnavailable("permission denied".into()));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 2, "STOP published on location loss");
        let mut seen = SeenIds::new(16);
        let inc = protocol::process_incoming(
            &published[1],
            &[group.member_keys().unwrap()],
            &mut seen,
        )
        .unwrap();
        assert_eq!(inc.payload.status, Status::Stop);
    }

    #[tokio::test(start_paused = true)]
    async fn start_arms_resume_flag_and_persists_it() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);

        f.engine.handle(EngineCmd::StartShare { msg: Some("on my way".into()) });
        // In-memory flag is armed with the share message (stored verbatim,
        // exactly as it is attached to broadcasts).
        assert!(f.engine.config.resume_share);
        assert_eq!(f.engine.config.resume_msg.as_deref(), Some("on my way"));
        // Persisted: a fresh load sees the same, and the sentinel exists.
        assert!(f.engine.store.resume_flag_path().exists());
        let reloaded = ConfigStore::new(&f.dir).load().unwrap();
        assert!(reloaded.resume_share);
        assert_eq!(reloaded.resume_msg.as_deref(), Some("on my way"));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_stop_disarms_resume_flag() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: Some("x".into()) });
        f.engine.handle(EngineCmd::StopShare);
        assert!(!f.engine.config.resume_share);
        assert_eq!(f.engine.config.resume_msg, None);
        assert!(!f.engine.store.resume_flag_path().exists());
        let reloaded = ConfigStore::new(&f.dir).load().unwrap();
        assert!(!reloaded.resume_share);
    }

    #[tokio::test(start_paused = true)]
    async fn location_unavailable_disarms_resume_flag() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine
            .handle(EngineCmd::LocationUnavailable("permission denied".into()));
        assert!(!f.engine.config.resume_share);
        assert!(!f.engine.store.resume_flag_path().exists());
    }

    #[tokio::test(start_paused = true)]
    async fn resume_flag_survives_process_shutdown() {
        // Simulate a reboot/kill while sharing: start a share, then drop the
        // command channel so run() takes its shutdown path (best-effort STOP).
        // That STOP must NOT disarm the persisted resume flag — otherwise we
        // could never resume after a crash/reboot.
        let dir = std::env::temp_dir().join(format!(
            "ntrack-engine-shutdown-{}-{}",
            std::process::id(),
            SHUTDOWN_N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::new(&dir);
        {
            let mut cfg = store.load().unwrap();
            cfg.groups.push(Group::new_member("G".into()).unwrap());
            store.save(&cfg).unwrap();
        }
        let pool = Arc::new(MockPool::default());
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let engine = Engine::new(store, pool.clone(), ui_tx);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(engine.run(cmd_rx));
        cmd_tx
            .send(EngineCmd::StartShare { msg: Some("hi".into()) })
            .unwrap();
        // Close the channel → run() processes StartShare, then breaks and runs
        // its shutdown STOP.
        drop(cmd_tx);
        handle.await.unwrap();

        let reloaded = ConfigStore::new(&dir).load().unwrap();
        assert!(
            reloaded.resume_share,
            "shutdown STOP must keep the resume flag armed"
        );
        assert_eq!(reloaded.resume_msg.as_deref(), Some("hi"));
        assert!(ConfigStore::new(&dir).resume_flag_path().exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    static SHUTDOWN_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[tokio::test(start_paused = true)]
    async fn resume_if_armed_starts_when_armed() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        f.engine
            .handle(EngineCmd::Mutate(Box::new(|c| c.resume_share = true)));
        drain(&mut f);

        f.engine.handle(EngineCmd::ResumeShareIfArmed);
        let evs = drain(&mut f);
        assert!(
            evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))),
            "an armed resume must start sharing"
        );
        // A fix now publishes, confirming the share is genuinely active.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_if_armed_noops_when_disarmed() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);

        // resume_share defaults to false → no resume.
        f.engine.handle(EngineCmd::ResumeShareIfArmed);
        let evs = drain(&mut f);
        assert!(!evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))));
        assert!(f.pool.published.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn rotate_group_changes_subscription_and_offers_new_key() {
        let mut f = fixture();
        let g = add_member_group(&mut f, "Crew");
        drain(&mut f);

        f.engine.handle(EngineCmd::RotateGroup { group_hex: g.public.clone() });
        let evs = drain(&mut f);

        // new key offered for redistribution
        let share = evs
            .iter()
            .find_map(|e| match e {
                UiEvent::GroupShare(s) => Some(s),
                _ => None,
            })
            .expect("share dialog data emitted");
        assert_ne!(share.npub, keys::npub(&g.public_key().unwrap()));
        assert!(share.nsec.is_some());

        // config snapshot reflects the new key, same name
        let cfg = evs
            .iter()
            .find_map(|e| match e {
                UiEvent::Config(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(cfg.groups.len(), 1);
        assert_eq!(cfg.groups[0].name, "Crew");
        assert_ne!(cfg.groups[0].id, g.public);

        // subscription now points at the new key only
        let filter = f.pool.subscription.lock().unwrap().clone().unwrap();
        let json = serde_json::to_value(&filter).unwrap();
        assert_eq!(json["#p"][0], cfg.groups[0].id);

        // rotating an unknown group is a no-op
        f.engine.handle(EngineCmd::RotateGroup { group_hex: "deadbeef".into() });
        assert!(drain(&mut f).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn group_share_exposes_nsec_for_members_only() {
        let mut f = fixture();
        let g = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::RequestGroupShare { group_hex: g.public.clone() });
        let evs = drain(&mut f);
        let share = evs
            .iter()
            .find_map(|e| match e {
                UiEvent::GroupShare(s) => Some(s),
                _ => None,
            })
            .unwrap();
        assert_eq!(share.name, "G");
        assert!(share.npub.starts_with("npub1"));
        assert!(share.nsec.as_ref().unwrap().expose().starts_with("nsec1"));
    }

    #[tokio::test(start_paused = true)]
    async fn group_share_carries_oldest_relays() {
        let mut f = fixture();
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| {
            c.relays = ["a", "b", "c", "d"].iter().map(|s| format!("wss://{s}")).collect();
        })));
        let g = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::RequestGroupShare { group_hex: g.public.clone() });
        let evs = drain(&mut f);
        let share = evs
            .iter()
            .find_map(|e| match e {
                UiEvent::GroupShare(s) => Some(s),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            share.relays,
            vec!["wss://a".to_string(), "wss://b".to_string(), "wss://c".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn imported_group_relays_reach_pool_and_prune_on_remove() {
        let mut f = fixture();
        let public = "deadbeef".to_string();
        // Import a (send-only) group carrying a new relay via the same Config
        // API the controller drives through EngineCmd::Mutate.
        let p = public.clone();
        f.engine.handle(EngineCmd::Mutate(Box::new(move |c| {
            c.add_imported_group("G".into(), p, None, &["wss://new.example".to_string()]);
        })));
        drain(&mut f);
        assert!(
            f.pool.relays.lock().unwrap().iter().any(|r| r == "wss://new.example"),
            "imported relay must reach the pool"
        );
        // Removing the group prunes the auto-added relay, and the prune reaches
        // the pool on the next sync.
        f.engine.handle(EngineCmd::Mutate(Box::new(move |c| {
            c.remove_group(&public);
        })));
        drain(&mut f);
        assert!(
            !f.pool.relays.lock().unwrap().iter().any(|r| r == "wss://new.example"),
            "pruned relay must be removed from the pool"
        );
    }

    // ---- alert (duress) & check-in (dead-man's switch) -----------------

    fn last_share(evs: &[UiEvent]) -> Option<ShareSnapshot> {
        evs.iter().rev().find_map(|e| match e {
            UiEvent::Share(s) => Some(s.clone()),
            _ => None,
        })
    }

    fn notify_kinds(evs: &[UiEvent]) -> Vec<NotifyKind> {
        evs.iter()
            .filter_map(|e| match e {
                UiEvent::Notify { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    fn last_set_interval(evs: &[UiEvent]) -> Option<u64> {
        evs.iter().rev().find_map(|e| match e {
            UiEvent::SetLocationInterval(ms) => Some(*ms),
            _ => None,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn raising_alert_publishes_immediately_marked_and_boosts_cadence() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1);
        let ev0 = f.pool.published.lock().unwrap()[0].clone();
        assert_eq!(decrypt_for(&ev0, &group).payload.alert, None, "normal ACTIVE has no alert");
        drain(&mut f);

        // Raising the alert republishes at once (marked) and boosts the cadence.
        f.engine.handle(EngineCmd::SetAlert(true));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 2, "raising the alert republishes immediately");
        assert!(decrypt_for(&published[1], &group).payload.alert.is_some());
        let evs = drain(&mut f);
        assert_eq!(last_set_interval(&evs), Some(ALERT_INTERVAL_SECS * 1000));
        assert!(last_share(&evs).unwrap().alert);
        assert!(f.engine.config.alert_active, "alert persisted for resume");
    }

    #[tokio::test(start_paused = true)]
    async fn alert_heartbeat_rebroadcasts_a_stale_fix() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        f.engine.handle(EngineCmd::SetAlert(true)); // republishes (#2)
        assert_eq!(f.pool.published.lock().unwrap().len(), 2);

        // No new fix arrives. While alerting, a tick past the alert interval
        // re-sends the last-known position (unlike the normal quiet path).
        tokio::time::advance(Duration::from_secs(ALERT_INTERVAL_SECS + 1)).await;
        f.engine.handle(EngineCmd::Tick);
        assert_eq!(
            f.pool.published.lock().unwrap().len(),
            3,
            "an alert heartbeat re-broadcasts even an already-sent fix"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn clearing_alert_drops_the_marker_and_relaxes_cadence() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::StartShare { msg: None });
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        f.engine.handle(EngineCmd::SetAlert(true));
        drain(&mut f);

        f.engine.handle(EngineCmd::SetAlert(false));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(
            decrypt_for(published.last().unwrap(), &group).payload.alert,
            None,
            "clearing republishes without the marker"
        );
        let evs = drain(&mut f);
        assert_eq!(last_set_interval(&evs), Some(30 * 1000), "cadence back to the default");
        assert!(!last_share(&evs).unwrap().alert);
        assert!(!f.engine.config.alert_active);
    }

    #[tokio::test(start_paused = true)]
    async fn panic_force_starts_share_and_alerts() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        // Not sharing yet — panic starts a share already alerting.
        f.engine.handle(EngineCmd::Panic);
        let evs = drain(&mut f);
        assert!(
            evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))),
            "panic force-starts a share"
        );
        assert!(last_share(&evs).map(|s| s.sharing && s.alert).unwrap_or(false));

        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert!(decrypt_for(&published[0], &group).payload.alert.is_some());
        assert!(f.engine.config.resume_share && f.engine.config.alert_active);
    }

    #[tokio::test(start_paused = true)]
    async fn panic_without_selection_shares_to_all_groups() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let id = group.public.clone();
        f.engine.handle(EngineCmd::Mutate(Box::new(move |c| {
            for g in &mut c.groups {
                if g.public == id {
                    g.selected = false;
                }
            }
        })));
        drain(&mut f);

        f.engine.handle(EngineCmd::Panic);
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1, "panic shares even with nothing selected");
        assert!(decrypt_for(&published[0], &group).payload.alert.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn panic_without_any_group_is_rejected() {
        let mut f = fixture();
        drain(&mut f);
        f.engine.handle(EngineCmd::Panic);
        let evs = drain(&mut f);
        assert!(evs.iter().any(|e| matches!(e, UiEvent::Toast(_))));
        assert!(f.pool.published.lock().unwrap().is_empty());
        assert!(!evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))));
    }

    #[tokio::test(start_paused = true)]
    async fn alert_resumes_with_the_share() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::Panic);
        assert!(f.engine.config.alert_active);
        // Simulate a process restart: drop the in-memory share, keep the
        // persisted flags.
        f.engine.share = None;
        drain(&mut f);

        f.engine.handle(EngineCmd::ResumeShareIfArmed);
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert!(
            decrypt_for(&published[0], &group).payload.alert.is_some(),
            "a resumed emergency share is still alerting"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_alert_notifies_once_and_pins_the_track() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        let sender = keys::generate();

        // A normal ACTIVE: no alert, no notification.
        feed(&mut f, event_with(&sender, &group, Payload::active(1.0, 2.0, 1000, None), 1000));
        let evs = drain(&mut f);
        assert!(notify_kinds(&evs).is_empty());
        assert!(!last_tracks(evs).unwrap()[0].alert);

        // It transitions to an alert: exactly one PeerAlert; track marked.
        feed(
            &mut f,
            event_with(
                &sender,
                &group,
                Payload::active(1.0, 2.0, 1100, None).with_alert(Some(1100)),
                1100,
            ),
        );
        let evs = drain(&mut f);
        assert_eq!(notify_kinds(&evs), vec![NotifyKind::PeerAlert]);
        assert!(last_tracks(evs).unwrap()[0].alert);

        // A sticky-alert heartbeat does NOT re-notify (edge-triggered).
        feed(
            &mut f,
            event_with(
                &sender,
                &group,
                Payload::active(1.0, 2.0, 1200, None).with_alert(Some(1100)),
                1200,
            ),
        );
        assert!(notify_kinds(&drain(&mut f)).is_empty(), "no re-alert on a heartbeat");
    }

    #[tokio::test(start_paused = true)]
    async fn alerting_track_pins_above_a_newer_normal_track() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        let alerter = keys::generate();
        let normal = keys::generate();
        // Alerter is older; the normal sender is newer.
        feed(
            &mut f,
            event_with(
                &alerter,
                &group,
                Payload::active(1.0, 2.0, 1000, None).with_alert(Some(1000)),
                1000,
            ),
        );
        feed(&mut f, event_with(&normal, &group, Payload::active(3.0, 4.0, 2000, None), 2000));
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].alert, "the alerting sender is pinned to the top despite being older");
    }

    #[tokio::test(start_paused = true)]
    async fn arm_checkin_persists_and_sets_sentinel() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::ArmCheckin { secs: 600 });
        assert_eq!(f.engine.config.checkin_period_secs, Some(600));
        assert!(f.engine.config.checkin_deadline.is_some());
        assert!(f.engine.store.checkin_flag_path().exists());
        let snap = last_share(&drain(&mut f)).unwrap();
        assert!(snap.checkin_deadline.is_some() && !snap.checkin_grace);
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_safe_rearms_for_another_period() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::ArmCheckin { secs: 600 });
        // Wind the deadline almost down so the re-arm's reset is visible.
        if let Some(CheckinState::Armed { deadline, .. }) = &mut f.engine.checkin {
            *deadline = now_secs() + 5;
        }
        f.engine.handle(EngineCmd::Checkin);
        // Confirming safety re-arms rather than disarming: still armed, the
        // deadline pushed back out to a fresh full period, sentinel intact.
        match f.engine.checkin {
            Some(CheckinState::Armed { deadline, period_secs, .. }) => {
                assert_eq!(period_secs, 600);
                assert!(deadline >= now_secs() + 590, "deadline reset to a fresh period");
            }
            _ => panic!("check-in should re-arm after confirming safe"),
        }
        assert_eq!(f.engine.config.checkin_period_secs, Some(600));
        assert!(f.engine.store.checkin_flag_path().exists());
    }

    #[tokio::test(start_paused = true)]
    async fn disarm_checkin_clears_state_and_sentinel() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::ArmCheckin { secs: 600 });
        f.engine.handle(EngineCmd::DisarmCheckin);
        assert!(f.engine.checkin.is_none());
        assert_eq!(f.engine.config.checkin_deadline, None);
        assert_eq!(f.engine.config.checkin_period_secs, None);
        assert!(!f.engine.store.checkin_flag_path().exists());
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_escalates_to_panic_on_deadline() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.handle(EngineCmd::ArmCheckin { secs: 600 });
        // Rewind the (wall-clock) deadline into the past; tokio's mock clock
        // wouldn't move `now_secs`.
        if let Some(CheckinState::Armed { deadline, .. }) = &mut f.engine.checkin {
            *deadline = now_secs() - 1;
        }
        drain(&mut f);

        f.engine.handle(EngineCmd::Tick);
        let evs = drain(&mut f);
        assert!(
            evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))),
            "escalation starts a share"
        );
        assert!(notify_kinds(&evs).contains(&NotifyKind::CheckinEscalated));
        assert!(f.engine.config.alert_active);
        assert!(f.engine.checkin.is_none(), "the check-in is cleared once it fires");
        assert!(!f.engine.store.checkin_flag_path().exists());

        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert!(decrypt_for(&f.pool.published.lock().unwrap()[0], &group).payload.alert.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_reminder_fires_once_before_the_deadline() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        f.engine.checkin =
            Some(CheckinState::Armed { deadline: now_secs() + 10, period_secs: 600, reminded: false });
        f.engine.handle(EngineCmd::Tick);
        assert_eq!(notify_kinds(&drain(&mut f)), vec![NotifyKind::CheckinReminder]);
        // No second reminder, and not yet escalated.
        f.engine.handle(EngineCmd::Tick);
        assert!(notify_kinds(&drain(&mut f)).is_empty());
        assert!(f.pool.published.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_reminder_lead_scales_with_the_period() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        // A long period reminds at 10% remaining, not a fixed lead: with
        // period 1000 the lead is 100 s, so 200 s out (20% left) is too early.
        f.engine.checkin =
            Some(CheckinState::Armed { deadline: now_secs() + 200, period_secs: 1000, reminded: false });
        f.engine.handle(EngineCmd::Tick);
        assert!(
            notify_kinds(&drain(&mut f)).is_empty(),
            "no reminder while more than 10% of the period remains"
        );
        // Inside the 10% window (90 s out) it fires.
        if let Some(CheckinState::Armed { deadline, .. }) = &mut f.engine.checkin {
            *deadline = now_secs() + 90;
        }
        f.engine.handle(EngineCmd::Tick);
        assert_eq!(notify_kinds(&drain(&mut f)), vec![NotifyKind::CheckinReminder]);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_checkin_on_start_enters_grace_without_escalating() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| {
            c.checkin_deadline = Some(now_secs() - 1);
            c.checkin_period_secs = Some(600);
        })));
        drain(&mut f);

        f.engine.handle(EngineCmd::EvaluateCheckinOnStart);
        let evs = drain(&mut f);
        // Grace, not immediate escalation: nothing published, location not started.
        assert!(f.pool.published.lock().unwrap().is_empty());
        assert!(!evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))));
        assert!(notify_kinds(&evs).contains(&NotifyKind::CheckinGrace));
        let snap = last_share(&evs).unwrap();
        assert!(snap.checkin_grace && snap.checkin_deadline.is_some());
        assert!(matches!(f.engine.checkin, Some(CheckinState::StartupGrace { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn unexpired_checkin_on_start_resumes_countdown() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| {
            c.checkin_deadline = Some(now_secs() + 600);
            c.checkin_period_secs = Some(600);
        })));
        drain(&mut f);

        f.engine.handle(EngineCmd::EvaluateCheckinOnStart);
        let evs = drain(&mut f);
        assert!(notify_kinds(&evs).is_empty(), "a future deadline resumes silently");
        assert!(matches!(f.engine.checkin, Some(CheckinState::Armed { .. })));
        assert!(!last_share(&evs).unwrap().checkin_grace);
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_grace_escalates_after_its_window() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);
        // Enter grace with the window already elapsed.
        f.engine.checkin = Some(CheckinState::StartupGrace { until: now_secs() });
        f.engine.handle(EngineCmd::Tick);
        let evs = drain(&mut f);
        assert!(
            evs.iter().any(|e| matches!(e, UiEvent::NeedLocation(true))),
            "an unconfirmed grace window escalates to a share"
        );
        assert!(f.engine.config.alert_active);
    }

    #[tokio::test(start_paused = true)]
    async fn checkin_safe_during_grace_rearms_and_never_escalates() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        // Grace re-arms from the persisted period, so seed it.
        f.engine.handle(EngineCmd::Mutate(Box::new(|c| {
            c.checkin_deadline = Some(now_secs() - 1);
            c.checkin_period_secs = Some(600);
        })));
        drain(&mut f);
        f.engine.checkin = Some(CheckinState::StartupGrace { until: now_secs() + 60 });
        f.engine.handle(EngineCmd::Checkin);
        // Confirming safe re-arms into a fresh countdown (not disarmed)…
        assert!(matches!(
            f.engine.checkin,
            Some(CheckinState::Armed { period_secs: 600, .. })
        ));
        f.engine.handle(EngineCmd::Tick);
        // …and no alert fired.
        assert!(
            f.pool.published.lock().unwrap().is_empty(),
            "confirming safe within the grace window cancels the alarm"
        );
    }

    // ---- track history & GPX export ------------------------------------

    /// Build a signed kind:3434 event from `sender` to `group` with explicit
    /// `created_at` (event/publish time); `payload` carries the capture `ts`.
    fn event_with(sender: &Keys, group: &Group, payload: Payload, created_at: u64) -> Event {
        let plaintext = serde_json::to_string(&payload).unwrap();
        let gpk = group.public_key().unwrap();
        let content =
            nip44::encrypt(sender.secret_key(), &gpk, &plaintext, nip44::Version::V2).unwrap();
        EventBuilder::new(Kind::Custom(protocol::EVENT_KIND), content)
            .tags([Tag::public_key(gpk)])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(sender)
            .unwrap()
    }

    fn active_event(
        sender: &Keys,
        group: &Group,
        lat: f64,
        lng: f64,
        ts: u64,
        created_at: u64,
    ) -> Event {
        event_with(sender, group, Payload::active(lat, lng, ts, None), created_at)
    }

    fn feed(f: &mut Fixture, ev: Event) {
        f.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming { url: "wss://r".into(), event: Box::new(ev) }));
    }

    fn feed_fetch(f: &mut Fixture, corr: u64, ev: Event) {
        f.engine.handle(EngineCmd::Pool(PoolEvent::FetchEvent {
            corr,
            url: "wss://r".into(),
            event: Box::new(ev),
        }));
    }

    fn export(f: &mut Fixture, sender: &Keys, group: &Group) {
        f.engine.handle(EngineCmd::ExportTrack {
            sender_hex: sender.public_key().to_hex(),
            group_hex: group.public.clone(),
        });
    }

    /// The GPX of the last `TrackExport`, if any.
    fn last_export_gpx(evs: Vec<UiEvent>) -> Option<String> {
        evs.into_iter().rev().find_map(|e| match e {
            UiEvent::TrackExport { gpx_xml, .. } => Some(gpx_xml),
            _ => None,
        })
    }

    fn trkpt_count(gpx: &str) -> usize {
        gpx.matches("<trkpt ").count()
    }

    #[tokio::test(start_paused = true)]
    async fn history_accumulates_active_points_while_display_shows_newest() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let key = (sender.public_key().to_hex(), group.public.clone());

        for (i, ts) in [1000u64, 1060, 1120].iter().enumerate() {
            feed(&mut f, active_event(&sender, &group, 48.0 + i as f64, 11.0, *ts, 5000 + *ts));
        }
        // All three points retained, ascending by ts.
        let pts: Vec<u64> = f.engine.history[&key].points.iter().map(|p| p.ts).collect();
        assert_eq!(pts, vec![1000, 1060, 1120]);
        // The latest-point display shows the newest fix only.
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].lat, 50.0, "display tracks the newest point");
    }

    #[tokio::test(start_paused = true)]
    async fn history_dedups_by_ts_keeping_larger_created_at() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let key = (sender.public_key().to_hex(), group.public.clone());

        // Same capture ts, two different publishes; the later (larger
        // created_at) must win.
        feed(&mut f, active_event(&sender, &group, 1.0, 1.0, 1000, 100));
        feed(&mut f, active_event(&sender, &group, 2.0, 2.0, 1000, 200));
        let pts = &f.engine.history[&key].points;
        assert_eq!(pts.len(), 1, "same ts collapses to one point");
        assert_eq!(pts[0].lat, 2.0, "the later publish wins the tie");
        assert_eq!(pts[0].created_at, 200);
    }

    #[tokio::test(start_paused = true)]
    async fn out_of_order_active_lands_in_history_but_not_display() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let key = (sender.public_key().to_hex(), group.public.clone());

        feed(&mut f, active_event(&sender, &group, 1.0, 1.0, 100, 100));
        feed(&mut f, active_event(&sender, &group, 2.0, 2.0, 200, 200));
        // A late-delivered older fix: must enter history but NOT move display.
        feed(&mut f, active_event(&sender, &group, 9.0, 9.0, 150, 150));

        let pts: Vec<u64> = f.engine.history[&key].points.iter().map(|p| p.ts).collect();
        assert_eq!(pts, vec![100, 150, 200], "out-of-order point still retained");
        let tracks = last_tracks(drain(&mut f)).unwrap();
        assert_eq!(tracks[0].lat, 2.0, "display stays on the newest (created_at) point");
    }

    #[tokio::test(start_paused = true)]
    async fn active_recorded_stop_not() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let key = (sender.public_key().to_hex(), group.public.clone());

        feed(&mut f, event_with(&sender, &group, Payload::active(1.0, 2.0, 500, None), 500));
        feed(&mut f, event_with(&sender, &group, Payload::stop(), 600));
        let pts = &f.engine.history[&key].points;
        assert_eq!(pts.len(), 1, "ACTIVE recorded, STOP is a boundary not a point");
        assert_eq!(pts[0].ts, 500);
    }

    #[tokio::test(start_paused = true)]
    async fn global_session_cap_evicts_least_recent() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        drain(&mut f);

        // Fill one past the cap, each a distinct sender (= distinct session),
        // with strictly increasing recency.
        let mut senders = Vec::new();
        for i in 0..=MAX_HISTORY_SESSIONS as u64 {
            let s = keys::generate();
            feed(&mut f, active_event(&s, &group, 1.0, 1.0, 1000 + i, 1000 + i));
            senders.push(s);
        }
        assert_eq!(f.engine.history.len(), MAX_HISTORY_SESSIONS, "capped");
        // The first (least-recently-updated) session was evicted; the newest
        // survives.
        let oldest = (senders[0].public_key().to_hex(), group.public.clone());
        let newest = (senders.last().unwrap().public_key().to_hex(), group.public.clone());
        assert!(!f.engine.history.contains_key(&oldest), "oldest session evicted");
        assert!(f.engine.history.contains_key(&newest), "newest session kept");
    }

    #[tokio::test(start_paused = true)]
    async fn export_with_no_points_emits_toast() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        drain(&mut f);

        // No history, no connected relays (fetch_relays defaults to 0).
        export(&mut f, &sender, &group);
        let evs = drain(&mut f);
        assert!(last_export_gpx(evs.clone()).is_none(), "no GPX emitted");
        assert!(evs
            .iter()
            .any(|e| matches!(e, UiEvent::Toast(t) if t.contains("No track points"))));
    }

    #[tokio::test(start_paused = true)]
    async fn export_zero_relays_emits_live_buffer_immediately() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        feed(&mut f, active_event(&sender, &group, 1.0, 2.0, 1000, 1000));
        drain(&mut f);

        // fetch_relays == 0 → no backfill; ship the live buffer at once.
        export(&mut f, &sender, &group);
        let gpx = last_export_gpx(drain(&mut f)).expect("export emitted immediately");
        assert_eq!(trkpt_count(&gpx), 1);
        assert!(f.engine.pending_exports.is_empty(), "nothing left pending");
    }

    #[tokio::test(start_paused = true)]
    async fn export_merges_live_and_backfill_deduped_and_sorted() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        // Live points at ts 100 and 200.
        feed(&mut f, active_event(&sender, &group, 1.0, 1.0, 100, 100));
        feed(&mut f, active_event(&sender, &group, 2.0, 2.0, 200, 200));
        drain(&mut f);

        f.pool.set_fetch_relays(1);
        export(&mut f, &sender, &group);
        assert_eq!(f.engine.pending_exports.len(), 1, "awaiting backfill");

        // Backfill delivers an in-between point (150), a duplicate ts (200) and
        // an out-of-order earlier point (50).
        feed_fetch(&mut f, 0, active_event(&sender, &group, 5.0, 5.0, 150, 150));
        feed_fetch(&mut f, 0, active_event(&sender, &group, 9.0, 9.0, 200, 999));
        feed_fetch(&mut f, 0, active_event(&sender, &group, 7.0, 7.0, 50, 50));
        f.engine.handle(EngineCmd::Pool(PoolEvent::FetchEose { corr: 0, url: "wss://r".into() }));

        let gpx = last_export_gpx(drain(&mut f)).expect("export emitted on EOSE");
        // 50, 100, 150, 200 — duplicate ts=200 collapsed.
        assert_eq!(trkpt_count(&gpx), 4);
        // Times appear in ascending order.
        let p50 = gpx.find("1970-01-01T00:00:50Z").unwrap();
        let p100 = gpx.find("1970-01-01T00:01:40Z").unwrap();
        let p200 = gpx.find("1970-01-01T00:03:20Z").unwrap();
        assert!(p50 < p100 && p100 < p200, "trkpts sorted ascending by time");
    }

    #[tokio::test(start_paused = true)]
    async fn refetched_already_seen_event_is_recovered_by_export() {
        // The load-bearing bypass: after a restart the ephemeral history is
        // empty but `seen` still holds the id (it is persisted). Backfill must
        // recover the point — process_incoming would drop it as Duplicate.
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let ev = active_event(&sender, &group, 3.0, 4.0, 1000, 1000);
        feed(&mut f, ev.clone()); // records into history AND seen
        // Simulate the restart: drop the in-memory history, keep `seen`.
        f.engine.history.clear();
        assert!(f.engine.seen.contains(&ev.id), "id still in the replay window");
        drain(&mut f);

        f.pool.set_fetch_relays(1);
        export(&mut f, &sender, &group);
        feed_fetch(&mut f, 0, ev); // already-seen event, re-fetched
        f.engine.handle(EngineCmd::Pool(PoolEvent::FetchEose { corr: 0, url: "wss://r".into() }));

        let gpx = last_export_gpx(drain(&mut f)).expect("export emitted");
        assert_eq!(trkpt_count(&gpx), 1, "seen-but-refetched point recovered, once");
    }

    #[tokio::test(start_paused = true)]
    async fn export_times_out_to_live_only() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        feed(&mut f, active_event(&sender, &group, 1.0, 2.0, 1000, 1000));
        drain(&mut f);

        f.pool.set_fetch_relays(1);
        export(&mut f, &sender, &group);
        // No FetchEose arrives (unreachable relay). The tick timeout completes
        // the export with the live buffer only.
        assert!(last_export_gpx(drain(&mut f)).is_none(), "still waiting before timeout");
        tokio::time::advance(FETCH_TIMEOUT + Duration::from_secs(1)).await;
        f.engine.handle(EngineCmd::Tick);
        let gpx = last_export_gpx(drain(&mut f)).expect("timed-out export ships live buffer");
        assert_eq!(trkpt_count(&gpx), 1);
        assert!(f.engine.pending_exports.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn export_does_not_block_other_commands() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        f.engine.handle(EngineCmd::StartShare { msg: None });
        drain(&mut f);

        f.pool.set_fetch_relays(1);
        export(&mut f, &sender, &group); // pending; no EOSE yet
        assert_eq!(f.engine.pending_exports.len(), 1);

        // Sharing keeps working while the backfill is in flight.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        assert_eq!(
            f.pool.published.lock().unwrap().len(),
            1,
            "a location fix still publishes while an export is pending"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn export_backfill_filter_targets_sender_and_group() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        drain(&mut f);

        f.pool.set_fetch_relays(1);
        export(&mut f, &sender, &group);
        let filter = f.pool.last_fetch_filter().expect("fetch dispatched");
        let json = serde_json::to_value(&filter).unwrap();
        assert_eq!(json["kinds"], serde_json::json!([3434]));
        assert_eq!(json["authors"], serde_json::json!([sender.public_key().to_hex()]));
        assert_eq!(json["#p"], serde_json::json!([group.public]));
        assert!(json["since"].as_u64().unwrap() > 0);
        assert_eq!(json["limit"].as_u64().unwrap(), BACKFILL_LIMIT as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn export_filename_uses_label_when_present() {
        let mut f = fixture();
        let group = add_member_group(&mut f, "G");
        let sender = keys::generate();
        let hex = sender.public_key().to_hex();
        feed(&mut f, active_event(&sender, &group, 1.0, 2.0, 1000, 1000));
        let h = hex.clone();
        f.engine
            .handle(EngineCmd::Mutate(Box::new(move |c| c.set_label(&h, "Anna's phone"))));
        drain(&mut f);

        export(&mut f, &sender, &group);
        let evs = drain(&mut f);
        let name = evs.iter().rev().find_map(|e| match e {
            UiEvent::TrackExport { suggested_filename, .. } => Some(suggested_filename.clone()),
            _ => None,
        });
        let name = name.expect("export emitted");
        assert!(name.starts_with("ntrack-Anna-s-phone-"), "got {name}");
        assert!(name.ends_with(".gpx"));
    }

    #[test]
    fn slugify_sanitizes_labels() {
        assert_eq!(slugify("Anna's phone"), "Anna-s-phone");
        assert_eq!(slugify("  weird // name **"), "weird-name");
        assert_eq!(slugify("✓✓✓"), "track");
        assert_eq!(slugify(""), "track");
        assert_eq!(slugify("npub1abc"), "npub1abc");
    }
}
