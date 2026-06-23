//! `anchor-fs` — the only crate that touches a wire protocol or WinFsp (spec §4, §5).
//!
//! It implements `anchor-core`'s [`RemoteFs`] trait for FTP/FTPS and SFTP, and (behind the
//! `winfsp` feature) owns the WinFsp host glue that attaches a backend to a Windows drive
//! letter. `anchor-core` depends on none of this; the UIs depend on it only through
//! [`build_backend`] and [`mount`].

pub mod ftp;
pub mod sftp;

#[cfg(feature = "winfsp")]
pub mod winfsp_host;

use std::path::{Component, Path};
use std::sync::Arc;

use anchor_core::config::{ConnectionConfig, Protocol};
use anchor_core::credentials::Secret;
use anchor_core::error::Result;
// AnchorError is only used by the no-WinFsp fallback `mount` below.
#[cfg(not(feature = "winfsp"))]
use anchor_core::error::AnchorError;
use anchor_core::mount::StopHandle;
use anchor_core::remote_fs::RemoteFs;

pub use ftp::FtpBackend;
pub use sftp::SftpBackend;

/// Build the backend for a connection. This is the §3.4 extension point: adding a protocol
/// is one new `Protocol` arm here plus the backend module — nothing in `anchor-core` or the
/// UIs changes.
pub fn build_backend(conn: &ConnectionConfig, secret: &Secret) -> Result<Arc<dyn RemoteFs>> {
    let backend: Arc<dyn RemoteFs> = match conn.protocol {
        Protocol::Ftp | Protocol::Ftps => Arc::new(FtpBackend::new(conn, secret)),
        Protocol::Sftp => Arc::new(SftpBackend::new(conn, secret)),
    };
    Ok(backend)
}

/// Translate a WinFsp-supplied Windows path into the backend's POSIX path space, applying
/// the connection's `remote_path` root. The root (`\`) maps to the configured root; `\a\b`
/// maps to `<root>/a/b`.
pub(crate) fn to_remote(root: &str, p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in p.components() {
        if let Component::Normal(os) = comp {
            parts.push(os.to_string_lossy().into_owned());
        }
    }
    let root = root.trim_end_matches('/');
    if parts.is_empty() {
        if root.is_empty() {
            "/".to_string()
        } else {
            root.to_string()
        }
    } else if root.is_empty() {
        format!("/{}", parts.join("/"))
    } else {
        format!("{}/{}", root, parts.join("/"))
    }
}

/// Attach `backend` to the connection's drive letter via WinFsp, returning a stop-handle
/// that unmounts it (spec §4.5). `MountManager` stores this and calls it on unmount.
///
/// Without the `winfsp` feature this returns a clear error rather than mounting — the
/// backends and every non-mount path still work, which is how the workspace stays buildable
/// and testable on machines without WinFsp.
#[cfg(feature = "winfsp")]
pub fn mount(
    conn: &ConnectionConfig,
    backend: Arc<dyn RemoteFs>,
    runtime: tokio::runtime::Handle,
) -> Result<StopHandle> {
    winfsp_host::mount(conn, backend, runtime)
}

/// See the feature-gated variant above.
#[cfg(not(feature = "winfsp"))]
pub fn mount(
    _conn: &ConnectionConfig,
    _backend: Arc<dyn RemoteFs>,
    _runtime: tokio::runtime::Handle,
) -> Result<StopHandle> {
    Err(AnchorError::Other(
        "WinFsp support was not compiled in; rebuild with `--features winfsp` on a machine \
         with WinFsp installed (https://winfsp.dev)"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::to_remote;
    use std::path::Path;

    #[test]
    fn to_remote_maps_paths() {
        // Root with a non-"/" base.
        assert_eq!(to_remote("/srv/media", Path::new("\\")), "/srv/media");
        assert_eq!(
            to_remote("/srv/media", Path::new("\\a\\b.txt")),
            "/srv/media/a/b.txt"
        );
        // Root "/" base.
        assert_eq!(to_remote("/", Path::new("\\")), "/");
        assert_eq!(to_remote("/", Path::new("\\dir\\f")), "/dir/f");
        // Trailing slash on root is normalized.
        assert_eq!(to_remote("/srv/", Path::new("\\x")), "/srv/x");
    }
}
