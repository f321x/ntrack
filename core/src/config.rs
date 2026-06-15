//! Persisted application state: sender key, groups, relays, settings and
//! the replay-protection tail.
//!
//! Stored as JSON in the app's private data directory. Writes are atomic
//! (temp file + rename). Secrets are stored as bech32 strings wrapped in
//! [`SecretString`] so they can never leak through logging.

use std::path::{Path, PathBuf};

use nostr::{Keys, PublicKey};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::keys::{self, SecretString};

/// One sharing group, identified by its recipient pseudonym key.
///
/// * `secret` present → full member: can send and receive.
/// * `secret` absent → send-only: we know the public key but cannot decrypt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Group {
    pub name: String,
    /// Recipient pseudonym public key, hex.
    pub public: String,
    /// Recipient pseudonym secret (nsec bech32), if we are a member.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub secret: Option<SecretString>,
    /// Selected for sharing on the Share screen.
    #[serde(default = "default_true")]
    pub selected: bool,
    /// Relays this group's invite carried (normalized, deduped, at most
    /// [`crate::invite::MAX_INVITE_RELAYS`]). Records provenance so the relays a
    /// group brought in can be pruned when it is removed; empty for groups not
    /// imported from a relay-bearing invite.
    #[serde(default)]
    pub relays: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Group {
    pub fn new_member(name: String) -> Result<Self> {
        let k = keys::generate();
        Ok(Self {
            name,
            public: k.public_key().to_hex(),
            secret: Some(keys::nsec(&k)),
            selected: true,
            relays: Vec::new(),
        })
    }

    pub fn public_key(&self) -> Result<PublicKey> {
        keys::parse_public(&self.public)
    }

    pub fn member_keys(&self) -> Option<Keys> {
        let sk = keys::parse_secret(self.secret.as_ref()?.expose()).ok()?;
        Some(Keys::new(sk))
    }

    /// Rotate the recipient pseudonym key in place (NIP-GART requires that
    /// implementations provide a rotation mechanism). The new secret must be
    /// redistributed to members out of band.
    pub fn rotate(&mut self) {
        let k = keys::generate();
        self.public = k.public_key().to_hex();
        self.secret = Some(keys::nsec(&k));
    }
}

/// Human-assigned label for a (pseudonymous) sender key seen while tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SenderLabel {
    pub pubkey: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Dedicated sender key (nsec). Generated on first share; never a main
    /// Nostr identity (ntrack has no concept of one).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sender_secret: Option<SecretString>,
    /// The user's chosen display name, broadcast with each location so members
    /// see it in their Track tab. Empty → receivers derive a default handle
    /// from the sender key (see [`crate::keys::derive_name`]).
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    /// Relays that were auto-added by importing a relay-bearing invite and are
    /// therefore eligible for auto-removal when the groups that brought them are
    /// removed. Default and manually-added relays are never listed here, so they
    /// are never auto-removed. Kept in lockstep with `relays`.
    #[serde(default)]
    pub auto_relays: Vec<String>,
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Seconds between location publishes while sharing.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Attach a NIP-40 expiration tag to outgoing events.
    #[serde(default = "default_true")]
    pub use_expiration: bool,
    #[serde(default)]
    pub sender_labels: Vec<SenderLabel>,
    /// Persisted replay-protection tail (processed event ids, hex).
    #[serde(default)]
    pub processed_ids: Vec<String>,
    /// True while a share is active and the user has not explicitly stopped
    /// it. Persisted so a reboot/crash can offer to resume. Set on
    /// `start_share`; cleared only on an explicit stop — never by the
    /// best-effort STOP emitted at process shutdown (that *is* the
    /// reboot-while-sharing case we want to resume from).
    #[serde(default)]
    pub resume_share: bool,
    /// Message attached to the share, restored on resume.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resume_msg: Option<String>,
}

/// Floor on the relay count for *automatic* removal. Auto-pruning never takes a
/// user below this many relays; going lower is a deliberate manual action.
pub const MIN_RELAYS: usize = 3;

/// Normalize, dedup and cap a set of invite relay hints (the shared front door
/// for both fresh imports and re-scan merges).
fn normalize_invite_relays(urls: &[String]) -> Vec<String> {
    crate::relay::normalize_dedup(urls)
        .into_iter()
        .take(crate::invite::MAX_INVITE_RELAYS)
        .collect()
}

pub fn default_relays() -> Vec<String> {
    vec![
        "wss://relay.damus.io".into(),
        "wss://nos.lol".into(),
        "wss://offchain.pub".into(),
    ]
}

