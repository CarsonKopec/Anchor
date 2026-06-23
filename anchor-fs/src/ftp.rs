//! FTP / FTPS backend (spec §5.2).
//!
//! Built on `suppaftp`'s **synchronous** API. suppaftp's async mode is `async-std`-backed,
//! which doesn't compose cleanly with Anchor's tokio `block_on` bridge (spec §4.4); since
//! every WinFsp callback already dedicates one dispatcher thread to a blocking operation,
//! a synchronous FTP client called from inside the (await-free) `RemoteFs` methods is both
//! simpler and a better fit. The methods never hold the connection lock across an `.await`
//! (there are none), so the resulting futures are still `Send`.
//!
//! FTP is a structurally worse fit for "mounted drive" semantics than SFTP, for protocol
//! reasons that can't be designed around — see the per-method notes and spec §7.

use std::path::{Component, Path};
use std::str::FromStr;
use std::sync::Mutex;

use async_trait::async_trait;
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpStream, NativeTlsConnector, NativeTlsFtpStream, Status};

use anchor_core::config::ConnectionConfig;
use anchor_core::credentials::Secret;
use anchor_core::error::{AnchorError, Result};
use anchor_core::remote_fs::{DirEntry, RemoteFs, RemoteMetadata};

use crate::to_remote;

/// The control connection, which is a different concrete type for plain vs. TLS.
enum FtpConn {
    Plain(FtpStream),
    Tls(NativeTlsFtpStream),
}

/// Run `$body` against the (lazily-connected) control connection. `$s` binds to the inner
/// `&mut ImplFtpStream<_>` in each arm; because both type aliases expose identical inherent
/// methods, the body type-checks for both without duplication.
macro_rules! ftp {
    ($self:ident, $s:ident => $body:block) => {{
        let mut guard = $self.conn.lock().unwrap();
        $self.ensure(&mut guard)?;
        match guard.as_mut().expect("ensured connected") {
            FtpConn::Plain($s) => $body,
            FtpConn::Tls($s) => $body,
        }
    }};
}

/// FTP/FTPS implementation of [`RemoteFs`].
pub struct FtpBackend {
    label: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    tls: bool,
    root: String,
    conn: Mutex<Option<FtpConn>>,
}

impl FtpBackend {
    /// Build a backend from config + secret. The control connection is established lazily
    /// on first use (spec §5.2).
    pub fn new(conn: &ConnectionConfig, secret: &Secret) -> Self {
        FtpBackend {
            label: conn.name.clone(),
            host: conn.host.clone(),
            port: conn.port,
            username: conn.username.clone(),
            password: secret.expose().to_string(),
            tls: conn.protocol.uses_tls(),
            root: conn.remote_path.clone(),
            conn: Mutex::new(None),
        }
    }

    fn remote(&self, p: &Path) -> String {
        to_remote(&self.root, p)
    }

    fn is_root(p: &Path) -> bool {
        !p.components().any(|c| matches!(c, Component::Normal(_)))
    }

    /// Establish the control connection (synchronous).
    fn connect(&self) -> Result<FtpConn> {
        let addr = format!("{}:{}", self.host, self.port);
        if self.tls {
            let stream = NativeTlsFtpStream::connect(&addr).map_err(map_ftp)?;
            let tls = suppaftp::native_tls::TlsConnector::new()
                .map_err(|e| AnchorError::Connection(format!("TLS init failed: {e}")))?;
            let mut stream = stream
                .into_secure(NativeTlsConnector::from(tls), &self.host)
                .map_err(map_ftp)?;
            stream
                .login(&self.username, &self.password)
                .map_err(map_ftp)?;
            let _ = stream.transfer_type(FileType::Binary);
            Ok(FtpConn::Tls(stream))
        } else {
            let mut stream = FtpStream::connect(&addr).map_err(map_ftp)?;
            stream
                .login(&self.username, &self.password)
                .map_err(map_ftp)?;
            let _ = stream.transfer_type(FileType::Binary);
            Ok(FtpConn::Plain(stream))
        }
    }

    fn ensure(&self, guard: &mut Option<FtpConn>) -> Result<()> {
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }
        Ok(())
    }
}

#[async_trait]
impl RemoteFs for FtpBackend {
    fn label(&self) -> &str {
        &self.label
    }

