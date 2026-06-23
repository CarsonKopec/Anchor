//! `MountManager` — the in-memory state of all active mounts, and the single object both
//! `anchor-tray` and `anchor-cli` drive (spec §3.3, §7).
//!
//! It knows nothing about WinFsp or any protocol. The pieces that *do* (building a
//! backend, attaching it to a drive letter) are injected as closures by whoever
//! constructs the manager — in practice the CLI/tray, using `anchor-fs`. This is the
//! concrete reconciliation of §3.3 ("hands it a closure that does the actual WinFsp
//! attach and returns a stop-handle") with the §7 lifecycle (lookup → ensure letter free
//! → connect → build → mount → Mounted/Failed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;

use crate::config::{AnchorConfig, ConnectionConfig};
use crate::credentials::{Secret, Secrets};
use crate::error::{AnchorError, Result};
use crate::remote_fs::RemoteFs;

/// Lifecycle state of one connection (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    /// Not mounted.
    Unmounted,
    /// Backend is being built / drive is being attached.
    Connecting,
    /// Live at this drive letter.
    Mounted { drive_letter: String },
    /// Transport dropped; a reconnect is expected (set by background logic, not by
    /// `MountManager` itself — see "deliberately not responsible for" in §3.3).
    Reconnecting,
    /// The last mount attempt failed; `reason` is user-facing.
    Failed { reason: String },
}

/// Opaque "thing that can stop a mount" (spec §4.5). `anchor-fs::mount` produces one of
/// these; `MountManager` stores it and calls it on unmount, never knowing it wraps a
/// WinFsp `FileSystemHost`.
pub type StopHandle = Box<dyn FnOnce() -> Result<()> + Send>;

/// Builds a connected backend from a config + its secret. Injected by `anchor-fs`.
pub type BackendBuilder =
    Arc<dyn Fn(&ConnectionConfig, &Secret) -> Result<Arc<dyn RemoteFs>> + Send + Sync>;

/// Attaches a backend to a drive letter via WinFsp and returns a stop-handle. Injected by
/// `anchor-fs`.
pub type Mounter =
    Arc<dyn Fn(&ConnectionConfig, Arc<dyn RemoteFs>, Handle) -> Result<StopHandle> + Send + Sync>;

struct ActiveMount {
    drive_letter: String,
    backend: Arc<dyn RemoteFs>,
    stop: StopHandle,
}

/// Owns mount state for every configured connection and drives the mount lifecycle.
pub struct MountManager {
    runtime: Handle,
    secrets: Arc<dyn Secrets>,
    backend_builder: BackendBuilder,
    mounter: Mounter,
    config: Mutex<AnchorConfig>,
    states: Mutex<HashMap<String, MountState>>,
    active: Mutex<HashMap<String, ActiveMount>>,
    /// Serializes mount/unmount operations so drive-letter checks can't race.
    op_lock: Mutex<()>,
}

impl MountManager {
    /// Construct a manager. `runtime` is the tokio handle backends' async calls block on
    /// (spec §4.4); `secrets` is the credential source ([`crate::credentials::CredentialStore`]
    /// in production); `backend_builder`/`mounter` are supplied by `anchor-fs`.
    pub fn new(
        runtime: Handle,
        secrets: Arc<dyn Secrets>,
        config: AnchorConfig,
        backend_builder: BackendBuilder,
        mounter: Mounter,
    ) -> Self {
        let states = config
            .connections()
            .map(|c| (c.name.clone(), MountState::Unmounted))
            .collect();
        MountManager {
            runtime,
            secrets,
            backend_builder,
            mounter,
            config: Mutex::new(config),
            states: Mutex::new(states),
            active: Mutex::new(HashMap::new()),
            op_lock: Mutex::new(()),
        }
    }

    /// Replace the in-memory config (e.g. after `anchor add` edited the file). New
    /// connections start `Unmounted`; states for connections that are still mounted are
    /// preserved; vanished connections are dropped (unmount them first).
    pub fn reload_config(&self, config: AnchorConfig) {
        let mut states = self.states.lock().unwrap();
        let active = self.active.lock().unwrap();
        let mut next = HashMap::new();
        for c in config.connections() {
            let state = if let Some(m) = active.get(&c.name) {
                MountState::Mounted {
                    drive_letter: m.drive_letter.clone(),
                }
            } else {
                states
                    .get(&c.name)
                    .cloned()
                    .unwrap_or(MountState::Unmounted)
            };
            next.insert(c.name.clone(), state);
        }
        *states = next;
        *self.config.lock().unwrap() = config;
    }

