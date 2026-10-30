//! Tray icon app: per-chat status, "run now", and a transient panel window
//! (Iced) with Settings / Jobs / Logs views that edits the TOML config.
//! Talks to the running service daemon over loopback IPC so run-now stays
//! inside the single-deletion lock. With no daemon up, run-now is refused
//! rather than run unlocked — the lock only holds within one process.
//!
//! Process layout: the persistent muda tray icon stays alive for the whole
//! process; the Iced panel window is transient. Iced runs as a `daemon` on
//! the main thread (winit requires the main thread for its event loop) and
//! creates the window on demand with `window::open` when a tray item fires.
//! Closing the window (X) just destroys it — the daemon keeps running — and
//! the next tray click opens a fresh window in the same event loop. On Linux
//! the GTK tray loop is pumped from the Iced update loop on a 100 ms tick.
//!
//! Split: this file holds the tray/menu side and the types shared by the
//! panel tabs; `panel.rs` is the Iced app core, `settings.rs` / `jobs.rs` /
//! `logs.rs` are the three tab views and their state.

mod jobs;
mod logs;
mod panel;
mod settings;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use iced::widget::combo_box;
use iced::{Color, Task};
use muda::accelerator::Accelerator;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

pub(crate) use jobs::JobsState;
pub(crate) use logs::LogsState;
pub(crate) use panel::{panel_theme, panel_title, Message, PanelApp};
pub(crate) use settings::{
    detect_archives, fetch_dm_users, fetch_guild_channels, fetch_guild_channels_owned,
    fetch_guilds, save_config, DmRow, ManualChat, SettingsState,
};

/// The branding icon as autocropped 64px RGBA pixels — shared by the tray
/// icon and the window icon so they match.
fn brand_icon_pixels() -> Option<(Vec<u8>, u32, u32)> {
    // The repo's transparent branding icon (1024px, upscaled by the user),
    // embedded at compile time, original colors, rendered at 64px so the
    // tray's own downscale stays crisp.
    const EMBEDDED: &[u8] = include_bytes!("../../branding/icon-transparent.png");
    let img = image::load_from_memory(EMBEDDED).ok()?;
    let size = 64u32;
    // Autocrop transparent margins so the glyph fills the icon and
    // matches the visual weight of other tray icons.
    {
        let full = img.to_rgba8();
        let (w, h) = full.dimensions();
        let px = full.as_raw();
        let alpha = |x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
        let (mut minx, mut miny, mut maxx, mut maxy) = (w, h, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                if alpha(x, y) > 10 {
                    minx = minx.min(x);
                    maxx = maxx.max(x);
                    miny = miny.min(y);
                    maxy = maxy.max(y);
                }
            }
        }
        let cropped = if maxx > minx && maxy > miny {
            image::imageops::crop_imm(&full, minx, miny, maxx - minx + 1, maxy - miny + 1)
                .to_image()
        } else {
            full
        };
        let glyph =
            image::imageops::resize(&cropped, size, size, image::imageops::FilterType::Lanczos3);
        Some((glyph.into_raw(), size, size))
    }
}

/// The tray icon: branding PNG pixels, or a generated fallback.
fn tray_icon_rgba() -> tray_icon::Icon {
    match brand_icon_pixels() {
        Some((rgba, w, h)) => match tray_icon::Icon::from_rgba(rgba, w, h) {
            Ok(icon) => return icon,
            Err(e) => {
                tracing::warn!(error = %e, "tray icon from branding png failed; using fallback")
            }
        },
        None => tracing::warn!("cannot decode branding png; using fallback icon"),
    }
    fallback_icon()
}

