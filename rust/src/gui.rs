//! Tray icon app: per-chat status, "run now", and a settings window
//! (eframe/egui) that edits the TOML config. Talks to the running service
//! daemon over loopback IPC so run-now stays inside the single-deletion
//! lock; falls back to running directly when no daemon is up.

use std::path::PathBuf;
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

    let menu = build_menu(&chats)?;
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
            std::thread::spawn(move || {
                if let Err(e) = settings_window(path) {
                    tracing::error!(error = %e, "settings window failed");
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
            if let Some(chat) = other.strip_prefix("run:") {
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

fn build_menu(chats: &[String]) -> anyhow::Result<Menu> {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(
        "settings",
        "Settings…",
        true,
        None::<Accelerator>,
    ))?;
    menu.append(&MenuItem::with_id(
        "status",
        "Status",
        true,
        None::<Accelerator>,
    ))?;
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

struct SettingsApp {
    path: PathBuf,
    token: String,
    archive_dir: String,
    dm_policy: String,
    dm_users: String,
    default_schedule: String,
    default_retention: String,
    default_archive: bool,
    chats: Vec<ChatRow>,
    status: String,
}

struct ChatRow {
    key: String,
    schedule: String,
    retention: String,
    archive: bool,
}

impl SettingsApp {
    fn load(path: &PathBuf) -> Self {
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
        SettingsApp {
            path: path.clone(),
            token,
            archive_dir,
            dm_policy: format!("{:?}", dm.policy).to_lowercase(),
            dm_users: dm.users.join(", "),
            default_schedule: dm.default_schedule.to_string(),
            default_retention: dm.default_retention_days.to_string(),
            default_archive: dm.default_archive_before_delete,
            chats: chat
                .into_iter()
                .map(|(k, c)| ChatRow {
                    key: k,
                    schedule: c.schedule.to_string(),
                    retention: c.retention_days.to_string(),
                    archive: c.archive_before_delete,
                })
                .collect(),
            status: String::new(),
        }
    }

    fn save(&mut self) {
        let mut doc = toml::Table::new();
        doc.insert("token".into(), toml::Value::String(self.token.clone()));
        if !self.archive_dir.is_empty() {
            doc.insert(
                "archive_dir".into(),
                toml::Value::String(self.archive_dir.clone()),
            );
        }
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
            dm.insert(
                "default_retention_days".into(),
                toml::Value::Integer(n as i64),
            );
        }
        dm.insert(
            "default_archive_before_delete".into(),
            toml::Value::Boolean(self.default_archive),
        );
        doc.insert("dm".into(), toml::Value::Table(dm));
        let mut chats = toml::Table::new();
        for row in &self.chats {
            let mut t = toml::Table::new();
            t.insert("schedule".into(), toml::Value::String(row.schedule.clone()));
            if let Ok(n) = row.retention.parse::<u32>() {
                t.insert("retention_days".into(), toml::Value::Integer(n as i64));
            }
            t.insert(
                "archive_before_delete".into(),
                toml::Value::Boolean(row.archive),
            );
            chats.insert(row.key.clone(), toml::Value::Table(t));
        }
        doc.insert("chat".into(), toml::Value::Table(chats));
        let rendered = toml::to_string_pretty(&doc).unwrap_or_default();
        match std::fs::write(&self.path, rendered) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &self.path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
                self.status = format!("Saved {}", self.path.display());
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("guillotine settings");
            ui.label(format!("config file: {}", self.path.display()));
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Token:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.token)
                        .password(true)
                        .desired_width(420.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Archive dir:");
                ui.add(egui::TextEdit::singleline(&mut self.archive_dir).desired_width(420.0));
            });
            ui.add_space(8.0);

            ui.label("DM scope (no 'all' mode):");
            ui.horizontal(|ui| {
                ui.label("policy:");
                egui::ComboBox::from_id_salt("dm_policy")
                    .selected_text(&self.dm_policy)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.dm_policy, "nominate".into(), "nominate");
                        ui.selectable_value(&mut self.dm_policy, "exclude".into(), "exclude");
                    });
                ui.label("user ids (comma separated):");
                ui.add(egui::TextEdit::singleline(&mut self.dm_users).desired_width(280.0));
            });
            ui.horizontal(|ui| {
                ui.label("DM defaults — schedule:");
                ui.add(egui::TextEdit::singleline(&mut self.default_schedule).desired_width(120.0));
                ui.label("retain days:");
                ui.add(egui::TextEdit::singleline(&mut self.default_retention).desired_width(60.0));
                ui.checkbox(&mut self.default_archive, "archive before delete");
            });
            ui.add_space(8.0);

            ui.label("Chats (key: dm:<user_id> or guild:<id>/channel:<id>):");
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut remove: Option<usize> = None;
                for (i, row) in self.chats.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut row.key).desired_width(220.0));
                        ui.add(egui::TextEdit::singleline(&mut row.schedule).desired_width(120.0));
                        ui.add(egui::TextEdit::singleline(&mut row.retention).desired_width(60.0));
                        ui.checkbox(&mut row.archive, "archive");
                        if ui.button("remove").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.chats.remove(i);
                }
                if ui.button("add chat").clicked() {
                    self.chats.push(ChatRow {
                        key: "dm:".into(),
                        schedule: "daily".into(),
                        retention: "30".into(),
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

fn settings_window(path: PathBuf) -> anyhow::Result<()> {
    let app = SettingsApp::load(&path);
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 520.0])
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