    /// Snapshot the current state of `name`, if configured.
    pub fn status(&self, name: &str) -> Option<MountState> {
        self.states.lock().unwrap().get(name).cloned()
    }

    /// All connections and their current state, sorted by name. Used by the tray menu and
    /// `anchor list`.
    pub fn all_statuses(&self) -> Vec<(String, MountState)> {
        let states = self.states.lock().unwrap();
        let mut out: Vec<_> = states.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn set_state(&self, name: &str, state: MountState) {
        self.states.lock().unwrap().insert(name.to_string(), state);
    }

    fn connection(&self, name: &str) -> Result<ConnectionConfig> {
        self.config
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| AnchorError::Config(format!("no connection named '{name}'")))
    }

    /// Fail if `drive_letter` is already claimed by a *different* active mount (spec §3.3,
    /// the mechanism behind "multiple simultaneous mounts").
    pub fn ensure_drive_letter_free(&self, drive_letter: &str, except: &str) -> Result<()> {
        let active = self.active.lock().unwrap();
        let owns_letter = active
            .iter()
            .any(|(name, m)| name == except && m.drive_letter.eq_ignore_ascii_case(drive_letter));
        for (name, m) in active.iter() {
            if name != except && m.drive_letter.eq_ignore_ascii_case(drive_letter) {
                return Err(AnchorError::Config(format!(
                    "drive letter {drive_letter} is already in use by '{name}'"
                )));
            }
        }
        drop(active);

        if !owns_letter && windows_drive_letter_in_use(drive_letter) {
            return Err(AnchorError::Config(format!(
                "drive letter {drive_letter} is already in use by Windows"
            )));
        }
        Ok(())
    }

    /// Mount one connection, running the §7 sequence. Idempotent-ish: mounting an
    /// already-active connection is an error rather than a silent no-op.
    pub fn mount(&self, name: &str) -> Result<()> {
        let _op = self.op_lock.lock().unwrap();

        if self.active.lock().unwrap().contains_key(name) {
            return Err(AnchorError::Other(format!("'{name}' is already mounted")));
        }

        let conn = self.connection(name)?;
        self.set_state(name, MountState::Connecting);

        // Any failure from here — including a drive-letter clash — records Failed{reason}
        // so the tray/CLI can show why the attempt didn't take (spec §7 step 7).
        let result = (|| -> Result<ActiveMount> {
            self.ensure_drive_letter_free(&conn.drive_letter, name)?;
            let secret = self.secrets.retrieve(&conn.credential_key)?;
            let backend = (self.backend_builder)(&conn, &secret)?;
            let stop = (self.mounter)(&conn, backend.clone(), self.runtime.clone())?;
            Ok(ActiveMount {
                drive_letter: conn.drive_letter.clone(),
                backend,
                stop,
            })
        })();

        match result {
            Ok(active) => {
                let letter = active.drive_letter.clone();
                self.active.lock().unwrap().insert(name.to_string(), active);
                self.set_state(
                    name,
                    MountState::Mounted {
                        drive_letter: letter,
                    },
                );
                Ok(())
            }
            Err(e) => {
                self.set_state(
                    name,
                    MountState::Failed {
                        reason: e.to_string(),
                    },
                );
                Err(e)
            }
        }
    }

