# Relay hints in group invites — design

**Date:** 2026-06-15
**Status:** Approved (ready for implementation plan)

## Problem

A group invite (`ntrack://join?n=<name>&k=<key>`) carries no relay
information. A client adding a group from an invite therefore relies on its own
relay list, which may differ from the group creator's. We want invites to carry
the creator's relays so that members of a group converge on the same relays.

## Requirements (from the request)

1. Encode relay URLs in the group invite URI, **at most 3**.
2. If the sharer has more than 3 relays, share the **≤3 oldest** (relays added to
   the app first; the bundled defaults lead the list).
3. When importing a group from an invite that carries relays the user doesn't
   have yet, **add the missing relays** to the app.
4. When the user later **removes** that group, **remove** the relays that came in
   with it — but only if they aren't used by any other group, **and never reduce
   the user below 3 relays** (auto-pruning stops at a floor of 3; going lower is a
   manual action). This prevents relays accumulating across many add/remove
   cycles.
5. **Normalize** relay URLs so case-only differences don't create duplicates.

## Decisions (confirmed with the user)

- **Import surfacing:** add the missing relays **silently**, with a brief
  `"Added N relay(s) from invite"` toast.
- **Share scope:** **always** embed the oldest ≤3 relays in every invite/QR (no
  per-share opt-out).

## Current state (as explored)

- **URI build/parse:** `core/src/invite.rs`. `build_invite(name, key)` produces
  `ntrack://join?n=<percent-encoded name>&k=<bech32 key>`. `parse_invite` /
  `parse_shared` return `Invite { name: Option<String>, key: String }`. The
  scheme/host are case-insensitive; a trailing `#fragment` is stripped.
  Round-trip tests live in the same file; QR round-trip tests in
  `core/src/qr.rs` (which calls `build_invite` at `qr.rs:86`).
- **Relays:** `Config.relays: Vec<String>` (`core/src/config.rs`), **insertion
  order = age**, seeded by `default_relays()` (damus.io, nos.lol, offchain.pub) —
  so the "oldest ≤3" is simply the first 3 entries. `normalize_relay_url`
  (`core/src/relay.rs`) trims, maps `http(s)`→`ws(s)`, defaults bare hosts to
  `wss://`, strips a trailing slash, validates — but does **not** lowercase, so
  case-only duplicates currently slip past the `contains()` dedup in
  `on_add_relay` (`app/src/controller.rs`).
- **Groups:** `Config.groups: Vec<Group>` where
  `Group { name, public, secret: Option<SecretString>, selected }`. Import flows
  through a review screen (`import_group`, `controller.rs:~615`), which already
  calls `invite::parse_shared` on the key field; removal is a confirmed
  `groups.retain(|g| g.public != id)` mutate. One global `RelayPool` serves all
  groups; subscriptions filter on member group pubkeys.
- **Mutation/persistence:** all config edits go through `EngineCmd::Mutate`,
  which runs the closure, persists (atomic write), re-syncs the pool, and emits a
  config snapshot to the UI.

## Design

### Data model (`core/src/config.rs`)

Two new fields, both `#[serde(default)]` for backward-compat with existing
on-disk configs:

- `Config.auto_relays: Vec<String>` — the **eligibility set**: relays that were
  auto-added by an invite import and may be auto-pruned later. Default and
  manually-added relays are never in this set, so they are never auto-removed.
  Maintained in lockstep with `relays` (added and removed together).
- `Group.relays: Vec<String>` — **provenance**: the normalized (≤3) relay list
  the group's invite carried. Used on removal to know what the group brought and
  to detect relays still referenced by surviving groups.

`Invite` (`core/src/invite.rs`) gains `relays: Vec<String>`.

**Why both fields and not a refcount:** `Group.relays` is required regardless (a
removed group must know which relays it contributed). Given that, "is this relay
still referenced?" is derivable from the surviving groups' lists, so the only
extra state needed is a flag distinguishing auto-added from user-owned relays —
the `auto_relays` set. A separate refcount would be redundant state that can
desync.

### URI format (`core/src/invite.rs`)

```
ntrack://join?n=<name>&k=<key>&r=<relay>&r=<relay>&r=<relay>
```

- One repeated `r=` param per relay, at most 3.
- Each relay value percent-encoded with a URL-readable `AsciiSet` that keeps
  `: / . - _ ~` and encodes the query-breaking bytes (`& = # %`, space, etc.):
  e.g. `&r=wss://relay.damus.io`.
- **Build:** `build_invite(name, key, relays: &[String])` appends an `r=` for
  each of up to 3 relays.
- **Parse:** collect **all** `r=` values, percent-decode, `normalize_relay_url`
  each, dedup, and **defensively cap at 3** (a hostile/oversized URI cannot
  inject many relays). Populate `Invite.relays`.
- **Backward compat:** an invite without any `r=` parses to `relays: []`.

### Normalization (`core/src/relay.rs`)

Extend `normalize_relay_url` to **lowercase the scheme and authority
(`host[:port]`)** while preserving path/query case. This is what makes
case-only differences dedup (`wss://Relay.Damus.io` → `wss://relay.damus.io`).
The scheme is already emitted lowercase; the change is to lowercase the authority
portion of the remainder (split at the first `/`). Existing normalization tests
remain valid; this also tightens the existing manual-add dedup and the pool's
per-URL `HashMap` keys.

### Build path (sharing)

