# NIP-GART as implemented by ntrack

Canonical specification:
<https://gitea.gart.io/gart/gart-app-releases/src/branch/main/NIP-GART.md>

This document summarizes the protocol as implemented in `core/src/protocol.rs`
and maps each normative requirement to its implementation and test.

## Event

ntrack publishes and consumes Nostr **kind:694** events:

```json
{
  "kind": 694,
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
  Nostr identity, satisfying *"the event MUST be signed by a sender key,
  never by the user's main Nostr identity"*. It can be rotated in Settings.
* **Recipient pseudonym key** — a keypair shared by all members of a group.
  The public key is enough to *send* to the group; holding the secret
  (`nsec`) is required to *receive*. Key distribution is application-defined
  by the spec; ntrack shows the `nsec` as text + QR code and accepts pasted
  keys (treat the channel you use to share it as security-critical).

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
| `status` | string, REQUIRED| `"ACTIVE"` \| `"TEST"` \| `"STOP"`                 |
| `lat`    | number (WGS-84) | REQUIRED for ACTIVE/TEST, MUST be omitted for STOP |
| `lng`    | number (WGS-84) | REQUIRED for ACTIVE/TEST, MUST be omitted for STOP |
| `ts`     | unix seconds    | location capture time; same rules as `lat`/`lng`   |
| `msg`    | string, optional| MUST be omitted for STOP                           |
| `tester` | [npub], optional| TEST only; MUST NOT be present for ACTIVE          |
| `name`   | string, optional| ntrack extension: sender display name (see below)  |

A minimal STOP payload is exactly `{"status":"STOP"}` (test:
`stop_payload_serializes_minimal`). Unknown *fields* are tolerated on
receive (forward compatibility); unknown `status` values cause the event to
be dropped, as required (test: `unknown_status_is_dropped`).

### Display name (`name`) — ntrack extension

`name` is **not** part of the canonical NIP-GART payload; it is a
non-normative ntrack extension carrying the sender's self-chosen display name.

* Senders attach it to ACTIVE/TEST broadcasts (`GartPayload::with_name`) only
  when the user has set a custom name; it is omitted from STOP (kept minimal)
  and omitted entirely otherwise.
* When absent, the receiver derives a stable `Adjective Animal` handle from the
  sender key (`keys::derive_name`), so both ends agree on the default without
  it ever crossing the wire. Strict NIP-GART receivers (e.g. Gart) ignore the
  unknown field per the forward-compatibility rule above.
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

## Receiver pipeline

Implemented in `protocol::process_incoming`, exercised end-to-end in
`core/tests/relay_integration.rs`:

1. kind check (≠694 → drop)
2. **NIP-01 id + signature verification** — *"Receiver MUST verify the event
   id and signature per NIP-01 before any further processing"* (test:
   `tampered_event_fails_verification`)
3. **replay protection** — *"Receivers MUST track processed event ids"*;
   ntrack keeps a bounded (4096) id window, persisted across restarts
   (tests: `replay_is_dropped_by_event_id`, `dedup_and_eviction`)
4. ciphertext lookup for each held group key tagged in `p`
5. NIP-44 decrypt with the recipient pseudonym secret + event `pubkey`
6. payload validation per the table above (invalid → drop)
7. targeted `TEST` events (`tester` non-empty) are processed but not
   surfaced: ntrack holds no per-member identity, so a targeted test is by
   definition meant for someone else (test:
   `targeted_test_is_suppressed_untargeted_test_is_shown`)

`TEST` broadcasts are always rendered with a distinct **TEST** badge and are
never displayed like a live share (*"receivers MUST render this as a test"*).

### Export path (track backfill)

`process_for_export` runs the same verify → decrypt → validate body as
`process_incoming` (steps 1, 2, 4–7) but deliberately **omits the replay
dedup** (step 3) and never reads or writes `SeenIds`. It exists because
exporting a track re-fetches `kind:694` events the live path has already
seen; routing those through `process_incoming` would drop nearly all of them
as `Duplicate` and churn the bounded replay window. The shared body is
factored into a private `decrypt_and_validate`, so the live receiver's
behaviour is unchanged (tests: `process_for_export_decrypts_without_touching_seen`,
`process_for_export_still_verifies_and_validates`). The one-shot backfill
filter is `backfill_filter` — `{"kinds":[694], "authors":[<sender>],
"#p":[<group>], "since":…, "limit":…}`.

## Subscription

```json
{"kinds": [694], "#p": ["<recipient pubkey hex>", …], "since": <now - 6h>}
```

`since` bounds startup traffic; the replay window makes the overlap
harmless. (Test: `subscription_filter_shape`.)

## Other requirements

* **NIP-40 expiration** — senders MAY attach one; ntrack does by default
  (24 h) so location ciphertexts age out of relays (test:
  `expiration_tag_is_added_when_requested`).
* **Key rotation** — *"Implementations MUST provide a means to rotate the
  recipient pseudonym key"*: Groups → Rotate generates a fresh keypair,
  re-subscribes, and immediately offers the new key for redistribution
  (test: `rotate_group_changes_subscription_and_offers_new_key`). The UI
  prompts rotation when membership changes, per the spec's SHOULD.
* **No nsec logging** — *"Implementations MUST NOT log nsec values, even in
  debug builds"*: every secret is wrapped in `SecretString`, whose
  `Debug`/`Display` are redacted (tests:
  `secret_string_redacts_debug_and_display`,
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
* "Send test broadcast" publishes a `TEST` with the current position and no
  `tester` list (visible to every member), so the full pipeline can be
  verified before relying on it — mirroring Gart's operational-safety
  guidance.
