# The ntrack protocol

ntrack shares live location over Nostr as end-to-end-encrypted **kind:3434**
events. This document specifies the wire format ntrack implements and maps each
rule to its implementation and test in `core/src/protocol.rs`.

## Event

ntrack publishes and consumes Nostr **kind:3434** events:

```json
{
  "kind": 3434,
  "pubkey": "<sender-key pubkey, hex>",
  "created_at": 1722173222,
  "tags": [["p", "<recipient pseudonym pubkey, hex>"]],
  "content": "<NIP-44 ciphertext>",
  "id": "…",
  "sig": "…"
}
```

Roles:

* **Sender key** — a dedicated keypair that signs broadcasts. ntrack
  generates it on first use and never asks for (or supports) a personal
  Nostr identity: an event MUST be signed by a throwaway sender key, never by
  the user's main Nostr identity. It can be rotated in Settings.
* **Recipient pseudonym key** — a keypair shared by all members of a group.
  The public key is enough to *send* to the group; holding the secret
  (`nsec`) is required to *receive*. Key distribution is out of band; ntrack
  shows the `nsec` as text + QR code and accepts pasted keys (treat the
  channel you use to share it as security-critical).

## Content encryption

* Exactly one `p` tag → `content` is the bare NIP-44 (v2) ciphertext of the
  payload, encrypted from the sender key to the recipient pseudonym key.
* Multiple `p` tags → `content` is a JSON envelope; the same plaintext is
  independently encrypted per recipient:

```json
{
  "version": 1,
  "payloads": {
    "<recipient pubkey hex>": "<nip44 ciphertext>",
    "<recipient pubkey hex>": "<nip44 ciphertext>"
  }
}
```

The key set of `payloads` MUST equal the `p`-tag set — enforced on send
(recipients are deduplicated; tags and map are generated from the same set)
and covered by `multi_recipient_envelope_has_exact_p_tag_set`.

On receive, ntrack is liberal: it accepts a bare ciphertext or an envelope
(distinguished by the leading `{`, which base64 NIP-44 payloads can never
start with) and only requires that *its own* entry decrypts.

## Plaintext payload

| field    | type            | rules                                              |
|----------|-----------------|----------------------------------------------------|
| `status` | string, REQUIRED| `"ACTIVE"` \| `"STOP"`                             |
| `lat`    | number (WGS-84) | REQUIRED for ACTIVE, MUST be omitted for STOP      |
| `lng`    | number (WGS-84) | REQUIRED for ACTIVE, MUST be omitted for STOP      |
| `ts`     | unix seconds    | location capture time; same rules as `lat`/`lng`   |
| `msg`    | string, optional| MUST be omitted for STOP                           |
| `name`   | string, optional| sender display name (see below)                    |
| `alert`  | unix seconds, optional | duress-alert marker (see below); MUST be omitted for STOP |

A minimal STOP payload is exactly `{"status":"STOP"}` (test:
`stop_payload_serializes_minimal`). Unknown *fields* are tolerated on
receive (forward compatibility); unknown `status` values cause the event to
be dropped (test: `unknown_status_is_dropped`).

### Display name (`name`)

`name` carries the sender's self-chosen display name.

* Senders attach it to ACTIVE broadcasts (`Payload::with_name`) only when the
  user has set a custom name; it is omitted from STOP (kept minimal) and
  omitted entirely otherwise.
* When absent, the receiver derives a stable `Adjective Animal` handle from the
  sender key (`keys::derive_name`), so both ends agree on the default without
  it ever crossing the wire. Receivers that don't recognise the field ignore
  it per the forward-compatibility rule above.
* On receive the name is sanitized and length-capped before display, retained
  across a STOP, and overridden by any local label the user set. A per-key
  colour (`keys::display_color`) shown beside each card disambiguates the
  collisions that duplicate names can produce.

Tests: `name_roundtrips_through_the_payload`, `with_name_trims_and_drops_blank`
(protocol); `incoming_declared_name_and_color_surface_in_snapshot`,
`incoming_without_name_falls_back_to_derived_handle`,
`stop_retains_last_declared_name`, `outgoing_active_carries_configured_name_trimmed`,
`outgoing_active_omits_blank_name` (engine);
`derived_name_is_deterministic_and_well_formed`,
`display_color_is_deterministic_and_visible` (keys).

### Duress alert (`alert`)

`alert` escalates an ACTIVE broadcast into a duress alert. Its value is the
unix-seconds time the sender raised it; mere presence is the signal, and the
timestamp lets receivers show how long it has been up.

* It is **sticky / level-triggered**: the sender stamps it on *every* ACTIVE
  while the alert is up — re-broadcast every cycle even without a fresh fix — so
  any single broadcast that reaches a receiver re-asserts the alert, surviving
  dropped relays. It MUST be omitted from STOP, which clears the alert (STOP
  stays minimal).
* Receivers escalate on the no-alert→alert *edge* (one high-urgency
  notification per transition, never one per heartbeat) and pin the sender's
  track to the top, styled as an alert.
* Receivers predating the field ignore it (forward compatibility, like any
  unknown field) and just show a normal live track — an un-updated peer still
  sees the location, never a dropped event. This is the reason the alert is an
  additive field and **not** a new `status` value, which older receivers would
  drop (`unknown_status_is_dropped`).

Tests: `alert_roundtrips_and_is_omitted_on_stop` (protocol);
`raising_alert_publishes_immediately_marked_and_boosts_cadence`,
`alert_heartbeat_rebroadcasts_a_stale_fix`,
`clearing_alert_drops_the_marker_and_relaxes_cadence`,
`incoming_alert_notifies_once_and_pins_the_track`,
`alerting_track_pins_above_a_newer_normal_track`,
`panic_force_starts_share_and_alerts` (engine).