fn default_interval() -> u64 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sender_secret: None,
            display_name: String::new(),
            relays: default_relays(),
            auto_relays: Vec::new(),
            groups: Vec::new(),
            interval_secs: default_interval(),
            use_expiration: true,
            sender_labels: Vec::new(),
            processed_ids: Vec::new(),
            resume_share: false,
            resume_msg: None,
        }
    }
}

impl Config {
    /// Sender keys, generating and recording a dedicated key on first use.
    pub fn sender_keys(&mut self) -> Result<Keys> {
        if let Some(secret) = &self.sender_secret {
            let sk = keys::parse_secret(secret.expose())?;
            return Ok(Keys::new(sk));
        }
        let k = keys::generate();
        self.sender_secret = Some(keys::nsec(&k));
        Ok(k)
    }

    /// Rotate the dedicated sender key (privacy: unlinks future broadcasts
    /// from previous ones).
    pub fn rotate_sender(&mut self) -> Result<Keys> {
        self.sender_secret = None;
        self.sender_keys()
    }

    /// The relays to advertise in an invite: the oldest (first-added) up to
    /// [`crate::invite::MAX_INVITE_RELAYS`]. Insertion order is age, and the
    /// bundled defaults lead the list.
    pub fn invite_relays(&self) -> Vec<String> {
        self.relays
            .iter()
            .take(crate::invite::MAX_INVITE_RELAYS)
            .cloned()
            .collect()
    }

    /// Add a group imported from an invite, merging the relays the invite
    /// carried. Relays not already present are appended to `relays` and recorded
    /// in `auto_relays` (eligible for later auto-removal). Returns the number of
    /// relays newly added to the app.
    pub fn add_imported_group(
        &mut self,
        name: String,
        public: String,
        secret: Option<SecretString>,
        invite_relays: &[String],
    ) -> usize {
        let relays: Vec<String> = normalize_invite_relays(invite_relays);
        let added = self.absorb_relays(&relays);
        self.groups.push(Group { name, public, secret, selected: true, relays });
        added
    }

    /// Add relays not already present to `relays`, marking each new one auto
    /// (eligible for later auto-removal). Returns how many were newly added.
    /// Input must already be normalized.
    fn absorb_relays(&mut self, relays: &[String]) -> usize {
        let mut added = 0;
        for r in relays {
            if !self.relays.contains(r) {
                self.relays.push(r.clone());
                // r was absent from `relays`, and `auto_relays` ⊆ `relays`, so
                // it cannot already be present here — no dedup needed.
                self.auto_relays.push(r.clone());
                added += 1;
            }
        }
        added
    }

    /// Merge relays from a re-scanned invite into an already-imported group:
    /// add any not-yet-present relays (as auto-removable) and union them into
    /// the group's provenance list so they're pruned with it. No-op if the group
    /// isn't present. Returns the number of relays newly added to the app.
    pub fn merge_group_relays(&mut self, public: &str, invite_relays: &[String]) -> usize {
        if !self.groups.iter().any(|g| g.public == public) {
            return 0;
        }
        let relays = normalize_invite_relays(invite_relays);
        let added = self.absorb_relays(&relays);
        if let Some(g) = self.groups.iter_mut().find(|g| g.public == public) {
            for r in relays {
                if !g.relays.contains(&r) {
                    g.relays.push(r);
                }
            }
        }
        added
    }

    /// Collapse the persisted relay lists to their normalized, deduped form.
    /// Migrates configs written before relay URLs were case-normalized so legacy
    /// mixed-case entries don't linger as duplicates. Keeps `auto_relays` ⊆
    /// `relays` and normalizes each group's provenance list.
    fn normalize_relays(&mut self) {
        self.relays = crate::relay::normalize_dedup(&self.relays);
        let valid: std::collections::HashSet<String> = self.relays.iter().cloned().collect();
        self.auto_relays = crate::relay::normalize_dedup(&self.auto_relays)
            .into_iter()
            .filter(|r| valid.contains(r))
            .collect();
        for g in &mut self.groups {
            g.relays = crate::relay::normalize_dedup(&g.relays);
        }
    }

    /// Remove the group with the given public key (hex). Relays the group
    /// brought in are pruned if no longer referenced by another group, but never
    /// below [`MIN_RELAYS`]. Returns true if a group was removed.
    pub fn remove_group(&mut self, public: &str) -> bool {
        let Some(pos) = self.groups.iter().position(|g| g.public == public) else {
            return false;
        };
        let removed = self.groups.remove(pos);
        self.prune_auto_relays(&removed.relays);
        true
    }

