//! Integration tests: real RelayPool against an in-process mock Nostr relay
//! (plain tokio-tungstenite WebSocket server speaking NIP-01 frames).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, Keys};
use ntrack_core::dedup::SeenIds;
use ntrack_core::protocol::{self, Payload};
use ntrack_core::relay::{PoolEvent, Publisher, RelayPool};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// What the mock relay records / does per connection.
struct MockRelay {
    addr: String,
    /// Frames received from the client, JSON-decoded.
    rx: mpsc::UnboundedReceiver<serde_json::Value>,
    /// Send raw frames to the most recent client connection.
    tx: mpsc::UnboundedSender<String>,
    /// Drop (close) the active connection.
    kill: mpsc::UnboundedSender<()>,
}

/// Spawn a single-listener mock relay. Each accepted connection:
/// * forwards every received text frame (JSON-decoded) to `rx`
/// * answers EVENT frames with ["OK", id, true, ""]
/// * writes any frame pushed into `tx`
/// * closes when `kill` fires
async fn spawn_mock_relay() -> MockRelay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { continue };
            serve_conn(ws, &frames_tx, &mut out_rx, &mut kill_rx).await;
        }
    });

    MockRelay { addr, rx: frames_rx, tx: out_tx, kill: kill_tx }
}

async fn serve_conn(
    ws: WebSocketStream<TcpStream>,
    frames_tx: &mpsc::UnboundedSender<serde_json::Value>,
    out_rx: &mut mpsc::UnboundedReceiver<String>,
    kill_rx: &mut mpsc::UnboundedReceiver<()>,
) {
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                    // auto-ACK publishes like a permissive relay
                    if v[0] == "EVENT" {
                        let id = v[1]["id"].as_str().unwrap_or_default();
                        let ok = serde_json::json!(["OK", id, true, ""]).to_string();
                        let _ = sink.send(Message::Text(ok.into())).await;
                    }
                    let _ = frames_tx.send(v);
                }
                Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
            frame = out_rx.recv() => match frame {
                Some(f) => { let _ = sink.send(Message::Text(f.into())).await; }
                None => return,
            },
            _ = kill_rx.recv() => {
                let _ = sink.send(Message::Close(None)).await;
                return;
            }
        }
    }
}

async fn recv_frame(relay: &mut MockRelay) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), relay.rx.recv())
        .await
        .expect("timed out waiting for relay frame")
        .expect("relay channel closed")
}

async fn next_pool_event(rx: &mut mpsc::UnboundedReceiver<PoolEvent>) -> PoolEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for pool event")
        .expect("pool channel closed")
}

fn test_event() -> (Event, Keys, Keys) {
    let sender = Keys::generate();
    let group = Keys::generate();
    let event = protocol::build_event(
        &sender,
        &[group.public_key()],
        &Payload::active(48.2, 11.6, 1722173222, None),
        None,
    )
    .unwrap();
    (event, sender, group)
}

#[tokio::test]
async fn publish_reaches_relay_and_ok_is_reported() {
    let mut relay = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);
    pool.set_relays(&[relay.addr.clone()]);

    // wait for connect
    loop {
        if let PoolEvent::Status { connected: true, .. } = next_pool_event(&mut pool_rx).await {
            break;
        }
    }

    let (event, _, _) = test_event();
    pool.publish(event.clone());

    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "EVENT");
    assert_eq!(frame[1]["kind"], 3434);
    assert_eq!(frame[1]["id"], event.id.to_hex());

    // pool surfaces the OK as a PublishAck
    loop {
        match next_pool_event(&mut pool_rx).await {
            PoolEvent::PublishAck { event_id, accepted, .. } => {
                assert_eq!(event_id, event.id);
                assert!(accepted);
                break;
            }
            _ => continue,
        }
    }
    pool.shutdown();
}

#[tokio::test]
async fn subscription_is_sent_and_incoming_event_is_decryptable() {
    let mut relay = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);

    let (event, _, group) = test_event();
    let filter = protocol::subscription_filter(&[group.public_key()], 3600);
    pool.set_subscription(Some(filter));
    pool.set_relays(&[relay.addr.clone()]);

    // REQ arrives with our filter
    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "REQ");
    assert_eq!(frame[2]["kinds"][0], 3434);
    assert_eq!(frame[2]["#p"][0], group.public_key().to_hex());
    let subid = frame[1].as_str().unwrap().to_string();

    // relay delivers an event; pool surfaces it; protocol layer decrypts it
    let deliver = serde_json::json!(["EVENT", subid, event]).to_string();
    relay.tx.send(deliver).unwrap();
    loop {
        match next_pool_event(&mut pool_rx).await {
            PoolEvent::Incoming { event: got, .. } => {
                assert_eq!(got.id, event.id);
                let mut seen = SeenIds::new(8);
                let inc = protocol::process_incoming(&got, std::slice::from_ref(&group), &mut seen).unwrap();
                assert_eq!(inc.payload.lat, Some(48.2));
                break;
            }
            _ => continue,
        }
    }
    pool.shutdown();
}

