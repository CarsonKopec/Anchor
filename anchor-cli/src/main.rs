//! `anchor` — headless CLI (spec §9).
//!
//! Every subcommand below the UI shares its logic with the tray app: both drive the same
//! [`MountManager`] built over [`anchor_fs::build_backend`] + [`anchor_fs::mount`].
//!
//! Note on the mount model: WinFsp mounts live inside the process that creates them. So
//! `anchor mount` runs in the foreground and holds the drive until Ctrl+C; `unmount` /
//! `unmount-all` act on the current process (the tray app is the long-lived host for
//! background mounts).

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use tokio::runtime::Runtime;

use anchor_core::config::{AnchorConfig, ConnectionConfig, Protocol};
use anchor_core::credentials::{CredentialStore, Secrets};
use anchor_core::error::{AnchorError, Result};
use anchor_core::mount::{BackendBuilder, MountManager, MountState, Mounter};

#[derive(Parser)]
#[command(
    name = "anchor",
    version,
    about = "Mount FTP/FTPS/SFTP servers as Windows drive letters (backed by WinFsp)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show all configured connections and their current mount state.
    List,
    /// Create or update a connection in connections.tomlp.
    Add(AddArgs),
    /// Prompt for a password (hidden) and store it in Windows Credential Manager.
    SetPassword {
        /// Connection name.
        name: String,
    },
    /// Connectivity check: stat("/") without attaching a drive letter.
    Test {
        /// Connection name.
        name: String,
    },
    /// Mount a connection and hold it in the foreground until Ctrl+C.
    Mount {
        /// Connection name.
        name: String,
    },
    /// Unmount one connection (within this process).
    Unmount {
        /// Connection name.
        name: String,
    },
    /// Unmount everything (within this process).
    UnmountAll,
    /// Remove a connection and delete its stored credential.
    Remove {
        /// Connection name.
        name: String,
    },
}

