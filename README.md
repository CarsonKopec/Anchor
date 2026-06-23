# Anchor

Mount FTP / FTPS / SFTP servers as real Windows drive letters, backed by a kernel-mode
filesystem driver ([WinFsp](https://winfsp.dev)) — not a sync-to-local-folder. Runs in the
background from the system tray, or headless from a CLI. See [`ANCHOR_SPEC.md`](ANCHOR_SPEC.md)
for the full design.

```
M:\  ──►  sftp://media.example.com:22/srv/media
A:\  ──►  ftps://archive.example.com:21/        (read-only)
```

## Workspace layout

| Crate | Role |
|---|---|
| `anchor-core` | `RemoteFs` trait, `MountManager`, config (TOML+), credentials, caches. No protocol or WinFsp knowledge. |
| `anchor-fs` | FTP/FTPS + SFTP backends; the WinFsp host glue (behind the `winfsp` feature). The only crate that touches a wire protocol or WinFsp. |
| `anchor-cli` | `anchor` — headless CLI. |
| `anchor-tray` | `anchor-tray` — background system-tray app. |

Dependency direction is one-way: `anchor-core` knows nothing of WinFsp or any protocol, so a
new backend (WebDAV, S3, …) is a contained addition in `anchor-fs` (spec §3.4).

## Requirements

- Windows (WinFsp is Windows-only).
- Rust 1.80+ (MSVC toolchain).
- [**WinFsp**](https://winfsp.dev) installed — **required at runtime to mount** (delay-loaded,
  so the binaries build and non-mount commands run without it). Not bundled.
- To **build** with the `winfsp` feature: **LLVM/libclang** (`winget install LLVM.LLVM`, or the
  "C++ Clang tools" VS component) — `winfsp-sys` uses `bindgen`. Not needed for the default build.

## Building

The default build compiles and tests **without WinFsp installed**. The backends, config,
credentials, caches, and every non-mount CLI command work; only the actual drive-attach is
stubbed out (it returns a clear "WinFsp support not compiled in" error).

```sh
cargo build                 # whole workspace, no WinFsp needed
cargo test -p anchor-core   # unit tests (caches, mount state machine, config, credentials)
```

To get real mounting, build with the `winfsp` feature **on a machine with WinFsp installed**:

```sh
cargo build --release --features winfsp -p anchor-cli
cargo build --release --features winfsp -p anchor-tray
```

> WinFsp is **delay-loaded**: the binaries link and `--version`/`list`/`add`/`test`/
> `set-password` run even without `winfsp-x64.dll` present; it's loaded lazily on the first
> mount. The `/DELAYLOAD` link arg is applied by each binary crate's `build.rs`
> ([anchor-cli/build.rs](anchor-cli/build.rs), [anchor-tray/build.rs](anchor-tray/build.rs)).

### Crypto backend

`russh` is configured to use the **`ring`** crypto backend rather than the default
`aws-lc-rs`, because `aws-lc-sys` requires NASM and a configured MSVC build environment to
compile its assembly. `ring` builds on stock MSVC.

## CLI

```
anchor list                 Show all connections and their mount state
anchor add <name> --protocol sftp --host h --username u --drive-letter M: [--port N]
                            [--remote-path /p] [--read-only] [--credential-key K]
                            [--dir-cache-ttl-secs N] [--auto-mount-on-start]
anchor set-password <name>  Prompt (hidden) and store the password in Credential Manager
anchor test <name>          Connectivity check: stat("/") without mounting
anchor mount <name>         Mount and hold in the foreground until Ctrl+C
anchor unmount <name>       Unmount one connection (this process)
anchor unmount-all          Unmount everything (this process)
anchor remove <name>        Remove the connection and delete its stored credential
```

WinFsp mounts live inside the process that creates them, so `anchor mount` runs in the
foreground. For background mounts use **`anchor-tray`** (auto-mounts connections marked
`auto_mount_on_start`, and provides per-connection mount/unmount from the tray menu).

## Configuration

`%APPDATA%\Anchor\connections.tomlp` — [TOML+](https://crates.io/crates/tomlplus-syntax),
validated at load time by inline `@`-annotations (`@required`, `@enum`, `@type`, `@min`/`@max`,
`@pattern`). A bad value fails fast, naming the offending key and line. See
[`anchor-core/src/example_connections.tomlp`](anchor-core/src/example_connections.tomlp).

**Secrets are never stored in this file.** `credential_key` is a *reference* into Windows
Credential Manager (target name `Anchor:<credential_key>`); the password is written/read/deleted
through `CredWriteW`/`CredReadW`/`CredDeleteW`. This makes the config safe to back up or sync —
the file alone can't compromise a connection (spec §6.3).

## Security: SFTP host-key pinning (TOFU)

The SFTP backend pins host keys **trust-on-first-use**, OpenSSH-style (spec §5.1). On the first
successful connect, the server's SHA-256 key fingerprint is recorded in
`%APPDATA%\Anchor\known_hosts` (keyed by `host:port`); every later connect verifies the
presented key against it and **refuses on mismatch** with an actionable message (possible
man-in-the-middle), naming the line to delete if the key legitimately changed. Implemented in
[`anchor-fs/src/sftp.rs`](anchor-fs/src/sftp.rs) + [`anchor-core/src/host_keys.rs`](anchor-core/src/host_keys.rs);
verified live (learn → match → mismatch-refused).

## FTP caveats

FTP is a structurally worse fit for "mounted drive" semantics than SFTP (spec §5.2, §7):

- No random-access write — sequential write / append / full rewrite only; mid-file overwrite is
  unsupported. `set_len` only truncates to zero.
- LIST parsing assumes Unix `ls -l` format (covers vsftpd / ProFTPD / Pure-FTPd). Exotic /
  DOS-style servers are not handled in v1 (MLSD would be the fix).
- Directory detection is a `CWD` heuristic, costing an extra round trip per `stat`.

Prefer SFTP whenever the server supports both.

## Version coupling

`tray-icon` and `tao` version tightly together — bump them as a pair, not independently. (The
original spec pinned `tray-icon 0.14` / `tao 0.30`; this build tracks current `0.24` / `0.35`.)

## Status / verification

Implemented and verified on this (non-WinFsp) machine:

- `anchor-core`: builds + 17 unit tests pass (cache TTL/invalidation, read-ahead, mount state
  machine + drive-letter uniqueness, config parse/validate fail-fast, **real Credential Manager
  round-trip**).
- `anchor-fs` backends, `anchor-cli`, `anchor-tray`: build clean; the CLI's
  list/add/test/remove paths exercised end-to-end.

**Verified mounting a real SFTP drive** (with WinFsp installed):

- `cargo build --release --features winfsp` builds and links; `winfsp-x64.dll` delay-loads from
  the registry `InstallDir`.
- `anchor test` connects to a live SFTP server; `anchor mount` brings up `M:`, and listing it in
  Explorer/PowerShell serves the remote directory over SFTP. Unmount releases the drive cleanly.

- SFTP host-key **TOFU pinning** verified live (learn → match → mismatch refused).

Not yet exercised against a live server:

- The **FTP/FTPS** backend (only SFTP has been run end-to-end), and **write** paths on either.
