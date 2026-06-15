//! kind:3434 event construction, parsing and validation — the ntrack wire
//! protocol.
//!
//! Wire format recap (see docs/PROTOCOL.md):
//!
//! ```json
//! {
//!   "kind": 3434,
//!   "pubkey": "<sender-key pubkey, hex>",
//!   "created_at": 1722173222,
//!   "tags": [["p", "<recipient pseudonym pubkey, hex>"]],
//!   "content": "<NIP-44 ciphertext>",
//!   "id": "…", "sig": "…"
//! }
//! ```
//!
//! * Single recipient: `content` is the bare NIP-44 ciphertext encrypted to
//!   the recipient pseudonym key.
//! * Multiple recipients: `content` is
//!   `{"version":1,"payloads":{"<recipient hex>":"<ciphertext>", …}}` and the
//!   key set of `payloads` MUST equal the set of `p` tags.
//!
//! Decrypted plaintext payload:
//!
//! * `status` (required): `"ACTIVE" | "STOP"`
//! * `lat`, `lng`, `ts`: required for ACTIVE, MUST be omitted for STOP
//! * `msg`: optional, MUST be omitted for STOP
//! * `name`: optional sender-chosen display name (absent → the receiver
//!   derives a default handle from the sender key)

use std::collections::{BTreeMap, BTreeSet};

use nostr::nips::nip44;
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp};
use serde::{Deserialize, Serialize};

use crate::dedup::SeenIds;
use crate::error::{Error, Result};

/// Nostr event kind ntrack uses for encrypted live-location broadcasts.
pub const EVENT_KIND: u16 = 3434;

/// Broadcast status carried in the encrypted payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "ACTIVE")]
    Active,
    #[serde(rename = "STOP")]
    Stop,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Active => "ACTIVE",
            Status::Stop => "STOP",
        })
    }
}

/// Decrypted kind:3434 payload.
///
/// Unknown *additional* fields from future protocol revisions are tolerated
/// on receive; unknown `status` values cause the event to be dropped (the
/// `status` enum fails to deserialize, which callers treat as a drop).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lng: Option<f64>,
    /// Unix timestamp (seconds) at which the location was captured.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub msg: Option<String>,
    /// Sender-chosen display name. Carried on ACTIVE; omitted from STOP to keep
    /// it minimal. Receivers that don't understand it ignore it (forward
    /// compatibility) and fall back to a name derived from the sender key.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// Duress-alert marker: the unix-seconds time the sender raised the alert.
    /// Its mere presence means "this ACTIVE broadcast is an alert" — receivers
    /// escalate (loud notification, pinned card); the timestamp lets them show
    /// how long it has been up. Carried on ACTIVE only and re-sent on every
    /// broadcast while the alert is up, so the flag is sticky/level-triggered:
    /// any one received broadcast re-asserts it, surviving dropped relays.
    /// Omitted from STOP. Receivers predating the field ignore it (forward
    /// compatibility) and simply show a normal live track.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub alert: Option<u64>,
}

impl Payload {
    pub fn active(lat: f64, lng: f64, ts: u64, msg: Option<String>) -> Self {
        Self {
            status: Status::Active,
            lat: Some(lat),
            lng: Some(lng),
            ts: Some(ts),
            msg,
            name: None,
            alert: None,
        }
    }

    pub fn stop() -> Self {
        Self {
            status: Status::Stop,
            lat: None,
            lng: None,
            ts: None,
            msg: None,
            name: None,
            alert: None,
        }
    }

    /// Attach the sender's self-declared display name to an ACTIVE payload.
    /// Whitespace is trimmed and an empty name clears the field, so a blank
    /// configured name omits it from the wire entirely.
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        self
    }

    /// Mark an ACTIVE payload as a duress alert, stamping `since` (the unix
    /// seconds the alert was raised). `None` leaves it an ordinary broadcast.
    /// STOP payloads never carry it (see [`Payload::stop`] and `validate`).
    pub fn with_alert(mut self, since: Option<u64>) -> Self {
        self.alert = since;
        self
    }

    /// Enforce the payload field rules for each status.
    pub fn validate(&self) -> Result<()> {
        let invalid = |m: &str| Err(Error::InvalidPayload(m.into()));
        match self.status {
            Status::Active => {
                let (Some(lat), Some(lng), Some(_)) = (self.lat, self.lng, self.ts) else {
                    return invalid("lat, lng and ts MUST be present for ACTIVE");
                };
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                    return invalid("lat/lng out of WGS-84 range");
                }
            }
            Status::Stop => {
                if self.lat.is_some()
                    || self.lng.is_some()
                    || self.ts.is_some()
                    || self.msg.is_some()
                    || self.alert.is_some()
                {
                    return invalid("lat, lng, ts, msg and alert MUST be omitted for STOP");
                }
                // `name` describes the sender, not the broadcast, so it carries
                // no per-status rule — we simply never emit it on STOP (see
                // `stop`) and tolerate it on receive.
            }
        }
        Ok(())
    }
}

