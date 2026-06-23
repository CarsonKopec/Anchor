//! The single error type every layer funnels into.
//!
//! No backend returns a raw protocol error: `anchor-fs`'s WinFsp glue has exactly one
//! error type to translate into NTSTATUS codes (spec §3.1, §4.3), regardless of which
//! backend raised it.

use thiserror::Error;

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, AnchorError>;

/// Every fallible Anchor operation produces one of these. The variants map onto the
/// coarse NTSTATUS table in spec §4.3 — `NotFound` → `STATUS_OBJECT_NAME_NOT_FOUND`,
/// `PermissionDenied` → `STATUS_ACCESS_DENIED`, `Connection`/`Protocol` →
/// `STATUS_CONNECTION_DISCONNECTED`, everything else → `STATUS_UNSUCCESSFUL`.
#[derive(Debug, Error)]
pub enum AnchorError {
    /// A path does not exist on the remote.
    #[error("not found: {0}")]
    NotFound(String),

    /// The remote refused the operation for permission reasons.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The transport (TCP/TLS/SSH) is down or could not be established.
    #[error("connection error: {0}")]
    Connection(String),

    /// The peer spoke the protocol in a way we could not handle (bad LIST format,
    /// unexpected reply code, unsupported operation such as FTP mid-file overwrite).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// `connections.tomlp` is malformed or a value is out of range. The message names
    /// the offending key (spec §6.1).
    #[error("configuration error: {0}")]
    Config(String),

    /// Windows Credential Manager refused or could not satisfy a request.
    #[error("credential error: {0}")]
    Credential(String),

    /// A local I/O error not attributable to the remote.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Anything that does not fit a more specific variant (maps to STATUS_UNSUCCESSFUL).
    #[error("{0}")]
    Other(String),
}

impl AnchorError {
    /// Build an [`AnchorError::Other`] from anything string-like.
    pub fn other(msg: impl Into<String>) -> Self {
        AnchorError::Other(msg.into())
    }
}
