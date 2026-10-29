//! Tray icon app: per-chat status, "run now", and a transient panel window
//! (Iced) with Settings / Jobs / Logs views that edits the TOML config.
//! Talks to the running service daemon over loopback IPC so run-now stays
//! inside the single-deletion lock; falls back to running directly when no
//! daemon is up.
//!
//! Process layout: the persistent muda tray icon stays alive for the whole
//! process; the Iced panel window is transient. Iced runs as a `daemon` on
//! the main thread (winit requires the main thread for its event loop) and
//! creates the window on demand with `window::open` when a tray item fires.
//! Closing the window (X) just destroys it — the daemon keeps running — and
//! the next tray click opens a fresh window in the same event loop. On Linux
//! the GTK tray loop is pumped from the Iced update loop on a 100 ms tick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{
    button, checkbox, column, combo_box, container, progress_bar, row, rule, scrollable, text,
    text_input, Space,
};
use iced::{window, Color, Element, Font, Length, Size, Subscription, Task, Theme};

use muda::accelerator::Accelerator;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::config::{self, Config};

/// A simple generated 32x32 RGBA icon (a dark square with a lighter blade),
/// so no asset files are needed.
fn tray_icon_rgba() -> tray_icon::Icon {
    // The repo's transparent branding icon (1024px, upscaled by the user),
    // embedded at compile time, original colors, rendered at 64px so the
    // tray's own downscale stays crisp.
    const EMBEDDED: &[u8] = include_bytes!("../../branding/icon-transparent.png");
    match image::load_from_memory(EMBEDDED) {
        Ok(img) => {
            let size = 64u32;
            // Autocrop transparent margins so the glyph fills the icon and
            // matches the visual weight of other tray icons.
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
            let glyph = image::imageops::resize(
                &cropped,
                size,
                size,
                image::imageops::FilterType::Lanczos3,
            );
            match tray_icon::Icon::from_rgba(glyph.into_raw(), size, size) {
                Ok(icon) => return icon,
                Err(e) => {
                    tracing::warn!(error = %e, "tray icon from branding png failed; using fallback")
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot decode branding png; using fallback icon"),
    }
    fallback_icon()
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

/// Open an archive folder in the best available viewer: the Rust `viewer`
/// subcommand of this same binary when present, else the configured Python
/// viewer (config `archive_viewer` / GUILLOTINE_VIEWER), else the OS file
/// manager.
fn open_archive(folder: &Path) {
    if rust_viewer_available() {
        if let Ok(exe) = std::env::current_exe() {
            if std::process::Command::new(exe)
                .arg("viewer")
                .arg(folder)
                .spawn()
                .is_ok()
            {
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
async fn fetch_chat_labels(client: &crate::api::Client, chats: &[String]) -> HashMap<String, String> {
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
        let channels = channels_by_guild
            .entry(guild.clone())
            .or_default();
        if channels.is_empty() {
            *channels = fetch_guild_channels(client, &guild).await;
        }
        let channel_name = channels
            .iter()
            .find(|(id, _)| id == &channel)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| format!("#{channel}"));
        let guild_name = guild_names.get(&guild).cloned().unwrap_or_else(|| guild.clone());
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

// ---------------------------------------------------------------------------
// Iced panel: one transient window with a sidebar (Jobs / Settings / Logs)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Jobs,
    Settings,
    Logs,
}

/// A combo-box option that displays a friendly label but carries an id.
#[derive(Clone)]
struct Opt {
    id: String,
    label: String,
}

impl std::fmt::Display for Opt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// One clickable DM user row: checking it adds a [chat."dm:<id>"] job.
struct DmRow {
    user_id: String,
    name: String,
    enabled: bool,
    schedule: String,
    schedule_options: combo_box::State<String>,
    retention: String,
    archive: bool,
}

/// A manually entered (guild channel) chat row.
struct ManualChat {
    key: String,
    schedule: String,
    schedule_options: combo_box::State<String>,
    retention: String,
    archive: bool,
}

struct SettingsState {
    token: String,
    archive_dir: String,
    archive_viewer: Option<PathBuf>,
    dm_policy: String,
    dm_policy_options: combo_box::State<String>,
    dm_users: String,
    default_schedule: String,
    default_schedule_options: combo_box::State<String>,
    default_retention: String,
    default_archive: bool,
    dm_rows: Vec<DmRow>,
    dm_pending: bool,
    guilds: Vec<Opt>,
    guild_options: combo_box::State<Opt>,
    guild_pending: bool,
    selected_guild: String,
    channels: Vec<Opt>,
    channel_options: combo_box::State<Opt>,
    /// Guild id the currently fetched `channels` belong to.
    channels_for: String,
    channels_pending: bool,
    selected_channel: String,
    archives: Vec<(String, PathBuf)>,
    manual_chats: Vec<ManualChat>,
    armed_delete_all: Option<String>,
    status: String,
}

struct JobsState {
    running: Vec<serde_json::Value>,
    last: serde_json::Value,
    note: String,
    /// Chat key -> friendly label, fetched in the background from the API.
    labels: HashMap<String, String>,
}

struct LogsState {
    daemon: Vec<String>,
    gui: Vec<String>,
    last_daemon_file: Option<PathBuf>,
}

struct PanelApp {
    tab: Tab,
    /// The currently open panel window, if any. The window is transient:
    /// X destroys it (the daemon keeps running) and the next tray click opens
    /// a new one with `window::open`.
    window_id: Option<window::Id>,
    menu_rx: mpsc::Receiver<MenuEvent>,
    rt: tokio::runtime::Handle,
    client: Option<Arc<crate::api::Client>>,
    config_path: PathBuf,
    settings: SettingsState,
    jobs: JobsState,
    logs: LogsState,
}

const SCHEDULE_OPTIONS: [&str; 3] = ["daily", "weekly", "monthly"];

fn schedule_state() -> combo_box::State<String> {
    combo_box::State::new(SCHEDULE_OPTIONS.map(String::from).to_vec())
}

/// Muted/secondary text color on the dark theme.
fn muted() -> Color {
    Color::from_rgb(0.62, 0.62, 0.66)
}

/// Status color for a job phase: green done, red failed, amber running.
fn phase_color(phase: &str) -> Color {
    if phase == "done" {
        Color::from_rgb(0.4, 0.73, 0.42)
    } else if phase.contains("fail") {
        Color::from_rgb(0.94, 0.33, 0.31)
    } else {
        Color::from_rgb(1.0, 0.70, 0.0)
    }
}

fn ok_color() -> Color {
    Color::from_rgb(0.4, 0.73, 0.42)
}

fn err_color() -> Color {
    Color::from_rgb(0.94, 0.33, 0.31)
}

/// Short timestamp for grid cells: "2024-03-10 04:00:00" -> "03-10 04:00".
fn short_time(iso: &str) -> String {
    if iso.len() > 19 {
        format!("{} {}", &iso[5..10], &iso[11..19])
    } else {
        iso.to_string()
    }
}

#[derive(Clone)]
enum Message {
    /// 100 ms tick: pump the GTK tray loop (Linux) and drain tray menu events.
    FastTick,
    /// 1 s tick: refresh the jobs view from the daemon and re-tail the logs.
    SlowTick,
    WindowClosed(window::Id),
    WindowOpened(window::Id),
    TabSelected(Tab),
    // -- settings view --
    TokenChanged(String),
    ArchiveDirChanged(String),
    DetectArchives,
    ArchiveOpened(PathBuf),
    DmPolicySelected(String),
    DmUsersChanged(String),
    DefaultScheduleSelected(String),
    DefaultRetentionChanged(String),
    DefaultArchiveToggled(bool),
    DmUsersFetched(Vec<(String, String)>),
    GuildsFetched(Vec<(String, String)>),
    GuildSelected(Opt),
    ChannelsFetched(String, Vec<(String, String)>),
    ChannelSelected(Opt),
    AddChannel,
    DmRowToggled(usize, bool),
    DmRowSchedule(usize, String),
    DmRowRetention(usize, String),
    DmRowArchive(usize, bool),
    ManualKeyChanged(usize, String),
    ManualSchedule(usize, String),
    ManualRetention(usize, String),
    ManualArchive(usize, bool),
    RemoveManualChat(usize),
    AddManualChat,
    DeleteAllClicked(String),
    DeleteAllDone(String, Result<serde_json::Value, String>),
    Save,
    // -- jobs view --
    JobsFetched(Result<serde_json::Value, String>),
    LabelsFetched(HashMap<String, String>),
}

fn panel_theme(_: &PanelApp, _: window::Id) -> Theme {
    Theme::Dark
}

fn panel_title(_: &PanelApp, _: window::Id) -> String {
    String::from("guillotine")
}

fn panel_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(1080.0, 760.0),
        min_size: Some(Size::new(820.0, 560.0)),
        visible: true,
        ..Default::default()
    }
}

impl PanelApp {
    fn boot(
        config_path: PathBuf,
        rt: tokio::runtime::Handle,
        client: Option<Arc<crate::api::Client>>,
        menu_rx: mpsc::Receiver<MenuEvent>,
    ) -> (Self, Task<Message>) {
        let mut app = PanelApp {
            tab: Tab::Settings,
            window_id: None,
            menu_rx,
            rt,
            settings: SettingsState::load(&config_path),
            jobs: JobsState {
                running: Vec::new(),
                last: serde_json::json!({}),
                note: String::new(),
                labels: HashMap::new(),
            },
            logs: LogsState::new(),
            client,
            config_path,
        };
        let mut tasks = Vec::new();
        if app.client.is_some() {
            app.settings.dm_pending = true;
            app.settings.guild_pending = true;
            let c = app.client.clone();
            tasks.push(Task::perform(fetch_dm_users(c), Message::DmUsersFetched));
            let c = app.client.clone();
            tasks.push(Task::perform(fetch_guilds(c), Message::GuildsFetched));
            // Friendly chat labels for the jobs grids.
            let c = app.client.clone();
            let path = app.config_path.clone();
            tasks.push(Task::perform(
                async move {
                    let chats = crate::load_config(Some(&path))
                        .map(|(cfg, _)| cfg.chats().map(|(k, _)| k.to_string()).collect::<Vec<_>>())
                        .unwrap_or_default();
                    match &c {
                        Some(c) => fetch_chat_labels(c, &chats).await,
                        None => HashMap::new(),
                    }
                },
                Message::LabelsFetched,
            ));
        }
        (app, Task::batch(tasks))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(100)).map(|_| Message::FastTick),
            iced::time::every(Duration::from_secs(1)).map(|_| Message::SlowTick),
            window::close_events().map(Message::WindowClosed),
        ])
    }

    /// Open the panel window on the given tab. If a window is already open,
    /// just switch tabs.
    fn open_panel(&mut self, tab: Tab) -> Task<Message> {
        self.tab = tab;
        if self.window_id.is_some() {
            return Task::none();
        }
        let (id, task) = window::open(panel_window_settings());
        self.window_id = Some(id);
        task.map(Message::WindowOpened)
    }

    /// Handle one tray menu event; returns any task it produces.
    fn handle_menu_event(&mut self, id: &str) -> Task<Message> {
        match id {
            "settings" => self.open_panel(Tab::Settings),
            "jobs" => self.open_panel(Tab::Jobs),
            "status" => {
                let rt = self.rt.clone();
                rt.spawn(async {
                    match crate::ipc::send(serde_json::json!({ "cmd": "status" })).await {
                        Ok(resp) => {
                            tracing::info!(status = %resp, "status");
                            println!("guillotine status: {}", serde_json::to_string_pretty(&resp).unwrap_or_default());
                        }
                        Err(_) => println!("guillotine daemon is not running"),
                    }
                });
                Task::none()
            }
            "quit" => {
                // Quit is the only way to shut the app down; the Iced daemon
                // intentionally keeps running with zero windows.
                std::process::exit(0);
            }
            other => {
                if let Some(archive_path) = other.strip_prefix("view:") {
                    open_archive(Path::new(archive_path));
                } else if let Some(chat) = other.strip_prefix("run:") {
                    let chat = chat.to_string();
                    self.rt.spawn(async move {
                        match crate::ipc::send(serde_json::json!({ "cmd": "run_now", "chat": chat })).await {
                            Ok(resp) if resp["ok"] == true && resp["accepted"] == true => {
                                tracing::info!(chat = %chat, "run-now accepted by daemon, running in background");
                            }
                            Ok(resp) if resp["ok"] == true => {
                                tracing::info!(chat = %chat, "run-now done: {}", &resp["summary"]);
                            }
                            Ok(resp) => tracing::warn!(chat = %chat, "run-now failed: {}", &resp["error"]),
                            Err(e) => tracing::warn!(chat = %chat, "daemon unreachable, running directly: {e}"),
                        }
                    });
                }
                Task::none()
            }
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::FastTick => {
                // tray-icon/muda on Linux are GTK: pump its main loop so menu
                // events are delivered (esp. on Wayland via XWayland/appindicator).
                #[cfg(target_os = "linux")]
                while gtk::events_pending() {
                    gtk::main_iteration();
                }
                let mut tasks = Vec::new();
                while let Ok(event) = self.menu_rx.try_recv() {
                    tasks.push(self.handle_menu_event(event.id().0.as_str()));
                }
                Task::batch(tasks)
            }
            Message::SlowTick => {
                self.logs.refresh();
                Task::perform(
                    crate::ipc::send(serde_json::json!({ "cmd": "jobs" })),
                    |res| Message::JobsFetched(res.map_err(|e| e.to_string())),
                )
            }
            Message::WindowClosed(id) => {
                if self.window_id == Some(id) {
                    self.window_id = None;
                }
                Task::none()
            }
            Message::WindowOpened(id) => {
                self.window_id = Some(id);
                Task::none()
            }
            Message::TabSelected(tab) => {
                self.tab = tab;
                Task::none()
            }

            Message::TokenChanged(v) => {
                self.settings.token = v;
                Task::none()
            }
            Message::ArchiveDirChanged(v) => {
                self.settings.archive_dir = v;
                Task::none()
            }
            Message::DetectArchives => {
                self.settings.archives = detect_archives(Path::new(&self.settings.archive_dir));
                Task::none()
            }
            Message::ArchiveOpened(path) => {
                open_archive(&path);
                Task::none()
            }
            Message::DmPolicySelected(v) => {
                self.settings.dm_policy = v;
                Task::none()
            }
            Message::DmUsersChanged(v) => {
                self.settings.dm_users = v;
                Task::none()
            }
            Message::DefaultScheduleSelected(v) => {
                self.settings.default_schedule = v;
                Task::none()
            }
            Message::DefaultRetentionChanged(v) => {
                self.settings.default_retention = v;
                Task::none()
            }
            Message::DefaultArchiveToggled(v) => {
                self.settings.default_archive = v;
                Task::none()
            }
            Message::DmUsersFetched(users) => {
                self.settings.dm_pending = false;
                for (id, name) in users {
                    if let Some(row) = self.settings.dm_rows.iter_mut().find(|r| r.user_id == id) {
                        row.name = name;
                    } else {
                        self.settings.dm_rows.push(DmRow {
                            user_id: id,
                            name,
                            enabled: false,
                            schedule: "daily".to_string(),
                            schedule_options: schedule_state(),
                            retention: self.settings.default_retention.clone(),
                            archive: self.settings.default_archive,
                        });
                    }
                }
                self.settings
                    .dm_rows
                    .sort_by_key(|a| a.name.to_lowercase());
                Task::none()
            }
            Message::GuildsFetched(guilds) => {
                self.settings.guild_pending = false;
                let mut opts: Vec<Opt> = guilds
                    .into_iter()
                    .map(|(id, name)| Opt { id, label: name })
                    .collect();
                opts.sort_by_key(|a| a.label.to_lowercase());
                self.settings.channel_options = combo_box::State::new(Vec::new());
                self.settings.guild_options = combo_box::State::new(opts.clone());
                self.settings.guilds = opts;
                Task::none()
            }
            Message::GuildSelected(opt) => {
                self.settings.selected_guild = opt.id.clone();
                // Fetch this guild's text channels in the background.
                if self.settings.selected_guild != self.settings.channels_for {
                    let guild = self.settings.selected_guild.clone();
                    self.settings.channels_for = guild.clone();
                    self.settings.channels.clear();
                    self.settings.channel_options = combo_box::State::new(Vec::new());
                    self.settings.selected_channel.clear();
                    self.settings.channels_pending = true;
                    let client = self.client.clone();
                    return Task::perform(
                        fetch_guild_channels_owned(client, guild.clone()),
                        move |chans| Message::ChannelsFetched(guild, chans),
                    );
                }
                Task::none()
            }
            Message::ChannelsFetched(guild_id, channels) => {
                self.settings.channels_pending = false;
                // Ignore stale payloads for a guild that is no longer selected.
                if guild_id == self.settings.channels_for {
                    let mut opts: Vec<Opt> = channels
                        .into_iter()
                        .map(|(id, name)| Opt { id, label: name })
                        .collect();
                    opts.sort_by_key(|a| a.label.to_lowercase());
                    self.settings.channel_options = combo_box::State::new(opts.clone());
                    self.settings.channels = opts;
                }
                Task::none()
            }
            Message::ChannelSelected(opt) => {
                self.settings.selected_channel = opt.id;
                Task::none()
            }
            Message::AddChannel => {
                let key = format!(
                    "guild:{}/channel:{}",
                    self.settings.selected_guild, self.settings.selected_channel
                );
                if !self.settings.selected_guild.is_empty()
                    && !self.settings.selected_channel.is_empty()
                    && !self.settings.manual_chats.iter().any(|r| r.key == key)
                {
                    self.settings.manual_chats.push(ManualChat {
                        key,
                        schedule: self.settings.default_schedule.clone(),
                        schedule_options: schedule_state(),
                        retention: self.settings.default_retention.clone(),
                        archive: self.settings.default_archive,
                    });
                }
                Task::none()
            }
            Message::DmRowToggled(i, v) => {
                if let Some(r) = self.settings.dm_rows.get_mut(i) {
                    r.enabled = v;
                }
                Task::none()
            }
            Message::DmRowSchedule(i, v) => {
                if let Some(r) = self.settings.dm_rows.get_mut(i) {
                    r.schedule = v;
                }
                Task::none()
            }
            Message::DmRowRetention(i, v) => {
                if let Some(r) = self.settings.dm_rows.get_mut(i) {
                    r.retention = v;
                }
                Task::none()
            }
            Message::DmRowArchive(i, v) => {
                if let Some(r) = self.settings.dm_rows.get_mut(i) {
                    r.archive = v;
                }
                Task::none()
            }
            Message::ManualKeyChanged(i, v) => {
                if let Some(r) = self.settings.manual_chats.get_mut(i) {
                    r.key = v;
                }
                Task::none()
            }
            Message::ManualSchedule(i, v) => {
                if let Some(r) = self.settings.manual_chats.get_mut(i) {
                    r.schedule = v;
                }
                Task::none()
            }
            Message::ManualRetention(i, v) => {
                if let Some(r) = self.settings.manual_chats.get_mut(i) {
                    r.retention = v;
                }
                Task::none()
            }
            Message::ManualArchive(i, v) => {
                if let Some(r) = self.settings.manual_chats.get_mut(i) {
                    r.archive = v;
                }
                Task::none()
            }
            Message::RemoveManualChat(i) => {
                if i < self.settings.manual_chats.len() {
                    self.settings.manual_chats.remove(i);
                }
                Task::none()
            }
            Message::AddManualChat => {
                self.settings.manual_chats.push(ManualChat {
                    key: "guild:".into(),
                    schedule: "daily".into(),
                    schedule_options: schedule_state(),
                    retention: self.settings.default_retention.clone(),
                    archive: false,
                });
                Task::none()
            }
            Message::DeleteAllClicked(chat) => {
                // Two-click armed "Delete ALL now": first click arms, second
                // sends run-now with all=true (retention 0) through the daemon.
                if self.settings.armed_delete_all.as_deref() == Some(chat.as_str()) {
                    self.settings.armed_delete_all = None;
                    Task::perform(
                        crate::ipc::send(serde_json::json!({ "cmd": "run_now", "chat": chat.clone(), "all": true })),
                        move |res| Message::DeleteAllDone(chat, res.map_err(|e| e.to_string())),
                    )
                } else {
                    self.settings.armed_delete_all = Some(chat);
                    Task::none()
                }
            }
            Message::DeleteAllDone(chat, res) => {
                match res {
                    Ok(resp) if resp["ok"] == true && resp["accepted"] == true => {
                        tracing::info!(chat = %chat, "delete-all accepted by daemon, running in background")
                    }
                    Ok(resp) if resp["ok"] == true => {
                        tracing::info!(chat = %chat, "delete-all done: {}", &resp["summary"])
                    }
                    Ok(resp) => tracing::warn!(chat = %chat, "delete-all failed: {}", &resp["error"]),
                    Err(e) => tracing::warn!(chat = %chat, "daemon unreachable: {e}"),
                }
                Task::none()
            }
            Message::Save => {
                self.settings.status = save_config(&self.config_path, &self.settings);
                Task::none()
            }
            Message::JobsFetched(res) => {
                match res {
                    Ok(resp) if resp["ok"] == true => {
                        self.jobs.running = resp["running"].as_array().cloned().unwrap_or_default();
                        self.jobs.last = resp["last"].clone();
                        self.jobs.note.clear();
                    }
                    Ok(resp) => {
                        self.jobs.note = resp["error"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| "daemon not running".to_string());
                    }
                    Err(e) => {
                        self.jobs.note = format!("daemon not running ({e})");
                    }
                }
                Task::none()
            }
            Message::LabelsFetched(labels) => {
                self.jobs.labels = labels;
                Task::none()
            }
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let nav = |label: &'static str, tab: Tab| {
            let b = button(text(label).size(16))
                .width(Length::Fill)
                .on_press(Message::TabSelected(tab));
            if self.tab == tab {
                b.style(button::primary)
            } else {
                b.style(button::secondary)
            }
        };
        let sidebar = container(
            column![
                text("guillotine").size(24),
                Space::new().height(Length::Fixed(12.0)),
                nav("Jobs", Tab::Jobs),
                nav("Settings", Tab::Settings),
                nav("Logs", Tab::Logs),
            ]
            .spacing(6)
            .padding(12)
            .width(Length::Fixed(150.0)),
        )
        .height(Length::Fill);

        let content = scrollable(
            container(match self.tab {
                Tab::Settings => self.settings_view(),
                Tab::Jobs => self.jobs_view(),
                Tab::Logs => self.logs_view(),
            })
            .padding(20)
            .width(Length::Fill),
        )
        .height(Length::Fill);

        row![sidebar, rule::vertical(1.0), container(content).width(Length::Fill)]
            .height(Length::Fill)
            .into()
    }

    fn heading<'a>(label: impl Into<String>) -> Element<'a, Message> {
        text(label.into()).size(19).into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let s = &self.settings;
        let mut col = column![].spacing(14).width(Length::Fill);

        col = col.push(Self::heading("Settings"));
        col = col.push(
            text(format!("config: {}", self.config_path.display()))
                .font(Font::MONOSPACE)
                .size(12)
                .color(muted()),
        );

        // -- Connection --
        col = col.push(Self::heading("Connection"));
        col = col.push(
            row![
                text("Token:").width(Length::Fixed(90.0)),
                text_input("token", &s.token)
                    .secure(true)
                    .on_input(Message::TokenChanged)
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );

        // -- Archives --
        col = col.push(Self::heading(format!("Archives ({} detected)", s.archives.len())));
        col = col.push(
            row![
                text("Archive folder:").width(Length::Fixed(110.0)),
                text_input("folder", &s.archive_dir)
                    .on_input(Message::ArchiveDirChanged)
                    .width(Length::Fill),
                button(text("detect")).on_press(Message::DetectArchives),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        if s.archives.is_empty() {
            col = col.push(text("no existing archives found in this folder yet").color(muted()));
        } else {
            let links = s
                .archives
                .iter()
                .map(|(label, path)| {
                    button(text(format!("• {label}")))
                        .style(button::text)
                        .padding(2)
                        .on_press(Message::ArchiveOpened(path.clone()))
                        .into()
                })
                .collect::<Vec<Element<'_, Message>>>();
            col = col.push(
                scrollable(column(links).spacing(2))
                    .height(Length::Fixed(90.0))
                    .width(Length::Fill),
            );
        }

        // -- DM scope --
        col = col.push(Self::heading("DM scope"));
        col = col.push(
            text("No 'all DMs' mode: nominate users, or exclude specific users.").color(muted()),
        );
        col = col.push(
            row![
                text("policy:").width(Length::Fixed(90.0)),
                combo_box(
                    &s.dm_policy_options,
                    "policy",
                    Some(&s.dm_policy),
                    Message::DmPolicySelected,
                ),
                text("user ids:").width(Length::Fixed(60.0)),
                text_input("comma-separated user ids", &s.dm_users)
                    .on_input(Message::DmUsersChanged)
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        col = col.push(
            row![
                text("DM defaults — schedule:").width(Length::Fixed(170.0)),
                combo_box(
                    &s.default_schedule_options,
                    "schedule",
                    Some(&s.default_schedule),
                    Message::DefaultScheduleSelected,
                ),
                text("retain days:").width(Length::Fixed(80.0)),
                text_input("days", &s.default_retention)
                    .on_input(Message::DefaultRetentionChanged)
                    .width(Length::Fixed(60.0)),
                checkbox(s.default_archive)
                    .label("archive before delete")
                    .on_toggle(Message::DefaultArchiveToggled),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );

        // -- DM chats --
        col = col.push(Self::heading(format!("DM chats ({} available)", s.dm_rows.len())));
        if s.dm_pending {
            col = col.push(text("fetching your DMs…").color(muted()));
        }
        if s.dm_rows.is_empty() && !s.dm_pending {
            col = col.push(text("no DMs found — check the token above").color(muted()));
        }
        let dm_rows = s
            .dm_rows
            .iter()
            .enumerate()
            .map(|(i, r)| self.dm_row_view(i, r))
            .collect::<Vec<Element<'_, Message>>>();
        col = col.push(
            scrollable(column(dm_rows).spacing(6))
                .height(Length::Fixed(280.0))
                .width(Length::Fill),
        );

        // -- Guild channels --
        col = col.push(Self::heading(format!("Guild channels ({})", s.manual_chats.len())));
        if s.guild_pending {
            col = col.push(text("fetching your guilds…").color(muted()));
        }
        if s.guilds.is_empty() && !s.guild_pending {
            col = col.push(text("no guilds found — check the token above").color(muted()));
        }
        let selected_guild = s.guilds.iter().find(|o| o.id == s.selected_guild);
        let selected_channel = s.channels.iter().find(|o| o.id == s.selected_channel);
        let key = format!("guild:{}/channel:{}", s.selected_guild, s.selected_channel);
        let can_add = !s.selected_guild.is_empty()
            && !s.selected_channel.is_empty()
            && !s.manual_chats.iter().any(|r| r.key == key);
        col = col.push(
            row![
                text("guild:").width(Length::Fixed(60.0)),
                combo_box(&s.guild_options, "select…", selected_guild, Message::GuildSelected),
                text("channel:").width(Length::Fixed(60.0)),
                combo_box(
                    &s.channel_options,
                    if s.channels_pending { "fetching…" } else { "select…" },
                    selected_channel,
                    Message::ChannelSelected,
                ),
                button(text("add channel"))
                    .on_press_maybe(if can_add { Some(Message::AddChannel) } else { None }),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        col = col.push(
            text("manual entry (fallback) — key format: guild:<guild_id>/channel:<channel_id>")
                .color(muted())
                .size(12),
        );
        let manual_rows = s
            .manual_chats
            .iter()
            .enumerate()
            .map(|(i, r)| self.manual_chat_view(i, r))
            .collect::<Vec<Element<'_, Message>>>();
        col = col.extend(manual_rows);
        col = col.push(button(text("add guild channel")).on_press(Message::AddManualChat));

        // -- Save --
        let status_color = if s.status.starts_with("Saved") {
            ok_color()
        } else {
            err_color()
        };
        let mut save_row = row![button(text("Save")).on_press(Message::Save)].spacing(10);
        if !s.status.is_empty() {
            save_row = save_row.push(text(s.status.clone()).color(status_color));
        }
        col = col.push(save_row);

        col.into()
    }

    fn dm_row_view<'a>(&'a self, i: usize, r: &'a DmRow) -> Element<'a, Message> {
        let mut col = column![].spacing(4);
        col = col.push(
            row![
                checkbox(r.enabled).on_toggle(move |v| Message::DmRowToggled(i, v)),
                text(r.name.clone()).size(15),
                text(r.user_id.clone()).font(Font::MONOSPACE).size(11).color(muted()),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        if r.enabled {
            let chat = format!("dm:{}", r.user_id);
            col = col.push(
                row![
                    Space::new().width(Length::Fixed(24.0)),
                    combo_box(&r.schedule_options, "schedule", Some(&r.schedule), move |v| {
                        Message::DmRowSchedule(i, v)
                    }),
                    text("retain days:"),
                    text_input("days", &r.retention)
                        .on_input(move |v| Message::DmRowRetention(i, v))
                        .width(Length::Fixed(60.0)),
                    checkbox(r.archive)
                        .label("archive before delete")
                        .on_toggle(move |v| Message::DmRowArchive(i, v)),
                    self.delete_all_button(&chat),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }
        container(col)
            .padding(6)
            .width(Length::Fill)
            .style(container::rounded_box)
            .into()
    }

    fn manual_chat_view<'a>(&'a self, i: usize, r: &'a ManualChat) -> Element<'a, Message> {
        row![
            text_input("guild:<id>/channel:<id>", &r.key)
                .on_input(move |v| Message::ManualKeyChanged(i, v))
                .width(Length::Fixed(300.0)),
            combo_box(&r.schedule_options, "schedule", Some(&r.schedule), move |v| {
                Message::ManualSchedule(i, v)
            }),
            text_input("days", &r.retention)
                .on_input(move |v| Message::ManualRetention(i, v))
                .width(Length::Fixed(60.0)),
            checkbox(r.archive)
                .label("archive")
                .on_toggle(move |v| Message::ManualArchive(i, v)),
            self.delete_all_button(&r.key),
            button(text("remove")).on_press(Message::RemoveManualChat(i)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// Two-click armed "Delete ALL now" button.
    fn delete_all_button(&self, chat: &str) -> Element<'_, Message> {
        let armed = self.settings.armed_delete_all.as_deref() == Some(chat);
        let label = if armed {
            "Confirm: delete ALL messages?"
        } else {
            "Delete all now"
        };
        let b = button(text(label).color(Color::WHITE))
            .on_press(Message::DeleteAllClicked(chat.to_string()));
        if armed {
            b.style(button::danger).into()
        } else {
            b.into()
        }
    }

    fn jobs_view(&self) -> Element<'_, Message> {
        let j = &self.jobs;
        let mut col = column![].spacing(12).width(Length::Fill);
        col = col.push(Self::heading("Jobs"));
        col = col.push(
            text("live view of the daemon's job registry, refreshed every second").color(muted()),
        );
        if !j.note.is_empty() {
            col = col.push(text(j.note.clone()).color(err_color()));
        }

        col = col.push(Self::heading(format!("Running now ({})", j.running.len())));
        if j.running.is_empty() {
            col = col.push(text("no jobs running").color(muted()));
        } else {
            col = col.push(self.jobs_header());
            for job in &j.running {
                col = col.push(self.jobs_row(job));
            }
        }

        let empty = j.last.as_object().is_none_or(|o| o.is_empty());
        col = col.push(Self::heading("Last run per chat"));
        if empty {
            col = col.push(text("no runs recorded yet").color(muted()));
        } else if let Some(obj) = j.last.as_object() {
            col = col.push(
                row![
                    text("chat").size(13).color(muted()),
                    Space::new().width(Length::Fixed(230.0)),
                    text("last run").size(13).color(muted()),
                    Space::new().width(Length::Fixed(130.0)),
                    text("deleted").size(13).color(muted()),
                    Space::new().width(Length::Fixed(80.0)),
                    text("failed").size(13).color(muted()),
                    Space::new().width(Length::Fixed(80.0)),
                    text("complete").size(13).color(muted()),
                ]
                .spacing(8),
            );
            for (chat, r) in obj {
                let label = self.label_for(chat, None);
                let failed = r["failed"].as_u64().unwrap_or(0);
                let complete = r["complete"].as_bool().unwrap_or(false);
                col = col.push(
                    row![
                        container(
                            column![
                                text(label).size(14),
                                text(chat.clone()).font(Font::MONOSPACE).size(11).color(muted()),
                            ]
                            .spacing(2)
                        )
                        .width(Length::Fixed(230.0)),
                        text(short_time(r["last_run"].as_str().unwrap_or("")))
                            .font(Font::MONOSPACE)
                            .size(12)
                            .width(Length::Fixed(130.0)),
                        text(r["deleted"].to_string()).width(Length::Fixed(80.0)),
                        text(r["failed"].to_string())
                            .color(if failed > 0 { err_color() } else { muted() })
                            .width(Length::Fixed(80.0)),
                        text(r["complete"].to_string())
                            .color(if complete { ok_color() } else { phase_color("running") }),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
        }
        col.into()
    }

    fn jobs_header(&self) -> Element<'_, Message> {
        row![
            text("chat").size(13).color(muted()),
            Space::new().width(Length::Fixed(230.0)),
            text("phase").size(13).color(muted()),
            Space::new().width(Length::Fixed(150.0)),
            text("progress").size(13).color(muted()),
            Space::new().width(Length::Fixed(190.0)),
            text("counts (del/fail/skip)").size(13).color(muted()),
            Space::new().width(Length::Fixed(130.0)),
            text("started").size(13).color(muted()),
        ]
        .spacing(8)
        .into()
    }

    fn jobs_row<'a>(&'a self, job: &'a serde_json::Value) -> Element<'a, Message> {
        let key = job["chat"].as_str().unwrap_or("");
        let label = self.label_for(key, job["label"].as_str());
        let phase = job["phase"].as_str().unwrap_or("");
        let note = job["note"].as_str().unwrap_or("");
        let scanned = job["scanned"].as_u64().unwrap_or(0);
        let done = job["deleted"].as_u64().unwrap_or(0)
            + job["failed"].as_u64().unwrap_or(0)
            + job["skipped"].as_u64().unwrap_or(0);
        let frac = if scanned > 0 {
            (done as f32 / scanned as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let counts = format!("{} / {} / {}", job["deleted"], job["failed"], job["skipped"]);
        row![
            container(
                column![
                    text(label).size(14),
                    text(key.to_string()).font(Font::MONOSPACE).size(11).color(muted()),
                ]
                .spacing(2)
            )
            .width(Length::Fixed(230.0)),
            container(
                column![
                    text(phase.to_string()).color(phase_color(phase)),
                    text(note.to_string()).size(11).color(muted()),
                ]
                .spacing(2)
            )
            .width(Length::Fixed(150.0)),
            progress_bar(0.0..=1.0, frac).length(Length::Fixed(160.0)),
            text(counts).width(Length::Fixed(130.0)),
            text(short_time(job["started"].as_str().unwrap_or("")))
                .font(Font::MONOSPACE)
                .size(12),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// Friendly label for a chat key: the daemon-provided job label first,
    /// then the locally fetched map, else the raw key.
    fn label_for(&self, key: &str, job_label: Option<&str>) -> String {
        if let Some(l) = job_label.filter(|l| !l.is_empty()) {
            return l.to_string();
        }
        self.jobs
            .labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    fn logs_view(&self) -> Element<'_, Message> {
        let l = &self.logs;
        let daemon_lines: Vec<Element<'_, Message>> = if l.daemon.is_empty() {
            vec![text("(no daemon logs yet — is `guillotine service run` active?)").into()]
        } else {
            l.daemon
                .iter()
                .map(|line| {
                    text(line.clone())
                        .font(Font::MONOSPACE)
                        .size(11)
                        .shaping(iced::widget::text::Shaping::Advanced)
                        .into()
                })
                .collect()
        };
        let gui_lines: Vec<Element<'_, Message>> = if l.gui.is_empty() {
            vec![text("(nothing logged yet)").into()]
        } else {
            l.gui
                .iter()
                .map(|line| {
                    text(line.clone())
                        .font(Font::MONOSPACE)
                        .size(11)
                        .shaping(iced::widget::text::Shaping::Advanced)
                        .into()
                })
                .collect()
        };
        column![
            Self::heading("Logs"),
            text("daemon (service) — data dir logs, latest file, tail").color(muted()),
            scrollable(column(daemon_lines).spacing(1))
                .height(Length::FillPortion(1))
                .width(Length::Fill),
            Space::new().height(Length::Fixed(8.0)),
            text("this gui process (in-memory, last 500)").color(muted()),
            scrollable(column(gui_lines).spacing(1))
                .height(Length::FillPortion(1))
                .width(Length::Fill),
        ]
        .spacing(6)
        .height(Length::Fill)
        .into()
    }
}

impl SettingsState {
    fn load(path: &Path) -> Self {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let parsed: Result<Config, _> = toml::from_str(&raw);
        let (token, archive_dir, archive_viewer, dm, chat) = match parsed {
            Ok(c) => (
                c.token.clone().unwrap_or_default(),
                c.archive_dir
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                c.archive_viewer.clone(),
                Some(c.dm.clone()),
                c.chat,
            ),
            Err(_) => (String::new(), String::new(), None, None, Default::default()),
        };
        let dm = dm.unwrap_or_default();

        // Prefill the archive dir with the per-OS default when unset.
        let archive_dir = if archive_dir.is_empty() {
            config::default_archive_dir().display().to_string()
        } else {
            archive_dir
        };
        let archives = detect_archives(Path::new(&archive_dir));

        // Split existing chat entries into DM rows and manual rows.
        let mut dm_rows: Vec<DmRow> = Vec::new();
        let mut manual_chats: Vec<ManualChat> = Vec::new();
        for (k, c) in chat {
            if let Some(user) = k.strip_prefix("dm:") {
                dm_rows.push(DmRow {
                    user_id: user.to_string(),
                    name: user.to_string(),
                    enabled: true,
                    schedule: c.schedule.to_string(),
                    schedule_options: schedule_state(),
                    retention: c.retention_days.to_string(),
                    archive: c.archive_before_delete,
                });
            } else {
                manual_chats.push(ManualChat {
                    key: k,
                    schedule: c.schedule.to_string(),
                    schedule_options: schedule_state(),
                    retention: c.retention_days.to_string(),
                    archive: c.archive_before_delete,
                });
            }
        }

        SettingsState {
            token,
            archive_dir,
            archive_viewer,
            dm_policy: format!("{:?}", dm.policy).to_lowercase(),
            dm_policy_options: combo_box::State::new(vec!["nominate".to_string(), "exclude".to_string()]),
            dm_users: dm.users.join(", "),
            default_schedule: dm.default_schedule.to_string(),
            default_schedule_options: schedule_state(),
            default_retention: dm.default_retention_days.to_string(),
            default_archive: dm.default_archive_before_delete,
            dm_rows,
            dm_pending: false,
            guilds: Vec::new(),
            guild_options: combo_box::State::new(Vec::new()),
            guild_pending: false,
            selected_guild: String::new(),
            channels: Vec::new(),
            channel_options: combo_box::State::new(Vec::new()),
            channels_for: String::new(),
            channels_pending: false,
            selected_channel: String::new(),
            archives,
            manual_chats,
            armed_delete_all: None,
            status: String::new(),
        }
    }
}

impl LogsState {
    fn new() -> Self {
        LogsState {
            daemon: Vec::new(),
            gui: Vec::new(),
            last_daemon_file: None,
        }
    }

    fn refresh(&mut self) {
        // Tail the newest daemon log file (rolling daily), last 300 lines.
        let log_dir = crate::config::data_dir().join("logs");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&log_dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        if let Some(latest) = files.last().cloned() {
            if self.last_daemon_file.as_deref() != Some(&latest) {
                self.daemon.clear();
                self.last_daemon_file = Some(latest.clone());
            }
            if let Ok(content) = std::fs::read_to_string(&latest) {
                let lines: Vec<&str> = content.lines().collect();
                let tail: Vec<String> = lines
                    .iter()
                    .rev()
                    .take(300)
                    .rev()
                    .map(|l| l.to_string())
                    .collect();
                self.daemon = tail;
            }
        }
        let buf = crate::gui_log_buffer().lock().unwrap_or_else(|e| e.into_inner());
        self.gui = buf.clone();
    }
}

/// Atomic TOML save: validate rows first, then temp file + rename with 0600.
/// Returns the status line to show in the UI.
fn save_config(path: &Path, s: &SettingsState) -> String {
    // Validate schedules/retentions first so a bad row doesn't corrupt the file.
    for row in s.dm_rows.iter().filter(|r| r.enabled) {
        if let Err(msg) = validate_row(&row.schedule, &row.retention) {
            return msg;
        }
    }
    for row in &s.manual_chats {
        if let Err(msg) = validate_row(&row.schedule, &row.retention) {
            return msg;
        }
    }

    let mut doc = toml::Table::new();
    if s.token.is_empty() {
        doc.remove("token"); // let GUILLOTINE_TOKEN supply it
    } else {
        doc.insert("token".into(), toml::Value::String(s.token.clone()));
    }
    // round-trip the viewer path so saving from the GUI doesn't drop it
    if let Some(v) = &s.archive_viewer {
        doc.insert("archive_viewer".into(), toml::Value::String(v.display().to_string()));
    }
    if !s.archive_dir.is_empty() {
        doc.insert(
            "archive_dir".into(),
            toml::Value::String(s.archive_dir.clone()),
        );
    }
    let mut dm = toml::Table::new();
    dm.insert("policy".into(), toml::Value::String(s.dm_policy.clone()));
    let users: Vec<toml::Value> = s
        .dm_users
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| toml::Value::String(x.to_string()))
        .collect();
    dm.insert("users".into(), toml::Value::Array(users));
    dm.insert(
        "default_schedule".into(),
        toml::Value::String(s.default_schedule.clone()),
    );
    if let Ok(n) = s.default_retention.parse::<u32>() {
        dm.insert("default_retention_days".into(), toml::Value::Integer(n as i64));
    }
    dm.insert(
        "default_archive_before_delete".into(),
        toml::Value::Boolean(s.default_archive),
    );
    doc.insert("dm".into(), toml::Value::Table(dm));

    let mut chats = toml::Table::new();
    for row in s.dm_rows.iter().filter(|r| r.enabled) {
        let mut t = toml::Table::new();
        t.insert("schedule".into(), toml::Value::String(row.schedule.clone()));
        if let Ok(n) = row.retention.parse::<u32>() {
            t.insert("retention_days".into(), toml::Value::Integer(n as i64));
        }
        t.insert("archive_before_delete".into(), toml::Value::Boolean(row.archive));
        chats.insert(format!("dm:{}", row.user_id), toml::Value::Table(t));
    }
    for row in &s.manual_chats {
        let mut t = toml::Table::new();
        t.insert("schedule".into(), toml::Value::String(row.schedule.clone()));
        if let Ok(n) = row.retention.parse::<u32>() {
            t.insert("retention_days".into(), toml::Value::Integer(n as i64));
        }
        t.insert("archive_before_delete".into(), toml::Value::Boolean(row.archive));
        chats.insert(row.key.clone(), toml::Value::Table(t));
    }
    doc.insert("chat".into(), toml::Value::Table(chats));

    let rendered = toml::to_string_pretty(&doc).unwrap_or_default();
    // Atomic write: temp file + rename, so a crash mid-save can't corrupt
    // the only config file.
    let tmp = path.with_extension("toml.part");
    let write_result = std::fs::write(&tmp, rendered).and_then(|_| std::fs::rename(&tmp, path));
    match write_result {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            format!("Saved {}", path.display())
        }
        Err(e) => format!("Save failed: {e}"),
    }
}

/// Existing archive folders in `dir`, as ("name (chat label)", path) pairs.
fn detect_archives(dir: &Path) -> Vec<(String, PathBuf)> {
    crate::archive_format::list_archives(dir)
        .iter()
        .map(|p| {
            let meta = crate::archive_format::read_meta(p);
            let chat = meta.get("chat").and_then(|c| c.as_str()).unwrap_or("");
            let name = p.file_name().unwrap().to_string_lossy();
            let label = if chat.is_empty() {
                name.into_owned()
            } else {
                format!("{name} ({chat})")
            };
            (label, p.clone())
        })
        .collect()
}

/// Fetch the user's DM partners from the API: (user id, "Display (@username)").
async fn fetch_dm_users(client: Option<Arc<crate::api::Client>>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(c) = client {
        if let Ok(channels) = c.get_dm_channels().await {
            for (user, _channel) in c.dm_users(&channels) {
                let name = user
                    .global_name
                    .clone()
                    .unwrap_or_else(|| user.username.clone());
                out.push((user.id, format!("{name} (@{})", user.username)));
            }
        }
    }
    out
}

/// Fetch the user's guilds from the API: (guild id, name).
async fn fetch_guilds(client: Option<Arc<crate::api::Client>>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(c) = client {
        if let Ok(guilds) = c.get_guilds().await {
            for g in guilds {
                out.push((g.id, g.name));
            }
        }
    }
    out
}

/// Fetch a guild's text channels from the API: (channel id, "#name").
async fn fetch_guild_channels(client: &crate::api::Client, guild_id: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(channels) = client.get_guild_channels(guild_id).await {
        for c in channels {
            out.push((c.id, format!("#{}", c.name)));
        }
    }
    out
}

/// Owned-client variant of [`fetch_guild_channels`] for use in `Task::perform`.
async fn fetch_guild_channels_owned(
    client: Option<Arc<crate::api::Client>>,
    guild_id: String,
) -> Vec<(String, String)> {
    match client {
        Some(c) => fetch_guild_channels(&c, &guild_id).await,
        None => Vec::new(),
    }
}

fn validate_row(schedule: &str, retention: &str) -> Result<(), String> {
    use std::str::FromStr;
    if crate::config::Schedule::from_str(schedule).is_err() {
        Err(format!("invalid schedule {schedule:?} (use daily/weekly/monthly)"))
    } else if retention.parse::<u32>().is_err() {
        Err(format!("invalid retention {retention:?} (days)"))
    } else {
        Ok(())
    }
}
