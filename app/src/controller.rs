//! Controller: bridges the Slint UI, the core engine and the platform.
//!
//! Threading model:
//! * Slint callbacks run on the UI thread and call [`Controller`] methods.
//! * The engine runs inside a private tokio runtime; its [`UiEvent`]s are
//!   folded into [`ViewState`] and re-rendered onto the UI thread via
//!   `Weak::upgrade_in_event_loop`.
//! * A 1 s UI timer re-renders relative timestamps and expires toasts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ntrack_core::config::{ConfigStore, Group};
use ntrack_core::engine::{
    now_secs, ConfigSnapshot, Engine, EngineCmd, LocationSample, ShareSnapshot, TrackSnapshot,
    UiEvent,
};
use ntrack_core::keys::{parse_group_key, ParsedGroupKey};
use ntrack_core::protocol::Status;
use ntrack_core::relay::{normalize_relay_url, RelayPool};
use slint::Weak;
use tokio::sync::mpsc;

use crate::map;
use crate::platform::{Platform, PlatformEvent};
use crate::{GroupItem, MainWindow, MapMarker, MapTile, RelayItem, TrackItem};

/// Publish interval choices shown in the UI, in seconds.
pub const INTERVALS: [u64; 4] = [15, 30, 60, 300];
/// Check-in (dead-man's switch) duration choices on the Share screen, in
/// seconds. Index-aligned with the combo box in `app.slint`.
pub const CHECKIN_OPTIONS: [u64; 4] = [300, 900, 1800, 3600];
/// Index of the Groups page in the bottom tab bar (Share, Track, Groups,
/// Settings). An incoming invite switches here to pre-fill the import form.
const GROUPS_PAGE: i32 = 2;
/// How long a live share may go without an update before we stop showing it
/// as live: the longest publish interval a sender can pick plus a grace
/// buffer for relay/GPS jitter. A killed or offline app never gets to send
/// its best-effort STOP, so the "live" state must time out rather than be
/// trusted forever. Recomputed every second by the render timer.
const SHARE_TIMEOUT_SECS: u64 = INTERVALS[INTERVALS.len() - 1] + 60;
const TOAST_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone)]
enum Confirm {
    RotateGroup(String),
    DeleteGroup(String),
}

#[derive(Clone)]
enum AfterPermission {
    StartShare { msg: String },
    /// One-tap panic deferred until location permission is granted, so the
    /// engine's share-start isn't immediately torn down by a permission-denied
    /// report from `start_location` racing the permission dialog.
    Panic,
}

#[derive(Default)]
struct ViewState {
    config: Option<ConfigSnapshot>,
    share: Option<ShareSnapshot>,
    tracks: Vec<TrackSnapshot>,
    relays: Vec<(String, bool)>,
    toast: Option<(String, Instant)>,
    confirm: Option<Confirm>,
    after_permission: Option<AfterPermission>,
    location_active: bool,
    /// Relays from the last scanned/tapped invite, kept so importing the group
    /// (whose form only shows the bare key) still adds the relays it carried.
    pending_invite: Option<ntrack_core::invite::Invite>,
    /// Live-map view + tile cache (only touched while the map overlay is open).
    map: map::MapState,
}

/// Overscan (px) added around the viewport when choosing tiles to fetch, so a
/// short drag reveals already-loaded tiles instead of blank space.
const TILE_MARGIN: f64 = 128.0;

pub struct Controller {
    rt: tokio::runtime::Runtime,
    cmd_tx: mpsc::UnboundedSender<EngineCmd>,
    platform: Arc<dyn Platform>,
    ui: Weak<MainWindow>,
    view: Arc<Mutex<ViewState>>,
    /// Shared TLS config for fetching map tiles (rustls + ring, webpki roots).
    tls: Arc<tokio_rustls::rustls::ClientConfig>,
}

impl Controller {
    pub fn new(
        data_dir: PathBuf,
        platform: Arc<dyn Platform>,
        ui: Weak<MainWindow>,
    ) -> Arc<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let store = ConfigStore::new(&data_dir);
        rt.block_on(async {
            let pool = RelayPool::new(pool_tx);
            let engine = Engine::new(store, pool, ui_tx);
            tokio::spawn(engine.run(cmd_rx));
        });

        // pool events → engine commands
        let cmd_tx2 = cmd_tx.clone();
        rt.spawn(async move {
            while let Some(ev) = pool_rx.recv().await {
                if cmd_tx2.send(EngineCmd::Pool(ev)).is_err() {
                    break;
                }
            }
        });

