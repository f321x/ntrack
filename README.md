# ntrack

**Live location sharing over Nostr — end-to-end encrypted, pseudonymous, serverless.**

An Android app (Rust + [Slint](https://slint.dev) UI) that shares your live location with a
chosen group of people. Locations are NIP-44 encrypted, so relays only ever see ciphertext from a
throwaway sender key — no accounts, no central server, any Nostr relay works. The wire protocol is
a small Nostr convention built on `kind:3434` events — see [docs/PROTOCOL.md](docs/PROTOCOL.md).

## How it works

A **group** is a shared keypair. Hand its secret key (`nsec…`) to the people who should see you —
holding it lets them decrypt, the public key (`npub…`) is enough to send. Distribute keys over a
secure channel: the key *is* the membership, so rotate it whenever membership changes.

**Inviting someone** — a group's *Share key* dialog shows a QR code and a tappable
`ntrack://join?n=<name>&k=<key>` link that bundles the group **name** with its key, so the
recipient never retypes anything. They can **scan** it (Groups → *Scan QR code*), **tap** the link
(ntrack opens with the import form pre-filled), or copy/paste the string. A bare `nsec…`/`npub…`
still works too. For a member invite the link carries the secret, so share it over a secure
channel — exactly as you would the bare `nsec`.

- **Share** — publishes your encrypted location from a sender key that's never linked to a personal
  Nostr identity. Updates are velocity-based — frequent while you're moving, sparse while you're
  still — using a built-in preset (Battery saver / Normal / High) or a custom scheme you define.
- **Track** — subscribes to your groups, verifies + dedups + decrypts incoming events, and shows
  each sender live with an "open in maps" shortcut.
- **Alert** — for dangerous situations: one tap raises a duress alert (starting a share if you
  weren't already) that speeds up your updates and fires a loud, pinned notification on every group
  member's phone. Arm a **check-in** timer and ntrack escalates to that alert automatically if you
  don't confirm you're safe in time — even across a reboot.

Full protocol ↔ implementation ↔ test mapping: [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Build the APK

Only Docker is required — the Android SDK/NDK, Gradle, and Rust toolchain all live in the builder
image.

```sh
./build.sh                          # → dist/ntrack-debug.apk
adb install -r dist/ntrack-debug.apk
```

- Default ABI is `arm64-v8a`; for an emulator use `ABIS="arm64-v8a x86_64" ./build.sh`.
- The first build is slow (downloads the toolchain and compiles everything); caches make the rest
  fast.
- Podman is auto-detected if Docker is absent. Behind a TLS-inspecting proxy, drop its CA `.crt`
  into `docker/certs/` (see `docker/certs/README.md`).

### Releases

Tagged releases (`v*`) publish a signed APK two ways: to the repository's **Releases**
page on GitHub (download `ntrack-<version>.apk` and `adb install` it) and to the
[Zapstore](https://zapstore.dev) app store (install and auto-update over Nostr).
Maintainers: see [docs/RELEASING.md](docs/RELEASING.md) for the one-time signing-key
setup and how to cut a release.

## Develop

```sh
cargo test --workspace                      # protocol, engine, config, relay-pool tests
cargo clippy --workspace --all-targets
cargo run -p ntrack-app --features desktop  # run the app on desktop (simulated GPS walk)
```

The desktop build is the fastest way to iterate — it talks to real relays and is fully
interoperable with the Android build.

```
core/      ntrack-core — protocol, keys, relay pool, engine (no UI)
app/       ntrack-app  — Slint UI, controller, Android JNI glue, desktop sim
android/   Gradle project — NativeActivity shell + foreground location service (plain Java)
docker/    builder image (SDK 34, NDK r27, Gradle 8.11)
docs/      PROTOCOL.md — protocol ↔ implementation ↔ tests
```

## Security & privacy

- Locations are end-to-end encrypted (NIP-44 v2); relays see only `kind:3434` ciphertext and
  pubkeys.
- Anyone with a group's `nsec` can decrypt its past *and* future locations — **rotate the key when
  membership changes** (Groups → Rotate). The sender key is rotatable too (Settings).
- Secrets live in private app storage and are never logged. Processed event ids are persisted, so a
  malicious relay can't replay old locations.
