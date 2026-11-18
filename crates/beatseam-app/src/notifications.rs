//! Port of `store/notifications`: the notification bell's list - one-off errors and long-running background operations' live status.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    Error,
    Progress,
}

/// 'queued' before a progress task first starts, 'paused' while a background pass yields to foreground work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationStatus {
    Queued,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationProgress {
    pub current: u64,
    pub total: u64,
    /// `done` entries stay in the list until dismissed.
    pub done: bool,
}

/// One entry in the bell's list - either a one-off error or a long-running background operation's live status, upserted by id as
/// it progresses rather than appended fresh each time. Plain data on purpose: an entry's action callback can't live in state, so
/// only its label does (`action_label`) - the callback sits in [`NotificationActions`], keyed by the same id.
#[derive(Clone, Debug, PartialEq)]
pub struct NotificationEntry {
    pub id: String,
    pub kind: NotificationKind,
    /// Short, human-readable summary - what the bell's collapsed list shows.
    pub title: String,
    /// Full detail shown once a list item is expanded (the raw error message for an error).
    pub detail: Option<String>,
    pub created_at: u64,
    pub progress: Option<NotificationProgress>,
    /// Label of the secondary action rendered on the row (e.g. "Cancel").
    pub action_label: Option<String>,
    pub status: Option<NotificationStatus>,
}

/// A progress entry as callers describe it (no kind or creation time yet).
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressUpsert {
    pub id: String,
    pub title: String,
    pub detail: Option<String>,
    pub progress: Option<NotificationProgress>,
    pub action_label: Option<String>,
    pub status: Option<NotificationStatus>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotificationsState {
    /// Newest first.
    pub items: Vec<NotificationEntry>,
}

impl NotificationsState {
    pub fn error_added(&mut self, entry: NotificationEntry) {
        self.items.insert(0, entry);
    }

    /// Creates or replaces a progress entry by id (keeping its original creation time). `now` is used only for a new entry.
    pub fn progress_upserted(&mut self, entry: ProgressUpsert, now: u64) {
        let index = self.items.iter().position(|n| n.id == entry.id);
        let created_at = index.map_or(now, |i| self.items[i].created_at);
        let next = NotificationEntry { id: entry.id, kind: NotificationKind::Progress, title: entry.title, detail: entry.detail, created_at, progress: entry.progress, action_label: entry.action_label, status: entry.status };
        match index {
            Some(i) => self.items[i] = next,
            None => self.items.insert(0, next),
        }
    }

    pub fn dismissed(&mut self, id: &str) {
        self.items.retain(|n| n.id != id);
    }

    pub fn cleared(&mut self) {
        self.items.clear();
    }

    /// An error entry for `title`/`detail`, newest first. `id` should be unique (see [`random_id`](beatseam_server_mode::random_id)).
    pub fn add_error(&mut self, id: String, title: &str, detail: Option<&str>, now: u64) {
        self.error_added(NotificationEntry { id, kind: NotificationKind::Error, title: title.to_owned(), detail: detail.map(str::to_owned), created_at: now, progress: None, action_label: None, status: None });
    }
}

/// Callbacks for notification rows' secondary actions (e.g. "Cancel" on a running scan), keyed by notification id. Callbacks aren't
/// plain data, so the state keeps only the label and this registry is kept in step with it.
#[derive(Clone, Default)]
pub struct NotificationActions {
    actions: Rc<RefCell<HashMap<String, Rc<dyn Fn()>>>>,
}

impl NotificationActions {
    pub fn set(&self, id: &str, on_press: Option<Rc<dyn Fn()>>) {
        match on_press {
            Some(f) => {
                self.actions.borrow_mut().insert(id.to_owned(), f);
            }
            None => {
                self.actions.borrow_mut().remove(id);
            }
        }
    }

    pub fn run(&self, id: &str) {
        let action = self.actions.borrow().get(id).cloned();
        if let Some(action) = action {
            action();
        }
    }

    pub fn clear(&self) {
        self.actions.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn upsert(id: &str, title: &str, current: u64) -> ProgressUpsert {
        ProgressUpsert { id: id.into(), title: title.into(), detail: None, progress: Some(NotificationProgress { current, total: 10, done: false }), action_label: None, status: None }
    }

    #[test]
    fn errors_are_listed_newest_first() {
        let mut state = NotificationsState::default();
        state.add_error("1".into(), "first", Some("raw message"), 100);
        state.add_error("2".into(), "second", None, 200);
        assert_eq!(state.items.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["second", "first"]);
        assert_eq!((state.items[1].detail.as_deref(), state.items[1].kind, state.items[1].created_at), (Some("raw message"), NotificationKind::Error, 100));
    }

    #[test]
    fn progress_is_upserted_by_id_keeping_the_original_creation_time_and_position() {
        let mut state = NotificationsState::default();
        state.progress_upserted(upsert("scan", "Scanning", 1), 100);
        state.add_error("e".into(), "an error", None, 150);
        state.progress_upserted(upsert("scan", "Scanning", 5), 999);
        assert_eq!(state.items.len(), 2);
        let scan = state.items.iter().find(|n| n.id == "scan").unwrap();
        assert_eq!((scan.created_at, scan.progress.map(|p| p.current), scan.kind), (100, Some(5), NotificationKind::Progress));
        assert_eq!(state.items[1].id, "scan", "it keeps its slot below the newer error");
    }

    #[test]
    fn dismissing_and_clearing_remove_entries() {
        let mut state = NotificationsState::default();
        state.progress_upserted(upsert("a", "A", 0), 1);
        state.progress_upserted(upsert("b", "B", 0), 2);
        state.dismissed("a");
        assert_eq!(state.items.len(), 1);
        state.cleared();
        assert!(state.items.is_empty());
    }

    #[test]
    fn a_finished_entry_stays_until_dismissed() {
        let mut state = NotificationsState::default();
        let mut done = upsert("scan", "Scanned", 10);
        done.progress = Some(NotificationProgress { current: 10, total: 10, done: true });
        state.progress_upserted(done, 1);
        assert_eq!(state.items[0].progress.map(|p| p.done), Some(true));
    }

    #[test]
    fn row_actions_run_by_id_and_can_be_removed_or_cleared() {
        let actions = NotificationActions::default();
        let ran = Rc::new(Cell::new(0));
        let r = ran.clone();
        actions.set("scan", Some(Rc::new(move || r.set(r.get() + 1))));
        actions.run("scan");
        actions.run("missing");
        assert_eq!(ran.get(), 1);
        actions.set("scan", None);
        actions.run("scan");
        assert_eq!(ran.get(), 1);
        let r = ran.clone();
        actions.set("x", Some(Rc::new(move || r.set(r.get() + 10))));
        actions.clear();
        actions.run("x");
        assert_eq!(ran.get(), 1);
    }
}