        let ctrl = Arc::new(Self {
            rt,
            cmd_tx,
            platform,
            ui,
            view: Arc::new(Mutex::new(ViewState::default())),
            tls: map::tls_config(),
        });
        ctrl.clone().spawn_ui_event_loop(ui_rx);
        ctrl
    }

    /// Feed platform events (location fixes, permission results) in.
    pub fn spawn_platform_forwarder(
        self: &Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<PlatformEvent>,
    ) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                ctrl.on_platform_event(ev);
            }
        });
    }

    fn on_platform_event(self: &Arc<Self>, ev: PlatformEvent) {
        match ev {
            PlatformEvent::Location(sample) => {
                let _ = self.cmd_tx.send(EngineCmd::Location(sample));
            }
            PlatformEvent::PermissionResult(granted) => {
                let pending = self.view.lock().unwrap().after_permission.take();
                if granted {
                    match pending {
                        Some(AfterPermission::StartShare { msg }) => {
                            let _ = self.cmd_tx.send(EngineCmd::StartShare {
                                msg: Some(msg).filter(|m| !m.trim().is_empty()),
                            });
                        }
                        Some(AfterPermission::Panic) => {
                            let _ = self.cmd_tx.send(EngineCmd::Panic);
                        }
                        None => {}
                    }
                } else {
                    self.toast("Location permission denied");
                    let sharing = self
                        .view
                        .lock()
                        .unwrap()
                        .share
                        .as_ref()
                        .map(|s| s.sharing)
                        .unwrap_or(false);
                    if sharing {
                        let _ = self
                            .cmd_tx
                            .send(EngineCmd::LocationUnavailable("permission denied".into()));
                    }
                }
            }
            PlatformEvent::IncomingInvite(raw) => self.on_incoming_invite(raw),
        }
    }

    /// Resume a share that was still active when the process last died (reboot,
    /// crash, swipe-away). Called once at startup; the engine no-ops unless its
    /// persisted resume flag is armed, so a normal launch does nothing.
    ///
    /// On Android the boot path already resumes headlessly inside the
    /// foreground service before the UI exists (see `headless`/`LocationService`);
    /// this is the hand-off that lets the freshly launched UI engine pick the
    /// share back up. We deliberately do not prompt for permission here — if the
    /// permission was lost the engine's normal location-unavailable path stops
    /// and disarms the share rather than nagging on every launch.
    pub fn resume_if_armed(self: &Arc<Self>) {
        let _ = self.cmd_tx.send(EngineCmd::ResumeShareIfArmed);
    }

    /// A scanned QR code or tapped `ntrack://join` link arrived. Pre-fill the
    /// Groups-tab import form and switch to it so the user can review the group
    /// before importing (we never import silently).
    fn on_incoming_invite(self: &Arc<Self>, raw: String) {
        let Some(invite) = ntrack_core::invite::parse_shared(&raw) else {
            self.toast("Not an ntrack invite");
            return;
        };
        // Stash so import_group can add the invite's relays: the form only shows
        // the bare key, so the relays would otherwise be lost between here and
        // the user tapping Import.
        self.view.lock().unwrap().pending_invite = Some(invite.clone());
        let name = invite.name.unwrap_or_default();
        let key = invite.key;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_import_name_text(name.into());
            ui.set_import_key_text(key.into());
            ui.set_current_page(GROUPS_PAGE);
        });
        self.toast("Invite scanned — review and tap Import");
    }

    /// The user tapped "Paste" on the Groups tab. Read the clipboard and fill
    /// the import form: a full `ntrack://join` invite pre-fills the name and
    /// stashes its relays (exactly like a scan), while a bare key drops into the
    /// key field without disturbing a name the user may already have typed.
    /// Unrecognized text is still shown in the key field so the user can see
    /// what was pasted and gets a precise error when they tap Import.
    fn paste_invite(self: &Arc<Self>) {
        let raw = self.platform.paste_text();
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            self.toast("Clipboard is empty");
            return;
        }
        match ntrack_core::invite::parse_shared(&raw) {
            Some(invite) => {
                self.view.lock().unwrap().pending_invite = Some(invite.clone());
                let name = invite.name.filter(|n| !n.trim().is_empty());
                let key = invite.key;
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    if let Some(name) = name {
                        ui.set_import_name_text(name.into());
                    }
                    ui.set_import_key_text(key.into());
                });
            }
            None => {
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    ui.set_import_key_text(raw.into());
                });
            }
        }
        self.toast("Pasted — review and tap Import");
    }

    fn spawn_ui_event_loop(self: Arc<Self>, mut ui_rx: mpsc::UnboundedReceiver<UiEvent>) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = ui_rx.recv().await {
                ctrl.on_ui_event(ev);
            }
        });
    }

    fn on_ui_event(self: &Arc<Self>, ev: UiEvent) {
        match ev {
            UiEvent::Config(c) => {
                self.view.lock().unwrap().config = Some(c);
            }
            UiEvent::Share(s) => {
                self.view.lock().unwrap().share = Some(s);
            }
            UiEvent::Tracks(t) => {
                self.view.lock().unwrap().tracks = t;
            }
            UiEvent::Relays(r) => {
                self.view.lock().unwrap().relays = r;
            }
            UiEvent::Toast(msg) => {
                self.view.lock().unwrap().toast = Some((msg, Instant::now() + TOAST_DURATION));
            }
            UiEvent::NeedLocation(on) => {
                let interval_ms = {
                    let mut view = self.view.lock().unwrap();
                    view.location_active = on;
                    view.config
                        .as_ref()
                        .map(|c| c.interval_secs * 1000)
                        .unwrap_or(30_000)
                };
                if on {
                    self.platform.start_location(interval_ms);
                } else {
                    self.platform.stop_location();
                }
            }
            UiEvent::SetLocationInterval(ms) => {
                // A duress alert boosting/relaxing the cadence: restart the
                // running location session at the new interval.
                let active = self.view.lock().unwrap().location_active;
                if active {
                    self.platform.stop_location();
                    self.platform.start_location(ms);
                }
                return; // no view-state change
            }
            UiEvent::Notify { title, body, .. } => {
                self.platform.notify_alert(&title, &body);
                return; // fire-and-forget; no view-state change
            }
            UiEvent::GroupShare(share) => {
                // The shared artifact is a self-describing invite URI carrying
                // the group name alongside the key, so the recipient never has
                // to type the name. The QR, Copy and Share all use it.
                let nsec = share.nsec.as_ref().map(|s| s.expose().to_string());
                // Members share the secret; send-only groups fall back to the npub.
                let key = nsec.clone().unwrap_or_else(|| share.npub.clone());
                let invite = ntrack_core::invite::build_invite(&share.name, &key, &share.relays);
                // Build the QR off the UI thread; create the image on it.
                let qr = qr_pixel_buffer(&invite);
                let name = share.name.clone();
                let npub = share.npub.clone();
                let nsec = nsec.unwrap_or_default();
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    ui.set_key_dialog_name(name.into());
                    ui.set_key_dialog_npub(npub.into());
                    ui.set_key_dialog_nsec(nsec.into());
                    ui.set_key_dialog_invite(invite.into());
                    if let Some(buf) = qr {
                        ui.set_key_dialog_qr(slint::Image::from_rgba8(buf));
                    }
                    ui.set_key_dialog_visible(true);
                });
                return; // dialog props set directly; no full render needed
            }
            UiEvent::TrackExport { suggested_filename, gpx_xml } => {
                // Hand the GPX to the platform: prefer opening it directly in a
                // track/maps app, falling back to the system share sheet. The
                // Android impl hops to the UI thread itself, so doing this from
                // the tokio worker is fine. No view-state change.
                self.platform.share_file(
                    &suggested_filename,
                    "application/gpx+xml",
                    gpx_xml.as_bytes(),
                    true,
                );
                return;
            }
        }
        self.schedule_render();
    }

    fn toast(self: &Arc<Self>, msg: &str) {
        self.view.lock().unwrap().toast = Some((msg.to_string(), Instant::now() + TOAST_DURATION));
        self.schedule_render();
    }

    fn schedule_render(self: &Arc<Self>) {
        let ctrl = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ctrl.render(&ui));
    }

    /// Render the entire view state onto the UI. Idempotent; called from the
    /// UI thread only.
    pub fn render(&self, ui: &MainWindow) {
        let mut view = self.view.lock().unwrap();

        // toast expiry
        if let Some((_, deadline)) = &view.toast {
            if Instant::now() >= *deadline {
                view.toast = None;
            }
        }
        ui.set_toast(
            view.toast
                .as_ref()
                .map(|(m, _)| m.as_str())
                .unwrap_or("")
                .into(),
        );

        if let Some(cfg) = &view.config {
            let groups: Vec<GroupItem> = cfg
                .groups
                .iter()
                .map(|g| GroupItem {
                    id: g.id.clone().into(),
                    name: g.name.clone().into(),
                    npub: shorten(&g.npub).into(),
                    can_receive: g.can_receive,
                    selected: g.selected,
                })
                .collect();
            ui.set_groups(slint::ModelRc::new(slint::VecModel::from(groups)));
            ui.set_can_receive_any(cfg.groups.iter().any(|g| g.can_receive));
            ui.set_sender_npub(cfg.sender_npub.clone().into());
            ui.set_display_name(cfg.display_name.clone().into());
            ui.set_default_name(cfg.default_name.clone().into());
            ui.set_interval_index(
                INTERVALS
                    .iter()
                    .position(|s| *s == cfg.interval_secs)
                    .unwrap_or(1) as i32,
            );
        }

        let relays: Vec<RelayItem> = view
            .relays
            .iter()
            .map(|(url, connected)| RelayItem {
                url: url.clone().into(),
                connected: *connected,
            })
            .collect();
        ui.set_relays(slint::ModelRc::new(slint::VecModel::from(relays)));

        let connected = view.relays.iter().filter(|(_, c)| *c).count();
        let total = view.relays.len();

        let share = view.share.clone().unwrap_or_default();
        ui.set_sharing(share.sharing);
        ui.set_waiting_fix(share.sharing && share.waiting_for_fix);
        let (headline, detail) = share_status_strings(&share, connected, total);
        ui.set_status_headline(headline.into());
        ui.set_status_detail(detail.into());

        // ---- duress alert + check-in (dead-man's switch) ----
        ui.set_alert(share.alert);
        let (checkin_armed, checkin_grace, checkin_countdown) = match share.checkin_deadline {
            Some(deadline) => (
                true,
                share.checkin_grace,
                fmt_countdown(deadline.saturating_sub(now_secs())),
            ),
            None => (false, false, String::new()),
        };
        ui.set_checkin_armed(checkin_armed);
        ui.set_checkin_grace(checkin_grace);
        ui.set_checkin_countdown(checkin_countdown.into());

        let tracks: Vec<TrackItem> = view
            .tracks
            .iter()
            .map(|t| {
                // The receiver's own label wins; otherwise the sender's
                // broadcast name (or a key-derived handle), computed by core.
                let title = if t.label.is_empty() {
                    t.name.clone()
                } else {
                    t.label.clone()
                };
                let has_coords = !(t.lat == 0.0 && t.lng == 0.0 && t.ts == 0);
                let (status, live) = track_liveness(t);
                let (r, g, b) = t.color;
                TrackItem {
                    sender: t.sender_hex.clone().into(),
                    title: title.into(),
                    label: t.label.clone().into(),
                    npub: t.sender_short.clone().into(),
                    group: t.group_name.clone().into(),
                    status: status.into(),
                    coords: if has_coords {
                        format!("{:.5}, {:.5}", t.lat, t.lng).into()
                    } else {
                        "".into()
                    },
                    ago: ago_string(t.ts, t.created_at),
                    msg: t.msg.clone().into(),
                    color: slint::Color::from_rgb_u8(r, g, b),
                    live,
                    has_coords,
                    alert: t.alert,
                }
            })
            .collect();
        ui.set_tracks(slint::ModelRc::new(slint::VecModel::from(tracks)));

        // ---- live map overlay ----
        ui.set_map_visible(view.map.open);
        if view.map.open {
            let m = &view.map;
            let placements =
                map::visible_tiles(m.center_lat, m.center_lng, m.zoom, m.vw, m.vh, TILE_MARGIN);
            let tiles: Vec<MapTile> = placements
                .iter()
                .filter_map(|p| match m.get(&p.id) {
                    Some(map::TileSlot::Loaded(buf)) => Some(MapTile {
                        dx: p.dx as f32,
                        dy: p.dy as f32,
                        img: slint::Image::from_rgba8(buf.clone()),
                    }),
                    _ => None,
                })
                .collect();
            ui.set_map_tiles(slint::ModelRc::new(slint::VecModel::from(tiles)));

            let mut markers: Vec<MapMarker> = Vec::new();
            let mut has_peers = false;
            for t in &view.tracks {
                if !is_live_with_coords(t) {
                    continue;
                }
                has_peers = true;
                let (dx, dy) = map::marker_offset(m.center_lat, m.center_lng, t.lat, t.lng, m.zoom);
                // Skip dots well outside the viewport (keeps offsets small too).
                if dx.abs() > m.vw / 2.0 + 64.0 || dy.abs() > m.vh / 2.0 + 64.0 {
                    continue;
                }
                let (r, g, b) = t.color;
                let title = if t.label.is_empty() {
                    t.name.clone()
                } else {
                    t.label.clone()
                };
                markers.push(MapMarker {
                    dx: dx as f32,
                    dy: dy as f32,
                    color: slint::Color::from_rgb_u8(r, g, b),
                    label: title.into(),
                });
            }
            ui.set_map_markers(slint::ModelRc::new(slint::VecModel::from(markers)));
            ui.set_map_has_peers(has_peers);
        }
    }

    /// Render synchronously when called on the UI thread (Slint callbacks
    /// are), so a pan/zoom redraws before the gesture's visual offset resets;
    /// falls back to posting if somehow off-thread.
    fn render_now(self: &Arc<Self>) {
        if let Some(ui) = self.ui.upgrade() {
            self.render(&ui);
        } else {
            self.schedule_render();
        }
    }

    // ---- live map overlay ------------------------------------------------

    /// Open the map, framed to show all current live peers.
    fn open_map_view(self: &Arc<Self>) {
        {
            let mut view = self.view.lock().unwrap();
            view.map.open = true;
            // Drop stale loading/failed tiles so a reopen retries cleanly while
            // keeping already-loaded imagery for an instant first paint.
            view.map.retain_loaded();
            let pts = live_points(&view.tracks);
            let (vw, vh) = (view.map.vw, view.map.vh);
            let (lat, lng, z) = map::fit(&pts, vw, vh);
            view.map.center_lat = lat;
            view.map.center_lng = lng;
            view.map.zoom = z;
        }
        self.refresh_map();
    }

    /// Re-frame the open map around the current live peers.
    fn map_fit(self: &Arc<Self>) {
        {
            let mut view = self.view.lock().unwrap();
            let pts = live_points(&view.tracks);
            let (vw, vh) = (view.map.vw, view.map.vh);
            let (lat, lng, z) = map::fit(&pts, vw, vh);
            view.map.center_lat = lat;
            view.map.center_lng = lng;
            view.map.zoom = z;
        }
        self.refresh_map();
    }

    fn map_pan(self: &Arc<Self>, dx: f64, dy: f64) {
        {
            let mut view = self.view.lock().unwrap();
            let m = &mut view.map;
            let (lat, lng) = map::pan(m.center_lat, m.center_lng, m.zoom, dx, dy);
            m.center_lat = lat;
            m.center_lng = lng;
        }
        self.refresh_map();
    }

    fn map_zoom(self: &Arc<Self>, delta: i32) {
        {
            let mut view = self.view.lock().unwrap();
            let z = (view.map.zoom as i32 + delta)
                .clamp(map::MIN_ZOOM as i32, map::MAX_ZOOM as i32) as u32;
            view.map.zoom = z;
        }
        self.refresh_map();
    }

    /// Commit a pinch: the cumulative scale factor maps to an integer zoom
    /// change (a doubling of the finger spread ≈ one level). The UI scales the
    /// tile layer live for feedback and resets once we refetch at the new
    /// level; a sub-threshold pinch (`delta == 0`) just snaps back, handled
    /// UI-side, so there's nothing to do here.
    fn map_pinch_zoom(self: &Arc<Self>, scale: f64) {
        if !(scale.is_finite() && scale > 0.0) {
            return;
        }
        let delta = scale.log2().round() as i32;
        if delta != 0 {
            self.map_zoom(delta);
        }
    }

    fn map_viewport(self: &Arc<Self>, w: f64, h: f64) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        {
            let mut view = self.view.lock().unwrap();
            if (view.map.vw - w).abs() < 0.5 && (view.map.vh - h).abs() < 0.5 {
                return; // unchanged — avoid a needless refetch/render
            }
            view.map.vw = w;
            view.map.vh = h;
        }
        self.refresh_map();
    }

    /// Mark visible-but-missing tiles as loading, spawn their fetches, then
    /// redraw whatever is already cached.
    fn refresh_map(self: &Arc<Self>) {
        let to_fetch: Vec<map::TileId> = {
            let mut view = self.view.lock().unwrap();
            if !view.map.open {
                return;
            }
            let (clat, clng, z, vw, vh) = {
                let m = &view.map;
                (m.center_lat, m.center_lng, m.zoom, m.vw, m.vh)
            };
            let placements = map::visible_tiles(clat, clng, z, vw, vh, TILE_MARGIN);
            let mut fetch = Vec::new();
            for p in &placements {
                if !view.map.contains(&p.id) {
                    view.map.insert(p.id, map::TileSlot::Loading);
                    fetch.push(p.id);
                }
            }
            fetch
        };
        for id in to_fetch {
            self.spawn_tile_fetch(id);
        }
        self.render_now();
    }

    fn spawn_tile_fetch(self: &Arc<Self>, id: map::TileId) {
        let ctrl = self.clone();
        let tls = self.tls.clone();
        self.rt.spawn(async move {
            let slot = match map::fetch_tile(tls, id).await {
                Some(buf) => map::TileSlot::Loaded(buf),
                None => map::TileSlot::Failed,
            };
            {
                let mut view = ctrl.view.lock().unwrap();
                if !view.map.open {
                    return; // map was closed before the tile arrived
                }
                view.map.insert(id, slot);
            }
            ctrl.schedule_render();
        });
    }

    // ---- UI callback wiring ---------------------------------------------

    pub fn attach(self: &Arc<Self>, ui: &MainWindow) {
        macro_rules! hook {
            ($setter:ident, |$ctrl:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
                let $ctrl = self.clone();
                ui.$setter(move |$($arg: $ty),*| $body);
            }};
        }

        hook!(on_toggle_group, |ctrl, id: slint::SharedString, on: bool| {
            let id = id.to_string();
            ctrl.mutate(move |c| {
                if let Some(g) = c.groups.iter_mut().find(|g| g.public == id) {
                    g.selected = on;
                }
            });
        });

        hook!(on_start_share, |ctrl, msg: slint::SharedString| {
            ctrl.start_share(msg.to_string());
        });

        hook!(on_stop_share, |ctrl| {
            let _ = ctrl.cmd_tx.send(EngineCmd::StopShare);
        });

        // ---- duress alert / panic / check-in ----
        hook!(on_panic, |ctrl| {
            ctrl.trigger_panic();
        });
        hook!(on_clear_alert, |ctrl| {
            let _ = ctrl.cmd_tx.send(EngineCmd::SetAlert(false));
        });
        hook!(on_arm_checkin, |ctrl, idx: i32| {
            let secs = CHECKIN_OPTIONS
                .get(idx.max(0) as usize)
                .copied()
                .unwrap_or(900);
            let _ = ctrl.cmd_tx.send(EngineCmd::ArmCheckin { secs });
        });
        hook!(on_checkin_safe, |ctrl| {
            let _ = ctrl.cmd_tx.send(EngineCmd::Checkin);
        });

        hook!(on_set_interval, |ctrl, idx: i32| {
            let secs = INTERVALS
                .get(idx.max(0) as usize)
                .copied()
                .unwrap_or(30);
            ctrl.mutate(move |c| c.interval_secs = secs);
            // apply immediately if location updates are running
            let active = ctrl.view.lock().unwrap().location_active;
            if active {
                ctrl.platform.stop_location();
                ctrl.platform.start_location(secs * 1000);
            }
        });

        hook!(on_message_edited, |ctrl, msg: slint::SharedString| {
            let m = msg.trim().to_string();
            let _ = ctrl
                .cmd_tx
                .send(EngineCmd::SetMessage(if m.is_empty() { None } else { Some(m) }));
        });

        hook!(on_open_map, |ctrl, idx: i32| {
            let item = ctrl
                .view
                .lock()
                .unwrap()
                .tracks
                .get(idx.max(0) as usize)
                .cloned();
            if let Some(t) = item {
                let label = if t.label.is_empty() { &t.name } else { &t.label };
                ctrl.platform.open_map(t.lat, t.lng, label);
            }
        });

        // ---- live map overlay ----
        hook!(on_open_map_view, |ctrl| {
            ctrl.open_map_view();
        });
        hook!(on_map_close, |ctrl| {
            ctrl.view.lock().unwrap().map.open = false;
            ctrl.render_now();
        });
        hook!(on_map_pan, |ctrl, dx: f32, dy: f32| {
            ctrl.map_pan(dx as f64, dy as f64);
        });
        hook!(on_map_zoom_in, |ctrl| {
            ctrl.map_zoom(1);
        });
        hook!(on_map_zoom_out, |ctrl| {
            ctrl.map_zoom(-1);
        });
        hook!(on_map_fit, |ctrl| {
            ctrl.map_fit();
        });
        hook!(on_map_pinch_zoom, |ctrl, scale: f32| {
            ctrl.map_pinch_zoom(scale as f64);
        });
        hook!(on_map_viewport, |ctrl, w: f32, h: f32| {
            ctrl.map_viewport(w as f64, h as f64);
        });

        hook!(on_export_track, |ctrl, idx: i32| {
            let item = ctrl
                .view
                .lock()
                .unwrap()
                .tracks
                .get(idx.max(0) as usize)
                .cloned();
            if let Some(t) = item {
                let _ = ctrl.cmd_tx.send(EngineCmd::ExportTrack {
                    sender_hex: t.sender_hex,
                    group_hex: t.group_hex,
                });
            }
        });

        hook!(on_rename_sender_confirmed, |ctrl,
                                           sender: slint::SharedString,
                                           label: slint::SharedString| {
            let (s, l) = (sender.to_string(), label.to_string());
            ctrl.mutate(move |c| c.set_label(&s, &l));
        });

        hook!(on_create_group, |ctrl, name: slint::SharedString| {
            let name = name.trim().to_string();
            if name.is_empty() {
                ctrl.toast("Give the group a name");
                return;
            }
            match Group::new_member(name) {
                Ok(group) => {
                    let id = group.public.clone();
                    ctrl.mutate(move |c| c.groups.push(group));
                    // immediately offer the key for distribution
                    let _ = ctrl.cmd_tx.send(EngineCmd::RequestGroupShare { group_hex: id });
                }
                Err(e) => ctrl.toast(&format!("Could not create group: {e}")),
            }
        });

        hook!(on_import_group, |ctrl,
                                name: slint::SharedString,
                                key: slint::SharedString| {
            ctrl.import_group(name.to_string(), key.to_string());
        });

        hook!(on_scan_qr, |ctrl| {
            ctrl.platform.scan_qr();
        });

        hook!(on_paste, |ctrl| {
            ctrl.paste_invite();
        });

        hook!(on_share_group, |ctrl, id: slint::SharedString| {
            let _ = ctrl
                .cmd_tx
                .send(EngineCmd::RequestGroupShare { group_hex: id.to_string() });
        });

        hook!(on_rotate_group, |ctrl, id: slint::SharedString| {
            ctrl.view.lock().unwrap().confirm = Some(Confirm::RotateGroup(id.to_string()));
            let _ = ctrl.ui.upgrade_in_event_loop(move |ui| {
                ui.set_confirm_title("Rotate group key?".into());
                ui.set_confirm_body(
                    "A new secret key is generated for this group. Every member must import the new key — the old one stops working for new broadcasts. Do this whenever someone leaves the group."
                        .into(),
                );
                ui.set_confirm_action("Rotate".into());
                ui.set_confirm_dialog_visible(true);
            });
        });

        hook!(on_delete_group, |ctrl, id: slint::SharedString| {
            ctrl.view.lock().unwrap().confirm = Some(Confirm::DeleteGroup(id.to_string()));
            let _ = ctrl.ui.upgrade_in_event_loop(move |ui| {
                ui.set_confirm_title("Delete group?".into());
                ui.set_confirm_body(
                    "The key is removed from this device. You will stop receiving locations for this group; other members are unaffected."
                        .into(),
                );
                ui.set_confirm_action("Delete".into());
                ui.set_confirm_dialog_visible(true);
            });
        });

        hook!(on_confirm_accepted, |ctrl| {
            let confirm = ctrl.view.lock().unwrap().confirm.take();
            match confirm {
                Some(Confirm::RotateGroup(id)) => {
                    let _ = ctrl.cmd_tx.send(EngineCmd::RotateGroup { group_hex: id });
                }
                Some(Confirm::DeleteGroup(id)) => {
                    ctrl.mutate(move |c| {
                        c.remove_group(&id);
                    });
                }
                None => {}
            }
        });

        hook!(on_add_relay, |ctrl, url: slint::SharedString| {
            match normalize_relay_url(url.as_str()) {
                Ok(u) => ctrl.mutate(move |c| c.add_relay(&u)),
                Err(_) => ctrl.toast("Invalid relay URL"),
            }
        });

        hook!(on_remove_relay, |ctrl, url: slint::SharedString| {
            let u = url.to_string();
            ctrl.mutate(move |c| c.remove_relay(&u));
        });

        hook!(on_rotate_sender, |ctrl| {
            ctrl.mutate(|c| {
                let _ = c.rotate_sender();
            });
            ctrl.toast("Sender key rotated — receivers will see you as a new sender");
        });

        hook!(on_set_display_name, |ctrl, name: slint::SharedString| {
            // Trimmed and stored verbatim; empty falls back to the derived
            // handle. Broadcast on the next location publish.
            let n = name.trim().to_string();
            ctrl.mutate(move |c| c.display_name = n);
            ctrl.toast("Name saved");
        });

        hook!(on_copy_text, |ctrl, text: slint::SharedString| {
            ctrl.platform.copy_text(text.as_str());
            ctrl.toast("Copied to clipboard");
        });

        hook!(on_system_share, |ctrl, text: slint::SharedString| {
            ctrl.platform.share_text(text.as_str());
        });
    }

    fn mutate(
        self: &Arc<Self>,
        f: impl FnOnce(&mut ntrack_core::config::Config) + Send + 'static,
    ) {
        let _ = self.cmd_tx.send(EngineCmd::Mutate(Box::new(f)));
    }

    /// One-tap panic: raise the alert and force-start a share to the emergency
    /// audience. With permission in hand we fire immediately; otherwise we
    /// request it and defer the panic to the grant (so a permission-denied
    /// report from `start_location` can't tear the just-started share down
    /// before the dialog resolves).
    fn trigger_panic(self: &Arc<Self>) {
        if self.platform.has_location_permission() {
            let _ = self.cmd_tx.send(EngineCmd::Panic);
        } else {
            self.view.lock().unwrap().after_permission = Some(AfterPermission::Panic);
            self.platform.request_location_permission();
        }
    }

    fn start_share(self: &Arc<Self>, msg: String) {
        // Refuse early when no group is selected so we don't pointlessly
        // prompt for permissions (engine re-checks anyway).
        let any_selected = self
            .view
            .lock()
            .unwrap()
            .config
            .as_ref()
            .map(|c| c.groups.iter().any(|g| g.selected))
            .unwrap_or(false);
        if !any_selected {
            self.toast("Select at least one group to share with");
            return;
        }
        if self.platform.has_location_permission() {
            let _ = self.cmd_tx.send(EngineCmd::StartShare {
                msg: Some(msg).filter(|m| !m.trim().is_empty()),
            });
        } else {
            self.view.lock().unwrap().after_permission =
                Some(AfterPermission::StartShare { msg });
            self.platform.request_location_permission();
        }
    }

    /// Best-effort count of how many of `relays` are not already configured,
    /// for the import toast. Mirrors `Config::add_imported_group`'s
    /// normalize/dedup/cap so the number matches what the engine actually adds.
    /// Falls back to the bundled defaults before the first config snapshot
    /// arrives (the engine starts from `Config::default`).
    fn count_new_relays(self: &Arc<Self>, relays: &[String]) -> usize {
        let view = self.view.lock().unwrap();
        let existing = view
            .config
            .as_ref()
            .map(|c| c.relays.clone())
            .unwrap_or_else(ntrack_core::config::default_relays);
        ntrack_core::relay::normalize_dedup(relays)
            .into_iter()
            .take(ntrack_core::invite::MAX_INVITE_RELAYS)
            .filter(|r| !existing.contains(r))
            .count()
    }

    fn import_group(self: &Arc<Self>, name: String, key: String) {
        let raw = key.trim();
        if raw.is_empty() {
            self.toast("Paste an invite, nsec1… or npub1…");
            return;
        }
        // Accept a full `ntrack://join` invite pasted into the key field too,
        // not just a bare key; an embedded name fills in when the name field
        // was left blank.
        let (embedded_name, key, mut relays) = match ntrack_core::invite::parse_shared(raw) {
            Some(invite) => (invite.name, invite.key, invite.relays),
            None => (None, raw.to_string(), Vec::new()),
        };
        // A scanned/tapped invite pre-fills only the bare key into the form, so
        // its relays arrive via the stash rather than via `raw`. Use them only
        // when they belong to the key actually being imported.
        if let Some(p) = self.view.lock().unwrap().pending_invite.take() {
            if p.key == key {
                relays.extend(p.relays);
            }
        }
        let parsed = match parse_group_key(&key) {
            Ok(p) => p,
            Err(e) => {
                self.toast(&format!("{e}"));
                return;
            }
        };
        let (public, secret) = match parsed {
            ParsedGroupKey::Member(keys) => (
                keys.public_key().to_hex(),
                Some(ntrack_core::keys::nsec(&keys)),
            ),
            ParsedGroupKey::SendOnly(pk) => (pk.to_hex(), None),
        };
        let exists = self
            .view
            .lock()
            .unwrap()
            .config
            .as_ref()
            .map(|c| c.groups.iter().any(|g| g.id == public))
            .unwrap_or(false);
        if exists {
            // Re-scanning an updated invite for a group we already have should
            // still converge relays: merge any it carries instead of dropping
            // them (e.g. the group migrated to or added a relay).
            let added = self.count_new_relays(&relays);
            if added > 0 {
                self.mutate(move |c| {
                    c.merge_group_relays(&public, &relays);
                });
                self.toast(&format!("Group already imported{}", relays_added_suffix(added)));
            } else {
                self.toast("This group key is already imported");
            }
            return;
        }
        // Name precedence: what the user typed, else the invite's embedded
        // name, else a derived placeholder.
        let name = name.trim().to_string();
        let name = if !name.is_empty() {
            name
        } else {
            embedded_name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Group {}", &public[..8]))
        };
        let receive = secret.is_some();
        let added = self.count_new_relays(&relays);
        self.mutate(move |c| {
            c.add_imported_group(name, public, secret, &relays);
        });
        let mut msg = if receive {
            "Group imported — you can send and receive".to_string()
        } else {
            "Group imported (send-only)".to_string()
        };
        if added > 0 {
            msg.push_str(&relays_added_suffix(added));
        }
        self.toast(&msg);
    }

    /// Drive location samples from the platform forwarder in tests/sim.
    pub fn inject_location(&self, sample: LocationSample) {
        let _ = self.cmd_tx.send(EngineCmd::Location(sample));
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
        // Give the engine a moment to flush state and emit the final STOP.
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// " · N relay(s) added" — the import-toast suffix when an invite contributed
/// `added` new relays. Empty input still formats fine; callers gate on `added`.
fn relays_added_suffix(added: usize) -> String {
    format!(" · {added} relay{} added", if added == 1 { "" } else { "s" })
}

/// "now", "12 s ago", "5 min ago", "3 h ago" — based on the location fix
/// time when present, otherwise the event timestamp.
fn ago_string(ts: u64, created_at: u64) -> slint::SharedString {
    let t = if ts > 0 { ts } else { created_at };
    if t == 0 {
        return "".into();
    }
    let now = now_secs();
    let d = now.saturating_sub(t);
    let s = match d {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{d} s ago"),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86400),
    };
    s.into()
}

/// A check-in countdown, "M:SS" (or "H:MM:SS" past an hour), re-rendered each
/// second by the UI timer.
fn fmt_countdown(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Whether an ACTIVE share whose last event arrived at `created_at` (unix
/// seconds) has been silent long enough to no longer count as live.
fn share_timed_out(created_at: u64) -> bool {
    now_secs().saturating_sub(created_at) > SHARE_TIMEOUT_SECS
}

/// Badge label and "live" flag for a track. A live (ACTIVE) sender that has
/// gone quiet past [`SHARE_TIMEOUT_SECS`] — app killed, GPS lost or offline,
/// so its STOP never arrived — is shown as ended rather than pinned live.
/// The last-known coordinates are kept (set by the caller) so the card still
/// shows where the sender was last seen, exactly like a real STOP.
fn track_liveness(t: &TrackSnapshot) -> (&'static str, bool) {
    match t.status {
        Status::Active if !share_timed_out(t.created_at) => ("LIVE", true),
        _ => ("ENDED", false),
    }
}

/// Whether a track is live *and* carries a real fix — the dots the map shows.
/// Mirrors the card's "has coords" rule (a bare STOP leaves lat/lng/ts zero).
fn is_live_with_coords(t: &TrackSnapshot) -> bool {
    let (_, live) = track_liveness(t);
    live && !(t.lat == 0.0 && t.lng == 0.0 && t.ts == 0)
}

/// (lat, lng) of every live peer with coordinates — the points the map frames.
fn live_points(tracks: &[TrackSnapshot]) -> Vec<(f64, f64)> {
    tracks
        .iter()
        .filter(|t| is_live_with_coords(t))
        .map(|t| (t.lat, t.lng))
        .collect()
}

fn shorten(npub: &str) -> String {
    if npub.len() > 21 {
        format!("{}…{}", &npub[..14], &npub[npub.len() - 6..])
    } else {
        npub.to_string()
    }
}

fn share_status_strings(s: &ShareSnapshot, connected: usize, total: usize) -> (String, String) {
    if !s.sharing {
        let headline = "Not sharing".to_string();
        let detail = if total == 0 {
            "Add a relay in Settings to get started.".to_string()
        } else {
            format!(
                "{connected}/{total} relays connected. Your location is only sent while sharing is on."
            )
        };
        return (headline, detail);
    }
    if s.waiting_for_fix {
        return (
            "Waiting for GPS fix…".to_string(),
            format!("{connected}/{total} relays connected. Sharing starts at the first fix."),
        );
    }
    let ack = if s.last_acked { "relay confirmed" } else { "sending…" };
    let last = s
        .last_publish
        .map(|t| {
            let d = now_secs().saturating_sub(t);
            if d < 5 {
                "just now".to_string()
            } else {
                format!("{d} s ago")
            }
        })
        .unwrap_or_else(|| "—".to_string());
    (
        "Sharing live location".to_string(),
        format!(
            "{connected}/{total} relays · {} updates sent · last {last} · {ack}",
            s.publish_count
        ),
    )
}

/// Render a QR code into a pixel buffer (dark modules on white, 4-module
/// quiet zone). Returns `None` if the payload doesn't fit a QR code.
fn qr_pixel_buffer(data: &str) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let quiet = 4usize;
    let size = (width + quiet * 2) as u32;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size, size);
    let stride = size as usize;
    let pixels = buf.make_mut_slice();
    for p in pixels.iter_mut() {
        *p = slint::Rgba8Pixel { r: 255, g: 255, b: 255, a: 255 };
    }
    let colors = code.to_colors();
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == qrcode::Color::Dark {
                pixels[(y + quiet) * stride + (x + quiet)] =
                    slint::Rgba8Pixel { r: 16, g: 18, b: 24, a: 255 };
            }
        }
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_strings() {
        let now = now_secs();
        assert_eq!(ago_string(now, 0).as_str(), "just now");
        assert_eq!(ago_string(now - 30, 0).as_str(), "30 s ago");
        assert_eq!(ago_string(now - 120, 0).as_str(), "2 min ago");
        assert_eq!(ago_string(now - 7200, 0).as_str(), "2 h ago");
        assert_eq!(ago_string(0, now - 90).as_str(), "1 min ago");
        assert_eq!(ago_string(0, 0).as_str(), "");
    }

    fn track(status: Status, created_at: u64) -> TrackSnapshot {
        TrackSnapshot {
            sender_hex: "ab".into(),
            sender_short: "npub1abc".into(),
            label: String::new(),
            name: "Swift Otter".into(),
            color: (140, 90, 200),
            group_name: "G".into(),
            status,
            live: status == Status::Active,
            alert: false,
            lat: 1.0,
            lng: 2.0,
            ts: created_at,
            created_at,
            msg: String::new(),
            group_hex: "cd".into(),
        }
    }

    #[test]
    fn share_timeout_boundary() {
        let now = now_secs();
        assert!(!share_timed_out(now), "a fresh update is not timed out");
        assert!(!share_timed_out(now - SHARE_TIMEOUT_SECS + 1), "still inside the window");
        assert!(share_timed_out(now - SHARE_TIMEOUT_SECS - 1), "past the window");
    }

    #[test]
    fn live_share_times_out_into_ended() {
        let now = now_secs();
        // A recent ACTIVE share is live.
        assert_eq!(track_liveness(&track(Status::Active, now)), ("LIVE", true));
        // An ACTIVE share we haven't heard from past the timeout is no longer
        // shown as live — the sender's app was killed before sending a STOP.
        assert_eq!(
            track_liveness(&track(Status::Active, now - SHARE_TIMEOUT_SECS - 1)),
            ("ENDED", false)
        );
        // A real STOP is always ended.
        assert_eq!(track_liveness(&track(Status::Stop, now)), ("ENDED", false));
    }

    #[test]
    fn qr_buffer_is_rendered() {
        let buf = qr_pixel_buffer("nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq").unwrap();
        assert!(buf.width() > 20);
        assert_eq!(buf.width(), buf.height());
        // corners of the quiet zone are white
        let px = buf.as_slice()[0];
        assert_eq!((px.r, px.g, px.b), (255, 255, 255));
    }

    #[test]
    fn status_strings() {
        let s = ShareSnapshot {
            sharing: false,
            last_publish: None,
            publish_count: 0,
            last_acked: false,
            waiting_for_fix: false,
            ..Default::default()
        };
        let (h, d) = share_status_strings(&s, 2, 3);
        assert_eq!(h, "Not sharing");
        assert!(d.contains("2/3"));

        let s = ShareSnapshot {
            sharing: true,
            last_publish: Some(now_secs()),
            publish_count: 7,
            last_acked: true,
            waiting_for_fix: false,
            ..Default::default()
        };
        let (h, d) = share_status_strings(&s, 3, 3);
        assert_eq!(h, "Sharing live location");
        assert!(d.contains("7 updates"));
        assert!(d.contains("confirmed"));
    }

    #[test]
    fn shorten_npub() {
        let long = "npub1abcdefghijklmnopqrstuvwxyz0123456789";
        let s = shorten(long);
        assert!(s.len() < long.len());
        assert!(s.starts_with("npub1"));
        assert!(s.contains('…'));
        assert_eq!(shorten("npub1short"), "npub1short");
    }
}
