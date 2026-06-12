//! Minimal in-memory Nostr relay for local development and demos.
//!
//!     cargo run -p ntrack-core --example mock_relay -- 127.0.0.1:7777
//!
//! Supports just enough NIP-01 for ntrack: EVENT (stored, fanned out to
//! matching subscriptions, OK'd), REQ (replays stored events matching
//! kinds/#p/since, then EOSE), CLOSE. Everything lives in memory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// (sender, subscriptions: subid → filter)
type Conn = (mpsc::UnboundedSender<String>, HashMap<String, Value>);

#[derive(Default)]
struct Shared {
    events: Mutex<Vec<Value>>,
    conns: Mutex<HashMap<u64, Conn>>,
}

fn filter_matches(filter: &Value, event: &Value) -> bool {
    if let Some(kinds) = filter.get("kinds").and_then(|k| k.as_array()) {
        let kind = event.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
        if !kinds.iter().any(|k| k.as_u64() == Some(kind)) {
            return false;
        }
    }
    if let Some(since) = filter.get("since").and_then(|s| s.as_u64()) {
        if event.get("created_at").and_then(|c| c.as_u64()).unwrap_or(0) < since {
            return false;
        }
    }
    if let Some(ptags) = filter.get("#p").and_then(|p| p.as_array()) {
        let wanted: Vec<&str> = ptags.iter().filter_map(|v| v.as_str()).collect();
        let has = event
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|tags| {
                tags.iter().any(|tag| {
                    let t = tag.as_array();
                    matches!(t, Some(t) if t.len() >= 2
                        && t[0].as_str() == Some("p")
                        && wanted.contains(&t[1].as_str().unwrap_or("")))
                })
            })
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    true
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7777".into());
    let listener = TcpListener::bind(&addr).await.expect("bind");
    println!("mock relay listening on ws://{addr}");

    let shared = Arc::new(Shared::default());
    let next_id = Arc::new(AtomicU64::new(1));

    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let shared = shared.clone();
        let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
            println!("[{conn_id}] connected: {peer}");
            let (mut sink, mut stream) = ws.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            shared
                .conns
                .lock()
                .unwrap()
                .insert(conn_id, (tx, HashMap::new()));

            loop {
                tokio::select! {
                    out = rx.recv() => match out {
                        Some(frame) => { let _ = sink.send(Message::Text(frame.into())).await; }
                        None => break,
                    },
                    msg = stream.next() => {
                        let text = match msg {
                            Some(Ok(Message::Text(t))) => t,
                            Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; continue }
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            _ => continue,
                        };
                        let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        handle_frame(conn_id, &frame, &shared);
                    }
                }
            }
            shared.conns.lock().unwrap().remove(&conn_id);
            println!("[{conn_id}] disconnected");
        });
    }
}

fn handle_frame(conn_id: u64, frame: &Value, shared: &Shared) {
    match frame.get(0).and_then(|v| v.as_str()) {
        Some("EVENT") => {
            let Some(event) = frame.get(1) else { return };
            let id = event.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let kind = event.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
            println!("[{conn_id}] EVENT kind {kind} id {}", &id[..12.min(id.len())]);
            shared.events.lock().unwrap().push(event.clone());

            let conns = shared.conns.lock().unwrap();
            for (other_id, (tx, subs)) in conns.iter() {
                for (subid, filter) in subs {
                    if filter_matches(filter, event) {
                        let _ = tx.send(json!(["EVENT", subid, event]).to_string());
                        println!("[{conn_id}] -> fan out to conn {other_id} sub {subid}");
                    }
                }
            }
            if let Some((tx, _)) = conns.get(&conn_id) {
                let _ = tx.send(json!(["OK", id, true, ""]).to_string());
            }
        }
        Some("REQ") => {
            let Some(subid) = frame.get(1).and_then(|v| v.as_str()) else { return };
            let filter = frame.get(2).cloned().unwrap_or_else(|| json!({}));
            println!("[{conn_id}] REQ {subid} {filter}");
            let stored = shared.events.lock().unwrap().clone();
            let conns = shared.conns.lock().unwrap();
            let Some((tx, _)) = conns.get(&conn_id) else { return };
            for event in stored.iter().filter(|e| filter_matches(&filter, e)) {
                let _ = tx.send(json!(["EVENT", subid, event]).to_string());
            }
            let _ = tx.send(json!(["EOSE", subid]).to_string());
            drop(conns);
            shared
                .conns
                .lock()
                .unwrap()
                .get_mut(&conn_id)
                .map(|(_, subs)| subs.insert(subid.to_string(), filter));
        }
        Some("CLOSE") => {
            if let Some(subid) = frame.get(1).and_then(|v| v.as_str()) {
                if let Some((_, subs)) = shared.conns.lock().unwrap().get_mut(&conn_id) {
                    subs.remove(subid);
                }
            }
        }
        _ => {}
    }
}
