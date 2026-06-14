//! The app engine: a single task that owns the configuration, the share
//! state machine and the tracking state, decoupled from any UI.
//!
//! The UI layer sends [`EngineCmd`]s and renders [`UiEvent`] snapshots; the
//! platform layer feeds [`LocationSample`]s and reacts to
//! [`UiEvent::NeedLocation`] by starting/stopping platform location updates.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr::{EventId, Keys, PublicKey};
use tokio::sync::mpsc;

use crate::config::{Config, ConfigStore};
use crate::dedup::SeenIds;
use crate::keys;
use crate::protocol::{self, GartPayload, Status};
use crate::relay::{PoolEvent, Publisher};

/// How far back the tracking subscription looks on (re)start. Replay
/// protection makes overlap harmless.
pub const SINCE_LOOKBACK_SECS: u64 = 6 * 3600;
/// NIP-40 expiration attached to outgoing events when enabled.
pub const EXPIRATION_SECS: u64 = 24 * 3600;
/// Capacity of the processed-event-id replay window.
pub const SEEN_CAPACITY: usize = 4096;
/// A location sample older than this is not good enough for a TEST.
const FRESH_SAMPLE_SECS: u64 = 60;
/// Give up on a pending TEST if no fix arrives in this time.
const TEST_FIX_TIMEOUT: Duration = Duration::from_secs(45);

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
    /// Update the message attached to subsequent location publishes.
    SetMessage(Option<String>),
    StopShare,
    SendTest,
    Location(LocationSample),
    /// Permission was denied or location turned off by the platform.
    LocationUnavailable(String),
    /// Ask the engine to emit the share dialog data for a group.
    RequestGroupShare { group_hex: String },
    /// Rotate a group's recipient pseudonym key (NIP-GART MUST-provide).
    /// Emits the refreshed config plus a [`UiEvent::GroupShare`] carrying the
    /// new secret for redistribution to members.
    RotateGroup { group_hex: String },
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShareSnapshot {
    pub sharing: bool,
    pub test_pending: bool,
    /// Unix seconds of the last successful publish hand-off.
    pub last_publish: Option<u64>,
    pub publish_count: u64,
    /// At least one relay acknowledged the latest event.
    pub last_acked: bool,
    pub waiting_for_fix: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackSnapshot {
    pub sender_hex: String,
    pub sender_short: String,
    pub label: String,
    pub group_name: String,
    pub status: Status,
    pub live: bool,
    pub is_test: bool,
    pub lat: f64,
    pub lng: f64,
    /// Location capture time (unix seconds); 0 when unknown (bare STOP).
    pub ts: u64,
    pub created_at: u64,
    pub msg: String,
}

/// Data for the "share group key" dialog.
#[derive(Debug, Clone)]
pub struct GroupShare {
    pub name: String,
    pub npub: String,
    /// nsec to hand to new members; `None` for send-only groups.
    pub nsec: Option<keys::SecretString>,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Config(ConfigSnapshot),
    Share(ShareSnapshot),
    Tracks(Vec<TrackSnapshot>),
    Relays(Vec<(String, bool)>),
    GroupShare(GroupShare),
    /// Platform layer should start (true) / stop (false) location updates.
    NeedLocation(bool),
    Toast(String),
}

struct ShareState {
    sender: Keys,
    recipients: Vec<PublicKey>,
    msg: Option<String>,
    last_publish_at: Option<tokio::time::Instant>,
    last_sample: Option<LocationSample>,
    last_event_id: Option<EventId>,
    last_publish_ts: Option<u64>,
    publish_count: u64,
    last_acked: bool,
}

#[derive(Clone)]
struct TrackState {
    group: PublicKey,
    payload: GartPayload,
    created_at: u64,
    /// Coordinates retained from the last ACTIVE/TEST when a STOP arrives.
    last_coords: Option<(f64, f64, u64)>,
}

pub struct Engine<P: EnginePool> {
    store: ConfigStore,
    config: Config,
    pool: Arc<P>,
    seen: SeenIds,
    seen_dirty: bool,
    share: Option<ShareState>,
    test_pending: Option<(tokio::time::Instant, Option<String>)>,
    /// keyed by (sender hex, group hex)
    tracks: BTreeMap<(String, String), TrackState>,
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
            test_pending: None,
            tracks: BTreeMap::new(),
            ui_tx,
        }
    }

    /// Run the engine until [`EngineCmd::Shutdown`] (or channel close).
    pub async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<EngineCmd>) {
        self.sync_pool();
        self.emit_config();
        self.emit_share();
        self.emit_tracks();

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
            }
            EngineCmd::StartShare { msg } => self.start_share(msg),
            EngineCmd::SetMessage(msg) => {
                if let Some(s) = &mut self.share {
                    s.msg = msg.filter(|m| !m.trim().is_empty());
                }
            }
            EngineCmd::StopShare => self.stop_share(),
            EngineCmd::SendTest => self.send_test(),
            EngineCmd::Location(sample) => self.on_location(sample),
            EngineCmd::LocationUnavailable(reason) => {
                if self.share.is_some() || self.test_pending.is_some() {
                    self.toast(format!("Location unavailable: {reason}"));
                }
                self.test_pending = None;
                if self.share.is_some() {
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
        if self.share.is_some() {
            return;
        }
        let recipients = self.selected_recipients();
        if recipients.is_empty() {
            self.toast("Select at least one group to share with".into());
            self.emit_share();
            return;
        }
        let sender = match self.config.sender_keys() {
            Ok(k) => k,
            Err(e) => {
                self.toast(format!("Sender key error: {e}"));
                return;
            }
        };
        self.persist();
        self.share = Some(ShareState {
            sender,
            recipients,
            msg: msg.filter(|m| !m.trim().is_empty()),
            last_publish_at: None,
            last_sample: None,
            last_event_id: None,
            last_publish_ts: None,
            publish_count: 0,
            last_acked: false,
        });
        let _ = self.ui_tx.send(UiEvent::NeedLocation(true));
        self.emit_share();
    }

    fn stop_share(&mut self) {
        if let Some(state) = self.share.take() {
            match protocol::build_event(
                &state.sender,
                &state.recipients,
                &GartPayload::stop(),
                self.expiration(),
            ) {
                Ok(event) => self.pool.publish(event),
                Err(e) => log::error!("failed to build STOP event: {e}"),
            }
        }
        let _ = self
            .ui_tx
            .send(UiEvent::NeedLocation(self.test_pending.is_some()));
        self.emit_share();
    }

    fn send_test(&mut self) {
        let recipients = self.selected_recipients();
        if recipients.is_empty() {
            self.toast("Select at least one group first".into());
            return;
        }
        // Fresh fix available (e.g. while sharing): send immediately.
        let fresh = self.share.as_ref().and_then(|s| s.last_sample).filter(|s| {
            now_secs().saturating_sub(s.ts_secs()) < FRESH_SAMPLE_SECS
        });
        if let Some(sample) = fresh {
            self.publish_payload(GartPayload::test(
                sample.lat,
                sample.lng,
                sample.ts_secs(),
                None,
                None,
            ));
            self.toast("Test broadcast sent".into());
            return;
        }
        self.test_pending = Some((tokio::time::Instant::now() + TEST_FIX_TIMEOUT, None));
        let _ = self.ui_tx.send(UiEvent::NeedLocation(true));
        self.toast("Waiting for a location fix…".into());
        self.emit_share();
    }

    fn on_location(&mut self, sample: LocationSample) {
        if let Some((_, _msg)) = self.test_pending.take() {
            self.publish_payload(GartPayload::test(
                sample.lat,
                sample.lng,
                sample.ts_secs(),
                None,
                None,
            ));
            self.toast("Test broadcast sent".into());
            let _ = self
                .ui_tx
                .send(UiEvent::NeedLocation(self.share.is_some()));
        }
        let interval = Duration::from_secs(self.config.interval_secs.max(5));
        let due = match &self.share {
            Some(s) => match s.last_publish_at {
                None => true,
                Some(at) => at.elapsed() >= interval,
            },
            None => false,
        };
        if let Some(s) = &mut self.share {
            s.last_sample = Some(sample);
        }
        if due {
            self.publish_active(sample);
        } else {
            self.emit_share();
        }
    }

    fn on_tick(&mut self) {
        if let Some((deadline, _)) = &self.test_pending {
            if tokio::time::Instant::now() >= *deadline {
                self.test_pending = None;
                self.toast("Test failed: no location fix".into());
                let _ = self
                    .ui_tx
                    .send(UiEvent::NeedLocation(self.share.is_some()));
                self.emit_share();
            }
        }
        let interval = Duration::from_secs(self.config.interval_secs.max(5));
        let due_sample = self.share.as_ref().and_then(|s| {
            let due = match s.last_publish_at {
                None => true,
                Some(at) => at.elapsed() >= interval,
            };
            if due { s.last_sample } else { None }
        });
        if let Some(sample) = due_sample {
            self.publish_active(sample);
        }
    }

    fn publish_active(&mut self, sample: LocationSample) {
        let msg = self.share.as_ref().and_then(|s| s.msg.clone());
        self.publish_payload(GartPayload::active(
            sample.lat,
            sample.lng,
            sample.ts_secs(),
            msg,
        ));
    }

    /// Build, sign and hand a payload to the relay pool, updating share
    /// statistics. Uses the share sender key, or the configured sender key
    /// for one-off TESTs outside a share session.
    fn publish_payload(&mut self, payload: GartPayload) {
        let (sender, recipients) = match &self.share {
            Some(s) => (s.sender.clone(), s.recipients.clone()),
            None => {
                let sender = match self.config.sender_keys() {
                    Ok(k) => k,
                    Err(e) => {
                        self.toast(format!("Sender key error: {e}"));
                        return;
                    }
                };
                self.persist();
                (sender, self.selected_recipients())
            }
        };
        if recipients.is_empty() {
            return;
        }
        let is_active = payload.status == Status::Active;
        match protocol::build_event(&sender, &recipients, &payload, self.expiration()) {
            Ok(event) => {
                let id = event.id;
                self.pool.publish(event);
                if let Some(s) = &mut self.share {
                    if is_active {
                        s.last_publish_at = Some(tokio::time::Instant::now());
                        s.last_publish_ts = Some(now_secs());
                        s.publish_count += 1;
                        s.last_event_id = Some(id);
                        s.last_acked = false;
                    }
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
                        if drop == protocol::DropReason::TestNotForUs {
                            self.seen_dirty = true;
                        }
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
        }
    }

    fn apply_incoming(&mut self, inc: protocol::Incoming) {
        let key = (inc.sender.to_hex(), inc.group.to_hex());
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
        self.tracks.insert(
            key,
            TrackState {
                group: inc.group,
                payload: inc.payload,
                created_at: inc.created_at,
                last_coords,
            },
        );
        self.emit_tracks();
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
        let sender_npub = self
            .config
            .sender_secret
            .as_ref()
            .and_then(|s| keys::parse_secret(s.expose()).ok())
            .map(|sk| keys::npub(&Keys::new(sk).public_key()))
            .unwrap_or_default();
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
        }));
    }

    fn emit_share(&self) {
        let snap = match &self.share {
            Some(s) => ShareSnapshot {
                sharing: true,
                test_pending: self.test_pending.is_some(),
                last_publish: s.last_publish_ts,
                publish_count: s.publish_count,
                last_acked: s.last_acked,
                waiting_for_fix: s.last_sample.is_none(),
            },
            None => ShareSnapshot {
                sharing: false,
                test_pending: self.test_pending.is_some(),
                last_publish: None,
                publish_count: 0,
                last_acked: false,
                waiting_for_fix: false,
            },
        };
        let _ = self.ui_tx.send(UiEvent::Share(snap));
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
            let sender_short = keys::parse_public(sender_hex)
                .map(|pk| keys::short_npub(&pk))
                .unwrap_or_else(|_| sender_hex.clone());
            out.push(TrackSnapshot {
                sender_hex: sender_hex.clone(),
                sender_short,
                label: self
                    .config
                    .label_for(sender_hex)
                    .unwrap_or_default()
                    .to_string(),
                group_name,
                status: t.payload.status,
                live: t.payload.status == Status::Active,
                is_test: t.payload.status == Status::Test,
                lat,
                lng,
                ts,
                created_at: t.created_at,
                msg: t.payload.msg.clone().unwrap_or_default(),
            });
        }
        // Most recently updated first.
        out.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        let _ = self.ui_tx.send(UiEvent::Tracks(out));
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// The Publisher trait lives in relay.rs but the engine needs two more pool
// operations; extend via a sub-trait so tests can mock everything at once.
pub trait EnginePool: Publisher {
    fn set_relays_list(&self, relays: &[String]);
    fn relay_status_list(&self) -> Vec<(String, bool)>;
}

