//! The Iced panel app core: boot, subscriptions, tray-event handling, the
//! update loop, and the window shell. The three tab views live in
//! `settings.rs` / `jobs.rs` / `logs.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{button, column, combo_box, container, row, rule, scrollable, text, Space};
use iced::{window, Element, Length, Size, Subscription, Task, Theme};

use muda::MenuEvent;

use super::{
    detect_archives, fetch_chat_labels, fetch_dm_users, fetch_guild_channels_owned, fetch_guilds,
    open_archive, save_config, schedule_state, DmRow, JobsState, LogsState, ManualChat, Opt,
    SettingsState, Tab,
};

pub(crate) struct PanelApp {
    pub(crate) tab: Tab,
    /// The currently open panel window, if any. The window is transient:
    /// X destroys it (the daemon keeps running) and the next tray click opens
    /// a new one with `window::open`.
    pub(crate) window_id: Option<window::Id>,
    pub(crate) menu_rx: mpsc::Receiver<MenuEvent>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) client: Option<Arc<crate::api::Client>>,
    pub(crate) config_path: PathBuf,
    pub(crate) settings: SettingsState,
    pub(crate) jobs: JobsState,
    pub(crate) logs: LogsState,
}
#[derive(Clone)]
pub(crate) enum Message {
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
pub(crate) fn panel_theme(_: &PanelApp, _: window::Id) -> Theme {
    Theme::Dark
}

pub(crate) fn panel_title(_: &PanelApp, _: window::Id) -> String {
    String::from("guillotine")
}

fn panel_window_settings() -> window::Settings {
    window::Settings {
        icon: crate::gui::window_icon(),
        size: Size::new(1080.0, 760.0),
        min_size: Some(Size::new(820.0, 560.0)),
        visible: true,
        ..Default::default()
    }
}

impl PanelApp {
    pub(crate) fn boot(
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

    pub(crate) fn subscription(&self) -> Subscription<Message> {
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

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
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

    pub(crate) fn view(&self, _window: window::Id) -> Element<'_, Message> {
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

    pub(crate) fn heading<'a>(label: impl Into<String>) -> Element<'a, Message> {
        text(label.into()).size(19).into()
    }

}
