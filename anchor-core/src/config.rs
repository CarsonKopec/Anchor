//! Configuration: `%APPDATA%\Anchor\connections.tomlp` in TOML+ (spec §6).
//!
//! Loaded and validated through the `tomlplus-syntax` crate (the user's own TOML+ core).
//! `parse()` builds the document, `validate()` enforces the inline `@`-annotations
//! (`@required`, `@enum`, `@type`, `@min`/`@max`, `@pattern`), and any error-severity
//! diagnostic fails the load with a message naming the offending line/key — rather than
//! surfacing later as a confusing runtime error mid-mount (spec §6.1).
//!
//! Secrets are never in this file: `credential_key` is a reference into Windows Credential
//! Manager (spec §6.3, [`crate::credentials`]).

use std::collections::BTreeMap;
use std::path::PathBuf;

use tomlplus_syntax::{parse, validate, LineIndex, Severity, Value};

use crate::error::{AnchorError, Result};

/// A starter config (one SFTP + one FTPS connection) with full annotations, suitable for
/// writing out as a template. Parses and validates clean.
pub const EXAMPLE_TOMLP: &str = include_str!("example_connections.tomlp");

/// Wire protocol for a connection (spec §6.2, `@enum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Plain FTP.
    Ftp,
    /// FTP with explicit TLS (FTPS).
    Ftps,
    /// SFTP over SSH.
    Sftp,
}

impl Protocol {
    /// Canonical lowercase token used in the config file.
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Ftp => "ftp",
            Protocol::Ftps => "ftps",
            Protocol::Sftp => "sftp",
        }
    }

    /// Parse from the config token.
    pub fn from_token(s: &str) -> Option<Protocol> {
        match s {
            "ftp" => Some(Protocol::Ftp),
            "ftps" => Some(Protocol::Ftps),
            "sftp" => Some(Protocol::Sftp),
            _ => None,
        }
    }

    /// Default TCP port when `port` is omitted.
    ///
    /// NOTE: the spec literally says "default 22", but that is only correct for SFTP — 22
    /// is the SSH port and FTP/FTPS listen on 21. Defaulting per-protocol avoids a
    /// silent-misconnect footgun; an explicit `port` always wins.
    pub fn default_port(&self) -> u16 {
        match self {
            Protocol::Sftp => 22,
            Protocol::Ftp | Protocol::Ftps => 21,
        }
    }

    /// Whether this protocol negotiates TLS (FTPS).
    pub fn uses_tls(&self) -> bool {
        matches!(self, Protocol::Ftps)
    }
}

/// One configured connection. Field set + defaults are exactly spec §6.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionConfig {
    /// Connection name (the `<name>` in `[connections.<name>]`). Used as the map key and
    /// in the tray/CLI. Must not contain `.` or whitespace.
    pub name: String,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Hostname or IP.
    pub host: String,
    /// TCP port (defaults per protocol if omitted).
    pub port: u16,
    /// Login user.
    pub username: String,
    /// Reference into Credential Manager — NEVER the secret itself (spec §6.3).
    pub credential_key: String,
    /// Windows drive letter to mount at, e.g. `"M:"`.
    pub drive_letter: String,
    /// Remote directory to expose as the drive root.
    pub remote_path: String,
    /// Mount read-only.
    pub read_only: bool,
    /// Directory-listing cache TTL in seconds (spec §3.2).
    pub dir_cache_ttl_secs: u64,
    /// Mount automatically when the tray app starts (spec §7).
    pub auto_mount_on_start: bool,
}

impl ConnectionConfig {
    /// Reject names that would break the `[connections.<name>]` section path or the menu.
    fn validate_name(name: &str) -> Result<()> {
        if name.is_empty() || name.contains('.') || name.chars().any(|c| c.is_whitespace()) {
            return Err(AnchorError::Config(format!(
                "connection name '{name}' is invalid (no dots or whitespace)"
            )));
        }
        Ok(())
    }
}

/// The whole config: an ordered set of connections.
#[derive(Debug, Clone, Default)]
pub struct AnchorConfig {
    connections: Vec<ConnectionConfig>,
}

impl AnchorConfig {
    /// Build from a list of connections (used by tests and `from_str`).
    pub fn from_connections(connections: Vec<ConnectionConfig>) -> Self {
        AnchorConfig { connections }
    }