`build_invite` takes the relays slice. Callers pass the first 3 of
`config.relays` (oldest):

- Share dialog (`app/src/controller.rs:~256`).
- QR builder (`core/src/qr.rs:86`) — its caller threads the relay slice in.

Relays are always embedded.

### Import path (silent add + toast)

New pure, unit-testable `Config` method:

- `add_relays_from_invite(&[String]) -> (Vec<String>, usize)` — normalize +
  dedup + cap-at-3 the input; append the genuinely-new ones to both `relays` and
  `auto_relays`; return `(full_normalized_list, num_newly_added)`. The full list
  (including relays already present) is what gets stored on the group for
  provenance.

In `import_group`:

- The mutate closure calls `add_relays_from_invite`, sets
  `group.relays = full_normalized_list`, and pushes the group.
- The controller computes `num_newly_added` from its current config snapshot and
  shows a `"Added N relay(s) from invite"` toast when N > 0 (best-effort count;
  the engine mutate remains the authoritative, idempotent add).
- Relay provenance reaches `import_group` two ways, unioned and deduped:
  - the QR/deep-link pre-fill path stashes the parsed `Invite` (incl. relays) as
    a pending import;
  - a full `ntrack://` URI pasted into the key field is parsed in-place via
    `parse_shared`, whose `Invite.relays` are honored.

### Remove path (prune)

New `Config` method:

- `prune_relays_for_removed_group(&[String] /* removed group's relays */)` —
  called **after** the group has been removed from `self.groups`. For each relay
  R in the removed group's list, prune R iff **all** hold:
  1. R ∈ `auto_relays` (auto-added, not user-owned),
  2. R ∉ any surviving group's `relays` (not still referenced), and
  3. removing R keeps `relays.len() >= 3` (floor).
  Pruned relays are removed from both `relays` and `auto_relays`.

The remove mutate closure becomes: find the group by `public`, remove it,
capture its `relays`, then call `prune_relays_for_removed_group`.

Manual relay management keeps `auto_relays` honest:

- `on_add_relay` removes the URL from `auto_relays` if present (the user now owns
  it → protected from future auto-prune).
- `on_remove_relay` also drops the URL from `auto_relays`.

## Edge cases

- An invite relay that is already a default/manual relay is not re-added and not
  placed in `auto_relays`, so it survives the group's removal.
- A relay carried by two imported groups survives removal of the first (still in
  the second group's `relays`).
- The min-3 floor: pruning stops once `relays.len()` would drop to 3; it never
  auto-removes below 3. If a user is already at ≤3, nothing is auto-pruned.
- Invalid relay URLs in an invite are skipped silently during parse/normalize.

## Testing

- `core/src/invite.rs`: build+parse round-trip with relays; multiple `r=`
  collection; cap-at-3 on parse; normalize+dedup on parse; old (no `r=`) URI →
  empty relays.
- `core/src/relay.rs`: case-insensitive normalization (mixed-case host →
  lowercase; path case preserved).
- `core/src/config.rs` (pure helpers) and `core/src/engine.rs` (against the mock
  pool): import adds only genuinely-new relays and reports the right count;
  removal prunes an auto-added relay; removal keeps a relay used by another
  group; removal keeps a default relay; min-3 floor respected; manual-add
  protects a relay from later auto-prune.
- `core/src/qr.rs`: QR build→scan→parse round-trip carrying relays.

## Files touched

- `core/src/invite.rs` — `Invite.relays`; `build_invite` arg; `r=` build +
  parse (+ cap-at-3, normalize, dedup); tests.
- `core/src/config.rs` — `Config.auto_relays`; `Group.relays`;
  `add_relays_from_invite`; `prune_relays_for_removed_group`; tests.
- `core/src/relay.rs` — lowercase scheme/authority in `normalize_relay_url`;
  test.
- `core/src/qr.rs` — thread relays into the QR invite build; test.
- `app/src/controller.rs` — pass oldest-3 relays to `build_invite`; import
  threading + toast; remove closure prune; `auto_relays` upkeep in manual
  add/remove.

No UI-layout changes; no Android/Java changes.

## Post-review refinements

An adversarial review of the implementation added three refinements beyond the
original design:

1. **Re-scan convergence.** Importing an invite for an already-imported group no
   longer just rejects with "already imported" and drops the invite's relays.
   `Config::merge_group_relays(public, invite_relays)` merges any new relays
   (auto-added, unioned into the group's provenance list) so members re-scanning
   an updated invite converge on relays the group later added. The toast becomes
   `"Group already imported · N relay(s) added"` when N > 0.
2. **Load-time normalization migration.** `ConfigStore::load` calls
   `Config::normalize_relays`, collapsing legacy mixed-case relay entries (from
   configs written before relay URLs were case-normalized) so they don't linger
   as duplicates. Keeps `auto_relays ⊆ relays` and normalizes each group's
   provenance list.
3. **Correct host case-folding.** `normalize_relay_url` lowercases the host with
   Unicode case folding (`to_lowercase`, so non-ASCII hosts dedup) and leaves any
   `user:pass@` userinfo verbatim (userinfo is case-sensitive per RFC 3986). IDN
   ↔ punycode unification is not attempted (relays use ASCII/punycode hosts).

The toast relay count remains a best-effort estimate from the config snapshot
(the engine's `add_imported_group`/`merge_group_relays` return values are
authoritative); it can over-count only if two imports race within a single
config-snapshot round-trip, which is unreachable via the UI.