#[derive(Args)]
struct AddArgs {
    /// Connection name (no dots or whitespace).
    name: String,
    /// Protocol: ftp | ftps | sftp.
    #[arg(long)]
    protocol: String,
    /// Hostname or IP.
    #[arg(long)]
    host: String,
    /// Login user.
    #[arg(long)]
    username: String,
    /// Drive letter to mount at, e.g. M:.
    #[arg(long = "drive-letter")]
    drive_letter: String,
    /// TCP port (defaults: 22 for sftp, 21 for ftp/ftps).
    #[arg(long)]
    port: Option<u16>,
    /// Credential Manager key (defaults to the connection name).
    #[arg(long = "credential-key")]
    credential_key: Option<String>,
    /// Remote directory to expose as the drive root.
    #[arg(long = "remote-path")]
    remote_path: Option<String>,
    /// Mount read-only.
    #[arg(long = "read-only")]
    read_only: bool,
    /// Directory-listing cache TTL in seconds (1-3600).
    #[arg(long = "dir-cache-ttl-secs")]
    dir_cache_ttl_secs: Option<u64>,
    /// Mount automatically when the tray app starts.
    #[arg(long = "auto-mount-on-start")]
    auto_mount_on_start: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(cli.command, &runtime) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build a [`MountManager`] wired to the real Credential Manager and the anchor-fs backend
/// builder + mounter (the same wiring the tray uses).
fn manager(runtime: &Runtime, config: AnchorConfig) -> MountManager {
    let secrets = Arc::new(CredentialStore::new());
    let builder: BackendBuilder = Arc::new(anchor_fs::build_backend);
    let mounter: Mounter = Arc::new(anchor_fs::mount);
    MountManager::new(runtime.handle().clone(), secrets, config, builder, mounter)
}

fn run(command: Command, runtime: &Runtime) -> Result<()> {
    match command {
        Command::List => list(runtime),
        Command::Add(args) => add(args),
        Command::SetPassword { name } => set_password(&name),
        Command::Test { name } => test(&name, runtime),
        Command::Mount { name } => mount(&name, runtime),
        Command::Unmount { name } => unmount(&name, runtime),
        Command::UnmountAll => unmount_all(runtime),
        Command::Remove { name } => remove(&name),
    }
}

fn list(runtime: &Runtime) -> Result<()> {
    let config = AnchorConfig::load()?;
    if config.is_empty() {
        println!("No connections configured. Add one with `anchor add <name> --protocol ... --host ... --username ... --drive-letter ...`.");
        return Ok(());
    }
    let manager = manager(runtime, config.clone());
    println!(
        "{:<16} {:<7} {:<28} {:<6} STATE",
        "NAME", "PROTO", "HOST", "DRIVE"
    );
    for (name, state) in manager.all_statuses() {
        let conn = config.get(&name);
        let (proto, host, drive) = match conn {
            Some(c) => (
                c.protocol.as_str().to_string(),
                format!("{}:{}", c.host, c.port),
                c.drive_letter.clone(),
            ),
            None => ("?".into(), "?".into(), "?".into()),
        };
        println!(
            "{name:<16} {proto:<7} {host:<28} {drive:<6} {}",
            fmt_state(&state)
        );
    }
    Ok(())
}

fn add(args: AddArgs) -> Result<()> {
    let protocol = Protocol::from_token(&args.protocol).ok_or_else(|| {
        AnchorError::Config(format!(
            "protocol must be ftp|ftps|sftp, got '{}'",
            args.protocol
        ))
    })?;
    let credential_key = args.credential_key.unwrap_or_else(|| args.name.clone());
    let conn = ConnectionConfig {
        name: args.name.clone(),
        protocol,
        host: args.host,
        port: args.port.unwrap_or_else(|| protocol.default_port()),
        username: args.username,
        credential_key,
        drive_letter: args.drive_letter,
        remote_path: args.remote_path.unwrap_or_else(|| "/".to_string()),
        read_only: args.read_only,
        dir_cache_ttl_secs: args.dir_cache_ttl_secs.unwrap_or(10),
        auto_mount_on_start: args.auto_mount_on_start,
    };

    let mut config = AnchorConfig::load()?;
    config.upsert(conn)?;
    // Reuse config.rs's fail-fast validation (drive-letter pattern, port/ttl ranges, …) by
    // round-tripping the serialized form before committing it to disk.
    AnchorConfig::from_tomlp(&config.to_tomlp())?;
    config.save()?;

    let path = AnchorConfig::path()?;
    println!(
        "Saved '{}' to {}. Set its password with `anchor set-password {}`.",
        args.name,
        path.display(),
        args.name
    );
    Ok(())
}

fn set_password(name: &str) -> Result<()> {
    let config = AnchorConfig::load()?;
    let conn = config
        .get(name)
        .ok_or_else(|| AnchorError::Config(format!("no connection named '{name}'")))?;
    let password =
        rpassword::prompt_password(format!("Password for {}@{}: ", conn.username, conn.host))
            .map_err(|e| AnchorError::Other(format!("could not read password: {e}")))?;
    CredentialStore::new().store(&conn.credential_key, &password)?;
    println!("Stored credential for '{name}' in Windows Credential Manager.");
    Ok(())
}

fn test(name: &str, runtime: &Runtime) -> Result<()> {
    let config = AnchorConfig::load()?;
    let conn = config
        .get(name)
        .ok_or_else(|| AnchorError::Config(format!("no connection named '{name}'")))?
        .clone();
    let secret = CredentialStore::new().retrieve(&conn.credential_key)?;
    let backend = anchor_fs::build_backend(&conn, &secret)?;
    let meta = runtime.block_on(backend.stat(Path::new("/")))?;
    println!(
        "OK — connected to {}://{}:{} as {}; remote root '{}' is a {}.",
        conn.protocol.as_str(),
        conn.host,
        conn.port,
        conn.username,
        conn.remote_path,
        if meta.is_dir { "directory" } else { "file" }
    );
    Ok(())
}

fn mount(name: &str, runtime: &Runtime) -> Result<()> {
    let config = AnchorConfig::load()?;
    let manager = manager(runtime, config);
    manager.mount(name)?;
    let drive = match manager.status(name) {
        Some(MountState::Mounted { drive_letter }) => drive_letter,
        _ => "?".into(),
    };
    println!("Mounted '{name}' at {drive}. Press Ctrl+C to unmount.");
    runtime
        .block_on(tokio::signal::ctrl_c())
        .map_err(|e| AnchorError::Other(format!("failed to wait for Ctrl+C: {e}")))?;
    println!("\nUnmounting…");
    manager.unmount_all()?;
    Ok(())
}

fn unmount(name: &str, runtime: &Runtime) -> Result<()> {
    let config = AnchorConfig::load()?;
    let manager = manager(runtime, config);
    manager.unmount(name)?;
    println!(
        "Note: WinFsp mounts are held by the process that created them. If '{name}' was mounted \
         by a foreground `anchor mount` or the tray app, unmount it there."
    );
    Ok(())
}

fn unmount_all(runtime: &Runtime) -> Result<()> {
    let config = AnchorConfig::load()?;
    let manager = manager(runtime, config);
    manager.unmount_all()?;
    println!("Note: WinFsp mounts are held by the process that created them (see the tray app for background mounts).");
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut config = AnchorConfig::load()?;
    let removed = config
        .remove(name)
        .ok_or_else(|| AnchorError::Config(format!("no connection named '{name}'")))?;
    config.save()?;
    // Best-effort credential cleanup (missing entry is treated as success).
    CredentialStore::new().delete(&removed.credential_key)?;
    println!("Removed '{name}' and deleted its stored credential.");
    Ok(())
}

fn fmt_state(state: &MountState) -> String {
    match state {
        MountState::Unmounted => "unmounted".to_string(),
        MountState::Connecting => "connecting".to_string(),
        MountState::Mounted { drive_letter } => format!("mounted {drive_letter}"),
        MountState::Reconnecting => "reconnecting".to_string(),
        MountState::Failed { reason } => format!("failed: {reason}"),
    }
}
