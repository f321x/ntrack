#!/usr/bin/env bash
# Generate the persistent release signing keystore for ntrack and print the
# GitHub Actions secrets to configure.
#
# IMPORTANT: Android requires every update of an installed app to be signed with
# the SAME key. Keep release-signing/ safe and backed up — losing the keystore
# or its password means you can never ship an upgrade to existing installs.
#
# Usage: scripts/gen-release-keystore.sh

set -euo pipefail
cd "$(dirname "$0")/.."

OUTDIR=release-signing
KEYSTORE="$OUTDIR/ntrack-release.jks"
ALIAS=ntrack

if ! command -v keytool >/dev/null 2>&1; then
    echo "error: keytool not found — install a JDK (e.g. openjdk-17-jdk-headless)," >&2
    echo "       or generate it in the builder image instead: ./build.sh keystore" >&2
    exit 1
fi

if [ -e "$KEYSTORE" ]; then
    echo "error: $KEYSTORE already exists; refusing to overwrite." >&2
    echo "       Delete it first if you really want a new key (this invalidates" >&2
    echo "       updates for anyone who installed an APK signed with the old one)." >&2
    exit 1
fi
mkdir -p "$OUTDIR"

# A random alphanumeric password (used for both the store and the key). Read a
# finite chunk and slice it: piping an infinite /dev/urandom into `head -c 32`
# makes `tr` take SIGPIPE, which under `set -o pipefail` aborts this script
# silently before keytool ever runs.
PASS="$(head -c 4096 /dev/urandom | LC_ALL=C tr -dc 'A-Za-z0-9')"
PASS="${PASS:0:32}"
if [ "${#PASS}" -ne 32 ]; then
    echo "error: could not generate a random password" >&2
    exit 1
fi

keytool -genkeypair -v \
    -keystore "$KEYSTORE" \
    -alias "$ALIAS" \
    -keyalg RSA -keysize 4096 -validity 10000 \
    -storepass "$PASS" -keypass "$PASS" \
    -dname "CN=ntrack, OU=ntrack, O=ntrack, C=US"

# base64 without line wrapping (GNU uses -w0; BSD/macOS wraps, so strip newlines).
B64="$(base64 -w0 "$KEYSTORE" 2>/dev/null || base64 "$KEYSTORE" | tr -d '\n')"
printf '%s' "$B64" > "$OUTDIR/ntrack-release.jks.base64"
{
    echo "RELEASE_KEYSTORE_PASSWORD=$PASS"
    echo "RELEASE_KEY_ALIAS=$ALIAS"
    echo "RELEASE_KEY_PASSWORD=$PASS"
} > "$OUTDIR/secrets.env"

# When generated inside the builder container (./build.sh keystore), hand the
# files back to the host user instead of leaving them root-owned.
if [ -n "${CHOWN_UID:-}" ]; then
    chown -R "${CHOWN_UID}:${CHOWN_GID:-$CHOWN_UID}" "$OUTDIR" 2>/dev/null || true
fi

cat <<EOF

================================================================================
Release keystore created: $KEYSTORE
Back up the whole $OUTDIR/ directory somewhere safe (e.g. a password manager).
It is git-ignored and must never be committed.

Set these FOUR GitHub Actions secrets — Settings > Secrets and variables >
Actions > New repository secret — or run the gh commands below:

  RELEASE_KEYSTORE_BASE64    (contents of $OUTDIR/ntrack-release.jks.base64)
  RELEASE_KEYSTORE_PASSWORD  $PASS
  RELEASE_KEY_ALIAS          $ALIAS
  RELEASE_KEY_PASSWORD       $PASS

With the GitHub CLI (run from this repo):

  gh secret set RELEASE_KEYSTORE_BASE64   < $OUTDIR/ntrack-release.jks.base64
  gh secret set RELEASE_KEYSTORE_PASSWORD --body '$PASS'
  gh secret set RELEASE_KEY_ALIAS         --body '$ALIAS'
  gh secret set RELEASE_KEY_PASSWORD      --body '$PASS'

Then cut a release by pushing a tag:  git tag v0.2.0 && git push origin v0.2.0
================================================================================
EOF
