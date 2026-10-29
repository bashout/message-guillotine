//! Tray icon app: per-chat status, "run now", and a settings window
//! (eframe/egui) that edits the TOML config. Talks to the running service
//! daemon over loopback IPC so run-now stays inside the single-deletion
//! lock; falls back to running directly when no daemon is up.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use muda::accelerator::Accelerator;
use muda::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::config::{self, ChatKey, Config};

/// A simple generated 32x32 RGBA icon (a dark square with a lighter blade),
/// so no asset files are needed.
fn tray_icon_rgba() -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let in_blade = y >= 6 && y < 20 && x >= 8 && x < 26 && (x + y) % 7 != 0;
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

pub async fn run(config_arg: Option<&PathBuf>) -> anyhow::Result<()> {
    // GTK must be initialized before any tray/menu is created on Linux.
    #[cfg(target_os = "linux")]
    gtk::init()?;
    let (cfg, path) = crate::load_config(config_arg)?;
    let chats: Vec<String> = cfg.chats().map(|(k, _)| k.to_string()).collect();
    let archive_dir = cfg.resolve_archive_dir(None);
    let viewer = cfg
        .archive_viewer
        .clone()
        .or_else(|| std::env::var_os("GUILLOTINE_VIEWER").map(PathBuf::from));

    let menu = build_menu(&chats, &archive_dir, viewer.as_deref())?;
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

    handle_events(rx, path)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn handle_events(rx: mpsc::Receiver<MenuEvent>, config_path: PathBuf) -> anyhow::Result<()> {
    // tray-icon/muda on Linux are GTK: pump its main loop so menu events are
    // delivered (esp. on Wayland via XWayland/appindicator).
    let rt = tokio::runtime::Handle::current();
    loop {
        while gtk::events_pending() {
            gtk::main_iteration();
        }
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(event) => {
                if !dispatch(event.id().0.as_str(), &config_path, &rt) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn handle_events(rx: mpsc::Receiver<MenuEvent>, config_path: PathBuf) -> anyhow::Result<()> {
    let rt = tokio::runtime::Handle::current();
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(event) => {
                if !dispatch(event.id().0.as_str(), &config_path, &rt) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Returns false when the app should quit.
fn dispatch(id: &str, config_path: &PathBuf, rt: &tokio::runtime::Handle) -> bool {
    match id {
        "settings" => {
            let path = config_path.clone();
            let rt = rt.clone();
            std::thread::spawn(move || {
                if let Err(e) = settings_window(path, rt) {
                    tracing::error!(error = %e, "settings window failed");
                }
            });
        }
        "jobs" => {
            let rt = rt.clone();
            std::thread::spawn(move || {
                if let Err(e) = jobs_window(rt) {
                    tracing::error!(error = %e, "jobs window failed");
                }
            });
        }
        "status" => {
            rt.spawn(async {
                match crate::ipc::send(serde_json::json!({ "cmd": "status" })).await {
                    Ok(resp) => {
                        tracing::info!(status = %resp, "status");
                        println!("guillotine status: {}", serde_json::to_string_pretty(&resp).unwrap_or_default());
                    }
                    Err(_) => println!("guillotine daemon is not running"),
                }
            });
        }
        "quit" => return false,
        other => {
            if let Some(archive_path) = other.strip_prefix("view:") {
                open_archive(Path::new(archive_path));
            } else if let Some(chat) = other.strip_prefix("run:") {
                let chat = chat.to_string();
                rt.spawn(async move {
                    match crate::ipc::send(serde_json::json!({ "cmd": "run_now", "chat": chat })).await {
                        Ok(resp) if resp["ok"] == true => {
                            tracing::info!(chat = %chat, "run-now done: {}", &resp["summary"]);
                        }
                        Ok(resp) => tracing::warn!(chat = %chat, "run-now failed: {}", &resp["error"]),
                        Err(e) => tracing::warn!(chat = %chat, "daemon unreachable, running directly: {e}"),
                    }
                });
            }
        }
    }
    true
}

/// Open an archive folder in the configured Python viewer, or the OS file
/// manager when no viewer is configured.
fn open_archive(folder: &Path) {
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

fn build_menu(chats: &[String], archive_dir: &Path, _viewer: Option<&Path>) -> anyhow::Result<Menu> {
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
            run_sub.append(&MenuItem::with_id(
                format!("run:{chat}"),
                chat,
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
// Settings window (egui)
// ---------------------------------------------------------------------------

/// One clickable DM user row: checking it adds a [chat."dm:<id>"] job.
struct DmJobRow {
    user_id: String,
    name: String,
    enabled: bool,
    schedule: String,
    retention: String,
    archive: bool,
}

/// A manually entered (guild channel) chat row.
struct ChatRow {
    key: String,
    schedule: String,
    retention: String,
    archive: bool,
}

struct SettingsApp {
    path: PathBuf,
    token: String,
    archive_dir: String,
    dm_policy: String,
    dm_users: String,
    default_schedule: String,
    default_retention: String,
    default_archive: bool,
    dm_rows: Vec<DmJobRow>,
    armed_delete_all: Option<String>,
    dm_pending: bool,
    dm_rx: mpsc::Receiver<Vec<(String, String)>>,
    archives: Vec<String>,
    manual_chats: Vec<ChatRow>,
    status: String,
}

const SCHEDULE_OPTIONS: [&str; 3] = ["daily", "weekly", "monthly"];

impl SettingsApp {
    fn load(path: &PathBuf, rt: &tokio::runtime::Handle) -> Self {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let parsed: Result<Config, _> = toml::from_str(&raw);
        let (token, archive_dir, dm, chat) = match parsed {
            Ok(c) => (
                c.token.clone().unwrap_or_default(),
                c.archive_dir
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                Some(c.dm.clone()),
                c.chat,
            ),
            Err(_) => (String::new(), String::new(), None, Default::default()),
        };
        let dm = dm.unwrap_or_default();

        // Prefill the archive dir with the per-OS default when unset.
        let archive_dir = if archive_dir.is_empty() {
            config::default_archive_dir().display().to_string()
        } else {
            archive_dir
        };
        let archives = detect_archives(Path::new(&archive_dir));

        // Split existing chat entries into DM rows (matched against the
        // fetched DM list later) and manual rows.
        let mut dm_rows: Vec<DmJobRow> = Vec::new();
        let mut manual_chats: Vec<ChatRow> = Vec::new();
        for (k, c) in chat {
            if let Some(user) = k.strip_prefix("dm:") {
                dm_rows.push(DmJobRow {
                    user_id: user.to_string(),
                    name: user.to_string(),
                    enabled: true,
                    schedule: c.schedule.to_string(),
                    retention: c.retention_days.to_string(),
                    archive: c.archive_before_delete,
                });
            } else {
                manual_chats.push(ChatRow {
                    key: k,
                    schedule: c.schedule.to_string(),
                    retention: c.retention_days.to_string(),
                    archive: c.archive_before_delete,
                });
            }
        }

        // Fetch the available DM users in the background so they can be
        // clicked instead of typed.
        let (tx, rx) = mpsc::channel::<Vec<(String, String)>>();
        let dm_pending = !token.is_empty();
        if dm_pending {
            let token = token.clone();
            rt.spawn(async move {
                let _ = tx.send(fetch_dm_users(&token).await);
            });
        }

        SettingsApp {
            path: path.clone(),
            token,
            archive_dir,
            dm_policy: format!("{:?}", dm.policy).to_lowercase(),
            dm_users: dm.users.join(", "),
            default_schedule: dm.default_schedule.to_string(),
            default_retention: dm.default_retention_days.to_string(),
            default_archive: dm.default_archive_before_delete,
            dm_rows,
            armed_delete_all: None,
            dm_pending,
            dm_rx: rx,
            archives,
            manual_chats,
            status: String::new(),
        }
    }

    /// Poll the background DM fetch; merge names into existing rows and add
    /// new users as disabled rows.
    fn poll_dm_fetch(&mut self) {
        match self.dm_rx.try_recv() {
            Ok(users) => {
                self.dm_pending = false;
                for (id, name) in users {
                    if let Some(row) = self.dm_rows.iter_mut().find(|r| r.user_id == id) {
                        row.name = name;
                    } else {
                        self.dm_rows.push(DmJobRow {
                            user_id: id,
                            name,
                            enabled: false,
                            schedule: "daily".to_string(),
                            retention: self.default_retention.clone(),
                            archive: self.default_archive,
                        });
                    }
                }
                self.dm_rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.dm_pending = false,
        }
    }

    fn save(&mut self) {
        // Validate schedules/retentions first so a bad row doesn't corrupt the file.
        for row in self.dm_rows.iter().filter(|r| r.enabled) {
            validate_row(&row.schedule, &row.retention, &mut self.status);
            if !self.status.is_empty() {
                return;
            }
        }
        for row in &self.manual_chats {
            validate_row(&row.schedule, &row.retention, &mut self.status);
            if !self.status.is_empty() {
                return;
            }
        }

        let mut doc = toml::Table::new();
        doc.insert("token".into(), toml::Value::String(self.token.clone()));
        doc.insert(
            "archive_dir".into(),
            toml::Value::String(self.archive_dir.clone()),
        );
        let mut dm = toml::Table::new();
        dm.insert("policy".into(), toml::Value::String(self.dm_policy.clone()));
        let users: Vec<toml::Value> = self
            .dm_users
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| toml::Value::String(s.to_string()))
            .collect();
        dm.insert("users".into(), toml::Value::Array(users));
        dm.insert(
            "default_schedule".into(),
            toml::Value::String(self.default_schedule.clone()),
        );
        if let Ok(n) = self.default_retention.parse::<u32>() {
            dm.insert("default_retention_days".into(), toml::Value::Integer(n as i64));
        }
        dm.insert(
            "default_archive_before_delete".into(),
            toml::Value::Boolean(self.default_archive),
        );
        doc.insert("dm".into(), toml::Value::Table(dm));

        let mut chats = toml::Table::new();
        for row in self.dm_rows.iter().filter(|r| r.enabled) {
            let mut t = toml::Table::new();
            t.insert("schedule".into(), toml::Value::String(row.schedule.clone()));
            if let Ok(n) = row.retention.parse::<u32>() {
                t.insert("retention_days".into(), toml::Value::Integer(n as i64));
            }
            t.insert("archive_before_delete".into(), toml::Value::Boolean(row.archive));
            chats.insert(format!("dm:{}", row.user_id), toml::Value::Table(t));
        }
        for row in &self.manual_chats {
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
        match std::fs::write(&self.path, rendered) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
                }
                self.status = format!("Saved {}", self.path.display());
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }
}

/// Existing archive folders in `dir`, as "name (chat label)" strings.
fn detect_archives(dir: &Path) -> Vec<String> {
    crate::archive_format::list_archives(dir)
        .iter()
        .map(|p| {
            let meta = crate::archive_format::read_meta(p);
            let chat = meta.get("chat").and_then(|c| c.as_str()).unwrap_or("");
            if chat.is_empty() {
                p.file_name().unwrap().to_string_lossy().into_owned()
            } else {
                format!("{} ({})", p.file_name().unwrap().to_string_lossy(), chat)
            }
        })
        .collect()
}

/// Fetch the user's DM partners from the API: (user id, "Display (@username)").
async fn fetch_dm_users(token: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(mut client) = crate::api::Client::new(token) else {
        return out;
    };
    if client.get_current_user().await.ok().flatten().is_none() {
        return out;
    }
    if let Ok(channels) = client.get_dm_channels().await {
        for (user, _channel) in client.dm_users(&channels) {
            let name = user
                .global_name
                .clone()
                .unwrap_or_else(|| user.username.clone());
            out.push((user.id, format!("{name} (@{})", user.username)));
        }
    }
    out
}

/// Two-click armed "Delete ALL now" button: first click arms, second sends
/// run-now with all=true (retention 0) through the daemon's IPC.
fn delete_all_button(ui: &mut egui::Ui, armed: &mut Option<String>, chat: &str) {
    let is_armed = armed.as_deref() == Some(chat);
    let label = if is_armed { "Confirm: delete ALL messages?" } else { "Delete all now" };
    if ui.button(egui::RichText::new(label).color(egui::Color32::LIGHT_RED)).clicked() {
        if is_armed {
            *armed = None;
            let chat = chat.to_string();
            let rt = tokio::runtime::Handle::current();
            rt.spawn(async move {
                match crate::ipc::send(serde_json::json!({ "cmd": "run_now", "chat": chat, "all": true })).await {
                    Ok(resp) if resp["ok"] == true => tracing::info!(chat = %chat, "delete-all done: {}", &resp["summary"]),
                    Ok(resp) => tracing::warn!(chat = %chat, "delete-all failed: {}", &resp["error"]),
                    Err(e) => tracing::warn!(chat = %chat, "daemon unreachable: {e}"),
                }
            });
        } else {
            *armed = Some(chat.to_string());
        }
    }
}

fn validate_row(schedule: &str, retention: &str, status: &mut String) {
    use std::str::FromStr;
    if crate::config::Schedule::from_str(schedule).is_err() {
        *status = format!("invalid schedule {schedule:?} (use daily/weekly/monthly)");
    } else if retention.parse::<u32>().is_err() {
        *status = format!("invalid retention {retention:?} (days)");
    } else {
        status.clear();
    }
}

fn schedule_combo(ui: &mut egui::Ui, schedule: &mut String) {
    egui::ComboBox::from_id_salt(format!("sched-{schedule:p}"))
        .selected_text(schedule.as_str())
        .show_ui(ui, |ui| {
            for opt in SCHEDULE_OPTIONS {
                ui.selectable_value(schedule, opt.to_string(), opt);
            }
        });
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_dm_fetch();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("guillotine settings");
            ui.label(format!("config file: {}", self.path.display()));
            ui.add_space(6.0);

            egui::CollapsingHeader::new("Connection")
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Token:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token)
                                .password(true)
                                .desired_width(480.0),
                        );
                    });
                });

            egui::CollapsingHeader::new(format!(
                "Archives ({} detected)",
                self.archives.len()
            ))
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Archive folder:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.archive_dir).desired_width(480.0),
                    );
                    if ui.button("detect").clicked() {
                        self.archives = detect_archives(Path::new(&self.archive_dir));
                    }
                });
                if !self.archives.is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(90.0)
                        .show(ui, |ui| {
                            for a in &self.archives {
                                ui.label(format!("• {a}"));
                            }
                        });
                } else {
                    ui.label("no existing archives found in this folder yet");
                }
            });

            egui::CollapsingHeader::new("DM scope")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("No 'all DMs' mode: nominate users, or exclude specific users.");
                    ui.horizontal(|ui| {
                        ui.label("policy:");
                        egui::ComboBox::from_id_salt("dm_policy")
                            .selected_text(&self.dm_policy)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.dm_policy, "nominate".into(), "nominate (only listed)");
                                ui.selectable_value(&mut self.dm_policy, "exclude".into(), "exclude (all except listed)");
                            });
                        ui.label("user ids:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.dm_users).desired_width(240.0),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("DM defaults — schedule:");
                        schedule_combo(ui, &mut self.default_schedule);
                        ui.label("retain days:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.default_retention)
                                .desired_width(50.0),
                        );
                        ui.checkbox(&mut self.default_archive, "archive before delete");
                    });
                });

            egui::CollapsingHeader::new(format!("DM chats ({} available)", self.dm_rows.len()))
                .default_open(true)
                .show(ui, |ui| {
                    if self.dm_pending {
                        ui.label("fetching your DMs…");
                    }
                    if self.dm_rows.is_empty() && !self.dm_pending {
                        ui.label("no DMs found — check the token above");
                    }
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for row in &mut self.dm_rows {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut row.enabled, "");
                                ui.label(format!("{}  [{}]", row.name, row.user_id));
                            });
                            if row.enabled {
                                ui.horizontal(|ui| {
                                    ui.add_space(24.0);
                                    schedule_combo(ui, &mut row.schedule);
                                    ui.label("retain days:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut row.retention)
                                            .desired_width(50.0),
                                    );
                                    ui.checkbox(&mut row.archive, "archive before delete");
                                    delete_all_button(ui, &mut self.armed_delete_all, &format!("dm:{}", row.user_id));
                                });
                            }
                            ui.separator();
                        }
                    });
                });

            egui::CollapsingHeader::new(format!("Guild channels ({})", self.manual_chats.len()))
                .default_open(false)
                .show(ui, |ui| {
                    ui.label("key format: guild:<guild_id>/channel:<channel_id>");
                    let mut remove: Option<usize> = None;
                    for (i, row) in self.manual_chats.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut row.key).desired_width(260.0),
                            );
                            schedule_combo(ui, &mut row.schedule);
                            ui.add(
                                egui::TextEdit::singleline(&mut row.retention).desired_width(50.0),
                            );
                            ui.checkbox(&mut row.archive, "archive");
                            delete_all_button(ui, &mut self.armed_delete_all, &row.key);
                            if ui.button("remove").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        self.manual_chats.remove(i);
                    }
                    if ui.button("add guild channel").clicked() {
                        self.manual_chats.push(ChatRow {
                            key: "guild:".into(),
                            schedule: "daily".into(),
                            retention: self.default_retention.clone(),
                            archive: false,
                        });
                    }
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.save();
                }
                ui.label(&self.status);
            });
        });
    }
}