#[tokio::test]
async fn reconnect_resubscribes_and_flushes_queued_publishes() {
    let mut relay = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);

    let (event, _, group) = test_event();
    pool.set_subscription(Some(protocol::subscription_filter(
        &[group.public_key()],
        3600,
    )));
    pool.set_relays(&[relay.addr.clone()]);

    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "REQ");

    // Drop the connection server-side; pool should reconnect (backoff 1s),
    // re-subscribe, and flush events published while offline.
    relay.kill.send(()).unwrap();
    loop {
        if let PoolEvent::Status { connected: false, .. } = next_pool_event(&mut pool_rx).await {
            break;
        }
    }
    pool.publish(event.clone()); // queued while offline

    // after reconnect: REQ first, then the queued EVENT
    let frame = tokio::time::timeout(Duration::from_secs(10), relay.rx.recv())
        .await
        .expect("reconnect timed out")
        .unwrap();
    assert_eq!(frame[0], "REQ", "resubscribe happens before flushing queue");
    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "EVENT");
    assert_eq!(frame[1]["id"], event.id.to_hex());
    pool.shutdown();
}

#[tokio::test]
async fn removed_relay_is_disconnected_and_new_relay_added() {
    let mut relay_a = spawn_mock_relay().await;
    let mut relay_b = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);

    pool.set_relays(&[relay_a.addr.clone()]);
    loop {
        if let PoolEvent::Status { connected: true, url } = next_pool_event(&mut pool_rx).await {
            assert_eq!(url, relay_a.addr);
            break;
        }
    }

    // swap relay A for relay B
    pool.set_relays(&[relay_b.addr.clone()]);
    loop {
        if let PoolEvent::Status { connected: true, url } = next_pool_event(&mut pool_rx).await {
            if url == relay_b.addr {
                break;
            }
        }
    }

    let (event, _, _) = test_event();
    pool.publish(event.clone());
    let frame = recv_frame(&mut relay_b).await;
    assert_eq!(frame[0], "EVENT");

    // relay A must not receive the publish (its socket is closed); give it
    // a moment then assert silence.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        relay_a.rx.try_recv().is_err(),
        "removed relay should receive nothing"
    );
    pool.shutdown();
}

#[tokio::test]
async fn fetch_backfill_reqs_streams_events_then_closes_on_eose() {
    let mut relay = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);
    pool.set_relays(&[relay.addr.clone()]);

    // wait for connect (so fetch sees the relay as connected)
    loop {
        if let PoolEvent::Status { connected: true, .. } = next_pool_event(&mut pool_rx).await {
            break;
        }
    }

    let (event, _, group) = test_event();
    let filter = protocol::backfill_filter(group.public_key(), event.pubkey, 3600, 100);
    let n = pool.fetch(7, filter);
    assert_eq!(n, 1, "dispatched to the one connected relay");

    // A one-shot REQ arrives on the correlated subid (distinct from the live one).
    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "REQ");
    let subid = frame[1].as_str().unwrap().to_string();
    assert_eq!(subid, "ntrack-fetch-7");
    assert_eq!(frame[2]["kinds"][0], 3434);
    assert_eq!(frame[2]["authors"][0], event.pubkey.to_hex());

    // The relay streams one stored event, then EOSE on that subid.
    relay
        .tx
        .send(serde_json::json!(["EVENT", subid, event]).to_string())
        .unwrap();
    relay
        .tx
        .send(serde_json::json!(["EOSE", subid]).to_string())
        .unwrap();

    let mut got_event = false;
    let mut got_eose = false;
    while !(got_event && got_eose) {
        match next_pool_event(&mut pool_rx).await {
            PoolEvent::FetchEvent { corr, event: got, .. } => {
                assert_eq!(corr, 7);
                assert_eq!(got.id, event.id);
                got_event = true;
            }
            PoolEvent::FetchEose { corr, .. } => {
                assert_eq!(corr, 7);
                got_eose = true;
            }
            other => panic!("unexpected pool event during backfill: {other:?}"),
        }
    }

    // After EOSE the pool closes the one-shot subscription so it never lingers.
    let frame = recv_frame(&mut relay).await;
    assert_eq!(frame[0], "CLOSE");
    assert_eq!(frame[1].as_str().unwrap(), "ntrack-fetch-7");
    pool.shutdown();
}

#[tokio::test]
async fn fetch_skips_disconnected_relays() {
    // A relay we never connect to (bogus address) must not be counted.
    let (pool_tx, _pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);
    pool.set_relays(&["ws://127.0.0.1:1".to_string()]); // refused, never connects

    let (event, _, group) = test_event();
    let filter = protocol::backfill_filter(group.public_key(), event.pubkey, 3600, 100);
    assert_eq!(pool.fetch(1, filter), 0, "no connected relays → reaches none");
    pool.shutdown();
}

#[tokio::test]
async fn publish_fans_out_to_all_relays() {
    let mut relay_a = spawn_mock_relay().await;
    let mut relay_b = spawn_mock_relay().await;
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
    let pool = RelayPool::new(pool_tx);
    pool.set_relays(&[relay_a.addr.clone(), relay_b.addr.clone()]);

    let mut connected = 0;
    while connected < 2 {
        if let PoolEvent::Status { connected: true, .. } = next_pool_event(&mut pool_rx).await {
            connected += 1;
        }
    }
    let status = pool.relay_status();
    assert_eq!(status.len(), 2);
    assert!(status.iter().all(|(_, c)| *c));

    let (event, _, _) = test_event();
    pool.publish(event.clone());
    for relay in [&mut relay_a, &mut relay_b] {
        let frame = recv_frame(relay).await;
        assert_eq!(frame[0], "EVENT");
        assert_eq!(frame[1]["id"], event.id.to_hex());
    }
    pool.shutdown();
}

// Arc is used by RelayPool::new's return type; silence unused-import lints
// in case of refactors.
#[allow(dead_code)]
fn _assert_pool_is_send_sync(p: Arc<RelayPool>) -> impl Send + Sync {
    p
}
