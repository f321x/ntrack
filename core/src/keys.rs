//! Key handling helpers.
//!
//! NIP-GART distinguishes two key roles:
//!
//! * **sender key** — a dedicated keypair used only to sign kind:694 events.
//!   It MUST be distinct from the user's main Nostr identity (we never even
//!   ask for a main identity, every key in ntrack is app-generated).
//! * **recipient pseudonym key** — a keypair shared by all members of a
//!   group. Knowing the public key is enough to *send* to the group;
//!   the secret key is required to *receive* (decrypt).
//!
//! The spec requires that implementations never log `nsec` values, even in
//! debug builds. All secrets in this crate are wrapped in [`SecretString`],
//! whose `Debug`/`Display` implementations redact the value.

use nostr::nips::nip19::{FromBech32, ToBech32};
use nostr::{Keys, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A secret string (nsec / hex secret key) that redacts itself in all
/// textual formatting. Use [`SecretString::expose`] to access the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// Access the underlying secret. Never feed the result into a logger.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Generate a fresh keypair (used for sender keys and new group keys).
pub fn generate() -> Keys {
    Keys::generate()
}

/// Parse a secret key from `nsec1…` bech32 or 64-char hex.
pub fn parse_secret(input: &str) -> Result<SecretKey> {
    let s = input.trim();
    SecretKey::from_bech32(s)
        .or_else(|_| SecretKey::from_hex(s))
        .map_err(|_| Error::InvalidKey("expected nsec1… or 64-char hex secret key".into()))
}

/// Parse a public key from `npub1…` bech32 or 64-char hex.
pub fn parse_public(input: &str) -> Result<PublicKey> {
    let s = input.trim();
    PublicKey::from_bech32(s)
        .or_else(|_| PublicKey::from_hex(s))
        .map_err(|_| Error::InvalidKey("expected npub1… or 64-char hex public key".into()))
}

/// Result of parsing arbitrary user-supplied key material for a group.
pub enum ParsedGroupKey {
    /// Full membership: secret key known, can send *and* receive.
    Member(Keys),
    /// Send-only: only the public key is known.
    SendOnly(PublicKey),
}

/// Accepts `nsec1…`, `npub1…` or hex (tried as secret first, then public).
pub fn parse_group_key(input: &str) -> Result<ParsedGroupKey> {
    let s = input.trim();
    if let Ok(sk) = parse_secret(s) {
        // Hex is ambiguous between secret and public keys; bech32 is not.
        // Treating ambiguous hex as a secret is the safer default for
        // recipient pseudonym keys, which members import as secrets.
        if s.starts_with("npub1") {
            return Ok(ParsedGroupKey::SendOnly(parse_public(s)?));
        }
        return Ok(ParsedGroupKey::Member(Keys::new(sk)));
    }
    Ok(ParsedGroupKey::SendOnly(parse_public(s)?))
}

/// Bech32 `npub1…` encoding of a public key.
pub fn npub(pk: &PublicKey) -> String {
    pk.to_bech32().expect("bech32 encoding cannot fail")
}

/// Bech32 `nsec1…` encoding of a secret key, wrapped to stay redacted.
pub fn nsec(keys: &Keys) -> SecretString {
    SecretString::new(
        keys.secret_key()
            .to_bech32()
            .expect("bech32 encoding cannot fail"),
    )
}

/// Short human-readable form of an npub, e.g. `npub1abcd…wxyz`.
pub fn short_npub(pk: &PublicKey) -> String {
    let n = npub(pk);
    if n.len() > 17 {
        format!("{}…{}", &n[..12], &n[n.len() - 5..])
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_redacts_debug_and_display() {
        let s = SecretString::new("nsec1verysecret".into());
        assert_eq!(format!("{s:?}"), "SecretString(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "nsec1verysecret");
    }

    #[test]
    fn parse_roundtrip_bech32_and_hex() {
        let keys = generate();
        let nsec_str = nsec(&keys);
        let parsed = parse_secret(nsec_str.expose()).unwrap();
        assert_eq!(Keys::new(parsed).public_key(), keys.public_key());

        let hex = keys.secret_key().to_secret_hex();
        let parsed2 = parse_secret(&hex).unwrap();
        assert_eq!(Keys::new(parsed2).public_key(), keys.public_key());

        let npub_str = npub(&keys.public_key());
        assert_eq!(parse_public(&npub_str).unwrap(), keys.public_key());
        assert_eq!(
            parse_public(&keys.public_key().to_hex()).unwrap(),
            keys.public_key()
        );
    }

    #[test]
    fn parse_group_key_variants() {
        let keys = generate();
        match parse_group_key(nsec(&keys).expose()).unwrap() {
            ParsedGroupKey::Member(k) => assert_eq!(k.public_key(), keys.public_key()),
            _ => panic!("nsec should yield Member"),
        }
        match parse_group_key(&npub(&keys.public_key())).unwrap() {
            ParsedGroupKey::SendOnly(pk) => assert_eq!(pk, keys.public_key()),
            _ => panic!("npub should yield SendOnly"),
        }
        assert!(parse_group_key("garbage").is_err());
    }
}
