//! Live progress of running jobs, shared between the scheduler, the IPC
//! server and (via IPC) the tray's Jobs window.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobProgress {
    pub chat: String,
    pub phase: String,
    pub scanned: u64,
    pub deleted: u64,
    pub failed: u64,
    pub skipped: u64,
    pub started: String,
    pub updated: String,
    pub note: String,
}

#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<BTreeMap<String, JobProgress>>,
}

impl JobRegistry {
    pub fn start(&self, chat: &str, phase: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).insert(
            chat.to_string(),
            JobProgress {
                chat: chat.to_string(),
                phase: phase.to_string(),
                scanned: 0,
                deleted: 0,
                failed: 0,
                skipped: 0,
                started: now.clone(),
                updated: now,
                note: String::new(),
            },
        );
    }

    pub fn update(&self, chat: &str, f: impl FnOnce(&mut JobProgress)) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = inner.get_mut(chat) {
            f(p);
            p.updated = chrono::Utc::now().to_rfc3339();
        }
    }

    pub fn set_phase(&self, chat: &str, phase: &str, note: &str) {
        self.update(chat, |p| {
            p.phase = phase.to_string();
            p.note = note.to_string();
        });
    }

    pub fn finish(&self, chat: &str, note: &str) {
        self.update(chat, |p| {
            p.phase = "done".to_string();
            p.note = note.to_string();
        });
    }

    pub fn remove(&self, chat: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(chat);
    }

    pub fn snapshot(&self) -> Vec<JobProgress> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }
}

pub type SharedRegistry = Arc<JobRegistry>;

pub fn new_registry() -> SharedRegistry {
    Arc::new(JobRegistry::default())
}