## Receiver pipeline

Implemented in `protocol::process_incoming`, exercised end-to-end in
`core/tests/relay_integration.rs`:

1. kind check (≠3434 → drop)
2. **NIP-01 id + signature verification** — the receiver MUST verify the event
   id and signature per NIP-01 before any further processing (test:
   `tampered_event_fails_verification`)
3. **replay protection** — receivers MUST track processed event ids; ntrack
   keeps a bounded (4096) id window, persisted across restarts (tests:
   `replay_is_dropped_by_event_id`, `dedup_and_eviction`)
4. ciphertext lookup for each held group key tagged in `p`
5. NIP-44 decrypt with the recipient pseudonym secret + event `pubkey`
6. payload validation per the table above (invalid → drop)

### Dedup-free decrypt path (export & startup replay)

`verify_and_decrypt` runs the same verify → decrypt → validate body as
`process_incoming` (steps 1, 2, 4–6) but deliberately **omits the replay
dedup** (step 3) and never reads or writes `SeenIds`. Two callers need an
already-seen event decrypted rather than dropped:

* **track export** re-fetches `kind:3434` events the live path has already
  seen, via the one-shot `backfill_filter` — `{"kinds":[3434],
  "authors":[<sender>], "#p":[<group>], "since":…, "limit":…}`; and
* the **live receiver itself**, which re-folds the relay's `since` replay
  (below) into the in-memory track display. That display — unlike the
  persisted `SeenIds` — does not survive a restart, so without this the Track
  tab would stay empty after relaunch even though the relay re-delivers your
  (and your peers') recent sessions.

Routing either through `process_incoming` would drop the events as `Duplicate`
and churn the bounded replay window. The shared body is factored into a
private `decrypt_and_validate`, so the live receiver's verify/decrypt
behaviour is unchanged (tests: `verify_and_decrypt_decrypts_without_touching_seen`,
`verify_and_decrypt_still_verifies_and_validates`). The replay rebuild folds
for **display only** — it never re-fires the one-shot alert notification and is
skipped when the display already holds an equal-or-newer point, so a
steady-state duplicate from a second relay stays a no-op (tests:
`replayed_seen_event_repopulates_the_track_after_restart`,
`duplicate_does_not_regress_a_newer_displayed_point`,
`replayed_alert_rebuilds_track_without_renotifying`).

## Subscription

```json
{"kinds": [3434], "#p": ["<recipient pubkey hex>", …], "since": <now - 24h>}
```

`since` bounds startup traffic while still recovering each peer's last-known
location: it matches the 24 h NIP-40 expiration below, so any event still alive
on a relay (a peer's most recent fix, even if they haven't published in hours)
is fetched, and anything older has aged out anyway. Because the persisted
replay window has usually already seen these events, the dedup suppresses their
*side effects* (no duplicate alert notifications), but the live receiver still
re-folds them into the in-memory track display (see the dedup-free path above)
— which does not survive a restart — so a peer's, or your own, last session
resurfaces on launch instead of vanishing. (Tests: `subscription_filter_shape`,
`replayed_seen_event_repopulates_the_track_after_restart`.)

## Other requirements

* **NIP-40 expiration** — senders MAY attach one; ntrack does by default
  (24 h) so location ciphertexts age out of relays (test:
  `expiration_tag_is_added_when_requested`).
* **Key rotation** — Groups → Rotate generates a fresh keypair, re-subscribes,
  and immediately offers the new key for redistribution (test:
  `rotate_group_changes_subscription_and_offers_new_key`). The UI prompts
  rotation when membership changes.
* **No nsec logging** — secrets are never logged, even in debug builds: every
  secret is wrapped in `SecretString`, whose `Debug`/`Display` are redacted
  (tests: `secret_string_redacts_debug_and_display`,
  `config_json_never_contains_plain_marker_but_keeps_secret`).

## ntrack's sending behaviour

* Live sharing publishes `ACTIVE` events at the configured interval
  (15 s – 5 min) with the latest GPS fix; `ts` is the fix time, `created_at`
  the publish time. The GPS is sampled at that same cadence — never faster,
  since powering the radio is the dominant battery cost — and each fix is
  broadcast at most once: if the GPS stalls and yields no new position,
  ntrack stays quiet rather than re-sending a stale point.
* Stopping a share (including when location becomes unavailable, and on
  app shutdown, best effort) publishes a `STOP` so receivers don't show a
  stale live state.
* A **duress alert** — raised from the Share screen, or by a one-tap panic that
  force-starts the share first — marks each `ACTIVE` with `alert` and overrides
  the configured interval with a fast fixed cadence (15 s), re-broadcasting the
  last-known fix every cycle even if the GPS has stalled: being found quickly
  outweighs battery in an emergency. Raising or clearing it re-publishes
  immediately so the change reaches the group without waiting for the next tick.
* A **check-in** (dead-man's switch) is a local timer, not a wire concept: if
  the user doesn't confirm safety before it elapses, ntrack escalates to the
  same alert+share. Confirming safety re-arms it for another full period — it
  repeats until explicitly disarmed — with a reminder notification posted once
  when 10% of the period remains. It is persisted, so a deadline that lapses
  while the app is down is re-evaluated at startup — granting a brief grace
  window (with a notification) before firing, rather than a false alarm on a
  phone that merely ran out of battery.
* ntrack originates only `ACTIVE` and `STOP` events. The first `ACTIVE` goes
  out as soon as sharing starts.
