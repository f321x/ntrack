//! Minimal Nostr relay pool: publish kind:694 events to every configured
//! relay and maintain one logical subscription, with automatic reconnect
//! and offline publish queueing.
//!
//! The wire protocol is plain NIP-01:
//! out: `["EVENT", <event>]`, `["REQ", <subid>, <filter>]`, `["CLOSE", <subid>]`
//! in:  `["EVENT", <subid>, <event>]`, `["OK", <id>, <bool>, <msg>]`,
//!      `["EOSE", <subid>]`, `["NOTICE", <msg>]`, `["CLOSED", <subid>, <msg>]`

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventId, Filter};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

const SUB_ID: &str = "ntrack";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(25);
const STALE_AFTER: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const OFFLINE_QUEUE_CAP: usize = 64;

/// Events surfaced by the pool to the engine.
#[derive(Debug)]
pub enum PoolEvent {
    Status { url: String, connected: bool },
    Incoming { url: String, event: Box<Event> },
    PublishAck { url: String, event_id: EventId, accepted: bool, message: String },
    Eose { url: String },
}

#[derive(Debug, Clone)]
enum RelayCmd {
    Publish(Box<Event>),
    SetSubscription(Option<Box<Filter>>),
    Shutdown,
}

/// Abstraction over the pool so engines can be tested with a mock.
pub trait Publisher: Send + Sync + 'static {
    fn publish(&self, event: Event);
    fn set_subscription(&self, filter: Option<Filter>);
}

struct RelayHandle {
    cmd_tx: mpsc::UnboundedSender<RelayCmd>,
    connected: Arc<AtomicBool>,
}

pub struct RelayPool {
    relays: Mutex<HashMap<String, RelayHandle>>,
    filter: Mutex<Option<Filter>>,
    event_tx: mpsc::UnboundedSender<PoolEvent>,
    runtime: tokio::runtime::Handle,
}

