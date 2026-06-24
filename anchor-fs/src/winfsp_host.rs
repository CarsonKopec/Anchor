//! WinFsp host glue (spec §4) — **feature-gated behind `winfsp`**.
//!
//! This is the only code in the workspace that talks to WinFsp. It implements WinFsp's
//! [`FileSystemContext`] for one mounted drive, bridging WinFsp's synchronous dispatcher
//! callbacks to the async [`RemoteFs`] backend via `runtime.block_on` (spec §4.4), checking
//! the caches first (spec §3.2), invalidating them on mutation, and translating
//! [`AnchorError`] into NTSTATUS (spec §4.3).
//!
//! It cannot be compiled or run without WinFsp installed, so it is the part of Anchor that
//! must be built and verified on the real target machine (spec §12) — the rest of the
//! workspace builds and is tested without it.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_CONNECTION_DISCONNECTED, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL,
};

use anchor_core::cache::{DirCache, ReadAheadBuffer, StatCache};
use anchor_core::config::ConnectionConfig;
use anchor_core::error::{AnchorError, Result as AnchorResult};
use anchor_core::mount::StopHandle;
use anchor_core::remote_fs::{RemoteFs, RemoteMetadata};

/// `NtCreateFile` create-option bit instructing "create a directory" (vs. a file).
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// Windows file attribute bits we emit.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `Cleanup` flag set when the file is to be deleted (winfsp_sys::FspCleanupDelete == 1).
const FSP_CLEANUP_DELETE: u32 = 1;

/// One open handle in the filesystem. WinFsp calls in from many threads with only a shared
/// `&FileContext`, so any mutable state (the directory read buffer) uses interior mutability
/// — [`winfsp::filesystem::DirBuffer`] is built for exactly this.
pub struct AnchorFile {
    path: PathBuf,
    is_dir: bool,
    dir_buffer: winfsp::filesystem::DirBuffer,
}

/// The per-mount filesystem context (spec §4.2). One instance per mounted drive.
pub struct AnchorFsContext {
    backend: Arc<dyn RemoteFs>,
    runtime: tokio::runtime::Handle,
    dir_cache: DirCache,
    stat_cache: StatCache,
    read_buf: ReadAheadBuffer,
    read_only: bool,
    label: String,
}

impl AnchorFsContext {
    /// The sync→async bridge (spec §4.4): block this WinFsp dispatcher thread until the
    /// async backend operation completes. WinFsp gives each mount its own thread pool, so
    /// blocking here only stalls concurrent operations on *this* drive.
    fn run<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.runtime.block_on(fut)
    }

    fn stat(&self, path: &Path) -> std::result::Result<RemoteMetadata, FspError> {
        if let Some(meta) = self.stat_cache.get(path) {
            return Ok(meta);
        }
        let meta = self.run(self.backend.stat(path)).map_err(|e| nt(&e))?;
        self.stat_cache.insert(path, meta.clone());
        Ok(meta)
    }

    /// List a directory, consulting (and populating) the TTL'd [`DirCache`].
    fn list_cached(
        &self,
        path: &Path,
    ) -> std::result::Result<Vec<anchor_core::remote_fs::DirEntry>, FspError> {
        if let Some(cached) = self.dir_cache.get(path) {
            return Ok(cached);
        }
        let entries = self.run(self.backend.list_dir(path)).map_err(|e| nt(&e))?;
        self.stat_cache.insert_dir_entries(path, &entries);
        self.dir_cache.insert(path, entries.clone());
        Ok(entries)
    }

    fn invalidate_changed_path(&self, path: &Path) {
        self.dir_cache.invalidate_parent(path);
        self.stat_cache.invalidate(path);
        self.stat_cache.invalidate_parent_children(path);
        self.read_buf.invalidate(path);
    }

    fn deny_if_read_only(&self) -> std::result::Result<(), FspError> {
        if self.read_only {
            Err(FspError::NTSTATUS(STATUS_MEDIA_WRITE_PROTECTED.0))
        } else {
            Ok(())
        }
    }
}

