//! The `RemoteFs` trait — the only contract `anchor-fs`'s WinFsp glue depends on.
//!
//! Every backend (FTP, SFTP, and anything added later) implements this. See spec §3.1.

use std::path::Path;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::error::Result;

/// Metadata about a single remote node, normalized across protocols.
#[derive(Debug, Clone)]
pub struct RemoteMetadata {
    /// Whether the node is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories / when unknown).
    pub len: u64,
    /// Last-modified time if the server reported one, else `None`.
    pub modified: Option<SystemTime>,
}

impl RemoteMetadata {
    /// A directory with unknown size/mtime — handy for synthesized roots.
    pub fn dir() -> Self {
        RemoteMetadata {
            is_dir: true,
            len: 0,
            modified: None,
        }
    }
}

/// One entry within a directory listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Final path component (no directory part).
    pub name: String,
    /// Metadata for the entry.
    pub metadata: RemoteMetadata,
}

/// The backend contract. One instance per mount, never shared across drive letters.
///
/// `Send + Sync` is required because WinFsp's dispatcher calls in from multiple threads
/// concurrently (spec §3.1). `read`/`write` take an explicit `offset` so the cache layer
/// and WinFsp glue can implement seeking/partial I/O once, rather than each backend
/// reimplementing it.
#[async_trait]
pub trait RemoteFs: Send + Sync {
    /// Human-readable label for logs/UI (typically the connection name).
    fn label(&self) -> &str;

    /// Stat a single path.
    async fn stat(&self, path: &Path) -> Result<RemoteMetadata>;

    /// List the entries of a directory.
    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>>;

    /// Read up to `len` bytes starting at `offset`. May return fewer bytes at EOF.
    async fn read(&self, path: &Path, offset: u64, len: u32) -> Result<Vec<u8>>;

    /// Write `data` starting at `offset`. Returns the number of bytes written.
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u32>;

    /// Create a file or directory.
    async fn create(&self, path: &Path, is_dir: bool) -> Result<()>;

    /// Remove a file or directory.
    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()>;

    /// Rename/move `from` to `to`.
    async fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Truncate or extend a file to `len` bytes (backends may restrict this — see FTP §7).
    async fn set_len(&self, path: &Path, len: u64) -> Result<()>;

    /// Cheap liveness check on the underlying transport.
    async fn is_connected(&self) -> bool;

    /// Drop and re-establish the underlying transport.
    async fn reconnect(&self) -> Result<()>;
}