    /// Auto-remove the given (just-orphaned) relays: only those marked auto and
    /// no longer referenced by a surviving group, and never below [`MIN_RELAYS`].
    fn prune_auto_relays(&mut self, candidates: &[String]) {
        let still_used: std::collections::HashSet<String> = self
            .groups
            .iter()
            .flat_map(|g| g.relays.iter().cloned())
            .collect();
        for r in candidates {
            if self.relays.len() <= MIN_RELAYS {
                break; // floor: never auto-remove below MIN_RELAYS
            }
            if self.auto_relays.contains(r) && !still_used.contains(r) {
                self.relays.retain(|x| x != r);
                self.auto_relays.retain(|x| x != r);
            }
        }
    }

    /// Add a relay the user typed in manually. `url` must already be normalized.
    /// Marks it user-owned (drops it from `auto_relays`) so it is never
    /// auto-removed.
    pub fn add_relay(&mut self, url: &str) {
        if !self.relays.iter().any(|r| r == url) {
            self.relays.push(url.to_string());
        }
        self.auto_relays.retain(|r| r != url);
    }

    /// Remove a relay the user removed manually, clearing it from `auto_relays`
    /// too.
    pub fn remove_relay(&mut self, url: &str) {
        self.relays.retain(|r| r != url);
        self.auto_relays.retain(|r| r != url);
    }

    pub fn label_for(&self, pubkey_hex: &str) -> Option<&str> {
        self.sender_labels
            .iter()
            .find(|l| l.pubkey == pubkey_hex)
            .map(|l| l.label.as_str())
    }

    pub fn set_label(&mut self, pubkey_hex: &str, label: &str) {
        let label = label.trim();
        self.sender_labels.retain(|l| l.pubkey != pubkey_hex);
        if !label.is_empty() {
            self.sender_labels.push(SenderLabel {
                pubkey: pubkey_hex.to_string(),
                label: label.to_string(),
            });
        }
    }
}

