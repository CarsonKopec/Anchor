# ANCHOR_SPEC.md

**Anchor** — mounts FTP/SFTP servers as real Windows drive letters, runs in the background from the system tray.

Status: design + initial implementation scaffolded, not yet compiled/verified on Windows.
Version: 0.1.0 (pre-release)

---

## 1. Purpose & Scope

Anchor exists to do one thing RaiDrive does, scoped down: mount a remote FTP/SFTP server as a drive letter on Windows, backed by a real kernel-mode filesystem driver (WinFsp) rather than a sync-to-local-folder model. No GUI account wizard, no fifteen supported cloud providers — just FTP/FTPS/SFTP, configured via a TOML+ file, controlled from a tray icon or a CLI.

**In scope (v1):**
- Mount FTP, FTPS (explicit TLS), and SFTP as a Windows drive letter
- Multiple simultaneous mounts, each its own drive letter
- Full read-write (with FTP's caveats — see §7)
- Background tray app with per-connection mount/unmount menu
- Headless CLI for the same operations (scripting, server-core boxes)
- Credentials stored in Windows Credential Manager, never in the config file
- Config in TOML+ (`.tomlp`) with validation annotations

**Out of scope (v1):**
- WebDAV, S3, cloud-provider APIs (architecture allows adding them later — see §3.4)
- Linux/macOS support (WinFsp is Windows-only; a v2 could target FUSE for parity)
- Sharing a mount between multiple OS users
- Bandwidth throttling, scheduled mounts, conflict resolution UI

---

## 2. High-Level Architecture

```
┌─────────────────┐     ┌─────────────────┐
│   anchor-tray    │     │   anchor-cli    │
│  (background     │     │  (headless,     │
│   GUI shell)     │     │   scriptable)   │
└────────┬─────────┘     └────────┬────────┘
         │                        │
         └───────────┬────────────┘
                      │  both call the same API
                      ▼
            ┌──────────────────┐
            │   anchor-core     │
            │ ─────────────────│
            │ RemoteFs (trait)  │
            │ MountManager      │
            │ AnchorConfig      │
            │ CredentialStore   │
            │ DirCache /        │
            │  ReadAheadBuffer  │
            └─────────┬─────────┘
                      │  trait calls only — no protocol
                      │  or WinFsp knowledge here
                      ▼
            ┌──────────────────┐
            │    anchor-fs      │
            │ ─────────────────│
            │ WinFsp glue       │
            │  (AnchorFsContext)│
            │ FtpBackend        │
            │ SftpBackend       │
            └──────────────────┘
                      │
                      ▼
              WinFsp kernel driver
                      │
                      ▼
            Windows drive letter (M:, L:, ...)
```

**Dependency direction is one-way**: `anchor-core` has zero knowledge of WinFsp or any wire protocol. `anchor-fs` depends on `anchor-core` and implements its trait, plus owns the only WinFsp-touching code in the workspace. `anchor-tray` and `anchor-cli` depend on both but never touch a protocol or WinFsp call directly — they only call `MountManager` methods and render/report state.

This shape is what makes a third backend (WebDAV, S3, a NAS-specific protocol) a contained addition: one new file in `anchor-fs`, one new `Protocol` enum variant, one new match arm. Nothing in `anchor-core`, `anchor-tray`, or `anchor-cli` changes.

---

## 3. Core Abstractions (`anchor-core`)

### 3.1 The `RemoteFs` trait

Every backend — FTP, SFTP, and anything added later — implements this. It is the only contract `anchor-fs`'s WinFsp glue depends on.

```rust
#[async_trait]
pub trait RemoteFs: Send + Sync {
    fn label(&self) -> &str;
    async fn stat(&self, path: &Path) -> Result<RemoteMetadata>;
    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>>;
    async fn read(&self, path: &Path, offset: u64, len: u32) -> Result<Vec<u8>>;
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u32>;
    async fn create(&self, path: &Path, is_dir: bool) -> Result<()>;
    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()>;
    async fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    async fn set_len(&self, path: &Path, len: u64) -> Result<()>;
    async fn is_connected(&self) -> bool;
    async fn reconnect(&self) -> Result<()>;
}
```

Design notes:
- One backend instance per mount, not shared across mounts. `Send + Sync` is required because WinFsp's dispatcher calls in from multiple threads concurrently, but no instance is assumed safe to share between two *different* drive letters.
- `read`/`write` take an explicit `offset` — this is what lets the cache layer (§3.2) and the WinFsp glue (§4) implement seeking and partial reads without each backend reimplementing that logic. The cost is pushed onto SFTP (which handles it natively) and FTP (which has to fake it via REST — see §7).
- No method returns a raw protocol error type. Everything funnels through `AnchorError`, so `anchor-fs`'s WinFsp glue has exactly one error type to translate into NTSTATUS codes (§4.3), regardless of which backend raised it.

### 3.2 Caching layer

Two independent caches sit between WinFsp callbacks and the network:

**`DirCache`** — TTL-keyed directory listing cache.
- Rationale: Explorer re-lists directories constantly (icon overlays, thumbnail generation, live preview panes), and a LIST/readdir round-trip is the single biggest latency cost observed in similar tools (rclone mount, RaiDrive). A short TTL (default 10s, configurable per-connection via `dir_cache_ttl_secs`) absorbs this without meaningfully delaying visibility of remote-side changes.
- Invalidation: any local write, create, remove, or rename calls `invalidate_parent()` on the affected directory immediately, so the user's own actions are never delayed by the TTL — only changes made by *other* clients to the same remote path can lag by up to the TTL window.

**`ReadAheadBuffer`** — per-file sequential chunk cache.
- Rationale: most real read patterns from Explorer/apps are sequential (file preview, copy, media playback start), so satisfying each WinFsp `read` 1:1 against the network would mean one round-trip per buffer-sized chunk, which is far slower than necessary.
- Chunk size: 256 KiB (`READAHEAD_CHUNK`), chosen as a balance — large enough to amortize round-trip latency, small enough that a single seek into a large file (e.g. scrubbing a video) doesn't fetch megabytes of data that get thrown away.
- Cache key is per-path, single-chunk (not a generalized LRU of arbitrary chunks per file). This is intentionally simple for v1; a multi-chunk or windowed cache would help random-access-heavy workloads but adds real complexity for a benefit that doesn't matter for the typical "open file, read it" pattern this tool targets.
- Invalidated on any write to that path.

### 3.3 `MountManager`

Owns the in-memory state of all active mounts and is the single object both `anchor-tray` and `anchor-cli` drive. It does **not** know about WinFsp or any protocol — `anchor-fs` hands it a closure (`mount_fn`) that does the actual WinFsp attach and returns a stop-handle; `MountManager` just tracks state and calls that closure.

```rust
pub enum MountState {
    Unmounted,
    Connecting,
    Mounted { drive_letter: String },
    Reconnecting,
    Failed { reason: String },
}
```

Responsibilities:
- Tracks `MountState` per connection name.
- Enforces drive-letter uniqueness across simultaneously active mounts (`ensure_drive_letter_free`) — this is the concrete mechanism behind the "multiple simultaneous mounts" requirement; two connections configured with the same `drive_letter` will mount fine individually but the second `mount()` call against an already-claimed letter fails with a clear `AnchorError::Config`.
- `unmount_all()` is called on tray-app exit and CLI `unmount-all`, ensuring no orphaned WinFsp mounts survive process exit.

It is deliberately **not** responsible for: scheduling reconnects, persisting state across restarts (state is rebuilt from `auto_mount_on_start` config on each launch), or UI rendering.

### 3.4 Adding a backend later (extension point)

To add WebDAV (as the most likely v1.1 candidate):
1. Add `Protocol::WebDav` to the enum in `anchor-core::config`.
2. Implement `RemoteFs` for a new `WebDavBackend` in `anchor-fs/src/webdav.rs`.
3. Add a match arm in `anchor_fs::build_backend`.
4. Add the relevant fields to `ConnectionConfig` if WebDAV needs config that FTP/SFTP don't (e.g. a base URL instead of host+port).

No changes needed in `MountManager`, the WinFsp glue, the tray app, or the CLI — they only ever see `Arc<dyn RemoteFs>`.

---

## 4. Filesystem Glue (`anchor-fs`)

### 4.1 Why WinFsp

WinFsp is the kernel-mode filesystem driver that makes a *real* drive letter possible — the same approach RaiDrive, rclone's `mount` command, and SSHFS-Win all use. The alternative (a synced local folder) was explicitly rejected per the design discussion that produced this spec: a synced folder isn't a "mount," it has its own consistency and staleness problems, and doesn't match what was asked for.

WinFsp is not bundled with Anchor; it must be installed separately on the target machine (https://winfsp.dev). The installer/first-run experience should detect its absence and prompt before attempting a mount.

### 4.2 `AnchorFsContext`

Implements WinFsp's `FileSystemContext` trait. One instance per mounted drive. Holds:
- `backend: Arc<dyn RemoteFs>` — the actual connection
- `runtime: tokio::runtime::Handle` — see §4.4
- `dir_cache: DirCache`, `read_buf: ReadAheadBuffer` — sized/TTL'd from that connection's config
- `read_only: bool`

Each WinFsp callback (`open`, `read`, `write`, `read_directory`, `rename`, `create`, `cleanup`, `set_file_size`, `get_security_by_name`, `get_volume_info`) is implemented by:
1. Translating the WinFsp-supplied Windows path (`U16CStr`) to a `PathBuf`.
2. Checking the relevant cache first (`read_directory` checks `DirCache`; `read` checks `ReadAheadBuffer`).
3. On a cache miss, calling the corresponding `RemoteFs` method via `self.run(...)` (§4.4).
4. On any mutation (`write`, `create`, `remove` via `cleanup`, `rename`), invalidating the relevant cache entries.
5. Translating the `AnchorError` result into the WinFsp-expected `Result`/NTSTATUS (§4.3).

### 4.3 Error translation

| `AnchorError` variant | NTSTATUS |
|---|---|
| `NotFound` | `STATUS_OBJECT_NAME_NOT_FOUND` |
| `PermissionDenied` | `STATUS_ACCESS_DENIED` |
| `Connection` / `Protocol` | `STATUS_CONNECTION_DISCONNECTED` |
| everything else | `STATUS_UNSUCCESSFUL` |

This is intentionally coarse for v1. A finer mapping (e.g. distinguishing a stale handle from a genuine disconnect) is possible but wasn't worth the complexity before real-world testing shows which distinctions actually matter to calling applications.

### 4.4 Sync/async bridging

WinFsp's callbacks are synchronous, called from WinFsp's own dispatcher thread pool. `RemoteFs` is async (because both `suppaftp` and `russh`/`russh-sftp` are async-native). The bridge is `AnchorFsContext::run()`, which calls `self.runtime.block_on(fut)` — blocking the calling WinFsp dispatcher thread until the async operation completes.

This is the same pattern rclone's Go-based WinFsp mount uses (blocking a goroutine instead of a thread, but the same shape). The tradeoff: a slow network call blocks one dispatcher thread, not the whole mount, because WinFsp gives each mount its own thread pool — so one connection's bad network day doesn't stall a *different* mounted drive, only concurrent operations on *that* drive beyond the thread pool size.

### 4.5 `mount()` / stop-handle contract

```rust
pub fn mount(
    conn: &ConnectionConfig,
    backend: Arc<dyn RemoteFs>,
    runtime: tokio::runtime::Handle,
) -> Result<Box<dyn FnOnce() -> Result<()> + Send>>
```

Returns a boxed closure that, when called, stops and unmounts the WinFsp host. `MountManager` stores this closure and invokes it on `unmount()`. This indirection is what lets `anchor-core` hold a "thing that can stop a mount" without depending on `winfsp` or knowing what a `FileSystemHost` is.

---

## 5. Protocol Backends

### 5.1 SFTP (`anchor-fs/src/sftp.rs`)

Built on `russh` (SSH transport) + `russh-sftp` (SFTP subsystem). The straightforward backend — SFTP has native random-access read/write, proper `rename`/`mkdir`/`rmdir`, and no LIST-format ambiguity.

One control connection (`russh::client::Handle`) per backend instance, lazily established on first use and cached in a `Mutex<Option<...>>`. `reconnect()` drops the cached session and re-establishes on next access.

**Known gap, flagged for fix before any real-network use:** `check_server_key` currently returns `Ok(true)` unconditionally (accept-all). This needs to become TOFU (trust-on-first-use) host key pinning — store the fingerprint in `connections.tomlp` on first successful connect, and refuse/warn on mismatch thereafter, the way OpenSSH's `known_hosts` works. This is explicitly called out in the source comment and the README; it is the single most important hardening item before relying on Anchor over any network you don't fully control.

### 5.2 FTP/FTPS (`anchor-fs/src/ftp.rs`)

Built on `suppaftp`. FTP is a structurally worse fit for "mounted drive" semantics than SFTP, for protocol reasons that can't be designed around:

- **No random-access write.** FTP has no equivalent to SFTP's `pwrite`. Anchor's FTP backend uses `REST` (resume) + `STOR`/`APPE` to support sequential writes, appends, and full-file rewrites — but **cannot** correctly perform an in-place overwrite of a byte range in the middle of an already-written file. `set_len()` similarly only supports truncation to zero; arbitrary resize returns an error.
- **No standardized LIST format.** Parsing assumes Unix `ls -l`-style output (`parse_list_line`), which covers the large majority of real servers (vsftpd, ProFTPD, Pure-FTPd). Exotic or DOS-style FTP servers are not handled in v1; MLSD (a standardized alternative) would be the v1.1 fix.
- **Directory detection is a heuristic.** `stat()` determines `is_dir` by attempting a `CWD` into the path and checking success, then restoring the previous working directory. This works but costs an extra round trip per stat call relative to SFTP's single `lstat`.

Anchor still supports FTP because plenty of legacy infrastructure only speaks it, but the tray UI and documentation should steer users toward SFTP whenever the target server supports both.

---

## 6. Configuration (`anchor-core/src/config.rs`)

### 6.1 File location & format

`%APPDATA%\Anchor\connections.tomlp` — TOML+, validated via the `tomlplus` crate's annotation system (`@required`, `@type`, `@min`/`@max`, `@pattern`, `@enum`) at load time. A malformed entry (bad port range, missing `credential_key`, invalid drive-letter pattern) fails fast with a message naming the offending key, rather than surfacing as a confusing runtime error mid-mount.

### 6.2 Schema

```toml
[connections.<name>]
protocol        = "ftp" | "ftps" | "sftp"     # @required, @enum
host            = "<string>"                   # @required
port            = <int 1-65535>                # default 22
username        = "<string>"                   # @required
credential_key  = "<string>"                   # @required — key into Credential Manager, NEVER the secret
drive_letter    = "<X:>"                        # @pattern "^[A-Z]:$"
remote_path     = "<string>"                   # default "/"
read_only       = <bool>                        # default false
dir_cache_ttl_secs = <int 1-3600>               # default 10
auto_mount_on_start = <bool>                    # default false
```

See `anchor-core/src/example_connections.tomlp` for a fully annotated two-connection example (one SFTP, one FTPS).

### 6.3 Credentials — explicitly not in this file

`credential_key` is a *reference*, not a secret. The actual password (or SFTP key passphrase, if key-based auth is added later) lives in Windows Credential Manager under target name `Anchor:<credential_key>`, written/read/deleted through `anchor-core::credentials::CredentialStore`, which wraps the Win32 `CredWriteW`/`CredReadW`/`CredDeleteW` APIs directly (no third-party credential crate dependency).

This split is what makes it safe to back up, version-control (in a private repo), or sync `connections.tomlp` across machines without that file alone being enough to compromise any connection — whoever has the file still needs Credential Manager access on the *same Windows account* on the *same machine* to get the actual secret out, since Credential Manager entries are scoped to user+machine by Windows itself, not by Anchor.

---

## 7. Mount Lifecycle

```
                    anchor add (CLI) / hand-edit .tomlp
                                │
                                ▼
                    anchor set-password (writes to Credential Manager)
                                │
                                ▼
                    anchor test (optional: stat("/") without mounting)
                                │
                                ▼
              ┌──────────────────────────────────┐
              │  MountManager::mount(name, fn)     │
              │  1. look up ConnectionConfig       │
              │  2. ensure drive_letter not in use │
              │  3. state -> Connecting            │
              │  4. CredentialStore::retrieve()    │
              │  5. build_backend() -> Arc<RemoteFs>│
              │  6. anchor_fs::mount() -> WinFsp   │
              │  7. state -> Mounted{drive_letter} │
              │     or -> Failed{reason}            │
              └──────────────────────────────────┘
                                │
                       drive letter live in Explorer
                                │
                                ▼
              MountManager::unmount(name) — calls stored
              stop-closure, which calls host.stop()/unmount(),
              state -> Unmounted
```

On process exit (tray app `LoopDestroyed`, or CLI `unmount-all`), `MountManager::unmount_all()` runs the stop-closure for every active mount, so no WinFsp mount is left dangling after Anchor itself exits.

On tray-app startup, any connection with `auto_mount_on_start = true` is mounted automatically before the tray icon's event loop starts handling clicks.

---

## 8. Tray App (`anchor-tray`)

A console-less Windows GUI-subsystem binary (`#![windows_subsystem = "windows"]`) — launching it shows nothing but the tray icon, no terminal flash, satisfying the "runs in the background" requirement directly.

Menu is rebuilt from `MountManager::all_statuses()` on every menu-event and on a 500ms timer tick (to reflect state changes from background reconnect logic, not just direct clicks):

```
Anchor
├── media-box      [M:] ●        (click → unmount)
├── lanl-scratch   [unmounted]   (click → mount)
├── ─────────────
├── Open config file
├── Unmount All
└── Quit Anchor
```

Each menu item's ID encodes its action as a string (`mount:<name>` / `unmount:<name>`), parsed back out in the event handler — avoids needing a side-table mapping menu IDs back to connection names.

Built on `tray-icon` 0.14 + `tao` 0.30 (these two crates version tightly together; see README for the coupling note before bumping either independently).

---

## 9. CLI (`anchor-cli`)

Headless equivalent for scripting and for machines with no desktop session (server-core). Subcommands:

| Command | Purpose |
|---|---|
| `anchor list` | Show all configured connections and current mount state |
| `anchor add <name> --protocol ... --host ... --username ... --drive-letter ...` | Create/update a connection in `connections.tomlp` |
| `anchor set-password <name>` | Prompt (hidden input) and store the password in Credential Manager |
| `anchor test <name>` | Connectivity check (`stat("/")`) without attaching a drive letter |
| `anchor mount <name>` | Mount a configured connection |
| `anchor unmount <name>` | Unmount one connection |
| `anchor unmount-all` | Unmount everything |
| `anchor remove <name>` | Remove the connection and delete its stored credential |

The CLI and tray app share every line of logic below the UI layer — there is no behavior reachable from the tray that isn't also reachable from the CLI, and vice versa (aside from the tray's live menu rendering, which has no CLI equivalent by nature).

---

## 10. Integration with ToolBox

Anchor is a **standalone** binary pair (`anchor.exe` + `anchor-tray.exe`), not a subcommand merged into the ToolBox codebase. The intended integration point is a ToolBox-managed `tool.tomlp` manifest pointing at the built binaries — giving Anchor a ToolBox menu entry and consistent versioning/installation without coupling its code to ToolBox's binary.

This manifest is **not yet written**. It depends on whatever ToolBox currently expects from a "long-running background tool" entry (one that should auto-start and stay resident) versus a normal CLI-tool entry (invoked, runs, exits) — that distinction should be confirmed against ToolBox's actual manifest schema before drafting it, rather than guessed at here.

---

## 11. Known Limitations & Open Items

| Item | Status | Severity before real use |
|---|---|---|
| SFTP host key verification is accept-all | Stubbed, flagged in code + README | **High** — fix before any untrusted network |
| FTP backend: no in-place random-access write | Protocol limitation, not fixable in this backend | Medium — use SFTP when available |
| FTP LIST parsing assumes Unix `ls -l` format | v1 simplification | Low-Medium — fails on exotic/DOS-style servers |
| No free-space / quota reporting | Placeholder values returned | Low — cosmetic (Explorer disk-space bar) |
| `tray-icon`/`tao` version coupling | Documented, pinned correctly for now | Low — risk only on future version bumps |
| Not yet compiled or run on Windows | **Open** | **High** — see §12 |
| ToolBox manifest | Not started, intentionally deferred | N/A until base mount/unmount path verified |

---

## 12. Verification Plan (next steps)

This spec describes a design that has been implemented in source form but not yet built or run, since it was authored in a non-Windows sandbox with no access to WinFsp, Windows Credential Manager, or a live FTP/SFTP server to test against. Before relying on it:

1. `cargo build --release` on the actual Windows target machine — expect to need to chase API drift in the `winfsp` and `russh`/`russh-sftp` crates, since their exact trait/method signatures move between versions faster than this spec can track.
2. `anchor test <name>` against a real server of each protocol (FTP, FTPS, SFTP) before ever mounting.
3. Mount read-only first against a non-critical path, confirm `dir` listing and file open/read in Explorer.
4. Only then test write paths, starting with sequential write/append, and explicitly avoid in-place-overwrite testing on the FTP backend (§7) until/unless that limitation is revisited.
5. Fix SFTP host-key pinning (§5.1) before pointing any mount at a server reachable over an untrusted network.