    /// Iterate connections in file order.
    pub fn connections(&self) -> impl Iterator<Item = &ConnectionConfig> {
        self.connections.iter()
    }

    /// Look up a connection by name.
    pub fn get(&self, name: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// All connection names.
    pub fn names(&self) -> Vec<String> {
        self.connections.iter().map(|c| c.name.clone()).collect()
    }

    /// Number of configured connections.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Whether there are no connections.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Insert or replace a connection (matched by name).
    pub fn upsert(&mut self, conn: ConnectionConfig) -> Result<()> {
        ConnectionConfig::validate_name(&conn.name)?;
        match self.connections.iter_mut().find(|c| c.name == conn.name) {
            Some(existing) => *existing = conn,
            None => self.connections.push(conn),
        }
        Ok(())
    }

    /// Remove a connection by name, returning it if present.
    pub fn remove(&mut self, name: &str) -> Option<ConnectionConfig> {
        if let Some(i) = self.connections.iter().position(|c| c.name == name) {
            Some(self.connections.remove(i))
        } else {
            None
        }
    }

    /// Path to the on-disk config: `%APPDATA%\Anchor\connections.tomlp` (spec §6.1).
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| AnchorError::Config("APPDATA environment variable is not set".into()))?;
        Ok(base.join("Anchor").join("connections.tomlp"))
    }

    /// Load from the default path. A missing file yields an empty config (not an error).
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        Self::load_from(&path)
    }

    /// Load from a specific path. A missing file yields an empty config.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(AnchorConfig::default());
        }
        let source = std::fs::read_to_string(path)?;
        Self::from_tomlp(&source).map_err(|e| match e {
            AnchorError::Config(msg) => AnchorError::Config(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    /// Parse + validate TOML+ source into a config.
    pub fn from_tomlp(source: &str) -> Result<Self> {
        let doc = parse(source);
        let line_index = LineIndex::new(source);

        // Collect every error-severity diagnostic from both parsing and annotation
        // validation, rendering each with a 1-based line:col so the user can jump to it.
        let mut diagnostics = doc.diagnostics.clone();
        diagnostics.extend(validate(&doc));
        let errors: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| {
                let (line, col) = line_index.position(d.span.start);
                format!("line {}:{}: {}", line + 1, col + 1, d.message)
            })
            .collect();
        if !errors.is_empty() {
            return Err(AnchorError::Config(errors.join("; ")));
        }

        let connections_dict = match doc.config.get("connections") {
            None => return Ok(AnchorConfig::default()),
            Some(Value::Dict(map)) => map,
            Some(other) => {
                return Err(AnchorError::Config(format!(
                    "`connections` must be a table, found {}",
                    other.type_name()
                )))
            }
        };

        let mut connections = Vec::new();
        for (name, value) in connections_dict {
            let fields = match value {
                Value::Dict(d) => d,
                other => {
                    return Err(AnchorError::Config(format!(
                        "connection '{name}' must be a table, found {}",
                        other.type_name()
                    )))
                }
            };
            connections.push(parse_connection(name, fields)?);
        }

        Ok(AnchorConfig { connections })
    }

    /// Serialize back to annotated TOML+ (so hand-edits stay self-validating).
    pub fn to_tomlp(&self) -> String {
        let mut s = String::new();
        s.push_str(
            "# Anchor connections — managed by `anchor` (CLI) / `anchor-tray`.\n\
             # Secrets are NOT stored here. `credential_key` references Windows Credential\n\
             # Manager under target name `Anchor:<credential_key>`. See ANCHOR_SPEC.md §6.\n",
        );
        for c in &self.connections {
            s.push_str(&format!("\n[connections.{}]\n", c.name));
            s.push_str("@required\n@enum: [\"ftp\", \"ftps\", \"sftp\"]\n");
            s.push_str(&format!("protocol = \"{}\"\n", c.protocol.as_str()));
            s.push_str("@required\n");
            s.push_str(&format!("host = \"{}\"\n", escape(&c.host)));
            s.push_str("@type: int\n@min: 1\n@max: 65535\n");
            s.push_str(&format!("port = {}\n", c.port));
            s.push_str("@required\n");
            s.push_str(&format!("username = \"{}\"\n", escape(&c.username)));
            s.push_str("@required\n");
            s.push_str(&format!(
                "credential_key = \"{}\"\n",
                escape(&c.credential_key)
            ));
            s.push_str("@pattern: \"[A-Z]:\"\n");
            s.push_str(&format!("drive_letter = \"{}\"\n", escape(&c.drive_letter)));
            s.push_str(&format!("remote_path = \"{}\"\n", escape(&c.remote_path)));
            s.push_str(&format!("read_only = {}\n", c.read_only));
            s.push_str("@type: int\n@min: 1\n@max: 3600\n");
            s.push_str(&format!("dir_cache_ttl_secs = {}\n", c.dir_cache_ttl_secs));
            s.push_str(&format!(
                "auto_mount_on_start = {}\n",
                c.auto_mount_on_start
            ));
        }
        s
    }

    /// Write to the default path, creating `%APPDATA%\Anchor\` if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    /// Write to a specific path, creating parent directories.
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_tomlp())?;
        Ok(())
    }
}