    /// Probe active mounts and update state to `Mounted` or `Reconnecting`.
    ///
    /// This is intentionally light-weight UI state, not a destructive remount. WinFsp still
    /// owns the drive; the backend is asked to reconnect its transport in place.
    pub fn check_active_mounts(&self) {
        const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

        let probes: Vec<_> = {
            let active = self.active.lock().unwrap();
            active
                .iter()
                .map(|(name, mount)| {
                    (
                        name.clone(),
                        mount.drive_letter.clone(),
                        mount.backend.clone(),
                    )
                })
                .collect()
        };

        for (name, drive_letter, backend) in probes {
            let healthy = self.runtime.block_on(async {
                match tokio::time::timeout(HEALTH_TIMEOUT, backend.is_connected()).await {
                    Ok(true) => true,
                    Ok(false) => tokio::time::timeout(HEALTH_TIMEOUT, backend.reconnect())
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false),
                    Err(_) => false,
                }
            });

            if healthy {
                self.set_state(&name, MountState::Mounted { drive_letter });
            } else {
                self.set_state(&name, MountState::Reconnecting);
            }
        }
    }

    /// Unmount one connection. Calling the stored stop-closure releases the WinFsp host.
    /// Unmounting something that isn't mounted just normalizes its state to `Unmounted`.
    pub fn unmount(&self, name: &str) -> Result<()> {
        let _op = self.op_lock.lock().unwrap();
        let active = self.active.lock().unwrap().remove(name);
        let res = match active {
            Some(m) => (m.stop)(),
            None => Ok(()),
        };
        self.set_state(name, MountState::Unmounted);
        res
    }

    /// Unmount every active mount (tray exit, CLI `unmount-all`). Ensures no WinFsp mount
    /// survives process exit. Returns the first error encountered, after attempting all.
    pub fn unmount_all(&self) -> Result<()> {
        let _op = self.op_lock.lock().unwrap();
        let drained: Vec<(String, ActiveMount)> = self.active.lock().unwrap().drain().collect();
        let mut first_err = None;
        for (name, m) in drained {
            if let Err(e) = (m.stop)() {
                first_err.get_or_insert(e);
            }
            self.set_state(&name, MountState::Unmounted);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Names of currently-active (mounted) connections.
    pub fn active_names(&self) -> Vec<String> {
        self.active.lock().unwrap().keys().cloned().collect()
    }

    /// Borrow a clone of the current config (for the tray's "open config file" etc.).
    pub fn config_snapshot(&self) -> AnchorConfig {
        self.config.lock().unwrap().clone()
    }
}

fn drive_letter_index(drive_letter: &str) -> Option<u32> {
    let mut chars = drive_letter.chars();
    let letter = chars.next()?;
    if chars.next()? != ':' || chars.next().is_some() || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(letter.to_ascii_uppercase() as u32 - 'A' as u32)
}

fn drive_letter_in_mask(drive_letter: &str, mask: u32) -> bool {
    drive_letter_index(drive_letter)
        .map(|index| mask & (1 << index) != 0)
        .unwrap_or(false)
}

#[cfg(all(windows, not(test)))]
fn windows_drive_letter_in_use(drive_letter: &str) -> bool {
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    mask != 0 && drive_letter_in_mask(drive_letter, mask)
}

#[cfg(any(not(windows), test))]
fn windows_drive_letter_in_use(_drive_letter: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionConfig, Protocol};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// In-memory secret source — no Credential Manager needed.
    struct FakeSecrets;
    impl Secrets for FakeSecrets {
        fn retrieve(&self, key: &str) -> Result<Secret> {
            Ok(Secret::new(format!("secret-for-{key}")))
        }
    }

    /// A no-op backend that satisfies the trait for state-machine tests.
    struct NullBackend {
        label: String,
        connected: Arc<AtomicBool>,
    }

    impl NullBackend {
        fn new(label: impl Into<String>) -> Self {
            NullBackend {
                label: label.into(),
                connected: Arc::new(AtomicBool::new(true)),
            }
        }

        fn with_health(label: impl Into<String>, connected: Arc<AtomicBool>) -> Self {
            NullBackend {
                label: label.into(),
                connected,
            }
        }
    }

    #[async_trait::async_trait]
    impl RemoteFs for NullBackend {
        fn label(&self) -> &str {
            &self.label
        }
        async fn stat(&self, _: &Path) -> Result<crate::remote_fs::RemoteMetadata> {
            Ok(crate::remote_fs::RemoteMetadata::dir())
        }
        async fn list_dir(&self, _: &Path) -> Result<Vec<crate::remote_fs::DirEntry>> {
            Ok(vec![])
        }
        async fn read(&self, _: &Path, _: u64, _: u32) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn write(&self, _: &Path, _: u64, d: &[u8]) -> Result<u32> {
            Ok(d.len() as u32)
        }
        async fn create(&self, _: &Path, _: bool) -> Result<()> {
            Ok(())
        }
        async fn remove(&self, _: &Path, _: bool) -> Result<()> {
            Ok(())
        }
        async fn rename(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
        async fn set_len(&self, _: &Path, _: u64) -> Result<()> {
            Ok(())
        }
        async fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        async fn reconnect(&self) -> Result<()> {
            if self.connected.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(AnchorError::Connection("still offline".into()))
            }
        }
    }

    fn conn(name: &str, letter: &str) -> ConnectionConfig {
        ConnectionConfig {
            name: name.to_string(),
            protocol: Protocol::Sftp,
            host: "h".into(),
            port: 22,
            username: "u".into(),
            credential_key: name.to_string(),
            drive_letter: letter.to_string(),
            remote_path: "/".into(),
            read_only: false,
            dir_cache_ttl_secs: 10,
            auto_mount_on_start: false,
        }
    }

    fn manager(conns: Vec<ConnectionConfig>) -> (MountManager, Arc<AtomicUsize>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        // Keep the runtime alive for the life of the manager.
        std::mem::forget(rt);

        let stops = Arc::new(AtomicUsize::new(0));
        let stops_for_mounter = stops.clone();

        let builder: BackendBuilder =
            Arc::new(|c, _s| Ok(Arc::new(NullBackend::new(c.name.clone())) as Arc<dyn RemoteFs>));
        let mounter: Mounter = Arc::new(move |_c, _b, _h| {
            let stops = stops_for_mounter.clone();
            let stop: StopHandle = Box::new(move || {
                stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            Ok(stop)
        });

        let cfg = AnchorConfig::from_connections(conns);
        let mgr = MountManager::new(handle, Arc::new(FakeSecrets), cfg, builder, mounter);
        (mgr, stops)
    }

    #[test]
    fn mount_then_unmount_transitions_and_calls_stop() {
        let (mgr, stops) = manager(vec![conn("a", "M:")]);
        assert_eq!(mgr.status("a"), Some(MountState::Unmounted));

        mgr.mount("a").unwrap();
        assert_eq!(
            mgr.status("a"),
            Some(MountState::Mounted {
                drive_letter: "M:".into()
            })
        );

        mgr.unmount("a").unwrap();
        assert_eq!(mgr.status("a"), Some(MountState::Unmounted));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn duplicate_drive_letter_is_rejected() {
        let (mgr, _) = manager(vec![conn("a", "M:"), conn("b", "M:")]);
        mgr.mount("a").unwrap();
        let err = mgr.mount("b").unwrap_err();
        assert!(matches!(err, AnchorError::Config(_)));
        assert!(matches!(mgr.status("b"), Some(MountState::Failed { .. })));
        // 'a' is unaffected.
        assert_eq!(
            mgr.status("a"),
            Some(MountState::Mounted {
                drive_letter: "M:".into()
            })
        );
    }

    #[test]
    fn same_letter_ok_once_first_is_unmounted() {
        let (mgr, _) = manager(vec![conn("a", "M:"), conn("b", "M:")]);
        mgr.mount("a").unwrap();
        mgr.unmount("a").unwrap();
        mgr.mount("b").unwrap(); // letter now free
        assert_eq!(
            mgr.status("b"),
            Some(MountState::Mounted {
                drive_letter: "M:".into()
            })
        );
    }

    #[test]
    fn unmount_all_stops_everything() {
        let (mgr, stops) = manager(vec![conn("a", "M:"), conn("b", "L:")]);
        mgr.mount("a").unwrap();
        mgr.mount("b").unwrap();
        mgr.unmount_all().unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 2);
        assert_eq!(mgr.status("a"), Some(MountState::Unmounted));
        assert_eq!(mgr.status("b"), Some(MountState::Unmounted));
    }

    #[test]
    fn mounting_twice_errors() {
        let (mgr, _) = manager(vec![conn("a", "M:")]);
        mgr.mount("a").unwrap();
        assert!(mgr.mount("a").is_err());
    }

    #[test]
    fn drive_letter_mask_detection_handles_valid_letters() {
        assert_eq!(drive_letter_index("A:"), Some(0));
        assert_eq!(drive_letter_index("M:"), Some(12));
        assert_eq!(drive_letter_index("z:"), Some(25));
        assert_eq!(drive_letter_index("M"), None);
        assert_eq!(drive_letter_index("MM:"), None);

        let mask = (1 << 0) | (1 << 12);
        assert!(drive_letter_in_mask("A:", mask));
        assert!(drive_letter_in_mask("M:", mask));
        assert!(!drive_letter_in_mask("Z:", mask));
    }

    #[test]
    fn health_check_marks_reconnecting_then_restores_mounted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        std::mem::forget(rt);

        let connected = Arc::new(AtomicBool::new(false));
        let connected_for_builder = connected.clone();
        let builder: BackendBuilder = Arc::new(move |c, _s| {
            Ok(Arc::new(NullBackend::with_health(
                c.name.clone(),
                connected_for_builder.clone(),
            )) as Arc<dyn RemoteFs>)
        });
        let mounter: Mounter = Arc::new(move |_c, _b, _h| {
            let stop: StopHandle = Box::new(|| Ok(()));
            Ok(stop)
        });
        let mgr = MountManager::new(
            handle,
            Arc::new(FakeSecrets),
            AnchorConfig::from_connections(vec![conn("a", "M:")]),
            builder,
            mounter,
        );

        mgr.mount("a").unwrap();
        mgr.check_active_mounts();
        assert_eq!(mgr.status("a"), Some(MountState::Reconnecting));

        connected.store(true, Ordering::SeqCst);
        mgr.check_active_mounts();
        assert_eq!(
            mgr.status("a"),
            Some(MountState::Mounted {
                drive_letter: "M:".into()
            })
        );
    }
}
