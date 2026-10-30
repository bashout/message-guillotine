//! Live progress of running jobs, shared between the scheduler, the IPC
//! server and (via IPC) the tray's Jobs window.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobProgress {
    pub chat: String,
    /// Friendly display name ("Display (@user)" / "Guild #channel"); falls
    /// back to `chat` when the lookup failed.
    pub label: String,
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
    pub fn start(&self, chat: &str, label: &str, phase: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).insert(
            chat.to_string(),
            JobProgress {
                chat: chat.to_string(),
                label: label.to_string(),
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

    pub fn snapshot(&self) -> Vec<JobProgress> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

pub type SharedRegistry = Arc<JobRegistry>;

pub fn new_registry() -> SharedRegistry {
    Arc::new(JobRegistry::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_update_finish_snapshot_semantics() {
        let reg = new_registry();
        assert!(reg.snapshot().is_empty());
        reg.start("dm:1", "Alice (@alice)", "starting");
        reg.set_phase("dm:1", "deleting", "keeping last 7 days");
        reg.update("dm:1", |p| {
            p.scanned = 100;
            p.deleted = 40;
            p.failed = 2;
            p.skipped = 3;
        });
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        let j = &snap[0];
        assert_eq!(j.chat, "dm:1");
        assert_eq!(j.label, "Alice (@alice)");
        assert_eq!(j.phase, "deleting");
        assert_eq!(j.note, "keeping last 7 days");
        assert_eq!((j.scanned, j.deleted, j.failed, j.skipped), (100, 40, 2, 3));
        assert!(!j.started.is_empty());
        assert!(j.updated >= j.started, "updated must not go backwards");

        reg.finish("dm:1", "deleted 40 (failed 2, skipped 3)");
        let j = &reg.snapshot()[0];
        assert_eq!(j.phase, "done");
        assert_eq!(j.note, "deleted 40 (failed 2, skipped 3)");
        // counters survive finish
        assert_eq!(j.deleted, 40);

        // a second job coexists; snapshot is sorted by chat key (BTreeMap)
        reg.start("dm:0", "Bob", "starting");
        let snap = reg.snapshot();
        let chats: Vec<&str> = snap.iter().map(|j| j.chat.as_str()).collect();
        assert_eq!(chats, vec!["dm:0", "dm:1"]);

        // update on an unknown chat is a no-op, not a panic or a new entry
        reg.update("dm:999", |p| p.deleted = 1);
        assert_eq!(reg.snapshot().len(), 2);

        // restart of the same chat resets counters
        reg.start("dm:1", "Alice (@alice)", "starting");
        let snap = reg.snapshot();
        let j = snap.iter().find(|j| j.chat == "dm:1").unwrap();
        assert_eq!(j.scanned, 0);
        assert_eq!(j.phase, "starting");
    }

    #[test]
    fn poisoned_lock_is_tolerated() {
        let reg = new_registry();
        // poison this registry's inner mutex by panicking while holding it
        let reg2 = reg.clone();
        let handle = std::thread::spawn(move || {
            let _g = reg2.inner.lock().unwrap();
            panic!("poison on purpose");
        });
        let _ = handle.join();
        // the registry must still work after poisoning (unwrap_or_else into_inner)
        reg.start("dm:1", "lbl", "starting");
        reg.update("dm:1", |p| p.deleted = 1);
        reg.finish("dm:1", "done");
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].deleted, 1);
        assert_eq!(snap[0].phase, "done");
    }
}
