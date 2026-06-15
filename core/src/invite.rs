//! App-level group invite format.
//!
//! A group invite bundles the group's *name* with its recipient pseudonym
//! key into one self-describing string so the importing user no longer has to
//! type the name by hand. The same string is what the QR code encodes, what
//! the "Copy"/"Share" buttons emit, and what a tapped `ntrack://` deep link
//! carries — one artifact for all three sharing paths.
//!
//! Wire form: `ntrack://join?n=<percent-encoded name>&k=<bech32 key>&r=<relay>…`
//!
//! * `k` is today's shared key, unchanged: an `nsec1…` for full members or an
//!   `npub1…` for send-only groups. It is bech32 (URL-safe), so it is never
//!   percent-encoded.
//! * `n` is optional and percent-encoded (names may contain spaces, `&`,
//!   emoji, …).
//! * `r` is an optional relay hint, repeated once per relay (at most
//!   [`MAX_INVITE_RELAYS`]) — the sharer's oldest relays, so importers converge
//!   on the same relays. Values are percent-encoded (URL-safe characters kept
//!   readable); on parse they are normalized, deduped and capped.
//!
//! This is purely an app convenience layer; it is **not** part of the wire
//! protocol and never appears in a kind:694 event. Importing also accepts a
//! bare `nsec1…`/`npub1…`/hex string (see [`parse_shared`]).

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::keys;

/// URI scheme + host that identify an ntrack group invite.
const PREFIX: &str = "ntrack://join";

/// Encode everything that is not an ASCII alphanumeric. This keeps the name
/// unambiguous inside the query string (spaces become `%20`, `&`→`%26`,
/// `=`→`%3D`, multibyte UTF-8 → `%XX%XX…`).
const NAME_SET: &AsciiSet = NON_ALPHANUMERIC;

/// Encode a relay URL inside an `r=` query value. Keep the URL-safe characters
/// that occur in `wss://host:port/path` readable (`:`, `/`, `.`, `-`, `_`, `~`)
/// and encode everything else — in particular the query delimiters `&`, `=`,
/// `#` and any whitespace — so the value can't break out of its parameter.
const RELAY_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b':')
    .remove(b'/')
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

/// Maximum number of relay hints carried by an invite. Bounds the URI/QR size
/// and caps how many relays a single invite can add to (or imply for) a client.
pub const MAX_INVITE_RELAYS: usize = 3;

/// A parsed invite: the (optional) group name and the bech32 key string.
///
/// The key is kept as the raw string the user will import; validation into a
/// [`keys::ParsedGroupKey`] happens at import time, exactly as for a manually
/// pasted key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub name: Option<String>,
    pub key: String,
    /// Relay hints carried by the invite (normalized, deduped, at most
    /// [`MAX_INVITE_RELAYS`]). Empty for a bare key or an older invite without
    /// `r=` parameters.
    pub relays: Vec<String>,
}

/// Build an `ntrack://join` invite URI from a group name, bech32 key and up to
/// [`MAX_INVITE_RELAYS`] relay hints (extra relays are ignored).
///
/// An empty (or whitespace-only) name is omitted entirely.
pub fn build_invite(name: &str, key: &str, relays: &[String]) -> String {
    let name = name.trim();
    let mut uri = String::from(PREFIX);
    uri.push('?');
    if !name.is_empty() {
        uri.push_str("n=");
        uri.extend(utf8_percent_encode(name, NAME_SET));
        uri.push('&');
    }
    uri.push_str("k=");
    uri.push_str(key);
    for relay in relays.iter().take(MAX_INVITE_RELAYS) {
        uri.push_str("&r=");
        uri.extend(utf8_percent_encode(relay, RELAY_SET));
    }
    uri
}