/// rustls 0.23 needs a process-level crypto provider; the TLS stack pulled
/// in by tokio-tungstenite deliberately enables none, so install ring
/// before the first `wss://` handshake (idempotent, races are harmless —
/// `install_default` simply fails if one is already set).
pub fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl RelayPool {
    /// Create a pool that reports relay activity on `event_tx`. Must be
    /// called from within a tokio runtime (its handle is captured for
    /// spawning relay tasks later, possibly from non-runtime threads).
    pub fn new(event_tx: mpsc::UnboundedSender<PoolEvent>) -> Arc<Self> {
        ensure_crypto_provider();
        Arc::new(Self {
            relays: Mutex::new(HashMap::new()),
            filter: Mutex::new(None),
            event_tx,
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Reconcile the set of relay connections with `urls`.
    pub fn set_relays(&self, urls: &[String]) {
        let urls: Vec<String> = urls
            .iter()
            .filter_map(|u| normalize_relay_url(u).ok())
            .collect();
        let mut relays = self.relays.lock().unwrap();
        relays.retain(|url, handle| {
            let keep = urls.contains(url);
            if !keep {
                let _ = handle.cmd_tx.send(RelayCmd::Shutdown);
            }
            keep
        });
        let filter = self.filter.lock().unwrap().clone();
        for url in urls {
            if relays.contains_key(&url) {
                continue;
            }
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            if let Some(f) = &filter {
                let _ = cmd_tx.send(RelayCmd::SetSubscription(Some(Box::new(f.clone()))));
            }
            let connected = Arc::new(AtomicBool::new(false));
            self.runtime.spawn(relay_task(
                url.clone(),
                cmd_rx,
                self.event_tx.clone(),
                connected.clone(),
            ));
            relays.insert(url, RelayHandle { cmd_tx, connected });
        }
    }

    pub fn relay_status(&self) -> Vec<(String, bool)> {
        let relays = self.relays.lock().unwrap();
        let mut v: Vec<(String, bool)> = relays
            .iter()
            .map(|(url, h)| (url.clone(), h.connected.load(Ordering::Relaxed)))
            .collect();
        v.sort();
        v
    }

    pub fn shutdown(&self) {
        let mut relays = self.relays.lock().unwrap();
        for (_, h) in relays.drain() {
            let _ = h.cmd_tx.send(RelayCmd::Shutdown);
        }
    }
}

impl Publisher for RelayPool {
    fn publish(&self, event: Event) {
        let relays = self.relays.lock().unwrap();
        let boxed = Box::new(event);
        for h in relays.values() {
            let _ = h.cmd_tx.send(RelayCmd::Publish(boxed.clone()));
        }
    }

    fn set_subscription(&self, filter: Option<Filter>) {
        *self.filter.lock().unwrap() = filter.clone();
        let relays = self.relays.lock().unwrap();
        for h in relays.values() {
            let _ = h
                .cmd_tx
                .send(RelayCmd::SetSubscription(filter.clone().map(Box::new)));
        }
    }
}

/// Normalize user input into a `ws(s)://` URL. Bare hosts get `wss://`.
pub fn normalize_relay_url(input: &str) -> Result<String, crate::Error> {
    let s = input.trim();
    let invalid = || crate::Error::Other(format!("invalid relay url: {input:?}"));
    // The scheme is case-insensitive (RFC 3986). Match it on a lowercased copy,
    // then slice the original so the remainder keeps its case until we decide
    // which part of it (the authority) to lowercase below. ASCII lowercasing
    // preserves byte offsets, so the fixed prefix lengths are valid on `s`.
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("wss://") {
        ("wss", &s["wss://".len()..])
    } else if lower.starts_with("ws://") {
        ("ws", &s["ws://".len()..])
    } else if lower.starts_with("https://") {
        ("wss", &s["https://".len()..])
    } else if lower.starts_with("http://") {
        ("ws", &s["http://".len()..])
    } else if s.contains("://") {
        return Err(crate::Error::Other(format!("unsupported scheme: {s}")));
    } else {
        ("wss", s)
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err(invalid());
    }
    // The host is case-insensitive and is lowercased so case-only differences
    // don't create duplicate relays; the path is case-sensitive and is kept
    // verbatim. Within the authority, any userinfo (`user:pass@`) is also
    // case-sensitive (RFC 3986), so only the `host[:port]` part is folded.
    let normalized = match rest.split_once('/') {
        Some((authority, path)) => format!("{scheme}://{}/{path}", lower_host(authority)),
        None => format!("{scheme}://{}", lower_host(rest)),
    };
    Ok(normalized)
}

/// Lowercase the `host[:port]` of an authority, leaving any `user:pass@`
/// userinfo untouched. Uses full Unicode case folding so non-ASCII hosts dedup
/// too (IDN/punycode unification is not attempted — relays use ASCII/punycode
/// hostnames).
fn lower_host(authority: &str) -> String {
    match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => format!("{userinfo}@{}", hostport.to_lowercase()),
        None => authority.to_lowercase(),
    }
}

/// Normalize a list of relay URLs, dropping any that are invalid and removing
/// duplicates while preserving first-seen order. Normalization is
/// case-insensitive (see [`normalize_relay_url`]), so URLs differing only by
/// case collapse to a single entry.
pub fn normalize_dedup(urls: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in urls {
        if let Ok(n) = normalize_relay_url(u) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// Per-relay connection task: connect → resubscribe → pump messages,
/// with exponential backoff reconnect and bounded offline publish queue.
async fn relay_task(
    url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<RelayCmd>,
    event_tx: mpsc::UnboundedSender<PoolEvent>,
    connected: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut sub: Option<Box<Filter>> = None;
    let mut queue: VecDeque<Box<Event>> = VecDeque::new();

    'reconnect: loop {
        // Drain any commands that arrived while we were disconnected.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if apply_offline_cmd(cmd, &mut sub, &mut queue) {
                        return;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }

        let conn = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(url.clone()),
        )
        .await;

        let ws = match conn {
            Ok(Ok((ws, _resp))) => ws,
            Ok(Err(e)) => {
                log::debug!("relay {url}: connect failed: {e}");
                if wait_backoff(&mut cmd_rx, &mut sub, &mut queue, &mut backoff).await {
                    return;
                }
                continue 'reconnect;
            }
            Err(_) => {
                log::debug!("relay {url}: connect timed out");
                if wait_backoff(&mut cmd_rx, &mut sub, &mut queue, &mut backoff).await {
                    return;
                }
                continue 'reconnect;
            }
        };

        log::info!("relay {url}: connected");
        backoff = Duration::from_secs(1);
        connected.store(true, Ordering::Relaxed);
        let _ = event_tx.send(PoolEvent::Status { url: url.clone(), connected: true });

        let (mut sink, mut stream) = ws.split();
        let mut healthy = true;

        if let Some(f) = &sub {
            healthy &= send_req(&mut sink, f).await;
        }
        while healthy {
            let Some(ev) = queue.pop_front() else { break };
            healthy &= send_event(&mut sink, &ev).await;
        }

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_rx = Instant::now();
        let mut shutdown = false;

        while healthy {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(RelayCmd::Shutdown) => {
                        let _ = sink.send(Message::Close(None)).await;
                        shutdown = true;
                        break;
                    }
                    Some(RelayCmd::Publish(ev)) => {
                        healthy = send_event(&mut sink, &ev).await;
                        if !healthy {
                            push_bounded(&mut queue, ev);
                        }
                    }
                    Some(RelayCmd::SetSubscription(f)) => {
                        if sub.is_some() {
                            healthy &= send_json(&mut sink, &serde_json::json!(["CLOSE", SUB_ID])).await;
                        }
                        sub = f;
                        if let Some(f) = &sub {
                            healthy &= send_req(&mut sink, f).await;
                        }
                    }
                },
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_rx = Instant::now();
                        handle_incoming(&url, text.as_str(), &event_tx);
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        last_rx = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        log::debug!("relay {url}: closed by remote");
                        healthy = false;
                    }
                    Some(Err(e)) => {
                        log::debug!("relay {url}: read error: {e}");
                        healthy = false;
                    }
                    Some(Ok(_)) => {}
                },
                _ = ping.tick() => {
                    if last_rx.elapsed() > STALE_AFTER {
                        log::debug!("relay {url}: stale connection, reconnecting");
                        healthy = false;
                    } else {
                        healthy &= sink.send(Message::Ping(Vec::new().into())).await.is_ok();
                    }
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        let _ = event_tx.send(PoolEvent::Status { url: url.clone(), connected: false });
        if shutdown {
            return;
        }
        if wait_backoff(&mut cmd_rx, &mut sub, &mut queue, &mut backoff).await {
            return;
        }
    }
}

