---
name: rust-slint-android-app
description: >-
  Build or extend a Rust-based Android app with a Slint UI, using the ntrack
  repository as the worked reference. Use when scaffolding such an app from
  scratch (Cargo workspace, cargo-ndk + Gradle build, JNI bridge to a Java
  NativeActivity shell) or when adding features to ntrack and you need the
  cross-language patterns: the UI-free core crate, the channel-driven engine,
  the Slint property/callback/model contract, the Platform trait, the
  foreground-service / boot-resume path, and the Docker build pipeline. Triggers
  on Rust + Slint + Android, cargo-ndk, cdylib for Android, android_main, JNI
  glue, NativeActivity, or "ship a Rust GUI as an APK".
---

# Rust + Slint Android app (ntrack reference)

This skill captures the architecture that lets a single Rust codebase run as a
desktop binary **and** ship as an Android APK with a native Slint UI, with the
OS-specific bits isolated behind a thin Java shell and a JNI bridge. ntrack
(this repo) is the worked example; every pattern below points at concrete files
you can read and copy.

**Read `CLAUDE.md` first** — it is the canonical map of the repo. This skill is
the *how to build one like it* / *how the cross-language machinery fits together*
companion. When a fact here disagrees with the code, the code wins; update this
file.

## When to use this

- Bootstrapping a new Rust+Slint+Android app and you want a proven layout
  instead of rediscovering the cargo-ndk / JNI / Gradle integration by trial.
- Adding a feature to ntrack that crosses the UI↔engine↔platform boundaries
  (a new screen, a new platform capability, a new persisted setting).
- Debugging the Android-only failure modes (CheckJNI aborts, `FindClass`
  failures on worker threads, foreground-service crashes, missing `.so`).

If the change is pure protocol/engine logic with no UI or OS surface, you
usually don't need this — work in `core/` and its `#[cfg(test)]` tests directly.

## The three layers (and why)

```
core/      ntrack-core  — UI-free, OS-free Rust. Protocol, keys, config, relay
                          pool, the engine. Runs and unit-tests on any host.
app/       ntrack-app   — Slint UI + glue. Builds as BOTH a cdylib (Android
                          loads it) and an rlib/binary (desktop dev build).
android/   Gradle project — a thin NativeActivity Java shell with ZERO
                          dependencies: loads the .so and forwards OS callbacks.
```

The discipline that makes this work: **all real logic lives in `core/`, behind
channels, with no `slint`/`jni`/`android` imports.** That is what keeps the bulk
of the app unit-testable off-device (`cargo test --workspace` with no emulator).
The `app/` crate is glue; the `android/` project is a shell. Resist putting
logic in the glue or the shell.

References: workspace `Cargo.toml`, `core/src/lib.rs`, `app/src/lib.rs`.

## 1. The Cargo workspace & crate types

Workspace root `Cargo.toml` declares both crates and a release profile tuned for
a small native lib (the `.so` ships in every APK):

```toml
[workspace]
resolver = "2"
members = ["core", "app"]

[profile.release]          # native libs in the APK use this profile
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"
```

The app crate is the linchpin — it is compiled two ways:

```toml
# app/Cargo.toml
[lib]
name = "ntrack_app"              # MUST match AndroidManifest android.app.lib_name
crate-type = ["cdylib", "rlib"]  # cdylib -> Android .so; rlib -> desktop binary

[[bin]]
name = "ntrack-desktop"
path = "src/main.rs"
required-features = ["desktop"]
```

**Feature flags select mutually exclusive Slint backends** (`default = []`, so a
bare `cargo build` picks neither and fails by design — you must choose):

```toml
[features]
default = []
desktop = ["slint/backend-winit", "slint/renderer-femtovg",
           "slint/renderer-software", "slint/accessibility"]
android = ["slint/backend-android-activity-06"]   # renders with Skia
```

A guard in `app/src/lib.rs` turns a misconfigured Android build into a clear
compile error instead of a baffling link failure:

```rust
#[cfg(all(target_os = "android", not(feature = "android")))]
compile_error!("Android builds need --no-default-features --features android");
```

