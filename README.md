# ntrack

**Live location sharing over Nostr — end-to-end encrypted, pseudonymous,
serverless.** Compatible with the [Gart app](https://gitea.gart.io/gart/gart-app-releases)
via the [NIP-GART protocol](https://gitea.gart.io/gart/gart-app-releases/src/branch/main/NIP-GART.md)
(Nostr kind:694).

ntrack is an Android app written in **Rust** with the **Slint** UI toolkit.
Your location is encrypted with NIP-44 to *group keys* you share with the
people who should see you. Relays only ever see ciphertext published by a
random-looking, dedicated sender key — no accounts, no server of ours, any
Nostr relay works.

## How it works

* A **group** is a shared keypair (the protocol's *recipient pseudonym
  key*). Everyone holding its secret (`nsec…`) can decrypt locations sent to
  the group; knowing only the public key (`npub…`) is enough to send.
* Create a group in the app and hand the key to your people (QR code, copy,
  or the system share sheet — use a secure channel; the key *is* the
  membership). Import a key someone gives you to track them.
* **Share** publishes encrypted `ACTIVE` location events at a configurable
  interval through your configured relays, from a dedicated sender key that
  is never linked to any personal Nostr identity. Stopping publishes `STOP`.
* **Track** subscribes to your groups (`kinds:[694]`, `#p` filter), verifies
  every event (NIP-01 id + signature), deduplicates replays, decrypts, and
  shows each sender live — with an "open in maps" shortcut.
* **Test broadcasts** (`TEST`) verify the whole pipeline — relays,
  encryption, your group members' devices — without starting a real share.
  They are always rendered as tests, never as live shares.

Protocol details and the mapping of every normative spec requirement to
implementation + test: [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Building the APK (Fedora or any Linux with Docker)

The entire toolchain (Android SDK/NDK, Gradle, Rust, cargo-ndk) lives in a
Docker image — nothing Android-related is needed on the host.

```sh
sudo dnf install -y docker          # or: moby-engine / podman
sudo systemctl enable --now docker

./build.sh                          # → dist/ntrack-debug.apk
adb install -r dist/ntrack-debug.apk
```

Notes:

* Bind mounts use the `:z` SELinux label, so it works out of the box on
  Fedora. Podman is auto-detected if Docker is absent (`DOCKER=podman` to
  force).
* The first build downloads the toolchain image layers and compiles the full
  Rust dependency tree (Slint downloads a prebuilt Skia for Android) —
  expect a long first run. Cargo/Gradle caches persist in named volumes and
  `.docker-target/`, so subsequent builds are fast.
* Default ABI is `arm64-v8a` (every modern phone). For an emulator or extra
  targets: `ABIS="arm64-v8a x86_64" ./build.sh`.
* Behind a TLS-inspecting (corporate) proxy? Drop the proxy's CA certificate
  as a `.crt` PEM into `docker/certs/` before building — it is installed
  into the image's system and Java trust stores (see
  `docker/certs/README.md`).
* `./build.sh test` runs the test suite in the container; `SKIP_TESTS=1
  ./build.sh` skips the test gate during an APK build. `./build.sh clean`
  removes (possibly root-owned) build artifacts.

## Development

```sh
cargo test --workspace        # protocol, engine, config, relay-pool tests
                              # (includes an in-process mock-relay suite)
cargo clippy --workspace --all-targets

# run the full app on the desktop with a simulated GPS walk:
cargo run -p ntrack-app --features desktop
```

The desktop build is the fastest way to iterate on UI and engine: it speaks
to real relays and is fully interoperable with the Android build (and Gart).

### Repository layout

```
core/      ntrack-core — NIP-GART protocol, keys, relay pool, engines (no UI)
app/       ntrack-app  — Slint UI, controller, Android JNI glue, desktop sim
android/   Gradle project: NativeActivity shell, LocationBridge, foreground
           service (plain framework Java, zero dependencies)
docker/    builder image (SDK 34, NDK r27, Gradle 8.11, stable Rust)
scripts/   build-apk.sh (runs inside the container)
docs/      PROTOCOL.md — spec ↔ implementation ↔ tests
```

## Security & privacy notes

* Locations are end-to-end encrypted (NIP-44 v2); relays see only kind:694
  ciphertext, recipient pseudonym keys, and a dedicated sender key.
* Anyone with a group's `nsec` can decrypt **past and future** broadcasts to
  that group: distribute keys over secure channels and **rotate the key
  whenever membership changes** (Groups → Rotate; the spec requires this
  capability and ntrack makes it one tap).
* The sender key can also be rotated (Settings) to unlink future shares from
  past ones.
* Secrets are stored in the app's private storage and are never logged
  (enforced by a redacting wrapper type, as the spec demands). Android
  Keystore-backed encryption at rest is a possible future hardening.
* Replay protection: processed event ids are tracked and persisted, so a
  malicious relay can't re-trigger old locations or alerts.
