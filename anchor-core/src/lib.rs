//! `anchor-core` — protocol- and WinFsp-agnostic core of Anchor (spec §3, §6).
//!
//! This crate has **zero** knowledge of WinFsp or any wire protocol. It defines the
//! [`RemoteFs`] contract every backend implements, the [`MountManager`] state machine the
//! UIs drive, configuration ([`AnchorConfig`]), credential storage ([`CredentialStore`]),
//! and the two caches that sit between WinFsp callbacks and the network. `anchor-fs`
//! depends on this crate and implements its trait; the UIs depend on it and only ever see
//! `Arc<dyn RemoteFs>` and `MountManager`.

pub mod cache;
pub mod config;
pub mod credentials;
pub mod error;
pub mod host_keys;
pub mod mount;
pub mod remote_fs;

pub use cache::{DirCache, ReadAheadBuffer, StatCache, READAHEAD_CHUNK};
pub use config::{AnchorConfig, ConnectionConfig, Protocol};
pub use credentials::{CredentialStore, Secret, Secrets};
pub use error::{AnchorError, Result};
pub use host_keys::HostKeyStore;
pub use mount::{BackendBuilder, MountManager, MountState, Mounter, StopHandle};
pub use remote_fs::{DirEntry, RemoteFs, RemoteMetadata};