**Crypto-provider gotcha:** ntrack pins `default-features = false` + the `ring`
provider on every TLS dependency (relay layer in `core`, map-tile fetch in
`app`) so the whole app has *one* crypto backend. rustls 0.23's default
`aws_lc_rs` pulls in C code that complicates the Android cross-build — avoid
adding a second crypto/cross-language dependency. See the comments on the
`rustls`/`tokio-rustls` deps.

## 2. The Slint UI layer

- `app/build.rs` compiles the `.slint` file at build time:
  ```rust
  let cfg = slint_build::CompilerConfiguration::new().with_style("material".into());
  slint_build::compile_with_config("ui/app.slint", cfg).expect("slint compile");
  ```
  (`material` scales well on Android and supports a dark scheme.)
- `app/src/lib.rs` pulls the generated code in with `slint::include_modules!();`
  — that macro is what surfaces `MainWindow`, the `struct`s, the property
  setters and the callback hooks to Rust.
- `slint` dep: `default-features = false` + `["std", "compat-1-2"]`. The backend
  comes from the feature flags above, never from slint's defaults.

**The Rust↔UI contract is entirely data-in / callbacks-out** (see the header
comment in `app/ui/app.slint` and the `MainWindow` component near the bottom):

- `export struct …` (e.g. `GroupItem`, `TrackItem`, `MapTile`) become Rust
  structs of the same name, imported via the `include_modules!` glob in
  `controller.rs` (`use crate::{GroupItem, TrackItem, …}`).
- `in property <…>` → Rust calls `ui.set_<name>(value)`. `in-out property` is
  two-way (e.g. an import form an incoming invite can pre-fill).
- `callback name(args)` → Rust registers a handler with `ui.on_<name>(closure)`.
- Lists cross as models: build a `Vec<ItemStruct>`, wrap it
  `slint::ModelRc::new(slint::VecModel::from(v))`, hand to `ui.set_…`. See
  `Controller::render` building `tracks`/`map_tiles`.

When you add an interaction: declare the `callback` in `app.slint`, wire it in
`Controller::attach`, and route it through an `EngineCmd` (don't mutate state in
the callback). When you add displayed data: add an `in property` (and/or a
`struct` field), then set it in `Controller::render`.

## 3. The channel-driven engine (the core design)

One async task — the **`Engine`** (`core/src/engine.rs`) — owns *all* mutable
app state (config, the share state machine, tracking). It never holds a UI or OS
handle. Everything crosses by channel:

- **In:** `EngineCmd` (e.g. `StartShare`, `StopShare`, `Mutate(Box<dyn FnOnce(&mut Config)>)`,
  `Location(sample)`, `Pool(event)`, `Tick`, `Shutdown`).
- **Out:** `UiEvent` carrying immutable **snapshots** (`ConfigSnapshot`,
  `ShareSnapshot`, `Tracks(Vec<TrackSnapshot>)`, …) plus side-effect requests
  (`NeedLocation(bool)`, `SetLocationInterval`, `Notify`, `Toast`).

State only ever leaves the engine as a snapshot; the UI never reads engine state
directly. This is what keeps rendering a pure fold of immutable data.

**Testability via a trait + mock:** the engine is generic over an `EnginePool`
trait (super-trait of `relay::Publisher`). Production uses `RelayPool`; tests
swap a `MockPool` and assert on published events / subscriptions with zero
network. Most behavior (share lifecycle, dedup, key rotation, alert/check-in
escalation, resume-after-reboot) is covered by in-engine `#[cfg(test)]` tests
against the mock — **prefer extending those over end-to-end tests.** See
`core/src/engine.rs` (`pub trait EnginePool`, `mod tests { struct MockPool … }`)
and `core/src/relay.rs` (`pub trait Publisher`).

When you add engine behavior: add an `EngineCmd` variant (or `UiEvent`), handle
it in the engine's `select!`/match loop, and add a mock-pool test next to the
others.

## 4. Threading & the UI bridge (`Controller`)

`app/src/controller.rs` wires Slint ↔ engine ↔ platform. The rules that keep it
from crashing:

- It owns a **private multi-threaded tokio runtime** and spawns forwarder tasks:
  pool→engine, engine→UI, platform→engine.
