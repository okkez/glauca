//! Desktop-notification support shared by the TUI and GUI front-ends.
//!
//! [`ItemTracker`] is a per-query session baseline counting how many items a background
//! sync surfaced — the `updated` half of `logic::ChangeCounts` and only that half, see
//! [`ItemTracker::observe`]. [`notify_updated_items`] is the OS notification primitive.
//!
//! The *decision* to fire (the on/off toggle, the `background` flag) stays in each
//! front-end's message handler; `engine.rs` never calls this module.

use std::collections::HashMap;

use crate::types::ItemEntry;

/// Item identity used to diff one sync against the previous one. Mirrors the
/// key `logic::count_changes` uses: (repo_owner, repo_name, number).
type ItemKey = (String, String, i64);

/// Per-query, in-memory baseline of the items last seen *this session*, used to
/// count how many items a background sync newly surfaced or changed.
///
/// Kept per session, not persisted: the goal is to notify about changes that arrive
/// *while the app runs*, not to replay everything unread since last launch. The first
/// observation only establishes the baseline, suppressing a startup notification storm.
#[derive(Default)]
pub struct ItemTracker {
    /// query_id -> (item key -> its last-seen `updated_at`).
    seen: HashMap<i64, HashMap<ItemKey, String>>,
}

impl ItemTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the latest `items` for `query_id` and return how many are new or updated
    /// versus the previous snapshot — the same rule as
    /// [`crate::logic::ChangeCounts::updated`].
    ///
    /// Only the fresh side is walked, so *removals are never counted* and a sync that
    /// merely pruned cannot fire a notification. The divergence from
    /// [`crate::logic::count_changes`], which does count removals for the banner, is
    /// deliberate: a disappearing item is not worth interrupting the user about.
    ///
    /// Returns `None` on the first observation of a query this session — that call only
    /// establishes the baseline, so callers must not notify on it.
    pub fn observe(&mut self, query_id: i64, items: &[ItemEntry]) -> Option<usize> {
        let snapshot: HashMap<ItemKey, String> = items
            .iter()
            .map(|it| {
                (
                    (it.repo_owner.clone(), it.repo_name.clone(), it.number),
                    it.updated_at.clone(),
                )
            })
            .collect();
        // Diff against the previous snapshot (if any) before replacing it.
        let changed = self.seen.get(&query_id).map(|prev_snapshot| {
            snapshot
                .iter()
                .filter(|(key, updated)| {
                    prev_snapshot
                        .get(*key)
                        .is_none_or(|prev_updated| prev_updated != *updated)
                })
                .count()
        });
        self.seen.insert(query_id, snapshot);
        changed
    }

    /// Observe `items` (always updating the baseline) and return the count to
    /// put in a desktop notification, or `None` if none should fire.
    ///
    /// Fires only when this load came from a `background` sync, `enabled` is set, and at
    /// least one item is new or updated. The baseline is maintained even when `enabled` is
    /// false, so toggling notifications on mid-session diffs against what was already seen
    /// rather than re-announcing everything.
    pub fn changed_count_to_notify(
        &mut self,
        query_id: i64,
        items: &[ItemEntry],
        background: bool,
        enabled: bool,
    ) -> Option<usize> {
        let n = self.observe(query_id, items)?;
        (background && enabled && n > 0).then_some(n)
    }
}

