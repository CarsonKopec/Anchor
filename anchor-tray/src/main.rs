//! `anchor-tray` — background system-tray app (spec §8).
//!
//! A console-less GUI-subsystem binary: launching it shows nothing but the tray icon, no
//! terminal flash, satisfying the "runs in the background" requirement. The menu is rebuilt
//! from [`MountManager::all_statuses`] on every menu event and on a 500 ms timer tick, so it
//! reflects state changes from background logic, not just direct clicks. Every action below
//! the UI is shared with `anchor-cli` (spec §9) — both drive the same `MountManager`.

#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use anchor_core::config::AnchorConfig;
use anchor_core::credentials::CredentialStore;
use anchor_core::mount::{BackendBuilder, MountManager, MountState, Mounter};

const MOUNT_SUPPORT_COMPILED_IN: bool = cfg!(feature = "winfsp");
const STARTUP_APP_NAME: &str = "Anchor";
const STARTUP_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

enum UserEvent {
    Menu(MenuEvent),
}

#[derive(Default)]
struct AutoReconnect {
    retries: HashMap<String, RetryState>,
}

struct RetryState {
    attempts: u32,
    next_attempt: Instant,
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start async runtime");

    let config = AnchorConfig::load().unwrap_or_default();
    let manager = make_manager(runtime.handle().clone(), config);

    // Mount auto_mount_on_start connections before the event loop handles clicks (spec §7).
    let auto: Vec<String> = manager
        .config_snapshot()
        .connections()
        .filter(|c| c.auto_mount_on_start)
        .map(|c| c.name.clone())
        .collect();
    for name in auto {
        let _ = manager.mount(&name);
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let mut next_tick = Instant::now() + Duration::from_millis(500);
    let mut tray: Option<TrayIcon> = None;
    let mut reconnect = AutoReconnect::default();
    let mut startup_enabled = startup_enabled();
    let mut next_health_check = Instant::now() + Duration::from_secs(15);

    // NOTE: `run` diverges (never returns), so `runtime` — though not captured below — is
    // never dropped and stays valid for the life of the process.
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_tick);

        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = Some(
                    TrayIconBuilder::new()
                        .with_tooltip("Anchor")
                        .with_icon(make_icon())
                        .with_menu(Box::new(build_menu(&manager, startup_enabled)))
                        .build()
                        .expect("failed to create tray icon"),
                );
                next_tick = Instant::now() + Duration::from_millis(500);
                *control_flow = ControlFlow::WaitUntil(next_tick);
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                let now = Instant::now();
                if now >= next_health_check {
                    manager.check_active_mounts();
                    next_health_check = now + Duration::from_secs(15);
                }
                retry_auto_mounts(&manager, &mut reconnect);
                if let Some(tray) = tray.as_ref() {
                    tray.set_menu(Some(Box::new(build_menu(&manager, startup_enabled))));
                }
                next_tick = Instant::now() + Duration::from_millis(500);
                *control_flow = ControlFlow::WaitUntil(next_tick);
            }
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if let Some(tray) = tray.as_ref() {
                    handle_menu(
                        menu_event.id().as_ref(),
                        &manager,
                        tray,
                        control_flow,
                        &mut startup_enabled,
                    );
                }
            }
            Event::LoopDestroyed => {
                // No orphaned WinFsp mounts survive process exit (spec §7).
                let _ = manager.unmount_all();
            }
            _ => {}
        }
    });
}

/// Build a [`MountManager`] wired to Credential Manager + the anchor-fs backend builder and
/// mounter — identical to the CLI's wiring (spec §9).
fn make_manager(handle: tokio::runtime::Handle, config: AnchorConfig) -> MountManager {
    let secrets = Arc::new(CredentialStore::new());
    let builder: BackendBuilder = Arc::new(anchor_fs::build_backend);
    let mounter: Mounter = Arc::new(anchor_fs::mount);
    MountManager::new(handle, secrets, config, builder, mounter)
}

fn retry_auto_mounts(manager: &MountManager, reconnect: &mut AutoReconnect) {
    if !MOUNT_SUPPORT_COMPILED_IN {
        return;
    }

    let now = Instant::now();
    let config = manager.config_snapshot();
    for conn in config.connections().filter(|c| c.auto_mount_on_start) {
        match manager.status(&conn.name) {
            Some(MountState::Failed { reason }) if should_auto_retry(&reason) => {
                let retry = reconnect
                    .retries
                    .entry(conn.name.clone())
                    .or_insert(RetryState {
                        attempts: 0,
                        next_attempt: now + Duration::from_secs(5),
                    });
                if now >= retry.next_attempt {
                    let _ = manager.mount(&conn.name);
                    retry.attempts = retry.attempts.saturating_add(1);
                    retry.next_attempt = now + retry_delay(retry.attempts);
                }
            }
            Some(MountState::Mounted { .. }) | Some(MountState::Unmounted) => {
                reconnect.retries.remove(&conn.name);
            }
            _ => {}
        }
    }
}

fn should_auto_retry(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    !(reason.contains("no stored credential")
        || reason.contains("winfsp support was not compiled in")
        || reason.contains("already mounted"))
}