- **Runtime ownership:** the `Controller` is `Arc`-shared and its tasks hold
  clones, so it must NOT own the `Runtime` (the last clone could drop it from a
  worker thread → "Cannot drop a runtime from within an async context" panic on
  Activity recreation). `run_app` (`app/src/lib.rs`) owns the `Runtime`; the
  `Controller` holds only a `Handle`. Tear the runtime down with
  `rt.shutdown_background()` from the non-worker thread after `ui.run()` returns.
- **UI-thread discipline:** Slint callbacks run on the UI thread and call
  `Controller` methods that send `EngineCmd`s. Incoming `UiEvent`s are folded
  into an `Arc<Mutex<ViewState>>` on a worker thread, then rendered back via
  `self.ui.upgrade_in_event_loop(move |ui| ctrl.render(&ui))`. `render()` is
  idempotent and **UI-thread-only**.
- A 1 s `slint::Timer` re-renders relative timestamps and expires toasts.

The `attach` method uses a small `hook!` macro to register every callback; copy
that shape when adding one.

## 5. The Platform abstraction

`app/src/platform.rs` is the *entire* OS surface the app needs beyond what Slint
covers — a single trait:

```rust
pub trait Platform: Send + Sync + 'static {
    fn has_location_permission(&self) -> bool;
    fn request_location_permission(&self);
    fn start_location(&self, interval_ms: u64);
    fn set_location_interval(&self, interval_ms: u64);   // re-tune in place
    fn stop_location(&self);
    fn open_map(&self, lat: f64, lng: f64, label: &str);
    fn notify_alert(&self, title: &str, body: &str);
    fn copy_text(&self, text: &str);
    fn paste_text(&self) -> String;
    fn share_text(&self, text: &str);
    fn share_file(&self, filename: &str, mime: &str, content: &[u8], prefer_view: bool);
    fn scan_qr(&self);
}
```

Two implementations, selected at the entry point:

- `app/src/glue.rs` — `AndroidPlatform`, JNI into Java. Compiles on **every**
  target (so host `cargo check`/`clippy` validate the JNI without an NDK) but is
  only *constructed* on Android.
- `app/src/sim.rs` — `SimPlatform`, a desktop simulator emitting a synthetic GPS
  walk, so you can run the whole app on the desktop.

Events flow back via an `mpsc` channel of `PlatformEvent` (`Location`,
`PermissionResult`, `IncomingInvite`). When you add a capability: add a trait
method, implement it in both `glue.rs` and `sim.rs`, and (if it produces async
results) add a `PlatformEvent` variant + a `native…` JNI callback.

## 6. The Android / JNI integration (the hard part)

This is where Android-specific knowledge is non-obvious. Study `app/src/glue.rs`
alongside `android/app/src/main/java/io/ntrack/app/`.

### Golden rules (each one prevents a real crash)

1. **Never pass an Android `Context` across JNI.** The android-activity glue
   publishes the **Application** context (`ndk_context::android_context().context()`),
   not the Activity. Handing it where Java expects an `Activity` *aborts under
   CheckJNI on real devices*. The bridge methods take only primitives/strings and
   resolve the live Activity **on the Java side** (`MainActivity.current()`). The
   Application context is used in Rust exactly once, at init, to reach the class
   loader.
2. **App classes are invisible to `FindClass` on native (tokio worker) threads.**
   To get your bridge class, go through the Application context's class loader:
   `context.getClassLoader().loadClass("io.ntrack.app.LocationBridge")` (see
   `AndroidPlatform::new`). Inside a Java-originated callback thread (e.g. the
   boot service path, `new_for_service`) `FindClass` *does* resolve app classes —
   two construction paths exist for exactly this reason.
3. **Register native callbacks explicitly.** `register_bridge_natives` calls
   `env.register_native_methods(class, &[NativeMethod { name, sig, fn_ptr }, …])`,
   binding Java `static native` methods to `extern "system" fn`s. The JNI
   signature strings (`"(DDFJ)V"`, `"([BIII)Ljava/lang/String;"`, …) must match
   the Java declarations exactly. Boot-service entry points instead use the
   `Java_io_ntrack_app_LocationService_nativeServiceStart` name-mangling
   convention (no registration needed — bound by symbol name at
   `System.loadLibrary`).