    async fn stat(&self, path: &Path) -> Result<RemoteMetadata> {
        if Self::is_root(path) {
            return Ok(RemoteMetadata::dir());
        }
        let remote = self.remote(path);
        // Directory detection is a heuristic: CWD into the path, restore on success. Costs
        // an extra round trip vs. SFTP's single lstat (spec §5.2).
        ftp!(self, s => {
            let saved = s.pwd().map_err(map_ftp)?;
            if s.cwd(&remote).is_ok() {
                let _ = s.cwd(&saved);
                Ok(RemoteMetadata::dir())
            } else {
                match s.size(&remote) {
                    Ok(sz) => Ok(RemoteMetadata { is_dir: false, len: sz as u64, modified: None }),
                    Err(_) => Err(AnchorError::NotFound(remote.clone())),
                }
            }
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let remote = self.remote(path);
        ftp!(self, s => {
            let lines = s.list(Some(&remote)).map_err(map_ftp)?;
            let mut out = Vec::with_capacity(lines.len());
            for line in &lines {
                if let Some(entry) = parse_list_line(line) {
                    if entry.name == "." || entry.name == ".." {
                        continue;
                    }
                    out.push(entry);
                }
            }
            Ok(out)
        })
    }

    async fn read(&self, path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
        let remote = self.remote(path);
        // Partial read via REST (resume) + RETR, reading at most `len` bytes then aborting
        // the data transfer if the file has more (spec §5.2/§7).
        ftp!(self, s => {
            let _ = s.transfer_type(FileType::Binary);
            if offset > 0 {
                s.resume_transfer(offset as usize).map_err(map_ftp)?;
            }
            let mut ds = s.retr_as_stream(&remote).map_err(map_ftp)?;
            let mut buf = vec![0u8; len as usize];
            let mut filled = 0usize;
            let mut io_err = None;
            while filled < buf.len() {
                match std::io::Read::read(&mut ds, &mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        io_err = Some(e);
                        break;
                    }
                }
            }
            let reached_eof = filled < buf.len() && io_err.is_none();
            if reached_eof {
                s.finalize_retr_stream(ds).map_err(map_ftp)?;
            } else {
                let _ = s.abort(ds);
            }
            if let Some(e) = io_err {
                return Err(AnchorError::Connection(e.to_string()));
            }
            buf.truncate(filled);
            Ok(buf)
        })
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u32> {
        let remote = self.remote(path);
        // FTP has no pwrite. We support sequential write / append / full rewrite via REST +
        // STOR; an in-place overwrite of a mid-file byte range is NOT correctly supported
        // (spec §7) — callers should prefer SFTP when random-access writes matter.
        ftp!(self, s => {
            let _ = s.transfer_type(FileType::Binary);
            if offset > 0 {
                s.resume_transfer(offset as usize).map_err(map_ftp)?;
            }
            let mut ws = s.put_with_stream(&remote).map_err(map_ftp)?;
            std::io::Write::write_all(&mut ws, data)
                .map_err(|e| AnchorError::Connection(e.to_string()))?;
            s.finalize_put_stream(ws).map_err(map_ftp)?;
            Ok(data.len() as u32)
        })
    }

    async fn create(&self, path: &Path, is_dir: bool) -> Result<()> {
        let remote = self.remote(path);
        ftp!(self, s => {
            if is_dir {
                s.mkdir(&remote).map_err(map_ftp)?;
            } else {
                // Create an empty file: open a STOR stream and finalize without writing.
                let ws = s.put_with_stream(&remote).map_err(map_ftp)?;
                s.finalize_put_stream(ws).map_err(map_ftp)?;
            }
            Ok(())
        })
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        let remote = self.remote(path);
        ftp!(self, s => {
            if is_dir {
                s.rmdir(&remote).map_err(map_ftp)?;
            } else {
                s.rm(&remote).map_err(map_ftp)?;
            }
            Ok(())
        })
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from_r = self.remote(from);
        let to_r = self.remote(to);
        ftp!(self, s => {
            s.rename(&from_r, &to_r).map_err(map_ftp)?;
            Ok(())
        })
    }

    async fn set_len(&self, path: &Path, len: u64) -> Result<()> {
        if len != 0 {
            return Err(AnchorError::Protocol(
                "FTP backend can only truncate to zero length; arbitrary resize is unsupported (spec §7)".into(),
            ));
        }
        let remote = self.remote(path);
        ftp!(self, s => {
            let ws = s.put_with_stream(&remote).map_err(map_ftp)?;
            s.finalize_put_stream(ws).map_err(map_ftp)?;
            Ok(())
        })
    }

    async fn is_connected(&self) -> bool {
        let mut guard = self.conn.lock().unwrap();
        match guard.as_mut() {
            None => false,
            Some(FtpConn::Plain(s)) => s.noop().is_ok(),
            Some(FtpConn::Tls(s)) => s.noop().is_ok(),
        }
    }

    async fn reconnect(&self) -> Result<()> {
        {
            let mut guard = self.conn.lock().unwrap();
            *guard = None;
        }
        let mut guard = self.conn.lock().unwrap();
        self.ensure(&mut guard)
    }
}

/// Parse one Unix `ls -l`-style LIST line into a [`DirEntry`] (spec §5.2). Uses suppaftp's
/// LIST parser, which covers vsftpd/ProFTPD/Pure-FTPd; exotic/DOS-style formats are not
/// handled in v1 (MLSD would be the v1.1 fix). Returns `None` for unparseable lines.
fn parse_list_line(line: &str) -> Option<DirEntry> {
    let f = suppaftp::list::File::from_str(line).ok()?;
    Some(DirEntry {
        name: f.name().to_string(),
        metadata: RemoteMetadata {
            is_dir: f.is_directory(),
            len: f.size() as u64,
            modified: Some(f.modified()),
        },
    })
}

/// Funnel every `suppaftp` error into the single [`AnchorError`] type (spec §3.1).
fn map_ftp(e: FtpError) -> AnchorError {
    match e {
        FtpError::ConnectionError(io) => AnchorError::Connection(io.to_string()),
        FtpError::SecureError(s) => AnchorError::Connection(s),
        FtpError::UnexpectedResponse(r) => match r.status {
            Status::FileUnavailable => AnchorError::NotFound(r.to_string()),
            Status::NotLoggedIn | Status::BadFilename => {
                AnchorError::PermissionDenied(r.to_string())
            }
            _ => AnchorError::Protocol(r.to_string()),
        },
        FtpError::BadResponse => AnchorError::Protocol("malformed FTP response".into()),
        FtpError::InvalidAddress(e) => AnchorError::Other(e.to_string()),
    }
}
