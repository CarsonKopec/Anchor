//! TOFU (trust-on-first-use) SSH host-key pinning (spec §5.1, §11).
//!
//! Fingerprints are stored OpenSSH-style in `%APPDATA%\Anchor\known_hosts`, keyed by
//! `host:port`. On the first successful connect to a server its key fingerprint is recorded;
//! on every subsequent connect the presented key is checked against the pinned one and a
//! mismatch is refused (potential man-in-the-middle).
//!
//! The spec suggested storing the fingerprint inside `connections.tomlp`; a dedicated
//! `known_hosts` file is used instead because it (a) matches how OpenSSH actually works —
//! which the spec explicitly references — (b) keys by `host:port` rather than per-connection
//! (two connections to the same host share trust), and (c) avoids the SFTP backend having to
//! rewrite the user's main config file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{AnchorError, Result};

/// A set of pinned host-key fingerprints (`host:port` → `SHA256:…`).
#[derive(Debug, Default, Clone)]
pub struct HostKeyStore {
    entries: BTreeMap<String, String>,
}

impl HostKeyStore {
    /// Path to the on-disk store: `%APPDATA%\Anchor\known_hosts`.
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| AnchorError::Config("APPDATA environment variable is not set".into()))?;
        Ok(base.join("Anchor").join("known_hosts"))
    }

    /// Load from the default path (missing file → empty store).
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        Self::load_from(&path)
    }

    /// Load from a specific path (missing file → empty store).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(host), Some(port), Some(fp)) = (parts.next(), parts.next(), parts.next()) {
                entries.insert(format!("{host}:{port}"), fp.to_string());
            }
        }
        Ok(HostKeyStore { entries })
    }

    fn key(host: &str, port: u16) -> String {
        format!("{host}:{port}")
    }

    /// The pinned fingerprint for `host:port`, if any.
    pub fn get(&self, host: &str, port: u16) -> Option<&str> {
        self.entries.get(&Self::key(host, port)).map(String::as_str)
    }

    /// Pin (or overwrite) the fingerprint for `host:port`.
    pub fn insert(&mut self, host: &str, port: u16, fingerprint: &str) {
        self.entries
            .insert(Self::key(host, port), fingerprint.to_string());
    }

    /// Drop the pin for `host:port`, returning whether one existed.
    pub fn remove(&mut self, host: &str, port: u16) -> bool {
        self.entries.remove(&Self::key(host, port)).is_some()
    }

    /// Serialize to the `known_hosts` text format.
    pub fn to_known_hosts(&self) -> String {
        let mut text = String::from(
            "# Anchor known SSH host keys (TOFU pinning). Format: <host> <port> <SHA256:...>\n\
             # Delete a line to re-trust that server on its next connection.\n",
        );
        for (k, fp) in &self.entries {
            if let Some((host, port)) = k.rsplit_once(':') {
                text.push_str(&format!("{host} {port} {fp}\n"));
            }
        }
        text
    }

    /// Write to the default path, creating `%APPDATA%\Anchor\` if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    /// Write to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_known_hosts())?;
        Ok(())
    }

    /// Convenience: load the default store, pin `host:port`, and save.
    pub fn pin(host: &str, port: u16, fingerprint: &str) -> Result<()> {
        let mut store = Self::load()?;
        store.insert(host, port, fingerprint);
        store.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_lookup() {
        let dir = std::env::temp_dir().join(format!("anchor-kh-{}", std::process::id()));
        let path = dir.join("known_hosts");

        let mut store = HostKeyStore::default();
        store.insert("media.example.com", 22, "SHA256:AAAA");
        store.insert("192.168.1.5", 2222, "SHA256:BBBB");
        store.save_to(&path).unwrap();

        let loaded = HostKeyStore::load_from(&path).unwrap();
        assert_eq!(loaded.get("media.example.com", 22), Some("SHA256:AAAA"));
        assert_eq!(loaded.get("192.168.1.5", 2222), Some("SHA256:BBBB"));
        assert_eq!(loaded.get("media.example.com", 23), None); // different port

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty() {
        let store =
            HostKeyStore::load_from(Path::new("Z:/anchor/does-not-exist/known_hosts")).unwrap();
        assert!(store.get("h", 22).is_none());
    }
}
