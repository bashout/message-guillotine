//! Jobs tab: live view of the daemon's job registry (running jobs) and the
//! last run per chat, refreshed every second over IPC.

use std::collections::HashMap;

use iced::widget::{column, container, progress_bar, row, text, Space};
use iced::{Element, Font, Length};

use super::{err_color, muted, ok_color, phase_color, short_time, Message, PanelApp};

pub(crate) struct JobsState {
    pub(crate) running: Vec<serde_json::Value>,
    pub(crate) last: serde_json::Value,
    pub(crate) note: String,
    /// Chat key -> friendly label, fetched in the background from the API.
    pub(crate) labels: HashMap<String, String>,
}

impl PanelApp {
    pub(crate) fn jobs_view(&self) -> Element<'_, Message> {
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
                                text(chat.clone())
                                    .font(Font::MONOSPACE)
                                    .size(11)
                                    .color(muted()),
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
                        text(r["complete"].to_string()).color(if complete {
                            ok_color()
                        } else {
                            phase_color("running")
                        }),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
        }
        col.into()
    }

    pub(crate) fn jobs_header(&self) -> Element<'_, Message> {
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

    pub(crate) fn jobs_row<'a>(&'a self, job: &'a serde_json::Value) -> Element<'a, Message> {
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
        let counts = format!(
            "{} / {} / {}",
            job["deleted"], job["failed"], job["skipped"]
        );
        row![
            container(
                column![
                    text(label).size(14),
                    text(key.to_string())
                        .font(Font::MONOSPACE)
                        .size(11)
                        .color(muted()),
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
    pub(crate) fn label_for(&self, key: &str, job_label: Option<&str>) -> String {
        if let Some(l) = job_label.filter(|l| !l.is_empty()) {
            return l.to_string();
        }
        self.jobs
            .labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
}