fn retry_delay(attempts: u32) -> Duration {
    let secs = match attempts {
        0 | 1 => 10,
        2 => 20,
        3 => 40,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// Render the current menu. Each connection's item ID encodes its action as a string
/// (`mount:<name>` / `unmount:<name>`), parsed back out in [`handle_menu`] — avoiding a
/// side-table mapping menu IDs to connection names (spec §8).
fn build_menu(manager: &MountManager, startup_enabled: bool) -> Menu {
    let menu = Menu::new();
    if !MOUNT_SUPPORT_COMPILED_IN {
        let _ = menu.append(&MenuItem::with_id(
            "winfsp-disabled",
            "Mounting unavailable: rebuild with WinFsp support",
            false,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    for (name, state) in manager.all_statuses() {
        let (label, id) = match &state {
            MountState::Mounted { drive_letter } => (
                format!("{name}   [{drive_letter}] ●   (unmount)"),
                format!("unmount:{name}"),
            ),
            MountState::Connecting => (format!("{name}   [connecting…]"), format!("noop:{name}")),
            MountState::Reconnecting => {
                (format!("{name}   [reconnecting…]"), format!("noop:{name}"))
            }
            MountState::Failed { .. } => (
                format!("{name}   [failed]   (retry)"),
                format!("mount:{name}"),
            ),
            MountState::Unmounted => (
                format!("{name}   [unmounted]   (mount)"),
                format!("mount:{name}"),
            ),
        };
        let enabled = match &state {
            MountState::Mounted { .. } => true,
            MountState::Failed { .. } | MountState::Unmounted => MOUNT_SUPPORT_COMPILED_IN,
            MountState::Connecting | MountState::Reconnecting => false,
        };
        let _ = menu.append(&MenuItem::with_id(id, label, enabled, None));
        if let MountState::Failed { reason } = &state {
            let _ = menu.append(&MenuItem::with_id(
                format!("error:{name}"),
                format!("  {}", menu_reason(reason)),
                false,
                None,
            ));
            if missing_credential(reason) {
                let _ = menu.append(&MenuItem::with_id(
                    format!("set-password:{name}"),
                    "  Set password...",
                    true,
                    None,
                ));
            }
        }
        if let MountState::Mounted { drive_letter } = &state {
            let _ = menu.append(&MenuItem::with_id(
                format!("open-drive:{name}"),
                format!("  Open {drive_letter}"),
                true,
                None,
            ));
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let startup_label = if startup_enabled {
        "Start Anchor on login: On"
    } else {
        "Start Anchor on login: Off"
    };
    let _ = menu.append(&MenuItem::with_id(
        "toggle-startup",
        startup_label,
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        "open-config",
        "Open config file",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id("unmount-all", "Unmount All", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quit Anchor", true, None));
    menu
}

fn menu_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 110;
    let single_line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_CHARS {
        return single_line;
    }
    let mut out: String = single_line.chars().take(MAX_CHARS - 3).collect();
    out.push_str("...");
    out
}

fn missing_credential(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("no stored credential")
}

fn startup_enabled() -> bool {
    Command::new("reg")
        .args(["query", STARTUP_RUN_KEY, "/v", STARTUP_APP_NAME])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn set_startup_enabled(enabled: bool) -> std::io::Result<()> {
    if enabled {
        let exe = std::env::current_exe()?;
        let value = format!("\"{}\"", exe.display());
        Command::new("reg")
            .args([
                "add",
                STARTUP_RUN_KEY,
                "/v",
                STARTUP_APP_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()?;
    } else {
        Command::new("reg")
            .args(["delete", STARTUP_RUN_KEY, "/v", STARTUP_APP_NAME, "/f"])
            .status()?;
    }
    Ok(())
}

fn open_drive(manager: &MountManager, name: &str) {
    if let Some(MountState::Mounted { drive_letter }) = manager.status(name) {
        shell_start(Path::new(&format!("{drive_letter}\\")));
    }
}

fn set_password(name: &str) {
    let exe = sibling_exe("anchor.exe");
    let cmdline = format!("\"{}\" set-password {name}", exe.display());
    let _ = Command::new("cmd")
        .args(["/C", "start", "Anchor password", "cmd", "/K"])
        .arg(cmdline)
        .spawn();
}

fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|parent| parent.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn shell_start(path: &Path) {
    let _ = Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn();
}

fn handle_menu(
    id: &str,
    manager: &MountManager,
    tray: &TrayIcon,
    control_flow: &mut ControlFlow,
    startup_enabled: &mut bool,
) {
    if let Some(name) = id.strip_prefix("mount:") {
        let _ = manager.mount(name);
    } else if let Some(name) = id.strip_prefix("unmount:") {
        let _ = manager.unmount(name);
    } else if let Some(name) = id.strip_prefix("open-drive:") {
        open_drive(manager, name);
    } else if let Some(name) = id.strip_prefix("set-password:") {
        set_password(name);
    } else if id == "unmount-all" {
        let _ = manager.unmount_all();
    } else if id == "toggle-startup" {
        let next = !*startup_enabled;
        if set_startup_enabled(next).is_ok() {
            *startup_enabled = next;
        }
    } else if id == "open-config" {
        open_config();
    } else if id == "quit" {
        let _ = manager.unmount_all();
        *control_flow = ControlFlow::Exit;
        return;
    } else {
        return; // noop:* / unknown
    }
    // Reflect the result immediately rather than waiting for the next tick.
    tray.set_menu(Some(Box::new(build_menu(manager, *startup_enabled))));
}

/// Open `connections.tomlp` in the user's default handler.
fn open_config() {
    if let Ok(path) = AnchorConfig::path() {
        shell_start(&path);
    }
}

/// A small generated tray icon (blue square with a dark border) — avoids bundling an asset.
fn make_icon() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let border = x < 2 || y < 2 || x >= SIZE - 2 || y >= SIZE - 2;
            let (r, g, b) = if border { (20, 30, 50) } else { (40, 110, 200) };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("failed to build tray icon")
}
