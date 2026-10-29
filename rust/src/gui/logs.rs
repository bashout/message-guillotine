//! Logs tab: tail of the daemon's log file plus this process's in-memory
//! ring buffer.

use std::path::PathBuf;

use iced::widget::{column, scrollable, text, Space};
use iced::{Element, Font, Length};

use super::{muted, Message, PanelApp};

pub(crate) struct LogsState {
    pub(crate) daemon: Vec<String>,
    pub(crate) gui: Vec<String>,
    pub(crate) last_daemon_file: Option<PathBuf>,
}

impl LogsState {
    pub(crate) fn new() -> Self {
        LogsState {
            daemon: Vec::new(),
            gui: Vec::new(),
            last_daemon_file: None,
        }
    }

    pub(crate) fn refresh(&mut self) {
        // Tail the newest daemon log file (rolling daily), last 300 lines.
        let log_dir = crate::config::data_dir().join("logs");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&log_dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        // Prefer the active (unrotated) guillotine.log; else newest by mtime.
        let active = log_dir.join("guillotine.log");
        let latest = if active.is_file() {
            Some(active)
        } else {
            files.sort_by_key(|f| {
                std::fs::metadata(f).and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            });
            files.last().cloned()
        };
        if let Some(latest) = latest {
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

impl PanelApp {
    pub(crate) fn logs_view(&self) -> Element<'_, Message> {
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
