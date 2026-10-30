//! Settings tab: edit the TOML config (token, archive folder, DM scope,
//! per-chat rows) and save it atomically after validation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::{
    button, checkbox, column, combo_box, container, row, scrollable, text, text_input, Space,
};
use iced::{Color, Element, Font, Length};

use crate::config::{self, Config};

use super::{err_color, muted, ok_color, schedule_state, Message, Opt, PanelApp};

/// One clickable DM user row: checking it adds a [chat."dm:<id>"] job.
pub(crate) struct DmRow {
    pub(crate) user_id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) schedule: String,
    pub(crate) schedule_options: combo_box::State<String>,
    pub(crate) retention: String,
    pub(crate) archive: bool,
}

pub(crate) struct ManualChat {
    pub(crate) key: String,
    pub(crate) schedule: String,
    pub(crate) schedule_options: combo_box::State<String>,
    pub(crate) retention: String,
    pub(crate) archive: bool,
}

pub(crate) struct SettingsState {
    pub(crate) token: String,
    pub(crate) archive_dir: String,
    pub(crate) archive_viewer: Option<PathBuf>,
    pub(crate) dm_policy: String,
    pub(crate) dm_policy_options: combo_box::State<String>,
    pub(crate) dm_users: String,
    pub(crate) default_schedule: String,
    pub(crate) default_schedule_options: combo_box::State<String>,
    pub(crate) default_retention: String,
    pub(crate) default_archive: bool,
    pub(crate) dm_rows: Vec<DmRow>,
    pub(crate) dm_pending: bool,
    pub(crate) guilds: Vec<Opt>,
    pub(crate) guild_options: combo_box::State<Opt>,
    pub(crate) guild_pending: bool,
    pub(crate) selected_guild: String,
    pub(crate) channels: Vec<Opt>,
    pub(crate) channel_options: combo_box::State<Opt>,
    /// Guild id the currently fetched `channels` belong to.
    pub(crate) channels_for: String,
    pub(crate) channels_pending: bool,
    pub(crate) selected_channel: String,
    pub(crate) archives: Vec<(String, PathBuf)>,
    pub(crate) manual_chats: Vec<ManualChat>,
    pub(crate) armed_delete_all: Option<String>,
    pub(crate) status: String,
}

impl SettingsState {
    pub(crate) fn load(path: &Path) -> Self {
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

impl PanelApp {
    pub(crate) fn settings_view(&self) -> Element<'_, Message> {
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

    pub(crate) fn dm_row_view<'a>(&'a self, i: usize, r: &'a DmRow) -> Element<'a, Message> {
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

    pub(crate) fn manual_chat_view<'a>(&'a self, i: usize, r: &'a ManualChat) -> Element<'a, Message> {
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
    pub(crate) fn delete_all_button(&self, chat: &str) -> Element<'_, Message> {
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

}

/// Returns the status line to show in the UI.
pub(crate) fn save_config(path: &Path, s: &SettingsState) -> String {
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
    // Atomic private write: temp file created 0600 + rename, so a crash
    // mid-save can't corrupt the only config file and the token is never
    // world-readable, even briefly.
    match config::write_file_private_atomic(path, &rendered) {
        Ok(()) => format!("Saved {}", path.display()),
        Err(e) => format!("Save failed: {e}"),
    }
}
/// Existing archive folders in `dir`, as ("name (chat label)", path) pairs.
pub(crate) fn detect_archives(dir: &Path) -> Vec<(String, PathBuf)> {
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
pub(crate) async fn fetch_dm_users(client: Option<Arc<crate::api::Client>>) -> Vec<(String, String)> {
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
pub(crate) async fn fetch_guilds(client: Option<Arc<crate::api::Client>>) -> Vec<(String, String)> {
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
pub(crate) async fn fetch_guild_channels(client: &crate::api::Client, guild_id: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(channels) = client.get_guild_channels(guild_id).await {
        for c in channels {
            out.push((c.id, format!("#{}", c.name)));
        }
    }
    out
}

/// Owned-client variant of [`fetch_guild_channels`] for use in `Task::perform`.
pub(crate) async fn fetch_guild_channels_owned(
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