/// Multi-recipient content envelope (`content` when the event has >1 `p` tag).
#[derive(Debug, Serialize, Deserialize)]
struct MultiPayload {
    version: u32,
    /// recipient pubkey (hex) → NIP-44 ciphertext of the same plaintext
    payloads: BTreeMap<String, String>,
}

const MULTI_PAYLOAD_VERSION: u32 = 1;

/// Build and sign a kind:3434 event for the given recipients.
///
/// * `sender` MUST be a dedicated sender key (the caller guarantees it is
///   never the user's main identity — ntrack only ever generates such keys).
/// * `recipients` are the recipient pseudonym public keys (deduplicated and
///   sorted internally so event construction is deterministic).
/// * `expiration_secs`: optional NIP-40 expiration, relative to now.
pub fn build_event(
    sender: &Keys,
    recipients: &[PublicKey],
    payload: &Payload,
    expiration_secs: Option<u64>,
) -> Result<Event> {
    payload.validate()?;
    let recipients: BTreeSet<PublicKey> = recipients.iter().copied().collect();
    if recipients.is_empty() {
        return Err(Error::InvalidPayload("at least one recipient is required".into()));
    }

    let plaintext = serde_json::to_string(payload)?;
    let encrypt_to = |pk: &PublicKey| -> Result<String> {
        nip44::encrypt(sender.secret_key(), pk, &plaintext, nip44::Version::V2)
            .map_err(|e| Error::Crypto(format!("nip44 encrypt: {e}")))
    };

    let content = if recipients.len() == 1 {
        encrypt_to(recipients.iter().next().expect("non-empty"))?
    } else {
        let mut payloads = BTreeMap::new();
        for pk in &recipients {
            payloads.insert(pk.to_hex(), encrypt_to(pk)?);
        }
        serde_json::to_string(&MultiPayload { version: MULTI_PAYLOAD_VERSION, payloads })?
    };

    let mut tags: Vec<Tag> = recipients.iter().map(|pk| Tag::public_key(*pk)).collect();
    if let Some(secs) = expiration_secs {
        tags.push(Tag::expiration(Timestamp::now() + secs));
    }

    EventBuilder::new(Kind::Custom(EVENT_KIND), content)
        .tags(tags)
        .sign_with_keys(sender)
        .map_err(|e| Error::Crypto(format!("sign: {e}")))
}

/// Why an incoming event was not turned into an [`Incoming`] update.
#[derive(Debug, PartialEq, Eq)]
pub enum DropReason {
    WrongKind,
    BadSignature,
    Duplicate,
    /// None of our group keys appear in the event's `p` tags.
    NotForUs,
    /// We are tagged but no ciphertext decrypts for us.
    NoCiphertext,
    DecryptFailed,
    /// Unknown status value or malformed payload JSON.
    BadPayload,
    /// Payload violates the field rules for its status.
    InvalidPayload,
}

/// A verified, decrypted, validated incoming broadcast.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub event_id: EventId,
    /// The (pseudonymous) sender key that signed the event.
    pub sender: PublicKey,
    /// The recipient pseudonym key (group) this was decrypted with.
    pub group: PublicKey,
    /// Event timestamp (`created_at`, seconds).
    pub created_at: u64,
    pub payload: Payload,
}

