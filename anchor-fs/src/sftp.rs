//! SFTP backend (spec §5.1).
//!
//! Built on `russh` (SSH transport) + `russh-sftp` (SFTP subsystem). The straightforward
//! backend: SFTP has native random-access read/write, proper rename/mkdir/rmdir, and no
//! LIST-format ambiguity. One SSH session per backend instance, established lazily and
//! cached; `reconnect()` drops it so the next access re-establishes.

use std::io::SeekFrom;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use russh::client::{self, AuthResult, Handle};
use russh::keys::ssh_key::{HashAlg, PublicKey};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use anchor_core::config::ConnectionConfig;
use anchor_core::credentials::Secret;
use anchor_core::error::{AnchorError, Result};
use anchor_core::host_keys::HostKeyStore;
use anchor_core::remote_fs::{DirEntry, RemoteFs, RemoteMetadata};

use crate::to_remote;

/// Result of the host-key check, shared out of the (consumed-by-russh) handler.
enum HostKeyOutcome {
    Pending,
    /// Presented key matched the pinned fingerprint.
    Matched,
    /// No prior pin existed; this fingerprint was learned (trust-on-first-use).
    Learned(String),
    /// Presented key did NOT match the pinned fingerprint — refuse.
    Mismatch {
        expected: String,
        got: String,
    },
}

/// russh client handler implementing TOFU host-key pinning (spec §5.1).
struct ClientHandler {
    expected: Option<String>,
    outcome: Arc<StdMutex<HostKeyOutcome>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // OpenSSH-style SHA-256 fingerprint, e.g. "SHA256:Nh0Me49Zh9fDw/…".
        let got = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        let mut outcome = self.outcome.lock().unwrap();
        match &self.expected {
            Some(expected) if expected == &got => {
                *outcome = HostKeyOutcome::Matched;
                Ok(true)
            }
            // A pinned key that no longer matches: refuse the connection.
            Some(expected) => {
                *outcome = HostKeyOutcome::Mismatch {
                    expected: expected.clone(),
                    got,
                };
                Ok(false)
            }
            // First contact with this host: accept and remember it.
            None => {
                *outcome = HostKeyOutcome::Learned(got);
                Ok(true)
            }
        }
    }
}

/// A live SSH session + its SFTP channel. Both must be kept alive together: dropping the
/// `Handle` tears down the transport the `SftpSession` rides on.
struct SftpConn {
    _session: Handle<ClientHandler>,
    sftp: SftpSession,
}

/// SFTP implementation of [`RemoteFs`].
pub struct SftpBackend {
    label: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    root: String,
    conn: Mutex<Option<Arc<SftpConn>>>,
}

impl SftpBackend {
    /// Build a backend from config + secret. The SSH session is established lazily on first
    /// use (spec §5.1).
    pub fn new(conn: &ConnectionConfig, secret: &Secret) -> Self {
        SftpBackend {
            label: conn.name.clone(),
            host: conn.host.clone(),
            port: conn.port,
            username: conn.username.clone(),
            password: secret.expose().to_string(),
            root: conn.remote_path.clone(),
            conn: Mutex::new(None),
        }
    }

    fn remote(&self, p: &Path) -> String {
        to_remote(&self.root, p)
    }

    /// Get the cached connection, establishing it if necessary. Returns an `Arc` so callers
    /// can release the lock before issuing (internally-synchronized) SFTP requests.
    async fn ensure(&self) -> Result<Arc<SftpConn>> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = Arc::new(self.connect().await?);
        *guard = Some(c.clone());
        Ok(c)
    }

    async fn connect(&self) -> Result<SftpConn> {
        // TOFU: look up any pinned fingerprint for this host:port, then verify in the handler.
        let expected = HostKeyStore::load()?
            .get(&self.host, self.port)
            .map(str::to_string);
        let outcome = Arc::new(StdMutex::new(HostKeyOutcome::Pending));
        let handler = ClientHandler {
            expected,
            outcome: outcome.clone(),
        };

        let config = Arc::new(client::Config::default());
        let mut session =
            match client::connect(config, (self.host.as_str(), self.port), handler).await {
                Ok(s) => s,
                Err(e) => {
                    // A rejected host key surfaces as a connect error; turn it into a clear,
                    // actionable message rather than a generic SSH failure.
                    if let HostKeyOutcome::Mismatch { expected, got } = &*outcome.lock().unwrap() {
                        let path = HostKeyStore::path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "known_hosts".into());
                        return Err(AnchorError::PermissionDenied(format!(
                            "SFTP host key for {}:{} does NOT match the pinned key — possible \
                         man-in-the-middle. Pinned {expected}, server presented {got}. Refusing \
                         to connect. If the server's key legitimately changed, delete its line \
                         from {path} and reconnect.",
                            self.host, self.port
                        )));
                    }
                    return Err(map_russh(e));
                }
            };

        // First successful contact: persist the learned fingerprint (spec §5.1).
        let learned = match &*outcome.lock().unwrap() {
            HostKeyOutcome::Learned(fp) => Some(fp.clone()),
            _ => None,
        };
        if let Some(fp) = learned {
            HostKeyStore::pin(&self.host, self.port, &fp)?;
        }

        match session
            .authenticate_password(&self.username, &self.password)
            .await
            .map_err(map_russh)?
        {
            AuthResult::Success => {}
            AuthResult::Failure { .. } => {
                return Err(AnchorError::PermissionDenied(
                    "SFTP authentication failed (check username/password)".into(),
                ))
            }
        }

        let channel = session.channel_open_session().await.map_err(map_russh)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(map_russh)?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(map_sftp)?;

        Ok(SftpConn {
            _session: session,
            sftp,
        })
    }
}