/// Parse an `ntrack://join?…` invite URI. Returns `None` for anything that is
/// not an ntrack invite or that lacks a key.
pub fn parse_invite(s: &str) -> Option<Invite> {
    let s = s.trim();
    // The scheme and host are case-insensitive (per RFC 3986); the query is
    // not. ASCII-lowercasing preserves byte offsets, so the prefix length is
    // the same in both strings.
    if !s.to_ascii_lowercase().starts_with(PREFIX) {
        return None;
    }
    let rest = &s[PREFIX.len()..];
    // A '#fragment' terminates the query (RFC 3986); drop it before parsing so
    // a mangled or shortener-appended fragment can't corrupt the last value.
    let rest = rest.split('#').next().unwrap_or(rest);
    // Accept an optional path slash, then require a query string.
    let query = rest.trim_start_matches('/').strip_prefix('?')?;

    let mut name = None;
    let mut key = None;
    let mut relays: Vec<String> = Vec::new();
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "n" => name = decode(v),
            "k" => key = decode(v),
            "r" => {
                if let Some(r) = decode(v) {
                    relays.push(r);
                }
            }
            _ => {}
        }
    }
    // Normalize + dedup defends against case-only duplicates and bad URLs; the
    // cap bounds how many relays a single invite can imply (a hostile or
    // oversized URI can't inject an unbounded list).
    let relays = crate::relay::normalize_dedup(&relays)
        .into_iter()
        .take(MAX_INVITE_RELAYS)
        .collect();
    Some(Invite { name, key: key.filter(|k| !k.is_empty())?, relays })
}

/// Parse an arbitrary shared string into an [`Invite`]: either an
/// `ntrack://join` URI (carrying the name) or a bare `nsec1…`/`npub1…`/hex key
/// (no name). Returns `None` if the string is neither.
pub fn parse_shared(s: &str) -> Option<Invite> {
    if let Some(invite) = parse_invite(s) {
        return Some(invite);
    }
    let s = s.trim();
    // Backward compatibility: a bare key shared the old way.
    if keys::parse_group_key(s).is_ok() {
        return Some(Invite { name: None, key: s.to_string(), relays: Vec::new() });
    }
    None
}