/// Apply a command while disconnected. Returns `true` on shutdown.
fn apply_offline_cmd(
    cmd: RelayCmd,
    sub: &mut Option<Box<Filter>>,
    queue: &mut VecDeque<Box<Event>>,
) -> bool {
    match cmd {
        RelayCmd::Shutdown => true,
        RelayCmd::Publish(ev) => {
            push_bounded(queue, ev);
            false
        }
        RelayCmd::SetSubscription(f) => {
            *sub = f;
            false
        }
    }
}

fn push_bounded(queue: &mut VecDeque<Box<Event>>, ev: Box<Event>) {
    queue.push_back(ev);
    while queue.len() > OFFLINE_QUEUE_CAP {
        queue.pop_front();
    }
}

/// Sleep for the backoff period while still servicing commands.
/// Returns `true` on shutdown.
async fn wait_backoff(
    cmd_rx: &mut mpsc::UnboundedReceiver<RelayCmd>,
    sub: &mut Option<Box<Filter>>,
    queue: &mut VecDeque<Box<Event>>,
    backoff: &mut Duration,
) -> bool {
    let deadline = Instant::now() + *backoff;
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            cmd = cmd_rx.recv() => match cmd {
                None => return true,
                Some(cmd) => {
                    if apply_offline_cmd(cmd, sub, queue) {
                        return true;
                    }
                }
            }
        }
    }
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

async fn send_json(sink: &mut WsSink, value: &serde_json::Value) -> bool {
    match serde_json::to_string(value) {
        Ok(text) => sink.send(Message::Text(text.into())).await.is_ok(),
        Err(e) => {
            log::error!("relay: failed to serialize frame: {e}");
            true // not a connection error
        }
    }
}

async fn send_req(sink: &mut WsSink, filter: &Filter) -> bool {
    send_json(sink, &serde_json::json!(["REQ", SUB_ID, filter])).await
}