/// Show a best-effort OS desktop notification for new/updated items in a query.
///
/// Synchronous — on Linux a blocking D-Bus round-trip — so callers must run it off their
/// UI/event loop. Any error (no notification daemon, etc.) is non-fatal but logged, so a
/// silent notification outage stays diagnosable.
pub fn notify_updated_items(query_name: &str, count: usize) {
    if let Err(e) = notify_rust::Notification::new()
        .summary("Glauca")
        .body(&format!("{query_name}: {count} updated"))
        .appname("Glauca")
        .show()
    {
        tracing::warn!(error = %e, "desktop notification failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(owner: &str, repo: &str, number: i64, updated_at: &str) -> ItemEntry {
        ItemEntry {
            repo_owner: owner.into(),
            repo_name: repo.into(),
            number,
            updated_at: updated_at.into(),
            ..Default::default()
        }
    }

    #[test]
    fn first_observation_only_establishes_baseline() {
        let mut tracker = ItemTracker::new();
        let items = vec![item("o", "r", 1, "t1"), item("o", "r", 2, "t1")];
        assert_eq!(tracker.observe(7, &items), None);
    }

    /// A sync that only pruned must not notify: `observe` walks the fresh side only, so a
    /// shrinking list counts as zero changes.
    #[test]
    fn removals_never_notify() {
        let mut tracker = ItemTracker::new();
        tracker.observe(7, &[item("o", "r", 1, "t1"), item("o", "r", 2, "t1")]);

        let remaining = vec![item("o", "r", 1, "t1")];
        assert_eq!(tracker.observe(7, &remaining), Some(0));
        assert_eq!(
            tracker.changed_count_to_notify(7, &remaining, true, true),
            None
        );
    }

    #[test]
    fn counts_new_and_updated_against_baseline() {
        let mut tracker = ItemTracker::new();
        tracker.observe(7, &[item("o", "r", 1, "t1"), item("o", "r", 2, "t1")]);
        // #2 unchanged, #1 updated_at bumped, #3 newly appeared => 2 changed.
        let changed = tracker.observe(
            7,
            &[
                item("o", "r", 1, "t2"),
                item("o", "r", 2, "t1"),
                item("o", "r", 3, "t1"),
            ],
        );
        assert_eq!(changed, Some(2));
    }

    #[test]
    fn no_change_reports_zero() {
        let mut tracker = ItemTracker::new();
        let items = vec![item("o", "r", 1, "t1")];
        tracker.observe(7, &items);
        assert_eq!(tracker.observe(7, &items), Some(0));
    }

    #[test]
    fn changed_count_to_notify_gates_on_flags_and_baseline() {
        let mut tracker = ItemTracker::new();
        let base = [item("o", "r", 1, "t1")];
        let grown = [item("o", "r", 1, "t1"), item("o", "r", 2, "t1")];
        // First load establishes the baseline: never notify, even with bg+enabled.
        assert_eq!(tracker.changed_count_to_notify(7, &base, true, true), None);
        // A foreground load (not background) never notifies, but still updates
        // the baseline so the next diff is against `grown`.
        assert_eq!(
            tracker.changed_count_to_notify(7, &grown, false, true),
            None
        );
        // No new items now (same set) => nothing to notify.
        assert_eq!(tracker.changed_count_to_notify(7, &grown, true, true), None);
        // A genuinely new item with bg+enabled => notify with its count.
        let more = [
            item("o", "r", 1, "t1"),
            item("o", "r", 2, "t1"),
            item("o", "r", 3, "t1"),
        ];
        assert_eq!(
            tracker.changed_count_to_notify(7, &more, true, true),
            Some(1)
        );
        // Disabled => no notification even when items change.
        let evenmore = [
            item("o", "r", 1, "t1"),
            item("o", "r", 2, "t1"),
            item("o", "r", 3, "t1"),
            item("o", "r", 4, "t1"),
        ];
        assert_eq!(
            tracker.changed_count_to_notify(7, &evenmore, true, false),
            None
        );
    }

    #[test]
    fn queries_are_tracked_independently() {
        let mut tracker = ItemTracker::new();
        assert_eq!(tracker.observe(1, &[item("o", "r", 1, "t1")]), None);
        // A different query's first observation is still a baseline.
        assert_eq!(tracker.observe(2, &[item("o", "r", 9, "t1")]), None);
        assert_eq!(tracker.observe(1, &[item("o", "r", 1, "t1")]), Some(0));
    }
}