#[async_trait]
impl RemoteFs for SftpBackend {
    fn label(&self) -> &str {
        &self.label
    }

    async fn stat(&self, path: &Path) -> Result<RemoteMetadata> {
        let c = self.ensure().await?;
        let meta = c.sftp.metadata(self.remote(path)).await.map_err(map_sftp)?;
        Ok(to_meta(&meta))
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let c = self.ensure().await?;
        let read_dir = c.sftp.read_dir(self.remote(path)).await.map_err(map_sftp)?;
        let mut out = Vec::new();
        for entry in read_dir {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            out.push(DirEntry {
                metadata: to_meta(&entry.metadata()),
                name,
            });
        }
        Ok(out)
    }

    async fn read(&self, path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
        let c = self.ensure().await?;
        let mut file = c
            .sftp
            .open_with_flags(self.remote(path), OpenFlags::READ)
            .await
            .map_err(map_sftp)?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset)).await.map_err(map_io)?;
        }
        let mut buf = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..]).await.map_err(map_io)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u32> {
        let c = self.ensure().await?;
        // SFTP has native pwrite: open without TRUNCATE, seek, write — preserving the rest
        // of the file. This is the random-access write FTP cannot do (spec §5.1 vs §7).
        let mut file = c
            .sftp
            .open_with_flags(self.remote(path), OpenFlags::WRITE | OpenFlags::CREATE)
            .await
            .map_err(map_sftp)?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset)).await.map_err(map_io)?;
        }
        file.write_all(data).await.map_err(map_io)?;
        file.flush().await.map_err(map_io)?;
        let _ = file.shutdown().await;
        Ok(data.len() as u32)
    }

    async fn create(&self, path: &Path, is_dir: bool) -> Result<()> {
        let c = self.ensure().await?;
        let remote = self.remote(path);
        if is_dir {
            c.sftp.create_dir(remote).await.map_err(map_sftp)?;
        } else {
            // create() opens with CREATE|TRUNCATE|WRITE, yielding an empty file.
            let file = c.sftp.create(remote).await.map_err(map_sftp)?;
            drop(file);
        }
        Ok(())
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        let c = self.ensure().await?;
        let remote = self.remote(path);
        if is_dir {
            c.sftp.remove_dir(remote).await.map_err(map_sftp)?;
        } else {
            c.sftp.remove_file(remote).await.map_err(map_sftp)?;
        }
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let c = self.ensure().await?;
        c.sftp
            .rename(self.remote(from), self.remote(to))
            .await
            .map_err(map_sftp)
    }

    async fn set_len(&self, path: &Path, len: u64) -> Result<()> {
        let c = self.ensure().await?;
        // SFTP truncate/extend via SETSTAT(size).
        let attrs = FileAttributes {
            size: Some(len),
            uid: None,
            user: None,
            gid: None,
            group: None,
            permissions: None,
            atime: None,
            mtime: None,
        };
        c.sftp
            .set_metadata(self.remote(path), attrs)
            .await
            .map_err(map_sftp)
    }

    async fn is_connected(&self) -> bool {
        let cached = { self.conn.lock().await.as_ref().cloned() };
        match cached {
            Some(c) => c.sftp.canonicalize(".").await.is_ok(),
            None => false,
        }
    }

    async fn reconnect(&self) -> Result<()> {
        {
            *self.conn.lock().await = None;
        }
        self.ensure().await.map(|_| ())
    }
}

fn to_meta(m: &FileAttributes) -> RemoteMetadata {
    RemoteMetadata {
        is_dir: m.is_dir(),
        len: m.size.unwrap_or(0),
        modified: m.modified().ok(),
    }
}

/// Map russh transport errors into [`AnchorError`].
fn map_russh(e: russh::Error) -> AnchorError {
    AnchorError::Connection(format!("SSH error: {e}"))
}

/// Map russh-sftp protocol errors into [`AnchorError`], distinguishing not-found / denied
/// so the WinFsp glue can emit the right NTSTATUS (spec §4.3).
fn map_sftp(e: russh_sftp::client::error::Error) -> AnchorError {
    use russh_sftp::client::error::Error as E;
    match e {
        E::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => AnchorError::NotFound(s.error_message),
            StatusCode::PermissionDenied => AnchorError::PermissionDenied(s.error_message),
            _ => AnchorError::Protocol(format!("{}: {}", s.status_code, s.error_message)),
        },
        E::IO(msg) => AnchorError::Connection(msg),
        E::Timeout => AnchorError::Connection("SFTP request timed out".into()),
        other => AnchorError::Protocol(other.to_string()),
    }
}

fn map_io(e: std::io::Error) -> AnchorError {
    AnchorError::Connection(e.to_string())
}