4. **Attach worker threads before JNI calls, and swallow errors.** Platform
   calls are fire-and-forget: `with_env` does `vm.attach_current_thread()`, runs
   the closure, logs + clears any pending Java exception, returns `Option`.
5. **Guard the event sink for swap.** A single `static RwLock<Option<Sender>>`
   holds the Java→Rust event sink so the UI engine and the headless boot engine
   can hand it off without a callback reading it mid-swap.

### The Java shell (plain framework APIs, zero deps)

- `MainActivity` extends `NativeActivity`: declares the lib via
  `<meta-data android:name="android.app.lib_name" android:value="ntrack_app"/>`,
  forwards `onRequestPermissionsResult` and deep-link `VIEW` intents to a static
  `LocationBridge`, exposes `current()`, and sets edge-to-edge layout (Slint
  reads window insets and exposes them as `safe-area-insets`).
- `LocationBridge` owns `LocationManager`, the runtime-permission flow, clipboard,
  share sheet, notifications — all `static` methods taking primitives, calling
  back via the registered `native…` methods. Resolve the live Activity through
  `MainActivity.current()` (fall back to the running `LocationService` when no
  Activity exists).
- `LocationService` is a `foregroundServiceType="location"` service: a keep-alive
  shell while the UI is open, and the **headless host** on boot (it
  `System.loadLibrary("ntrack_app")` and calls `nativeServiceStart`).
- `BootReceiver` (on `BOOT_COMPLETED`) starts the service only if a non-secret
  sentinel file (`resume.flag`/`checkin.flag`) says a share/check-in was armed —
  it never parses the config (which holds secrets).
- `AndroidManifest.xml`: `singleTask`, a broad `configChanges` set (so the
  Activity isn't recreated on rotation/locale), the `location`/`FOREGROUND_SERVICE`
  /`RECEIVE_BOOT_COMPLETED`/`POST_NOTIFICATIONS` permissions, and the
  `ntrack://join` deep-link intent filter.

### The headless boot engine (resume with no UI)

After a reboot there is no Activity, so the *same* `ntrack_core` engine is spun
up inside the foreground service (`app/src/headless.rs`), wired straight to the
platform, dropping every `UiEvent` except `NeedLocation`/`SetLocationInterval`/
`Notify`. **Exactly one engine may own the persisted config and publish at a
time** — when the user opens the app, `android_main` calls
`headless::claim_for_ui()` to tear the boot host down first; the share is handed
over via the persisted resume flag (`Controller::resume_if_armed`). If you add
background behavior, preserve this single-owner invariant.

### Android lifecycle gotcha

`set_location_interval` exists because a stop+restart of location would race
Android's `startForegroundService()`→`startForeground()` contract (crashing the
process) and flicker the OS location indicator. **Re-tune a running session in
place; never bounce the foreground service to change cadence.**

## 7. The build pipeline (cargo-ndk → Gradle → Docker)

The APK build is two stages, orchestrated by `scripts/build-apk.sh` inside the
`docker/Dockerfile` builder image (driven by `./build.sh`):

1. **cargo-ndk compiles the Rust `.so` BEFORE Gradle runs**, into
   `android/app/src/main/jniLibs/<abi>/`. Gradle only *packages* what's already
   there:
   ```sh
   cargo ndk -t arm64-v8a --platform 26 -o android/app/src/main/jniLibs \
       build -p ntrack-app --lib --release --no-default-features --features android
   ```
2. **Gradle assembles the APK** (`gradle assembleDebug` / `assembleRelease`).
   `android/app/build.gradle` has `dependencies { }` empty on purpose.

Non-obvious build facts (all encoded in `scripts/build-apk.sh` / the Dockerfile):

- **The "debug" APK ships release-profile `.so` files** — a debug Slint+Skia
  build is huge and slow. The APK variant (`debuggable true`, ephemeral debug
  signing) is independent of the Rust profile (always `--release`).
- **Bundle `libc++_shared.so`** into each `jniLibs/<abi>/` — Skia links the
  shared C++ STL. Copy it from the NDK sysroot per ABI.
- **Override `ANDROID_JAR`** to the installed platform jar so Slint's
  android-activity backend can compile its small build-time Java helper (cargo-ndk
  exports `ANDROID_PLATFORM=<minSdk>`, which would point at a non-existent
  `platforms/android-26`).