/// Process a raw event received from a relay: kind check → NIP-01 id+sig
/// verification → replay dedup → ciphertext lookup → NIP-44 decrypt → payload
/// validation.
///
/// `group_keys` are the recipient pseudonym keypairs we hold (one per group
/// we can receive for). On success the matching event id is recorded in
/// `seen`.
pub fn process_incoming(
    event: &Event,
    group_keys: &[Keys],
    seen: &mut SeenIds,
) -> std::result::Result<Incoming, DropReason> {
    if event.kind != Kind::Custom(EVENT_KIND) {
        return Err(DropReason::WrongKind);
    }
    // Verify the event id and signature per NIP-01 before any further
    // processing; drop on failure.
    if event.verify().is_err() {
        return Err(DropReason::BadSignature);
    }
    // Track processed event ids to prevent relay-replay-driven duplicates.
    if seen.contains(&event.id) {
        return Err(DropReason::Duplicate);
    }

    // Everything past dedup is shared with the export path; only this live
    // path records into `seen`.
    let incoming = decrypt_and_validate(event, group_keys)?;
    seen.insert(event.id);
    Ok(incoming)
}

/// Verify and decrypt an event for *export* (track backfill), WITHOUT the
/// replay dedup that [`process_incoming`] applies.
///
/// Backfill re-fetches events the live path has already seen; routing them
/// through [`process_incoming`] would drop nearly all of them as
/// [`DropReason::Duplicate`] and would also churn the bounded replay window.
/// This bypass is load-bearing: it never reads or writes any [`SeenIds`].
pub fn process_for_export(
    event: &Event,
    group_keys: &[Keys],
) -> std::result::Result<Incoming, DropReason> {
    if event.kind != Kind::Custom(EVENT_KIND) {
        return Err(DropReason::WrongKind);
    }
    if event.verify().is_err() {
        return Err(DropReason::BadSignature);
    }
    decrypt_and_validate(event, group_keys)
}

/// Shared receiver body: ciphertext lookup → NIP-44 decrypt → payload
/// validation. Assumes the kind and signature have already been checked and
/// performs no replay dedup, so both the live ([`process_incoming`]) and
/// export ([`process_for_export`]) paths can layer their own policy around it.
fn decrypt_and_validate(
    event: &Event,
    group_keys: &[Keys],
) -> std::result::Result<Incoming, DropReason> {
    let tagged: BTreeSet<PublicKey> = event.tags.public_keys().copied().collect();
    let ours: Vec<&Keys> = group_keys
        .iter()
        .filter(|k| tagged.contains(&k.public_key()))
        .collect();
    if ours.is_empty() {
        return Err(DropReason::NotForUs);
    }

    // Locate our ciphertext: bare NIP-44 payload or multi-recipient envelope.
    // NIP-44 ciphertexts are base64 and can never start with '{'.
    let multi: Option<MultiPayload> = if event.content.trim_start().starts_with('{') {
        serde_json::from_str(&event.content).ok()
    } else {
        None
    };

    let mut decrypted: Option<(String, PublicKey)> = None;
    let mut had_ciphertext = false;
    for keys in &ours {
        let ciphertext: Option<&str> = match &multi {
            Some(m) => m.payloads.get(&keys.public_key().to_hex()).map(String::as_str),
            None => Some(event.content.as_str()),
        };
        let Some(ciphertext) = ciphertext else { continue };
        had_ciphertext = true;
        if let Ok(plain) = nip44::decrypt(keys.secret_key(), &event.pubkey, ciphertext) {
            decrypted = Some((plain, keys.public_key()));
            break;
        }
    }
    let Some((plaintext, group)) = decrypted else {
        return Err(if had_ciphertext { DropReason::DecryptFailed } else { DropReason::NoCiphertext });
    };

    // Unknown status values fail Status deserialization → drop, as required.
    let payload: Payload = serde_json::from_str(&plaintext).map_err(|_| DropReason::BadPayload)?;
    payload.validate().map_err(|_| DropReason::InvalidPayload)?;

    Ok(Incoming {
        event_id: event.id,
        sender: event.pubkey,
        group,
        created_at: event.created_at.as_secs(),
        payload,
    })
}

/// Subscription filter for all groups we can receive for:
/// `{"kinds":[3434], "#p":[<recipient pubkeys>]}` plus a `since` bound to
/// keep startup traffic sane (dedup handles overlap).
pub fn subscription_filter(group_pubkeys: &[PublicKey], since_secs_ago: u64) -> nostr::Filter {
    nostr::Filter::new()
        .kind(Kind::Custom(EVENT_KIND))
        .pubkeys(group_pubkeys.iter().copied())
        .since(Timestamp::now() - since_secs_ago)
}