/// Escape a string for a TOML+ basic (double-quoted) string.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_connection(name: &str, fields: &BTreeMap<String, Value>) -> Result<ConnectionConfig> {
    ConnectionConfig::validate_name(name)?;

    let protocol_token = require_str(fields, name, "protocol")?;
    let protocol = Protocol::from_token(&protocol_token).ok_or_else(|| {
        AnchorError::Config(format!(
            "connection '{name}': protocol must be ftp|ftps|sftp, got '{protocol_token}'"
        ))
    })?;

    let host = require_str(fields, name, "host")?;
    let username = require_str(fields, name, "username")?;
    let credential_key = require_str(fields, name, "credential_key")?;

    let port = match opt_int(fields, name, "port")? {
        Some(p) => u16::try_from(p).ok().filter(|&p| p >= 1).ok_or_else(|| {
            AnchorError::Config(format!(
                "connection '{name}': port {p} out of range 1-65535"
            ))
        })?,
        None => protocol.default_port(),
    };

    let drive_letter = require_str(fields, name, "drive_letter")?;
    validate_drive_letter(name, &drive_letter)?;

    let remote_path = opt_str(fields, name, "remote_path")?.unwrap_or_else(|| "/".to_string());
    let read_only = opt_bool(fields, name, "read_only")?.unwrap_or(false);

    let dir_cache_ttl_secs = match opt_int(fields, name, "dir_cache_ttl_secs")? {
        Some(t) if (1..=3600).contains(&t) => t as u64,
        Some(t) => {
            return Err(AnchorError::Config(format!(
                "connection '{name}': dir_cache_ttl_secs {t} out of range 1-3600"
            )))
        }
        None => 10,
    };

    let auto_mount_on_start = opt_bool(fields, name, "auto_mount_on_start")?.unwrap_or(false);

    Ok(ConnectionConfig {
        name: name.to_string(),
        protocol,
        host,
        port,
        username,
        credential_key,
        drive_letter,
        remote_path,
        read_only,
        dir_cache_ttl_secs,
        auto_mount_on_start,
    })
}

fn validate_drive_letter(name: &str, dl: &str) -> Result<()> {
    let bytes = dl.as_bytes();
    let ok = bytes.len() == 2 && bytes[0].is_ascii_uppercase() && bytes[1] == b':';
    if ok {
        Ok(())
    } else {
        Err(AnchorError::Config(format!(
            "connection '{name}': drive_letter '{dl}' must match X: (one uppercase letter + colon)"
        )))
    }
}

fn require_str(fields: &BTreeMap<String, Value>, conn: &str, key: &str) -> Result<String> {
    match fields.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(AnchorError::Config(format!(
            "connection '{conn}': {key} must not be empty"
        ))),
        Some(other) => Err(AnchorError::Config(format!(
            "connection '{conn}': {key} must be a string, found {}",
            other.type_name()
        ))),
        None => Err(AnchorError::Config(format!(
            "connection '{conn}': required key '{key}' is missing"
        ))),
    }
}

fn opt_str(fields: &BTreeMap<String, Value>, conn: &str, key: &str) -> Result<Option<String>> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(AnchorError::Config(format!(
            "connection '{conn}': {key} must be a string, found {}",
            other.type_name()
        ))),
    }
}

