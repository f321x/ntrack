# Releasing ntrack

CI (`.github/workflows/ci.yml`) does three things:

| Trigger | Job(s) | Result |
| --- | --- | --- |
| Push to `master` | `test` | Compile + `cargo test --workspace` + clippy |
| Pull request | `test`, `debug-apk` | The above, plus an installable **debug** APK (uploaded as a build artifact) signed with an ephemeral debug key |
| Push a tag `v*` | `test`, `release-apk` | The above, plus a **signed release** APK published on the tag's GitHub Release **and** on the [Zapstore](https://zapstore.dev) app store |

The debug APK on PRs is signed with Android's auto-generated debug keystore,
which AGP regenerates inside each CI run — nothing to configure. The **release**
APK is signed with a *persistent* key you control, stored as GitHub Actions
secrets.

## One-time setup: the release signing key

Android requires every update to an installed app to be signed with the **same**
key as the version it replaces. Generate this key once and keep it forever —
**back it up; if you lose the keystore or its password you can never ship an
update** that upgrades an existing install (users would have to uninstall first).

### 1. Generate the keystore

Run the helper (needs `keytool`, i.e. any JDK, on your machine):

```sh
scripts/gen-release-keystore.sh
```

No JDK on your host? Generate it in the builder image instead (same output,
owned by you, not root):

```sh
./build.sh keystore
```

It writes a 4096-bit RSA keystore and a random password to the git-ignored
`release-signing/` directory and prints the exact secrets to set. To do it by
hand instead:

```sh
keytool -genkeypair -v -keystore ntrack-release.jks -alias ntrack \
    -keyalg RSA -keysize 4096 -validity 10000
base64 -w0 ntrack-release.jks > ntrack-release.jks.base64
```

### 2. Store the key in GitHub  ← **the part you must do**

Add **four repository secrets** under *Settings → Secrets and variables →
Actions → New repository secret* (or use the `gh secret set` commands the helper
prints):

| Secret | Value |
| --- | --- |
| `RELEASE_KEYSTORE_BASE64` | base64 of `ntrack-release.jks` |
| `RELEASE_KEYSTORE_PASSWORD` | the keystore password |
| `RELEASE_KEY_ALIAS` | the key alias (`ntrack` if you used the helper) |
| `RELEASE_KEY_PASSWORD` | the key password |

CI decodes `RELEASE_KEYSTORE_BASE64` to a file on the runner, hands it
(read-only) to the builder container, and Gradle signs `assembleRelease` with
it. The secrets are never written into the repo and are masked in logs. The
**keystore file itself is never committed** — `release-signing/`, `*.jks`, and
`*.keystore` are git-ignored.

## Publishing to Zapstore

The tag build also publishes the **same signed APK** to the
[Zapstore](https://zapstore.dev) app store, over Nostr, using the
[`zsp`](https://github.com/zapstore/zsp) CLI. The store listing (name, summary,
description, icon, screenshots, …) lives in [`zapstore.yaml`](../zapstore.yaml)
at the repo root — edit that file to change how ntrack appears in the store.

CI publishes in *yaml-mode with a local APK*: `zapstore.yaml` is the source of
truth (`--skip-metadata` disables external enrichment) and its `release_source`
points `zsp` at the just-built `dist/ntrack-release.apk`. The exact command is:

```sh
SIGN_WITH=$ZAPSTORE_NOSTR_NSEC zsp publish zapstore.yaml --quiet --skip-metadata
```

### The one secret you must set

| Secret | Value |
| --- | --- |
| `ZAPSTORE_NOSTR_NSEC` | the `nsec…` of the Nostr key apps are published under |

`zapstore.yaml` pins the matching **`pubkey`** (npub); `zsp` refuses to publish
if `ZAPSTORE_NOSTR_NSEC` resolves to a different key, so a wrong or missing
secret fails the job loudly rather than shipping under the wrong identity. Keep
this key — Zapstore associates the app with whoever first published it. (Same
caveat as the signing keystore: back it up.)

Two things are intentionally *not* required here:

- **No icon in the APK is needed** — ntrack's launcher icon is an adaptive XML
  resource with no raster mipmap, so `zapstore.yaml` points `icon:` at the
  committed `docs/branding/` PNG instead of relying on extraction.
- **No certificate linking is required** to publish. Optionally you can later
  cryptographically link the APK's signing certificate to the Nostr identity
  with `zsp identity --link-key …` so clients can verify provenance; see the
  zsp docs.

## Cutting a release

Versioning is derived from the tag, so just tag and push:

```sh
git tag v0.2.0
git push origin v0.2.0
```

- `versionName` = the tag without the leading `v` (e.g. `0.2.0`).
- `versionCode` = the workflow run number (monotonically increasing).

The `release-apk` job builds `dist/ntrack-release.apk`, renames it to
`ntrack-v0.2.0.apk`, and attaches it to the tag's GitHub Release (created with
auto-generated notes). Re-running the job re-uploads the asset. It then
publishes the same APK to Zapstore (see above).

## Building a signed release locally

Same builder image as CI:

```sh
NTRACK_KEYSTORE=release-signing/ntrack-release.jks \
NTRACK_KEYSTORE_PASSWORD=... \
NTRACK_KEY_ALIAS=ntrack \
NTRACK_KEY_PASSWORD=... \
./build.sh release            # -> dist/ntrack-release.apk
```

Without `NTRACK_KEYSTORE`, `./build.sh release` refuses to run rather than emit
an unsigned APK.