/// One-shot backfill filter for exporting a single sender's track within one
/// group: `{"kinds":[3434], "authors":[<sender>], "#p":[<group>], "since":…,
/// "limit":…}`. Pinning both the `author` (the sender's signing key) and the
/// single recipient `#p` (the group) keeps the relay result set tight.
pub fn backfill_filter(
    group: PublicKey,
    sender: PublicKey,
    since_secs_ago: u64,
    limit: usize,
) -> nostr::Filter {
    nostr::Filter::new()
        .kind(Kind::Custom(EVENT_KIND))
        .author(sender)
        .pubkey(group)
        .since(Timestamp::now() - since_secs_ago)
        .limit(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::generate;

    fn seen() -> SeenIds {
        SeenIds::new(128)
    }

    #[test]
    fn active_payload_roundtrip_single_recipient() {
        let sender = generate();
        let group = generate();
        let payload = Payload::active(48.137, 11.575, 1722173222, Some("hi".into()));
        let event = build_event(&sender, &[group.public_key()], &payload, None).unwrap();

        assert_eq!(event.kind, Kind::Custom(3434));
        // single recipient → bare ciphertext, not a JSON envelope
        assert!(!event.content.starts_with('{'));
        let ptags: Vec<_> = event.tags.public_keys().collect();
        assert_eq!(ptags, vec![&group.public_key()]);

        let mut s = seen();
        let inc = process_incoming(&event, std::slice::from_ref(&group), &mut s).unwrap();
        assert_eq!(inc.sender, sender.public_key());
        assert_eq!(inc.group, group.public_key());
        assert_eq!(inc.payload, payload);
    }

    #[test]
    fn multi_recipient_envelope_has_exact_p_tag_set() {
        let sender = generate();
        let g1 = generate();
        let g2 = generate();
        let g3 = generate();
        let payload = Payload::active(1.0, 2.0, 3, None);
        let event = build_event(
            &sender,
            &[g1.public_key(), g2.public_key(), g3.public_key()],
            &payload,
            None,
        )
        .unwrap();

        // content is the versioned envelope
        let multi: MultiPayload = serde_json::from_str(&event.content).unwrap();
        assert_eq!(multi.version, 1);
        let payload_keys: BTreeSet<String> = multi.payloads.keys().cloned().collect();
        let tag_keys: BTreeSet<String> =
            event.tags.public_keys().map(|pk| pk.to_hex()).collect();
        assert_eq!(payload_keys, tag_keys, "payloads key set MUST equal p tag set");
        assert_eq!(payload_keys.len(), 3);

        // every recipient can decrypt independently
        for g in [&g1, &g2, &g3] {
            let mut s = seen();
            let inc = process_incoming(&event, std::slice::from_ref(g), &mut s).unwrap();
            assert_eq!(inc.payload, payload);
            assert_eq!(inc.group, g.public_key());
        }
    }

    #[test]
    fn duplicate_recipients_are_deduplicated() {
        let sender = generate();
        let g = generate();
        let event = build_event(
            &sender,
            &[g.public_key(), g.public_key()],
            &Payload::stop(),
            None,
        )
        .unwrap();
        assert_eq!(event.tags.public_keys().count(), 1);
        assert!(!event.content.starts_with('{'), "deduped single recipient is bare");
    }

    #[test]
    fn validation_rules() {
        // ACTIVE missing fields
        let mut p = Payload::active(0.0, 0.0, 1, None);
        p.ts = None;
        assert!(p.validate().is_err());
        // out-of-range coordinates
        assert!(Payload::active(91.0, 0.0, 1, None).validate().is_err());
        assert!(Payload::active(0.0, -180.5, 1, None).validate().is_err());
        // STOP must omit everything
        let mut p = Payload::stop();
        p.msg = Some("x".into());
        assert!(p.validate().is_err());
        let mut p = Payload::stop();
        p.lat = Some(1.0);
        assert!(p.validate().is_err());
        // minimal ACTIVE/STOP validate cleanly
        assert!(Payload::active(1.0, 2.0, 3, None).validate().is_ok());
        assert!(Payload::stop().validate().is_ok());
    }

    #[test]
    fn stop_payload_serializes_minimal() {
        let json = serde_json::to_string(&Payload::stop()).unwrap();
        assert_eq!(json, r#"{"status":"STOP"}"#);
    }

    #[test]
    fn unknown_status_is_dropped() {
        let sender = generate();
        let group = generate();
        // craft an event whose plaintext has an unknown status
        let plaintext = r#"{"status":"PANIC","lat":1.0,"lng":2.0,"ts":3}"#;
        let content = nip44::encrypt(
            sender.secret_key(),
            &group.public_key(),
            plaintext,
            nip44::Version::V2,
        )
        .unwrap();
        let event = EventBuilder::new(Kind::Custom(EVENT_KIND), content)
            .tags([Tag::public_key(group.public_key())])
            .sign_with_keys(&sender)
            .unwrap();

        let mut s = seen();
        assert_eq!(
            process_incoming(&event, &[group], &mut s).unwrap_err(),
            DropReason::BadPayload
        );
    }

    #[test]
    fn replay_is_dropped_by_event_id() {
        let sender = generate();
        let group = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(1.0, 2.0, 3, None),
            None,
        )
        .unwrap();
        let mut s = seen();
        assert!(process_incoming(&event, std::slice::from_ref(&group), &mut s).is_ok());
        assert_eq!(
            process_incoming(&event, std::slice::from_ref(&group), &mut s).unwrap_err(),
            DropReason::Duplicate
        );
    }

    #[test]
    fn tampered_event_fails_verification() {
        let sender = generate();
        let group = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(1.0, 2.0, 3, None),
            None,
        )
        .unwrap();
        let mut json: serde_json::Value = serde_json::to_value(&event).unwrap();
        json["created_at"] = serde_json::json!(event.created_at.as_secs() + 1);
        let tampered: Event = serde_json::from_value(json).unwrap();

        let mut s = seen();
        assert_eq!(
            process_incoming(&tampered, &[group], &mut s).unwrap_err(),
            DropReason::BadSignature
        );
    }

    #[test]
    fn event_for_other_group_is_not_for_us() {
        let sender = generate();
        let group = generate();
        let other = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(1.0, 2.0, 3, None),
            None,
        )
        .unwrap();
        let mut s = seen();
        assert_eq!(
            process_incoming(&event, &[other], &mut s).unwrap_err(),
            DropReason::NotForUs
        );
    }

    #[test]
    fn wrong_key_decrypt_fails() {
        let sender = generate();
        let group = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(1.0, 2.0, 3, None),
            None,
        )
        .unwrap();
        // attacker holds a different secret for the same advertised pubkey:
        // simulate by reusing the event but giving the processor keys whose
        // pubkey we forcibly "tag" via a crafted event.
        let imposter = generate();
        let crafted = EventBuilder::new(Kind::Custom(EVENT_KIND), event.content.clone())
            .tags([Tag::public_key(imposter.public_key())])
            .sign_with_keys(&sender)
            .unwrap();
        let mut s = seen();
        assert_eq!(
            process_incoming(&crafted, &[imposter], &mut s).unwrap_err(),
            DropReason::DecryptFailed
        );
    }

    #[test]
    fn expiration_tag_is_added_when_requested() {
        let sender = generate();
        let group = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::stop(),
            Some(3600),
        )
        .unwrap();
        let exp = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr::TagKind::Expiration)
            .expect("expiration tag present");
        let now = Timestamp::now().as_secs();
        let val: u64 = exp.content().unwrap().parse().unwrap();
        assert!(val >= now + 3590 && val <= now + 3610);
    }

    #[test]
    fn subscription_filter_shape() {
        let g1 = generate().public_key();
        let g2 = generate().public_key();
        let f = subscription_filter(&[g1, g2], 3600);
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["kinds"], serde_json::json!([3434]));
        let pks: BTreeSet<String> = json["#p"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(pks, BTreeSet::from([g1.to_hex(), g2.to_hex()]));
        assert!(json["since"].as_u64().unwrap() > 0);
    }

    #[test]
    fn backfill_filter_shape() {
        let group = generate().public_key();
        let sender = generate().public_key();
        let f = backfill_filter(group, sender, 3600, 5000);
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["kinds"], serde_json::json!([3434]));
        // exactly one author (the sender) and one #p (the group)
        assert_eq!(json["authors"], serde_json::json!([sender.to_hex()]));
        assert_eq!(json["#p"], serde_json::json!([group.to_hex()]));
        assert!(json["since"].as_u64().unwrap() > 0);
        assert_eq!(json["limit"].as_u64().unwrap(), 5000);
    }

    #[test]
    fn process_for_export_decrypts_without_touching_seen() {
        let sender = generate();
        let group = generate();
        let event = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(48.2, 11.6, 1000, None),
            None,
        )
        .unwrap();

        // Record the event on the live path first.
        let mut s = seen();
        let live = process_incoming(&event, std::slice::from_ref(&group), &mut s).unwrap();
        assert_eq!(live.payload.lat, Some(48.2));
        let seen_len = s.len();
        assert!(s.contains(&event.id));

        // The export path returns the same decrypted result even though the id
        // is already "seen" — and it leaves `seen` completely untouched (it
        // takes no SeenIds at all).
        let exported = process_for_export(&event, std::slice::from_ref(&group)).unwrap();
        assert_eq!(exported.payload, live.payload);
        assert_eq!(exported.sender, sender.public_key());
        assert_eq!(exported.group, group.public_key());
        assert_eq!(s.len(), seen_len, "export must not grow the replay window");
    }

    #[test]
    fn process_for_export_still_verifies_and_validates() {
        let sender = generate();
        let group = generate();
        let other = generate();
        let good = build_event(
            &sender,
            &[group.public_key()],
            &Payload::active(1.0, 2.0, 3, None),
            None,
        )
        .unwrap();
        // not tagged for us → NotForUs
        assert_eq!(
            process_for_export(&good, &[other]).unwrap_err(),
            DropReason::NotForUs
        );
        // tampered → BadSignature
        let mut json: serde_json::Value = serde_json::to_value(&good).unwrap();
        json["created_at"] = serde_json::json!(good.created_at.as_secs() + 1);
        let tampered: Event = serde_json::from_value(json).unwrap();
        assert_eq!(
            process_for_export(&tampered, &[group]).unwrap_err(),
            DropReason::BadSignature
        );
    }

    #[test]
    fn name_roundtrips_through_the_payload() {
        let sender = generate();
        let group = generate();
        let payload = Payload::active(1.0, 2.0, 3, None).with_name(Some("Anna".into()));
        assert_eq!(payload.name.as_deref(), Some("Anna"));
        let event = build_event(&sender, &[group.public_key()], &payload, None).unwrap();
        let mut s = seen();
        let inc = process_incoming(&event, std::slice::from_ref(&group), &mut s).unwrap();
        assert_eq!(inc.payload.name.as_deref(), Some("Anna"));
    }

    #[test]
    fn with_name_trims_and_drops_blank() {
        let p = Payload::active(1.0, 2.0, 3, None).with_name(Some("  Bob ".into()));
        assert_eq!(p.name.as_deref(), Some("Bob"));
        let p = Payload::active(1.0, 2.0, 3, None).with_name(Some("   ".into()));
        assert_eq!(p.name, None, "a blank name is omitted from the wire");
        // STOP stays minimal: name is never serialized.
        assert_eq!(serde_json::to_string(&Payload::stop()).unwrap(), r#"{"status":"STOP"}"#);
    }

    #[test]
    fn alert_roundtrips_and_is_omitted_on_stop() {
        let sender = generate();
        let group = generate();
        // An ACTIVE alert carries the since-timestamp end to end.
        let payload = Payload::active(1.0, 2.0, 3, None).with_alert(Some(1_700_000_000));
        assert_eq!(payload.alert, Some(1_700_000_000));
        let event = build_event(&sender, &[group.public_key()], &payload, None).unwrap();
        let mut s = seen();
        let inc = process_incoming(&event, std::slice::from_ref(&group), &mut s).unwrap();
        assert_eq!(inc.payload.alert, Some(1_700_000_000));

        // STOP must reject an alert marker and still serialize minimally.
        let mut bad = Payload::stop();
        bad.alert = Some(1_700_000_000);
        assert!(bad.validate().is_err(), "alert MUST be omitted from STOP");
        assert_eq!(
            serde_json::to_string(&Payload::stop()).unwrap(),
            r#"{"status":"STOP"}"#
        );
    }

    #[test]
    fn payload_tolerates_unknown_extra_fields() {
        let json = r#"{"status":"ACTIVE","lat":1.0,"lng":2.0,"ts":3,"battery":42}"#;
        let p: Payload = serde_json::from_str(json).unwrap();
        assert_eq!(p.status, Status::Active);
    }
}