fn opt_int(fields: &BTreeMap<String, Value>, conn: &str, key: &str) -> Result<Option<i64>> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Integer(n)) => Ok(Some(*n)),
        Some(other) => Err(AnchorError::Config(format!(
            "connection '{conn}': {key} must be an integer, found {}",
            other.type_name()
        ))),
    }
}

fn opt_bool(fields: &BTreeMap<String, Value>, conn: &str, key: &str) -> Result<Option<bool>> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(AnchorError::Config(format!(
            "connection '{conn}': {key} must be a bool, found {}",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_template_parses_and_validates() {
        let cfg = AnchorConfig::from_tomlp(EXAMPLE_TOMLP).expect("example should be valid");
        assert_eq!(cfg.len(), 2, "example ships two connections");
        let sftp = cfg.get("media-box").expect("media-box present");
        assert_eq!(sftp.protocol, Protocol::Sftp);
        assert_eq!(sftp.drive_letter, "M:");
        let ftps = cfg.get("archive").expect("archive present");
        assert_eq!(ftps.protocol, Protocol::Ftps);
        assert!(ftps.read_only);
    }

    #[test]
    fn defaults_applied() {
        let src = r#"
[connections.minimal]
protocol = "sftp"
host = "h.example.com"
username = "u"
credential_key = "k"
drive_letter = "Z:"
"#;
        let cfg = AnchorConfig::from_tomlp(src).unwrap();
        let c = cfg.get("minimal").unwrap();
        assert_eq!(c.port, 22); // sftp default
        assert_eq!(c.remote_path, "/");
        assert_eq!(c.dir_cache_ttl_secs, 10);
        assert!(!c.read_only);
        assert!(!c.auto_mount_on_start);
    }

    #[test]
    fn ftp_default_port_is_21() {
        let src = r#"
[connections.legacy]
protocol = "ftp"
host = "h"
username = "u"
credential_key = "k"
drive_letter = "F:"
"#;
        let c = AnchorConfig::from_tomlp(src).unwrap();
        assert_eq!(c.get("legacy").unwrap().port, 21);
    }

    #[test]
    fn bad_drive_letter_named() {
        let src = r#"
[connections.bad]
protocol = "sftp"
host = "h"
username = "u"
credential_key = "k"
drive_letter = "m"
"#;
        let err = AnchorConfig::from_tomlp(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad") && msg.contains("drive_letter"),
            "got: {msg}"
        );
    }

    #[test]
    fn missing_required_key_named() {
        let src = r#"
[connections.oops]
protocol = "sftp"
host = "h"
drive_letter = "X:"
"#;
        // username + credential_key missing -> error naming one of them.
        let err = AnchorConfig::from_tomlp(src).unwrap_err().to_string();
        assert!(err.contains("oops"), "got: {err}");
    }

    #[test]
    fn annotation_enum_violation_is_caught_by_validator() {
        // Inline @enum annotation should make tomlplus-syntax reject a bad protocol even
        // before our own programmatic check.
        let src = r#"
[connections.x]
@enum: ["ftp", "ftps", "sftp"]
protocol = "scp"
host = "h"
username = "u"
credential_key = "k"
drive_letter = "X:"
"#;
        let err = AnchorConfig::from_tomlp(src).unwrap_err().to_string();
        assert!(err.contains("enum") || err.contains("scp"), "got: {err}");
    }

    #[test]
    fn roundtrip_through_tomlp() {
        let c = ConnectionConfig {
            name: "media-box".into(),
            protocol: Protocol::Sftp,
            host: "media.example.com".into(),
            port: 2222,
            username: "carson".into(),
            credential_key: "media-box".into(),
            drive_letter: "M:".into(),
            remote_path: "/srv/media".into(),
            read_only: false,
            dir_cache_ttl_secs: 30,
            auto_mount_on_start: true,
        };
        let cfg = AnchorConfig::from_connections(vec![c.clone()]);
        let text = cfg.to_tomlp();
        let reparsed = AnchorConfig::from_tomlp(&text).expect("roundtrip parses");
        assert_eq!(reparsed.get("media-box"), Some(&c));
    }
}