async fn send_event(sink: &mut WsSink, event: &Event) -> bool {
    send_json(sink, &serde_json::json!(["EVENT", event])).await
}

fn handle_incoming(url: &str, text: &str, event_tx: &mpsc::UnboundedSender<PoolEvent>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        log::debug!("relay {url}: unparseable frame");
        return;
    };
    let Some(arr) = value.as_array() else { return };
    match arr.first().and_then(|v| v.as_str()) {
        Some("EVENT") => {
            let Some(ev) = arr.get(2) else { return };
            match serde_json::from_value::<Event>(ev.clone()) {
                Ok(event) => {
                    let _ = event_tx.send(PoolEvent::Incoming {
                        url: url.to_string(),
                        event: Box::new(event),
                    });
                }
                Err(e) => log::debug!("relay {url}: bad event: {e}"),
            }
        }
        Some("OK") => {
            let id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| EventId::from_hex(s).ok());
            let accepted = arr.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            let message = arr
                .get(3)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(event_id) = id {
                let _ = event_tx.send(PoolEvent::PublishAck {
                    url: url.to_string(),
                    event_id,
                    accepted,
                    message,
                });
            }
        }
        Some("EOSE") => {
            let _ = event_tx.send(PoolEvent::Eose { url: url.to_string() });
        }
        Some("NOTICE") | Some("CLOSED") => {
            log::warn!("relay {url}: {text}");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_provider_is_installed_for_tls() {
        ensure_crypto_provider();
        ensure_crypto_provider(); // idempotent
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "wss:// handshakes would panic without a process-level provider"
        );
    }

    #[test]
    fn url_normalization() {
        assert_eq!(normalize_relay_url("relay.damus.io").unwrap(), "wss://relay.damus.io");
        assert_eq!(normalize_relay_url(" wss://nos.lol/ ").unwrap(), "wss://nos.lol");
        assert_eq!(normalize_relay_url("https://x.io").unwrap(), "wss://x.io");
        assert_eq!(normalize_relay_url("ws://127.0.0.1:8080").unwrap(), "ws://127.0.0.1:8080");
        assert!(normalize_relay_url("").is_err());
        assert!(normalize_relay_url("ftp://x").is_err());
        assert!(normalize_relay_url("wss://").is_err());
        assert!(normalize_relay_url("has space.com").is_err());
    }

    #[test]
    fn normalize_dedup_collapses_case_and_drops_invalid() {
        let input = vec![
            "wss://relay.damus.io".to_string(),
            "WSS://Relay.Damus.IO".to_string(), // case-only duplicate of the first
            "nos.lol".to_string(),              // bare host → wss://
            "ftp://bad".to_string(),            // invalid → dropped
            "wss://nos.lol".to_string(),        // duplicate of the bare host above
        ];
        assert_eq!(
            normalize_dedup(&input),
            vec!["wss://relay.damus.io".to_string(), "wss://nos.lol".to_string()]
        );
    }

    #[test]
    fn url_normalization_lowercases_scheme_and_host() {
        // Case-only differences must collapse so they don't create duplicate
        // relays. The host is case-insensitive; the path is not.
        assert_eq!(
            normalize_relay_url("WSS://Relay.Damus.IO").unwrap(),
            "wss://relay.damus.io"
        );
        assert_eq!(
            normalize_relay_url("Relay.Example.COM").unwrap(),
            "wss://relay.example.com"
        );
        // Path/query case is preserved (only the authority is lowercased).
        assert_eq!(
            normalize_relay_url("wss://Relay.example.com/Nostr").unwrap(),
            "wss://relay.example.com/Nostr"
        );
    }

    #[test]
    fn url_normalization_folds_non_ascii_host_and_keeps_userinfo() {
        // A non-ASCII host must case-fold so a case-only variant still dedups.
        assert_eq!(
            normalize_relay_url("WSS://Ärger.example").unwrap(),
            normalize_relay_url("wss://ärger.example").unwrap(),
        );
        // Userinfo is case-sensitive (RFC 3986): only the host is lowercased.
        assert_eq!(
            normalize_relay_url("wss://User:Pass@Relay.COM/Path").unwrap(),
            "wss://User:Pass@relay.com/Path"
        );
    }
}
