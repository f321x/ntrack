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
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
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
            relays: default_relays(),
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
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(Error::from),
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
