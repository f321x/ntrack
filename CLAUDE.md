# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

ntrack is an Android app (Rust + [Slint](https://slint.dev) UI) for live location sharing over
Nostr, implementing the **NIP-GART** protocol (`kind:694` events, NIP-44 v2 end-to-end
encryption). The protocol is documented in `docs/PROTOCOL.md`, which maps each normative spec
requirement to its implementation and test — read it before touching `core/src/protocol.rs`.

## Commands

All day-to-day development happens on the host with plain Cargo; only the APK build needs Docker.

```sh
cargo test --workspace                      # all unit + integration tests
cargo test -p ntrack-core <name>            # a single test by name (substring match)
cargo test -p ntrack-core --test relay_integration   # the integration test file only
cargo clippy --workspace --all-targets -- -D warnings   # the CI lint gate

cargo run -p ntrack-app --features desktop  # run the full app on the desktop (simulated GPS walk)
cargo run -p ntrack-core --example genkey                # print a fresh keypair
cargo run -p ntrack-core --example mock_relay -- 127.0.0.1:7777   # in-memory dev relay

./build.sh            # build the debug APK in Docker -> dist/ntrack-debug.apk
./build.sh release    # build a SIGNED release APK -> dist/ntrack-release.apk (needs signing env; see docs/RELEASING.md)
./build.sh test       # run the Rust test suite inside the builder container
./build.sh shell      # interactive shell in the builder container
./build.sh clean      # remove (possibly root-owned) build artifacts
ABIS="arm64-v8a x86_64" ./build.sh          # build extra ABIs (x86_64 for the emulator)
SKIP_TESTS=1 ./build.sh                      # skip the test gate during an APK build
SKIP_IMAGE_BUILD=1 ./build.sh               # reuse an existing builder image (CI pre-builds it with layer caching)
```

**CI** (`.github/workflows/ci.yml`): host tests + clippy on every push to `master`
and every PR; a debug APK (ephemeral key) on PRs; a *signed* release APK attached
to the GitHub Release on every `v*` tag. Release signing secrets and the
tag→version mapping are in `docs/RELEASING.md`.

**Host build dependency:** Slint's text layer links fontconfig on Linux, so even a host
`cargo test`/`clippy` of the GUI crate needs the dev package: `libfontconfig1-dev` (Debian/Ubuntu)
/ `fontconfig-devel` (Fedora). The desktop dev build stores config under `$NTRACK_DATA` (or
`$XDG_CONFIG_HOME/ntrack`).

## Architecture

Two Cargo workspace crates plus a dependency-free Android shell:

- **`core/` (`ntrack-core`)** — UI-free, runs on any host, fully unit-testable off-device.
  Protocol, keys, dedup, persisted config, relay pool, and the engine.
- **`app/` (`ntrack-app`)** — Slint UI + the glue tying core to the OS. Builds as both a
  `cdylib` (loaded by Android) and an `rlib`/binary (desktop).
- **`android/`** — Gradle project: a thin `NativeActivity` subclass, `LocationBridge`, and a
  foreground `LocationService`, all plain framework Java with **zero dependencies**.

### The channel-driven engine (the core design)

Everything revolves around a single async task — the **`Engine`** (`core/src/engine.rs`) — that
owns the config, the share state machine, and the tracking state. It is decoupled from both UI and
OS and communicates *only* over channels:

- **In:** `EngineCmd` (StartShare, StopShare, SendTest, Location, Mutate(config), RotateGroup,
  Pool events, Tick, …).
- **Out:** `UiEvent` carrying immutable **snapshots** (`ConfigSnapshot`, `ShareSnapshot`,
  `TrackSnapshot`, Toast, `NeedLocation(bool)`, …). The engine never holds a UI reference and the
  UI never reads engine state directly — state crosses the boundary only as snapshots.

The engine is generic over an `EnginePool` trait (a super-trait of `relay::Publisher`), so the
entire relay layer is swapped for a `MockPool` in tests. Most behavior (share lifecycle, replay
dedup, STOP-on-permission-loss, key rotation, subscription updates) is covered by in-engine
`#[cfg(test)]` tests against that mock — prefer extending those over end-to-end testing.

### Threading & the UI bridge

The **`Controller`** (`app/src/controller.rs`) wires the Slint UI to the engine:

- It owns a private multi-threaded tokio runtime and spawns the engine plus forwarder tasks
  (pool→engine, engine→UI, platform→engine).
- Slint callbacks run on the **UI thread**, call `Controller` methods, and send `EngineCmd`s.
- Incoming `UiEvent`s are folded into an `Arc<Mutex<ViewState>>` and rendered back onto the UI
  thread via `Weak::upgrade_in_event_loop`. `render()` is idempotent and **UI-thread-only**; a
  1 s timer re-renders relative timestamps and expires toasts. When adding a UI interaction, wire
  the Slint callback in `Controller::attach` and route it through an `EngineCmd`.

### Platform abstraction

`app/src/platform.rs` defines the `Platform` trait — the only things the app needs from the OS
that Slint doesn't cover (location updates, runtime permission, open-in-maps, clipboard, share
sheet). Two implementations:

- `glue.rs` (`AndroidPlatform`) — JNI into the Java `LocationBridge`.
- `sim.rs` (`SimPlatform`) — desktop simulator that emits a synthetic GPS walk.

The platform pushes `PlatformEvent`s (Location, PermissionResult) back through a channel. `glue.rs`
**compiles on every target** (so host `cargo check`/`clippy` validate the JNI code without an NDK)
but can only be *constructed* on Android.

### Relay & protocol layer

`relay::RelayPool` is a minimal Nostr relay pool (publish / subscribe / reconnect) over
tokio-tungstenite + rustls. `protocol.rs` builds, signs, parses, and validates `kind:694` events;
`process_incoming` verifies the NIP-01 id + signature, dedups via `SeenIds`, and decrypts. Group
subscriptions filter on `kinds:[694]` + `#p` (recipient pseudonym pubkeys).

## Conventions & gotchas that span files

- **Never pass an Android context across JNI.** The android-activity glue only publishes the
  *Application* context, not the Activity; handing it where Java expects an Activity aborts under
  CheckJNI on real devices. `LocationBridge` methods take only primitives/strings and resolve the
  live activity Java-side via `MainActivity.current()`. (See the header comment in `glue.rs`.)
- **Feature flags are mutually exclusive backends.** `--features desktop` (winit + femtovg) vs
  `--features android` (android-activity + Skia); `default = []`. The Android build uses
  `--no-default-features --features android`, and a `compile_error!` fires if you build for the
  android target without it.
- **Secrets never get logged.** Secret keys are wrapped in `keys::SecretString`, a redacting type;
  keep them inside it. The dedicated *sender key* is never a personal Nostr identity; *group keys*
  (recipient pseudonym keys) are `nsec` to receive / `npub` to send, and both are rotatable —
  rotation is a deliberate, member-redistribution-triggering action.
- **The "debug" APK ships release-profile native libs.** `cargo-ndk` compiles the `.so` files
  (release profile — a debug Slint+Skia build is huge) into `android/app/src/main/jniLibs/<abi>/`
  *before* Gradle runs; Gradle only packages them. `ntrack-core` keeps `overflow-checks` on even in
  release. Builder image targets: SDK 34, NDK r27, Gradle 8.11, `minSdk 26`.