/// Loads/saves a [`Config`] at a fixed path with atomic writes.
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("config.json") }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the non-secret "was sharing" sentinel, a sibling of
    /// `config.json`. The Android boot receiver checks its mere existence to
    /// decide whether to offer resuming, so it never has to parse (or even
    /// open) the secret-bearing config file.
    pub fn resume_flag_path(&self) -> PathBuf {
        self.path.with_file_name("resume.flag")
    }

    /// Create (on = true) or remove (on = false) the resume sentinel so it
    /// mirrors [`Config::resume_share`]. Best-effort: errors are ignored,
    /// since the flag is only an optimisation for the boot receiver.
    pub fn set_resume_flag(&self, on: bool) {
        let path = self.resume_flag_path();
        if on {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, []);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Load the config, falling back to defaults when the file is missing.
    /// A corrupt file is treated as an error so callers can decide (we never
    /// silently wipe keys).
    pub fn load(&self) -> Result<Config> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let mut cfg: Config = serde_json::from_slice(&bytes).map_err(Error::from)?;
                cfg.normalize_relays();
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(config)?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ntrack-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        // missing file → defaults
        let mut cfg = store.load().unwrap();
        assert_eq!(cfg.relays, default_relays());
        assert!(cfg.groups.is_empty());

        cfg.groups.push(Group::new_member("Family".into()).unwrap());
        let sender = cfg.sender_keys().unwrap();
        store.save(&cfg).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, cfg);
        // sender key is stable across loads
        let mut loaded = loaded;
        assert_eq!(loaded.sender_keys().unwrap().public_key(), sender.public_key());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_json_never_contains_plain_marker_but_keeps_secret() {
        // SecretString serializes transparently (we need the value back),
        // but Debug-formatting the whole config must not leak it.
        let mut cfg = Config::default();
        let k = cfg.sender_keys().unwrap();
        let nsec = keys::nsec(&k);
        let debug = format!("{cfg:?}");
        assert!(!debug.contains(nsec.expose()), "Debug must redact secrets");
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(nsec.expose()), "persistence keeps the secret");
    }

    #[test]
    fn group_rotation_changes_keys() {
        let mut g = Group::new_member("Team".into()).unwrap();
        let old_pub = g.public.clone();
        let old_secret = g.secret.clone().unwrap();
        g.rotate();
        assert_ne!(g.public, old_pub);
        assert_ne!(g.secret.unwrap().expose(), old_secret.expose());
    }

    #[test]
    fn corrupt_config_is_an_error_not_a_wipe() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        std::fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sender_labels() {
        let mut cfg = Config::default();
        cfg.set_label("ab", "Alice");
        assert_eq!(cfg.label_for("ab"), Some("Alice"));
        cfg.set_label("ab", "  ");
        assert_eq!(cfg.label_for("ab"), None);
    }

    #[test]
    fn display_name_defaults_empty_and_roundtrips() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        // An older config.json predating the display name still loads.
        std::fs::write(store.path(), br#"{"relays":[],"groups":[]}"#).unwrap();
        let mut cfg = store.load().unwrap();
        assert_eq!(cfg.display_name, "");
        // It persists and round-trips once set.
        cfg.display_name = "Anna".into();
        store.save(&cfg).unwrap();
        assert_eq!(store.load().unwrap().display_name, "Anna");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_fields_default_off_for_old_config() {
        // An older config.json predating the resume fields must still load,
        // defaulting to "not resuming".
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        std::fs::write(store.path(), br#"{"relays":[],"groups":[]}"#).unwrap();
        let cfg = store.load().unwrap();
        assert!(!cfg.resume_share);
        assert_eq!(cfg.resume_msg, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_flag_path_is_sibling_of_config() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        assert_eq!(store.resume_flag_path(), dir.join("resume.flag"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn member() -> (String, Option<SecretString>) {
        let k = keys::generate();
        (k.public_key().to_hex(), Some(keys::nsec(&k)))
    }

    #[test]
    fn invite_relays_returns_oldest_three() {
        let c = Config {
            relays: ["a", "b", "c", "d", "e"].iter().map(|s| format!("wss://{s}")).collect(),
            ..Default::default()
        };
        assert_eq!(
            c.invite_relays(),
            vec!["wss://a".to_string(), "wss://b".to_string(), "wss://c".to_string()]
        );
    }

    #[test]
    fn add_imported_group_adds_only_new_relays() {
        let mut c = Config::default(); // 3 default relays, no auto relays
        let (public, secret) = member();
        let invite = vec![
            "wss://new.example".to_string(),
            "wss://relay.damus.io".to_string(), // already a default
        ];
        let added = c.add_imported_group("G".into(), public.clone(), secret, &invite);
        assert_eq!(added, 1, "only the genuinely-new relay is counted");
        assert!(c.relays.contains(&"wss://new.example".to_string()));
        assert_eq!(c.auto_relays, vec!["wss://new.example".to_string()]);
        let g = c.groups.iter().find(|g| g.public == public).unwrap();
        // The group stores the full normalized provenance list (incl. the
        // already-present default), not just the newly-added relays.
        assert_eq!(
            g.relays,
            vec!["wss://new.example".to_string(), "wss://relay.damus.io".to_string()]
        );
    }

    #[test]
    fn add_imported_group_collapses_case_duplicates() {
        let mut c = Config::default();
        let (public, secret) = member();
        let added = c.add_imported_group(
            "G".into(),
            public,
            secret,
            &["WSS://New.Example".to_string(), "wss://new.example".to_string()],
        );
        assert_eq!(added, 1);
        assert_eq!(c.auto_relays, vec!["wss://new.example".to_string()]);
    }

    #[test]
    fn remove_group_prunes_its_auto_relay() {
        let mut c = Config::default();
        let (public, secret) = member();
        c.add_imported_group("G".into(), public.clone(), secret, &["wss://new.example".into()]);
        assert!(c.relays.contains(&"wss://new.example".to_string()));
        assert!(c.remove_group(&public));
        assert!(!c.relays.contains(&"wss://new.example".to_string()));
        assert!(c.auto_relays.is_empty());
        assert!(c.groups.is_empty());
    }

    #[test]
    fn remove_group_keeps_relay_used_by_another_group() {
        let mut c = Config::default();
        let (p1, s1) = member();
        let (p2, s2) = member();
        c.add_imported_group("G1".into(), p1.clone(), s1, &["wss://shared.example".into()]);
        c.add_imported_group("G2".into(), p2.clone(), s2, &["wss://shared.example".into()]);
        assert!(c.remove_group(&p1));
        assert!(
            c.relays.contains(&"wss://shared.example".to_string()),
            "still referenced by G2"
        );
        assert!(c.remove_group(&p2));
        assert!(
            !c.relays.contains(&"wss://shared.example".to_string()),
            "now unused → pruned"
        );
    }

    #[test]
    fn remove_group_keeps_default_relay_carried_by_invite() {
        let mut c = Config::default();
        let (public, secret) = member();
        // The invite re-lists a relay the user already had as a default.
        c.add_imported_group("G".into(), public.clone(), secret, &["wss://relay.damus.io".into()]);
        assert!(c.auto_relays.is_empty(), "a default is never marked auto");
        assert!(c.remove_group(&public));
        assert!(c.relays.contains(&"wss://relay.damus.io".to_string()));
    }

    #[test]
    fn auto_prune_respects_min_relays_floor() {
        let mut c = Config {
            relays: vec!["wss://d1".into(), "wss://d2".into()], // user trimmed below the floor
            ..Default::default()
        };
        let (public, secret) = member();
        c.add_imported_group("G".into(), public.clone(), secret, &["wss://n1".into(), "wss://n2".into()]);
        assert_eq!(c.relays.len(), 4);
        assert!(c.remove_group(&public));
        // Pruning stops at the floor: only one of the two auto relays is removed.
        assert_eq!(c.relays.len(), MIN_RELAYS);
        assert_eq!(c.auto_relays.len(), 1);
    }

    #[test]
    fn manual_add_protects_relay_from_auto_prune() {
        let mut c = Config::default();
        let (public, secret) = member();
        c.add_imported_group("G".into(), public.clone(), secret, &["wss://n1".into()]);
        assert_eq!(c.auto_relays, vec!["wss://n1".to_string()]);
        c.add_relay("wss://n1"); // user now owns it
        assert!(c.auto_relays.is_empty());
        assert!(c.remove_group(&public));
        assert!(c.relays.contains(&"wss://n1".to_string()), "user-owned relay survives");
    }

    #[test]
    fn add_relay_dedups() {
        let mut c = Config::default();
        let before = c.relays.len();
        c.add_relay("wss://new.example");
        c.add_relay("wss://new.example");
        assert_eq!(c.relays.len(), before + 1);
    }

    #[test]
    fn merge_group_relays_adds_to_existing_group() {
        let mut c = Config::default();
        let (public, secret) = member();
        c.add_imported_group("G".into(), public.clone(), secret, &["wss://n1".into()]);
        // Re-scanning an updated invite brings a second relay; n1 is already known.
        let added = c.merge_group_relays(&public, &["wss://n2".into(), "wss://n1".into()]);
        assert_eq!(added, 1, "only n2 is new");
        assert!(c.relays.contains(&"wss://n2".to_string()));
        assert!(c.auto_relays.contains(&"wss://n2".to_string()));
        let g = c.groups.iter().find(|g| g.public == public).unwrap();
        assert!(g.relays.contains(&"wss://n1".to_string()));
        assert!(g.relays.contains(&"wss://n2".to_string()), "provenance unions both");
        // Removing the group now prunes both auto relays it brought.
        assert!(c.remove_group(&public));
        assert!(!c.relays.contains(&"wss://n1".to_string()));
        assert!(!c.relays.contains(&"wss://n2".to_string()));
    }

    #[test]
    fn merge_group_relays_unknown_group_is_noop() {
        let mut c = Config::default();
        let before = c.relays.clone();
        assert_eq!(c.merge_group_relays("deadbeef", &["wss://n1".into()]), 0);
        assert_eq!(c.relays, before);
    }

    #[test]
    fn load_normalizes_legacy_mixed_case_relays() {
        // A config written before relay URLs were lowercased can hold case-only
        // duplicates; load must collapse them so they don't double up.
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        std::fs::write(
            store.path(),
            br#"{"relays":["wss://Relay.Example.COM","wss://relay.example.com"],"groups":[]}"#,
        )
        .unwrap();
        let cfg = store.load().unwrap();
        assert_eq!(cfg.relays, vec!["wss://relay.example.com".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_relay_also_clears_auto_relays() {
        let mut c = Config::default();
        let (public, secret) = member();
        c.add_imported_group("G".into(), public, secret, &["wss://n1".into()]);
        assert!(c.auto_relays.contains(&"wss://n1".to_string()));
        c.remove_relay("wss://n1");
        assert!(!c.relays.contains(&"wss://n1".to_string()));
        assert!(!c.auto_relays.contains(&"wss://n1".to_string()));
    }

    #[test]
    fn auto_relays_and_group_relays_default_empty_for_old_config() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        std::fs::write(
            store.path(),
            br#"{"relays":["wss://a"],"groups":[{"name":"G","public":"deadbeef","selected":true}]}"#,
        )
        .unwrap();
        let cfg = store.load().unwrap();
        assert!(cfg.auto_relays.is_empty());
        assert!(cfg.groups[0].relays.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_resume_flag_creates_and_removes_sentinel() {
        let dir = tmpdir();
        let store = ConfigStore::new(&dir);
        assert!(!store.resume_flag_path().exists());
        store.set_resume_flag(true);
        assert!(store.resume_flag_path().exists());
        store.set_resume_flag(false);
        assert!(!store.resume_flag_path().exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