fn settings_window(path: PathBuf, rt: tokio::runtime::Handle) -> anyhow::Result<()> {
    let app = SettingsApp::load(&path, &rt);
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 640.0])
            .with_title("guillotine settings"),
        ..Default::default()
    };
    // The settings window runs on its own thread while the main thread owns
    // the GTK tray loop; winit on Linux must then be told any_thread is ok.
    #[cfg(target_os = "linux")]
    {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;
        options.event_loop_builder = Some(Box::new(|builder| {
            EventLoopBuilderExtWayland::with_any_thread(builder, true);
            EventLoopBuilderExtX11::with_any_thread(builder, true);
        }));
    }
    eframe::run_native(
        "guillotine settings",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

// Keep the config import used even if the GUI code changes shape.
#[allow(dead_code)]
fn _unused(_: &Config, _: ChatKey, _: &config::DmScope) {}

// ---------------------------------------------------------------------------
// Jobs window (live progress, auto-refreshing every second)
// ---------------------------------------------------------------------------

struct JobsApp {
    rx: mpsc::Receiver<serde_json::Value>,
    running: Vec<serde_json::Value>,
    last: serde_json::Value,
    note: String,
}

impl eframe::App for JobsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(resp) = self.rx.try_recv() {
            if resp["ok"] == true {
                self.running = resp["running"].as_array().cloned().unwrap_or_default();
                self.last = resp["last"].clone();
                self.note.clear();
            } else {
                self.note = "daemon not running".to_string();
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("guillotine jobs");
            if !self.note.is_empty() {
                ui.label(&self.note);
            }
            ui.add_space(6.0);
            ui.label("Running now:");
            if self.running.is_empty() {
                ui.label("  none");
            }
            egui::Grid::new("jobs-grid").striped(true).show(ui, |ui| {
                ui.label(egui::RichText::new("chat").strong());
                ui.label(egui::RichText::new("phase").strong());
                ui.label(egui::RichText::new("scanned").strong());
                ui.label(egui::RichText::new("deleted").strong());
                ui.label(egui::RichText::new("failed").strong());
                ui.label(egui::RichText::new("skipped").strong());
                ui.label(egui::RichText::new("since").strong());
                ui.end_row();
                for job in &self.running {
                    ui.label(job["chat"].as_str().unwrap_or(""));
                    ui.label(format!("{} — {}", job["phase"].as_str().unwrap_or(""), job["note"].as_str().unwrap_or("")));
                    ui.label(job["scanned"].to_string());
                    ui.label(job["deleted"].to_string());
                    ui.label(job["failed"].to_string());
                    ui.label(job["skipped"].to_string());
                    let started = job["started"].as_str().unwrap_or("");
                    ui.label(if started.len() > 19 { &started[..19] } else { started });
                    ui.end_row();
                }
            });
            ui.add_space(10.0);
            ui.label("Last run per chat:");
            let empty = self.last.as_object().map_or(true, |o| o.is_empty());
            if empty {
                ui.label("  none yet");
            }
            if let Some(obj) = self.last.as_object() {
                egui::Grid::new("last-grid").striped(true).show(ui, |ui| {
                    ui.label(egui::RichText::new("chat").strong());
                    ui.label(egui::RichText::new("last run").strong());
                    ui.label(egui::RichText::new("deleted").strong());
                    ui.label(egui::RichText::new("failed").strong());
                    ui.label(egui::RichText::new("complete").strong());
                    ui.end_row();
                    for (chat, r) in obj {
                        ui.label(chat);
                        let when = r["last_run"].as_str().unwrap_or("");
                        ui.label(if when.len() > 19 { &when[..19] } else { when });
                        ui.label(r["deleted"].to_string());
                        ui.label(r["failed"].to_string());
                        ui.label(r["complete"].to_string());
                        ui.end_row();
                    }
                });
            }
        });
    }
}

fn jobs_window(rt: tokio::runtime::Handle) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    rt.spawn(async move {
        loop {
            match crate::ipc::send(serde_json::json!({ "cmd": "jobs" })).await {
                Ok(resp) => {
                    if tx.send(resp).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(serde_json::json!({ "ok": false, "error": format!("{e}") }));
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    let app = JobsApp {
        rx,
        running: Vec::new(),
        last: serde_json::json!({}),
        note: String::new(),
    };
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 420.0])
            .with_title("guillotine jobs"),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;
        options.event_loop_builder = Some(Box::new(|builder| {
            EventLoopBuilderExtWayland::with_any_thread(builder, true);
            EventLoopBuilderExtX11::with_any_thread(builder, true);
        }));
    }
    eframe::run_native(
        "guillotine jobs",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