/// Percent-decode a query value as UTF-8, returning `None` on malformed input.
fn decode(v: &str) -> Option<String> {
    percent_decode_str(v).decode_utf8().ok().map(|s| s.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_nsec() -> String {
        keys::nsec(&keys::generate()).expose().to_string()
    }

    #[test]
    fn build_then_parse_roundtrip() {
        let key = member_nsec();
        let uri = build_invite("Family", &key, &[]);
        assert!(uri.starts_with("ntrack://join?"), "got {uri}");
        let inv = parse_invite(&uri).expect("should parse");
        assert_eq!(inv.name.as_deref(), Some("Family"));
        assert_eq!(inv.key, key);
    }

    #[test]
    fn name_with_special_chars_roundtrips_exactly() {
        let key = member_nsec();
        let name = "Mom & Dad's 🚗 trip = fun";
        let uri = build_invite(name, &key, &[]);
        // The raw URI must not contain unencoded separators from the name.
        assert!(!uri.contains("Dad's"), "name must be percent-encoded: {uri}");
        let inv = parse_invite(&uri).expect("should parse");
        assert_eq!(inv.name.as_deref(), Some(name));
        assert_eq!(inv.key, key);
    }

    #[test]
    fn empty_name_is_omitted_and_parses_to_none() {
        let key = member_nsec();
        let uri = build_invite("   ", &key, &[]);
        assert_eq!(uri, format!("ntrack://join?k={key}"));
        let inv = parse_invite(&uri).expect("should parse");
        assert_eq!(inv.name, None);
        assert_eq!(inv.key, key);
    }

    #[test]
    fn parse_invite_rejects_foreign_or_keyless() {
        let key = member_nsec();
        assert!(parse_invite(&format!("https://join?k={key}")).is_none());
        assert!(parse_invite("nostr:npub1xxx").is_none());
        assert!(parse_invite(&format!("ntrack://other?k={key}")).is_none());
        // No key → not a usable invite.
        assert!(parse_invite("ntrack://join?n=Family").is_none());
        assert!(parse_invite("ntrack://join").is_none());
    }

    #[test]
    fn parse_invite_strips_fragment() {
        // RFC 3986: a '#fragment' terminates the query. A link-shortener or
        // messenger may append one; it must not corrupt the key/name.
        let key = member_nsec();
        let inv = parse_invite(&format!("ntrack://join?k={key}#section")).expect("parse");
        assert_eq!(inv.key, key);

        let inv2 = parse_invite(&format!("ntrack://join?n=Trip&k={key}#x")).expect("parse");
        assert_eq!(inv2.name.as_deref(), Some("Trip"));
        assert_eq!(inv2.key, key);
    }

    #[test]
    fn parse_invite_rejects_empty_key() {
        assert!(parse_invite("ntrack://join?k=").is_none());
        assert!(parse_invite("ntrack://join?n=Family&k=").is_none());
    }

    #[test]
    fn scheme_and_host_are_case_insensitive() {
        let key = member_nsec();
        let inv = parse_invite(&format!("NTRACK://JOIN?k={key}")).expect("should parse");
        assert_eq!(inv.key, key);
    }

    #[test]
    fn parse_shared_accepts_bare_nsec_and_npub() {
        let k = keys::generate();
        let nsec = keys::nsec(&k).expose().to_string();
        let npub = keys::npub(&k.public_key());

        let a = parse_shared(&nsec).expect("bare nsec");
        assert_eq!(a, Invite { name: None, key: nsec, relays: vec![] });

        let b = parse_shared(&npub).expect("bare npub");
        assert_eq!(b, Invite { name: None, key: npub, relays: vec![] });
    }

    #[test]
    fn parse_shared_accepts_invite_uri() {
        let key = member_nsec();
        let uri = build_invite("Hike", &key, &[]);
        let inv = parse_shared(&uri).expect("uri");
        assert_eq!(inv.name.as_deref(), Some("Hike"));
        assert_eq!(inv.key, key);
    }

    #[test]
    fn build_with_relays_roundtrips() {
        let key = member_nsec();
        let relays = vec!["wss://relay.damus.io".to_string(), "wss://nos.lol".to_string()];
        let uri = build_invite("Family", &key, &relays);
        let inv = parse_invite(&uri).expect("parse");
        assert_eq!(inv.name.as_deref(), Some("Family"));
        assert_eq!(inv.key, key);
        assert_eq!(inv.relays, relays);
    }

    #[test]
    fn build_without_relays_has_no_r_param_and_parses_empty() {
        let key = member_nsec();
        let uri = build_invite("Family", &key, &[]);
        assert!(!uri.contains("&r="), "no relays → no r= param: {uri}");
        let inv = parse_invite(&uri).expect("parse");
        assert!(inv.relays.is_empty());
    }

    #[test]
    fn build_caps_relays_at_three() {
        let key = member_nsec();
        let relays: Vec<String> = (0..5).map(|i| format!("wss://r{i}.example")).collect();
        let uri = build_invite("X", &key, &relays);
        assert_eq!(uri.matches("&r=").count(), 3, "uri: {uri}");
    }

    #[test]
    fn parse_collects_relays_normalized_and_deduped() {
        let key = member_nsec();
        // A case-only duplicate and a bare host that normalizes to an existing
        // entry must both collapse.
        let uri = format!(
            "ntrack://join?k={key}&r=WSS://Relay.Damus.IO&r=wss://relay.damus.io&r=nos.lol"
        );
        let inv = parse_invite(&uri).expect("parse");
        assert_eq!(
            inv.relays,
            vec!["wss://relay.damus.io".to_string(), "wss://nos.lol".to_string()]
        );
    }

    #[test]
    fn parse_caps_relays_at_three() {
        let key = member_nsec();
        let uri = format!(
            "ntrack://join?k={key}&r=wss://a.example&r=wss://b.example&r=wss://c.example&r=wss://d.example"
        );
        let inv = parse_invite(&uri).expect("parse");
        assert_eq!(
            inv.relays,
            vec![
                "wss://a.example".to_string(),
                "wss://b.example".to_string(),
                "wss://c.example".to_string(),
            ]
        );
    }

    #[test]
    fn parse_relay_value_with_path_roundtrips() {
        let key = member_nsec();
        let relays = vec!["wss://relay.example.com/Nostr".to_string()];
        let uri = build_invite("P", &key, &relays);
        let inv = parse_invite(&uri).expect("parse");
        assert_eq!(inv.relays, relays);
    }

    #[test]
    fn parse_shared_rejects_garbage() {
        assert!(parse_shared("hello world").is_none());
        assert!(parse_shared("").is_none());
        assert!(parse_shared("ntrack://join?n=x").is_none());
    }
}