impl FileSystemContext for AnchorFsContext {
    type FileContext = AnchorFile;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> AnchorResultFsp<FileSecurity> {
        let path = to_path(file_name);
        let meta = self.stat(&path)?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0, // ACLs not supported (persistent_acls = false)
            attributes: attributes(&meta),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> AnchorResultFsp<Self::FileContext> {
        let path = to_path(file_name);
        let meta = self.stat(&path)?;
        fill_file_info(file_info.as_mut(), &meta);
        Ok(AnchorFile {
            is_dir: meta.is_dir,
            path,
            dir_buffer: winfsp::filesystem::DirBuffer::new(),
        })
    }

    fn close(&self, _context: Self::FileContext) {
        // Nothing to release: the backend connection is owned by the context, not the handle.
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> AnchorResultFsp<Self::FileContext> {
        self.deny_if_read_only()?;
        let path = to_path(file_name);
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        self.run(self.backend.create(&path, is_dir))
            .map_err(|e| nt(&e))?;
        self.invalidate_changed_path(&path);
        let meta = self.stat(&path).unwrap_or(RemoteMetadata {
            is_dir,
            len: 0,
            modified: None,
        });
        fill_file_info(file_info.as_mut(), &meta);
        Ok(AnchorFile {
            path,
            is_dir,
            dir_buffer: winfsp::filesystem::DirBuffer::new(),
        })
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        // Deletion completes here, when the delete flag is set (spec §4.2 step 4).
        if flags & FSP_CLEANUP_DELETE != 0 && !self.read_only {
            let _ = self.run(self.backend.remove(&context.path, context.is_dir));
            self.invalidate_changed_path(&context.path);
        }
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> AnchorResultFsp<()> {
        let meta = self.stat(&context.path)?;
        fill_file_info(file_info, &meta);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> AnchorResultFsp<u32> {
        let entries = self.list_cached(&context.path)?;
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let mut info: DirInfo<255> = DirInfo::new();
            for entry in &entries {
                info.reset();
                info.set_name(&entry.name)?;
                fill_file_info(info.file_info_mut(), &entry.metadata);
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> AnchorResultFsp<()> {
        self.deny_if_read_only()?;
        let from = to_path(file_name);
        let to = to_path(new_file_name);
        self.run(self.backend.rename(&from, &to))
            .map_err(|e| nt(&e))?;
        self.invalidate_changed_path(&from);
        self.invalidate_changed_path(&to);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> AnchorResultFsp<()> {
        self.deny_if_read_only()?;
        // Allocation-size changes are advisory; only act on real end-of-file changes.
        if !set_allocation_size {
            self.run(self.backend.set_len(&context.path, new_size))
                .map_err(|e| nt(&e))?;
            self.invalidate_changed_path(&context.path);
        }
        let meta = self.stat(&context.path).unwrap_or(RemoteMetadata {
            is_dir: false,
            len: new_size,
            modified: None,
        });
        fill_file_info(file_info, &meta);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> AnchorResultFsp<u32> {
        let want = buffer.len() as u32;
        // Cache hit: satisfy entirely from the current read-ahead window (spec §3.2).
        if let Some(data) = self.read_buf.get(&context.path, offset, want) {
            buffer[..data.len()].copy_from_slice(&data);
            return Ok(data.len() as u32);
        }
        // Miss: fetch an adaptive read-ahead window at this offset, cache it, and return
        // the requested slice.
        let fetch = self.read_buf.next_fetch_len(&context.path, offset, want);
        let chunk = self
            .run(self.backend.read(&context.path, offset, fetch as u32))
            .map_err(|e| nt(&e))?;
        self.read_buf
            .fill_with_request(&context.path, offset, chunk.clone(), fetch);
        let n = (want as usize).min(chunk.len());
        buffer[..n].copy_from_slice(&chunk[..n]);
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> AnchorResultFsp<u32> {
        self.deny_if_read_only()?;
        // write_to_eof means "append": resolve the current size as the offset.
        let at = if write_to_eof {
            self.stat(&context.path).map(|m| m.len).unwrap_or(offset)
        } else {
            offset
        };
        let n = self
            .run(self.backend.write(&context.path, at, buffer))
            .map_err(|e| nt(&e))?;
        self.invalidate_changed_path(&context.path);
        let meta = self.stat(&context.path).unwrap_or(RemoteMetadata {
            is_dir: false,
            len: at + n as u64,
            modified: None,
        });
        fill_file_info(file_info, &meta);
        Ok(n)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> AnchorResultFsp<()> {
        // Placeholder capacity — remote FTP/SFTP rarely report quota; this only drives the
        // Explorer disk-space bar, which is cosmetic here (spec §11).
        out_volume_info.total_size = 1 << 40; // 1 TiB
        out_volume_info.free_size = 1 << 39; // 512 GiB
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }
}

/// Local alias for the WinFsp result type.
type AnchorResultFsp<T> = std::result::Result<T, FspError>;

/// Translate WinFsp's wide path into a [`PathBuf`] the backends map onto their path space.
fn to_path(file_name: &U16CStr) -> PathBuf {
    PathBuf::from(file_name.to_os_string())
}

fn attributes(meta: &RemoteMetadata) -> u32 {
    if meta.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    }
}

fn fill_file_info(fi: &mut FileInfo, meta: &RemoteMetadata) {
    fi.file_attributes = attributes(meta);
    fi.file_size = meta.len;
    fi.allocation_size = meta.len.div_ceil(4096) * 4096;
    let ft = to_filetime(meta.modified);
    fi.creation_time = ft;
    fi.last_access_time = ft;
    fi.last_write_time = ft;
    fi.change_time = ft;
    fi.index_number = 0;
}

/// Convert a `SystemTime` to a Windows FILETIME (100 ns ticks since 1601-01-01).
fn to_filetime(t: Option<SystemTime>) -> u64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600; // 1601→1970 in seconds
    match t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) {
        Some(d) => (d.as_secs() + EPOCH_DIFF_SECS) * 10_000_000 + (d.subsec_nanos() as u64) / 100,
        None => 0,
    }
}

/// The coarse [`AnchorError`] → NTSTATUS mapping of spec §4.3.
fn nt(e: &AnchorError) -> FspError {
    let status = match e {
        AnchorError::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        AnchorError::PermissionDenied(_) => STATUS_ACCESS_DENIED,
        AnchorError::Connection(_) | AnchorError::Protocol(_) => STATUS_CONNECTION_DISCONNECTED,
        _ => STATUS_UNSUCCESSFUL,
    };
    // Use FspError's i32 NTSTATUS variant directly: winfsp links a different `windows`-crate
    // version than ours, so `From<NTSTATUS>` wouldn't apply across the version boundary.
    FspError::NTSTATUS(status.0)
}

/// Add WinFsp's `bin` directory (from the registry `InstallDir`) to the DLL search path so
/// `winfsp_init`'s `LoadLibraryW("winfsp-x64.dll")` resolves. WinFsp registers under
/// `HKLM\SOFTWARE\WinFsp` (64-bit) or `HKLM\SOFTWARE\WOW6432Node\WinFsp` (32-bit installer).
fn add_winfsp_to_dll_search_path() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    for subkey in ["SOFTWARE\\WinFsp", "SOFTWARE\\WOW6432Node\\WinFsp"] {
        let sub: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = [0u16; 512];
        let mut size = std::mem::size_of_val(&buf) as u32;
        // SAFETY: `sub` is NUL-terminated; `buf`/`size` describe a valid output buffer.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(sub.as_ptr()),
                w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                Some(&mut size),
            )
        };
        if rc == ERROR_SUCCESS {
            let chars = (size as usize / 2).saturating_sub(1); // drop trailing NUL
            let mut dir: Vec<u16> = buf[..chars].to_vec();
            dir.extend("bin".encode_utf16()); // InstallDir ends with a backslash
            dir.push(0);
            // SAFETY: `dir` is a valid NUL-terminated wide string for the duration of the call.
            unsafe {
                let _ = SetDllDirectoryW(PCWSTR(dir.as_ptr()));
            }
            return;
        }
    }
}

/// Whether WinFsp initialized successfully; computed once on first mount. We use the
/// non-fatal `winfsp_init()` (not `winfsp_init_or_die`, which silently `process::exit`s) so a
/// missing/locating failure surfaces as a normal error the CLI/tray can report.
static WINFSP_INIT: OnceLock<bool> = OnceLock::new();

/// Attach `backend` to the connection's drive letter and return a stop-handle (spec §4.5).
pub fn mount(
    conn: &ConnectionConfig,
    backend: Arc<dyn RemoteFs>,
    runtime: tokio::runtime::Handle,
) -> AnchorResult<StopHandle> {
    let initialized = *WINFSP_INIT.get_or_init(|| {
        add_winfsp_to_dll_search_path();
        winfsp::winfsp_init().is_ok()
    });
    if !initialized {
        return Err(AnchorError::Other(
            "WinFsp could not be initialized — is WinFsp installed? (https://winfsp.dev)".into(),
        ));
    }

    let mut params = VolumeParams::new();
    params
        .filesystem_name("Anchor")
        .sector_size(512)
        .sectors_per_allocation_unit(1)
        .volume_serial_number(rand_serial(conn))
        .file_info_timeout(1000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .post_cleanup_when_modified_only(true);
    if conn.read_only {
        params.read_only_volume(true);
    }

    let context = AnchorFsContext {
        backend,
        runtime,
        dir_cache: DirCache::new(Duration::from_secs(conn.dir_cache_ttl_secs)),
        stat_cache: StatCache::new(Duration::from_secs(conn.dir_cache_ttl_secs)),
        read_buf: ReadAheadBuffer::new(),
        read_only: conn.read_only,
        label: conn.name.clone(),
    };

    // Pin the FineGuard strategy: AnchorFsContext is both Send and Sync, so `start()` would
    // otherwise be ambiguous between the FineGuard (Sync) and CoarseGuard (Send) impls.
    let mut host: FileSystemHost<AnchorFsContext, FineGuard> = FileSystemHost::new(params, context)
        .map_err(|e| AnchorError::Other(format!("WinFsp host creation failed: {e}")))?;
    host.mount(conn.drive_letter.clone()).map_err(|e| {
        AnchorError::Other(format!("WinFsp mount at {} failed: {e}", conn.drive_letter))
    })?;
    host.start()
        .map_err(|e| AnchorError::Other(format!("WinFsp dispatcher start failed: {e}")))?;

    // The stop-handle owns the running host; calling it unmounts and stops the dispatcher.
    let stop: StopHandle = Box::new(move || {
        let mut host = host;
        host.unmount();
        host.stop();
        Ok(())
    });
    Ok(stop)
}

/// A cheap, stable-ish volume serial derived from the connection name.
fn rand_serial(conn: &ConnectionConfig) -> u32 {
    let mut h: u32 = 2166136261;
    for b in conn.name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h
}