/// Install the .desktop entry and hicolor icon so docks/launches show
/// "Message Guillotine" with the branding icon instead of a template icon.
#[cfg(target_os = "linux")]
fn install_desktop_entry() {
    let Some(home) = dirs::home_dir() else { return };
    let apps = home.join(".local/share/applications");
    let icons = home.join(".local/share/icons/hicolor/256x256/apps");
    let exe = std::env::current_exe().ok();
    if let (Ok(_), Some(exe)) = (std::fs::create_dir_all(&apps), exe) {
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=Message Guillotine\nComment=Scheduled bulk delete and archive of your own chat messages\nExec={} gui\nIcon=guillotine\nTerminal=false\nCategories=Utility;\n",
            exe.display()
        );
        let _ = std::fs::write(apps.join("guillotine.desktop"), desktop);
    }
    if std::fs::create_dir_all(&icons).is_ok() {
        if let Some((rgba, w, h)) = brand_icon_pixels() {
            // save a 256px version for the icon theme
            if let Ok(img) =
                image::load_from_memory(include_bytes!("../../branding/icon-transparent.png"))
            {
                let big = image::imageops::resize(
                    &img.to_rgba8(),
                    256,
                    256,
                    image::imageops::FilterType::Lanczos3,
                );
                let _ = big.save(icons.join("guillotine.png"));
            }
            let _ = (rgba, w, h);
        }
    }
}

/// The window icon: same branding pixels, for the Iced panel window.
pub(crate) fn window_icon() -> Option<iced::window::Icon> {
    let (rgba, w, h) = brand_icon_pixels()?;
    match iced::window::icon::from_rgba(rgba, w, h) {
        Ok(icon) => Some(icon),
        Err(e) => {
            tracing::warn!(error = %e, "window icon failed; using default");
            None
        }
    }
}

/// A simple generated 32x32 RGBA icon (a dark square with a lighter blade).
fn fallback_icon() -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let in_blade = (6..20).contains(&y) && (8..26).contains(&x) && (x + y) % 7 != 0;
            let (r, g, b, a) = if in_blade {
                (220, 60, 60, 255)
            } else {
                (35, 35, 40, 255)
            };
            rgba[i..i + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("icon rgba")
}

/// GUI entry point. Runs on the main thread WITHOUT an ambient tokio runtime:
/// iced::daemon creates its own runtime on this thread. The async boot work
/// (login, label fetch) runs on a temporary runtime that is dropped before
/// the daemon starts.
pub fn run_sync(config_arg: Option<PathBuf>) -> anyhow::Result<()> {
    // GTK must be initialized before any tray/menu is created on Linux.
    // The Iced event loop below runs on this same (main) thread and pumps
    // GTK from its update loop, so the tray keeps receiving events.
    #[cfg(target_os = "linux")]
    gtk::init()?;
    let (cfg, path) = crate::load_config(config_arg.as_ref())?;
    let boot = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (client, labels) = boot.block_on(async {
        let chats: Vec<String> = cfg.chats().map(|(k, _)| k.to_string()).collect();
        let client = match cfg.token() {
            Ok(token) => {
                let mut c = crate::api::Client::new(token)?;
                let _ = c.get_current_user().await?;
                Some(Arc::new(c))
            }
            Err(_) => None,
        };
        let labels = match &client {
            Some(c) => fetch_chat_labels(c, &chats).await,
            None => HashMap::new(),
        };
        Ok::<_, anyhow::Error>((client, labels))
    })?;
    drop(boot);
    let archive_dir = cfg.resolve_archive_dir(None);
    let viewer = cfg
        .archive_viewer
        .clone()
        .or_else(|| std::env::var_os("GUILLOTINE_VIEWER").map(PathBuf::from));

    let chats: Vec<String> = cfg.chats().map(|(k, _)| k.to_string()).collect();
    #[cfg(target_os = "linux")]
    install_desktop_entry();

    let menu = build_menu(&chats, &archive_dir, viewer.as_deref(), &labels)?;
    let _tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("guillotine")
        .with_icon(tray_icon_rgba())
        .build()?;

    let (tx, rx) = mpsc::channel::<MenuEvent>();
    std::thread::spawn(move || {
        let receiver = MenuEvent::receiver();
        while let Ok(event) = receiver.recv() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });

    // Iced owns the main-thread event loop from here on; the tray menu
    // events are drained from `rx` on the panel's fast tick.
    let boot_rx = std::sync::Mutex::new(Some(rx));
    let (boot_path, boot_client) = (path.clone(), client.clone());
    iced::daemon(
        move || -> (PanelApp, Task<Message>) {
            // Boot runs inside iced's own tokio runtime: take the current
            // handle here (the boot-time runtime was dropped before this).
            let rt = tokio::runtime::Handle::current();
            let rx = boot_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| mpsc::channel::<MenuEvent>().1);
            PanelApp::boot(boot_path.clone(), rt, boot_client.clone(), rx)
        },
        PanelApp::update,
        PanelApp::view,
    )
    .subscription(PanelApp::subscription)
    .theme(panel_theme)
    .title(panel_title)
    .run()
    .map_err(|e| anyhow::anyhow!("iced error: {e}"))?;
    Ok(())
}