- Builder image pins: SDK platform 34, **NDK r27**, Gradle 8.11, cargo-ndk 4.1,
  `minSdk 26`, JDK 17, Android targets `aarch64`/`armv7`/`x86_64-linux-android`.
- Multi-ABI: `ABIS="arm64-v8a x86_64" ./build.sh` (x86_64 for the emulator).

CI (`.github/workflows/ci.yml`): host `cargo test` + `cargo clippy … -D warnings`
on every push/PR; a debug APK on PRs; a signed release APK on `v*` tags. The
host job needs `libfontconfig1-dev` (Slint's text layer links fontconfig on
Linux even for `cargo test`/`clippy` of the GUI crate).

## 8. Testing & local iteration

```sh
cargo test --workspace                                 # all unit + integration tests
cargo test -p ntrack-core <name>                       # one test by substring
cargo clippy --workspace --all-targets -- -D warnings  # the CI lint gate
cargo run -p ntrack-app --features desktop             # full app on desktop (sim GPS)
./build.sh                                              # debug APK in Docker
SKIP_TESTS=1 ./build.sh                                 # skip the test gate
```

- The desktop build (`SimPlatform`) is the fastest UI iteration loop and is
  wire-compatible with the Android build (talks to real relays).
- Prefer in-engine `#[cfg(test)]` mock-pool tests for behavior; reserve the
  Docker APK build for verifying packaging/JNI/manifest changes.
- `cargo run -p ntrack-core --example mock_relay -- 127.0.0.1:7777` gives an
  in-memory dev relay; `--example genkey` prints a keypair.

## 9. Gotcha checklist (skim before an Android change)

- [ ] No `Context` passed across JNI; Activity resolved via `MainActivity.current()`.
- [ ] App classes loaded via the app class loader on worker threads (not `FindClass`).
- [ ] `[lib] name` == manifest `android.app.lib_name`; `crate-type` has `cdylib`.
- [ ] Built with `--no-default-features --features android` (the `compile_error!` guard).
- [ ] JNI method signature strings match the Java `static native` declarations.
- [ ] `libc++_shared.so` bundled per ABI; `ANDROID_JAR` set.
- [ ] tokio `Runtime` owned by `run_app`, not the `Arc`'d `Controller`.
- [ ] `render()` only on the UI thread (`upgrade_in_event_loop`).
- [ ] Location cadence changed via `set_location_interval`, never stop+start.
- [ ] Exactly one engine owns the config/publishes (UI vs headless handoff).
- [ ] Secrets stay in `keys::SecretString` (redacting `Debug`/`Display`); never logged.
- [ ] New manifest permission added when you reach a new OS capability.

## 10. Bootstrapping a new app — suggested order

1. Workspace `Cargo.toml` + an empty `core` crate (pure Rust, no UI). Get
   `cargo test` green.
2. Model state + `EngineCmd`/`UiEvent` + the `Engine` task in `core`, with a
   `Publisher`/pool trait and a `MockPool` test. This is the bulk of the app.
3. `app` crate: `[lib] crate-type = ["cdylib","rlib"]`, the `desktop`/`android`
   features, `build.rs`, a minimal `app.slint` (`MainWindow` + a couple
   properties/callbacks), `slint::include_modules!()`.
4. `Platform` trait + `SimPlatform` + `Controller` + `run_app` + `src/main.rs`.
   Run on desktop (`--features desktop`) until the loop works end-to-end.
5. `android/` Gradle shell: `NativeActivity` subclass, manifest with the
   `lib_name` meta-data, `LocationBridge`, foreground service, boot receiver.
6. `glue.rs` (`AndroidPlatform`) + `android_main` + the `compile_error!` guard.
7. The build pipeline: Dockerfile (SDK/NDK/Gradle/Rust+targets/cargo-ndk),
   `scripts/build-apk.sh` (cargo-ndk → libc++_shared → Gradle), `build.sh`.
8. `headless.rs` + the boot path last, once foreground sharing works.

Keep logic flowing *down* into `core/` as you go; the moment you find yourself
writing real behavior in `glue.rs`, `controller.rs`, or Java, ask whether it
belongs in the engine instead.
