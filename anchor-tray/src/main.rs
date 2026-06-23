//! `anchor-tray` — background system-tray app (spec §8).
//!
//! A console-less GUI-subsystem binary: launching it shows nothing but the tray icon, no
//! terminal flash, satisfying the "runs in the background" requirement. The menu is rebuilt
//! from [`MountManager::all_statuses`] on every menu event and on a 500 ms timer tick, so it
//! reflects state changes from background logic, not just direct clicks. Every action below
//! the UI is shared with `anchor-cli` (spec §9) — both drive the same `MountManager`.

#![windows_subsystem = "windows"]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use anchor_core::config::AnchorConfig;
use anchor_core::credentials::CredentialStore;
use anchor_core::mount::{BackendBuilder, MountManager, MountState, Mounter};

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

    let event_loop = EventLoopBuilder::new().build();

    let tray = TrayIconBuilder::new()
        .with_tooltip("Anchor")
        .with_icon(make_icon())
        .with_menu(Box::new(build_menu(&manager)))
        .build()
        .expect("failed to create tray icon");

    let mut next_tick = Instant::now() + Duration::from_millis(500);

    // NOTE: `run` diverges (never returns), so `runtime` — though not captured below — is
    // never dropped and stays valid for the life of the process.
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_tick);

        // Handle menu clicks promptly (drain on every wake-up).
        while let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            handle_menu(menu_event.id().as_ref(), &manager, &tray, control_flow);
        }

        match event {
            Event::NewEvents(StartCause::Init)
            | Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // Rebuild to reflect current state (incl. background reconnects).
                tray.set_menu(Some(Box::new(build_menu(&manager))));
                next_tick = Instant::now() + Duration::from_millis(500);
                *control_flow = ControlFlow::WaitUntil(next_tick);
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

/// Render the current menu. Each connection's item ID encodes its action as a string
/// (`mount:<name>` / `unmount:<name>`), parsed back out in [`handle_menu`] — avoiding a
/// side-table mapping menu IDs to connection names (spec §8).
fn build_menu(manager: &MountManager) -> Menu {
    let menu = Menu::new();
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
        let _ = menu.append(&MenuItem::with_id(id, label, true, None));
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
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

fn handle_menu(id: &str, manager: &MountManager, tray: &TrayIcon, control_flow: &mut ControlFlow) {
    if let Some(name) = id.strip_prefix("mount:") {
        let _ = manager.mount(name);
    } else if let Some(name) = id.strip_prefix("unmount:") {
        let _ = manager.unmount(name);
    } else if id == "unmount-all" {
        let _ = manager.unmount_all();
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
    tray.set_menu(Some(Box::new(build_menu(manager))));
}

/// Open `connections.tomlp` in the user's default handler.
fn open_config() {
    if let Ok(path) = AnchorConfig::path() {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn();
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