/// Whether this binary provides the `viewer` subcommand (`guillotine viewer
/// <folder>`). Probed once per process via `viewer --help`: clap exits 0 when
/// the subcommand exists and non-zero when it does not.
fn rust_viewer_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        std::process::Command::new(exe)
            .args(["viewer", "--help"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Viewer subprocesses spawned from the tray, so Quit can stop them.
static VIEWER_CHILDREN: std::sync::Mutex<Vec<std::process::Child>> =
    std::sync::Mutex::new(Vec::new());

/// Stop any archive-viewer subprocesses this GUI spawned. Called on Quit.
pub(crate) fn shutdown_viewers() {
    if let Ok(mut children) = VIEWER_CHILDREN.lock() {
        for mut child in children.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn a child fully detached from this process: null stdio and its own
/// process group/session, so signals meant for the GUI (Ctrl-C, terminal
/// hangup, Quit) never reach the child and closing the viewer can never take
/// the tray app down with it.
fn spawn_detached(
    program: &Path,
    args: &[&std::ffi::OsStr],
) -> std::io::Result<std::process::Child> {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    cmd.spawn()
}

/// Open an archive folder in the best available viewer: the Rust `viewer`
/// subcommand of this same binary when present, else the configured Python
/// viewer (config `archive_viewer` / GUILLOTINE_VIEWER), else the OS file
/// manager. The viewer is spawned detached (see `spawn_detached`) so its
/// lifecycle is independent of the tray app: closing the viewer never kills
/// the GUI, and Quit stops the viewer via `shutdown_viewers`.
fn open_archive(folder: &Path) {
    if rust_viewer_available() {
        if let Ok(exe) = std::env::current_exe() {
            let args: Vec<&std::ffi::OsStr> =
                vec![std::ffi::OsStr::new("viewer"), folder.as_os_str()];
            if let Ok(child) = spawn_detached(&exe, &args) {
                if let Ok(mut children) = VIEWER_CHILDREN.lock() {
                    children.push(child);
                }
                return;
            }
        }
    }
    let viewer = crate::load_config(None)
        .ok()
        .and_then(|(cfg, _)| cfg.archive_viewer)
        .or_else(|| std::env::var_os("GUILLOTINE_VIEWER").map(PathBuf::from));
    let spawned = viewer.and_then(|v| {
        std::process::Command::new("python3")
            .arg(&v)
            .arg(folder)
            .spawn()
            .ok()
    });
    if spawned.is_none() {
        open_in_file_manager(folder);
    }
}

#[cfg(target_os = "linux")]
fn open_in_file_manager(folder: &Path) {
    let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
}

#[cfg(target_os = "macos")]
fn open_in_file_manager(folder: &Path) {
    let _ = std::process::Command::new("open").arg(folder).spawn();
}

#[cfg(target_os = "windows")]
fn open_in_file_manager(folder: &Path) {
    let _ = std::process::Command::new("explorer").arg(folder).spawn();
}

/// Map chat keys ("dm:<id>", "guild:<g>/channel:<c>") to friendly labels
/// ("Display (@username)", "GuildName #channel") via the API. Keys that
/// can't be resolved are simply absent from the map (callers fall back to
/// the raw key).
pub(crate) async fn fetch_chat_labels(
    client: &crate::api::Client,
    chats: &[String],
) -> HashMap<String, String> {
    use crate::config::ChatKey;
    let mut map = HashMap::new();
    if let Ok(channels) = client.get_dm_channels().await {
        for (user, _channel) in client.dm_users(&channels) {
            let name = user
                .global_name
                .clone()
                .unwrap_or_else(|| user.username.clone());
            map.insert(
                format!("dm:{}", user.id),
                format!("{name} (@{})", user.username),
            );
        }
    }
    let mut guild_names: HashMap<String, String> = HashMap::new();
    if let Ok(guilds) = client.get_guilds().await {
        for g in guilds {
            guild_names.insert(g.id, g.name);
        }
    }
    let mut channels_by_guild: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for chat in chats {
        let Ok(ChatKey::GuildChannel { guild, channel }) = ChatKey::parse(chat) else {
            continue;
        };
        let channels = channels_by_guild.entry(guild.clone()).or_default();
        if channels.is_empty() {
            *channels = fetch_guild_channels(client, &guild).await;
        }
        let channel_name = channels
            .iter()
            .find(|(id, _)| id == &channel)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| format!("#{channel}"));
        let guild_name = guild_names
            .get(&guild)
            .cloned()
            .unwrap_or_else(|| guild.clone());
        map.insert(chat.clone(), format!("{guild_name} {channel_name}"));
    }
    map
}

fn build_menu(
    chats: &[String],
    archive_dir: &Path,
    _viewer: Option<&Path>,
    labels: &HashMap<String, String>,
) -> anyhow::Result<Menu> {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(
        "settings",
        "Settings…",
        true,
        None::<Accelerator>,
    ))?;
    menu.append(&MenuItem::with_id(
        "jobs",
        "Jobs…",
        true,
        None::<Accelerator>,
    ))?;
    menu.append(&MenuItem::with_id(
        "status",
        "Status",
        true,
        None::<Accelerator>,
    ))?;
    let archives = crate::archive_format::list_archives(archive_dir);
    if !archives.is_empty() {
        let view_sub = Submenu::with_id("view-archive", "View archive ▸", true);
        for folder in archives.iter().take(25) {
            let name = folder.file_name().unwrap().to_string_lossy().into_owned();
            let id = format!("view:{}", folder.display());
            view_sub.append(&MenuItem::with_id(id, name, true, None::<Accelerator>))?;
        }
        menu.append(&view_sub)?;
    }
    if !chats.is_empty() {
        let run_sub = Submenu::with_id("run-now", "Run now", true);
        for chat in chats {
            // Raw key stays the action id; the menu shows the friendly name.
            let label = labels.get(chat).cloned().unwrap_or_else(|| chat.clone());
            run_sub.append(&MenuItem::with_id(
                format!("run:{chat}"),
                label,
                true,
                None::<Accelerator>,
            ))?;
        }
        menu.append(&run_sub)?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(
        "quit",
        "Quit",
        true,
        None::<Accelerator>,
    ))?;
    Ok(menu)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Jobs,
    Settings,
    Logs,
}

/// A combo-box option that displays a friendly label but carries an id.
#[derive(Clone)]
pub(crate) struct Opt {
    id: String,
    label: String,
}

impl std::fmt::Display for Opt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

pub(crate) const SCHEDULE_OPTIONS: [&str; 3] = ["daily", "weekly", "monthly"];

pub(crate) fn schedule_state() -> combo_box::State<String> {
    combo_box::State::new(SCHEDULE_OPTIONS.map(String::from).to_vec())
}

/// Muted/secondary text color on the dark theme.
pub(crate) fn muted() -> Color {
    Color::from_rgb(0.62, 0.62, 0.66)
}

/// Status color for a job phase: green done, red failed, amber running.
pub(crate) fn phase_color(phase: &str) -> Color {
    if phase == "done" {
        Color::from_rgb(0.4, 0.73, 0.42)
    } else if phase.contains("fail") {
        Color::from_rgb(0.94, 0.33, 0.31)
    } else {
        Color::from_rgb(1.0, 0.70, 0.0)
    }
}

pub(crate) fn ok_color() -> Color {
    Color::from_rgb(0.4, 0.73, 0.42)
}

pub(crate) fn err_color() -> Color {
    Color::from_rgb(0.94, 0.33, 0.31)
}

/// Short timestamp for grid cells: "2024-03-10 04:00:00" -> "03-10 04:00".
pub(crate) fn short_time(iso: &str) -> String {
    if iso.len() > 19 {
        format!("{} {}", &iso[5..10], &iso[11..19])
    } else {
        iso.to_string()
    }
}