impl EnginePool for crate::relay::RelayPool {
    fn set_relays_list(&self, relays: &[String]) {
        self.set_relays(relays);
    }
    fn relay_status_list(&self) -> Vec<(String, bool)> {
        self.relay_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Group;
    use nostr::{Event, Filter};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockPool {
        published: Mutex<Vec<Event>>,
        subscription: Mutex<Option<Filter>>,
        relays: Mutex<Vec<String>>,
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

        // After the interval elapses, the tick republishes the latest fix.
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
    async fn test_broadcast_waits_for_fix_and_times_out() {
        let mut f = fixture();
        add_member_group(&mut f, "G");
        drain(&mut f);

        f.engine.handle(EngineCmd::SendTest);
        assert!(drain(&mut f)
            .iter()
            .any(|e| matches!(e, UiEvent::NeedLocation(true))));

        // A fix arrives → TEST is published and location released.
        f.engine.handle(EngineCmd::Location(sample(now_secs() * 1000)));
        let published = f.pool.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert!(drain(&mut f)
            .iter()
            .any(|e| matches!(e, UiEvent::NeedLocation(false))));

        // Timeout path: request again, never deliver a fix.
        f.engine.handle(EngineCmd::SendTest);
        drain(&mut f);
        tokio::time::advance(Duration::from_secs(46)).await;
        f.engine.handle(EngineCmd::Tick);
        let evs = drain(&mut f);
        assert!(evs.iter().any(
            |e| matches!(e, UiEvent::Toast(t) if t.contains("no location fix"))
        ));
        assert_eq!(f.pool.published.lock().unwrap().len(), 1, "no extra publish");
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
            &GartPayload::active(10.0, 20.0, 1000, Some("hi".into())),
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
            &GartPayload::stop(),
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
    async fn subscription_follows_member_groups() {
        let mut f = fixture();
        drain(&mut f);
        assert!(f.pool.subscription.lock().unwrap().is_none());

        let g = add_member_group(&mut f, "A");
        let filter = f.pool.subscription.lock().unwrap().clone().unwrap();
        let json = serde_json::to_value(&filter).unwrap();
        assert_eq!(json["kinds"], serde_json::json!([694]));
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
}
