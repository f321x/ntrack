//! Key handling helpers.
//!
//! ntrack uses two key roles:
//!
//! * **sender key** — a dedicated keypair used only to sign kind:694 events.
//!   It MUST be distinct from the user's main Nostr identity (we never even
//!   ask for a main identity, every key in ntrack is app-generated).
//! * **recipient pseudonym key** — a keypair shared by all members of a
//!   group. Knowing the public key is enough to *send* to the group;
//!   the secret key is required to *receive* (decrypt).
//!
//! ntrack never logs `nsec` values, even in debug builds. All secrets in this
//! crate are wrapped in [`SecretString`], whose `Debug`/`Display`
//! implementations redact the value.

use nostr::hashes::{sha256, Hash};
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

// ---- identity name & color derived from a public key --------------------
//
// Senders are pseudonymous keys, so a raw npub makes a poor display name. To
// give every key a stable, human-readable identity we derive an
// "Adjective Animal" handle and an accent colour from a hash of the key. Both
// are deterministic, so two clients independently agree on the default for the
// same key; collisions are possible and acceptable (the colour disambiguates).

/// SHA-256 of a public key's lowercase hex — the per-identity seed shared by
/// the derived name and colour.
fn identity_seed(pk: &PublicKey) -> [u8; 32] {
    sha256::Hash::hash(pk.to_hex().as_bytes()).to_byte_array()
}

const NAME_ADJECTIVES: [&str; 64] = [
    "Amber", "Azure", "Bold", "Brave", "Bright", "Brisk", "Calm", "Clever", "Cobalt", "Cosmic",
    "Crimson", "Daring", "Dawn", "Deft", "Eager", "Electric", "Ember", "Fancy", "Fleet", "Fluffy",
    "Gentle", "Giddy", "Golden", "Grand", "Happy", "Hazel", "Honest", "Indigo", "Ivory", "Jade",
    "Jolly", "Keen", "Kind", "Lively", "Lucky", "Lunar", "Merry", "Mighty", "Mint", "Misty",
    "Noble", "Nimble", "Olive", "Plucky", "Proud", "Quick", "Quiet", "Royal", "Ruby", "Rustic",
    "Sandy", "Scarlet", "Sharp", "Shiny", "Silver", "Sleek", "Snowy", "Solar", "Spry", "Sunny",
    "Swift", "Teal", "Vivid", "Witty",
];

const NAME_ANIMALS: [&str; 64] = [
    "Otter", "Fox", "Falcon", "Heron", "Lynx", "Panda", "Tiger", "Eagle", "Wolf", "Bear",
    "Hawk", "Owl", "Raven", "Robin", "Sparrow", "Finch", "Crane", "Stork", "Swan", "Goose",
    "Duck", "Seal", "Whale", "Dolphin", "Orca", "Shark", "Ray", "Koi", "Carp", "Pike",
    "Bass", "Trout", "Newt", "Toad", "Frog", "Gecko", "Skink", "Viper", "Cobra", "Python",
    "Bison", "Moose", "Elk", "Deer", "Stag", "Hare", "Rabbit", "Mouse", "Vole", "Shrew",
    "Badger", "Marten", "Stoat", "Weasel", "Ferret", "Mink", "Beaver", "Marmot", "Lemur", "Macaw",
    "Parrot", "Toucan", "Magpie", "Jay",
];

/// Deterministic "Adjective Animal" display name derived from a public key,
/// used as the default identity for a (pseudonymous) sender until they pick a
/// name of their own.
pub fn derive_name(pk: &PublicKey) -> String {
    let seed = identity_seed(pk);
    let adjective = NAME_ADJECTIVES[seed[0] as usize % NAME_ADJECTIVES.len()];
    let animal = NAME_ANIMALS[seed[1] as usize % NAME_ANIMALS.len()];
    format!("{adjective} {animal}")
}

/// Deterministic accent colour (R, G, B) derived from a public key, for the
/// swatch beside each sender in the Track tab. The hue is taken from the last
/// three bytes of the key-hex SHA-256; near-black results are lifted so they
/// stay visible on the dark theme.
pub fn display_color(pk: &PublicKey) -> (u8, u8, u8) {
    let seed = identity_seed(pk);
    ensure_visible(seed[29], seed[30], seed[31])
}

/// Lift a colour whose brightest channel falls below a floor (so it does not
/// vanish against the dark UI), preserving hue by scaling all channels equally.
fn ensure_visible(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    const FLOOR: u16 = 140;
    let max = r.max(g).max(b) as u16;
    if max == 0 {
        return (FLOOR as u8, FLOOR as u8, FLOOR as u8);
    }
    if max >= FLOOR {
        return (r, g, b);
    }
    let lift = |c: u8| ((c as u16 * FLOOR) / max).min(255) as u8;
    (lift(r), lift(g), lift(b))
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
    fn derived_name_is_deterministic_and_well_formed() {
        let k = generate();
        let a = derive_name(&k.public_key());
        let b = derive_name(&k.public_key());
        assert_eq!(a, b, "same key → same name");
        // "Adjective Animal": exactly two non-empty, known words.
        let words: Vec<&str> = a.split(' ').collect();
        assert_eq!(words.len(), 2, "name is two words: {a}");
        assert!(NAME_ADJECTIVES.contains(&words[0]));
        assert!(NAME_ANIMALS.contains(&words[1]));
    }

    #[test]
    fn display_color_is_deterministic_and_visible() {
        let k = generate();
        let c1 = display_color(&k.public_key());
        let c2 = display_color(&k.public_key());
        assert_eq!(c1, c2, "same key → same colour");
        // The brightest channel always clears the visibility floor.
        assert!(c1.0.max(c1.1).max(c1.2) >= 140, "colour stays visible: {c1:?}");
    }

    #[test]
    fn ensure_visible_lifts_dark_colors_only() {
        // Already bright → untouched.
        assert_eq!(ensure_visible(200, 10, 30), (200, 10, 30));
        // Pure black → neutral grey at the floor.
        assert_eq!(ensure_visible(0, 0, 0), (140, 140, 140));
        // Dark hue → scaled up to the floor, hue (zero channels) preserved.
        let (r, g, b) = ensure_visible(10, 0, 5);
        assert_eq!(r.max(g).max(b), 140);
        assert_eq!(g, 0, "a zero channel stays zero so the hue is kept");
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
