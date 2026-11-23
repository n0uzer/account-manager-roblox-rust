//! Which game window belongs to which account, and whether it is still there.
//!
//! The manager starts clients with `--app` and then only sees a list of
//! `RobloxPlayerBeta.exe` processes. The client may also hand the game over to a
//! process other than the one that was spawned, so the pid returned by `spawn`
//! cannot be trusted. Instead, launches are matched to processes by *order*: the
//! oldest client process that appeared after a launch belongs to the oldest launch
//! still waiting for its window. That is the same assumption the window grid
//! already makes ("order in the grid is order of launch").
//!
//! This module is pure bookkeeping: the caller feeds it the current process list
//! and the current time, and it answers with events. That keeps it testable
//! without any client running.

use std::collections::{HashMap, VecDeque};

use crate::processes::Proc;

/// One second in FILETIME ticks (100 ns).
pub const SECOND: u64 = 10_000_000;
/// A process may be stamped a moment before the time the launch was recorded.
const SLACK: u64 = 2 * SECOND;
/// A window that vanishes sooner than this after it was matched is taken for the
/// launcher handing the game to another process, not for a closed window.
pub const HANDOFF_GRACE: u64 = 60 * SECOND;
/// A launch that has produced no window after this long is given up.
pub const GIVE_UP: u64 = 120 * SECOND;

/// Relaunches allowed per account within [`RELAUNCH_WINDOW`].
pub const RELAUNCH_LIMIT: usize = 3;
pub const RELAUNCH_WINDOW: u64 = 10 * 60 * SECOND;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    account: String,
    /// When the launch was made; only processes created after it can match.
    launched_at: u64,
    /// When the wait started. Differs from `launched_at` for a launch that went
    /// back to waiting after a hand-off.
    since: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Live {
    account: String,
    pid: u32,
    matched_at: u64,
    launched_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The launch produced a window.
    Opened { account: String, pid: u32 },
    /// A window that had been stable for a while is gone.
    Lost { account: String },
    /// The launch never produced a window.
    NeverOpened { account: String },
}

#[derive(Debug, Default)]
pub struct Tracker {
    pending: VecDeque<Pending>,
    live: Vec<Live>,
}

impl Tracker {
    /// Record a launch made at `at` (FILETIME ticks, taken just before the
    /// spawn). Anything already tracked for the account is replaced.
    pub fn launched(&mut self, account: &str, at: u64) {
        self.forget(account);
        self.pending.push_back(Pending {
            account: account.to_string(),
            launched_at: at,
            since: at,
        });
    }

    /// Stop following an account. Call this *before* closing its window on
    /// purpose, or the closing would be reported as a lost window.
    pub fn forget(&mut self, account: &str) {
        self.pending.retain(|pending| pending.account != account);
        self.live.retain(|live| live.account != account);
    }

    /// Stop following everything.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.live.clear();
    }

    /// Process that currently hosts the account's window.
    pub fn window_of(&self, account: &str) -> Option<u32> {
        self.live
            .iter()
            .find(|live| live.account == account)
            .map(|live| live.pid)
    }

    /// Pids of the windows the manager opened itself. A targeted close uses
    /// this on Linux, where a Flatpak instance does not expose its `HOME`.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn window_pids(&self) -> Vec<u32> {
        self.live.iter().map(|live| live.pid).collect()
    }

    #[cfg(test)]
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Launches that have not produced a window yet.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.live.is_empty()
    }

    /// Compare the tracked windows with the processes that exist now.
    pub fn poll(&mut self, procs: &[Proc], now: u64) -> Vec<Event> {
        let mut events = Vec::new();

        // 1. Windows that disappeared.
        let mut requeue = Vec::new();
        let mut kept = Vec::new();
        for live in std::mem::take(&mut self.live) {
            if procs.iter().any(|proc| proc.pid == live.pid) {
                kept.push(live);
            } else if now.saturating_sub(live.matched_at) < HANDOFF_GRACE {
                // Too young to be a real window: wait for the process that takes
                // over instead of calling it lost.
                requeue.push(Pending {
                    account: live.account,
                    launched_at: live.launched_at,
                    since: now,
                });
            } else {
                events.push(Event::Lost {
                    account: live.account,
                });
            }
        }
        self.live = kept;
        for pending in requeue.into_iter().rev() {
            self.pending.push_front(pending);
        }

        // 2. New processes go to the oldest launch that could have made them.
        let mut fresh: Vec<&Proc> = procs
            .iter()
            .filter(|proc| !self.live.iter().any(|live| live.pid == proc.pid))
            .collect();
        fresh.sort_by_key(|proc| proc.created);
        for proc in fresh {
            let slot = self
                .pending
                .iter()
                .position(|pending| pending.launched_at <= proc.created + SLACK);
            if let Some(pending) = slot.and_then(|index| self.pending.remove(index)) {
                events.push(Event::Opened {
                    account: pending.account.clone(),
                    pid: proc.pid,
                });
                self.live.push(Live {
                    account: pending.account,
                    pid: proc.pid,
                    matched_at: now,
                    launched_at: pending.launched_at,
                });
            }
        }

        // 3. Launches that never got a window.
        let mut waiting = VecDeque::new();
        for pending in std::mem::take(&mut self.pending) {
            if now.saturating_sub(pending.since) >= GIVE_UP {
                events.push(Event::NeverOpened {
                    account: pending.account,
                });
            } else {
                waiting.push_back(pending);
            }
        }
        self.pending = waiting;

        events
    }
}

/// Keeps a broken account from being relaunched in a loop: at most
/// [`RELAUNCH_LIMIT`] times within [`RELAUNCH_WINDOW`].
#[derive(Debug, Default)]
pub struct RelaunchLimiter {
    history: HashMap<String, Vec<u64>>,
}

impl RelaunchLimiter {
    /// Whether a relaunch is allowed now; if it is, it is counted.
    pub fn allow(&mut self, account: &str, now: u64) -> bool {
        let times = self.history.entry(account.to_string()).or_default();
        times.retain(|at| now.saturating_sub(*at) < RELAUNCH_WINDOW);
        if times.len() >= RELAUNCH_LIMIT {
            return false;
        }
        times.push(now);
        true
    }

    pub fn reset(&mut self) {
        self.history.clear();
    }
}

/// Whether one more window may be started. `running` is every client process
/// that exists, `pending` the launches that have no process yet; both occupy a
/// slot. A limit of 0 means no limit.
pub fn has_free_slot(running: usize, pending: usize, max: u32) -> bool {
    max == 0 || running + pending < max as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000 * SECOND;

    fn proc(pid: u32, created: u64) -> Proc {
        Proc { pid, created }
    }

    #[test]
    fn windows_are_matched_in_launch_order() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.launched("b", T0 + 2 * SECOND);

        let events = tracker.poll(
            &[proc(20, T0 + 6 * SECOND), proc(10, T0 + 4 * SECOND)],
            T0 + 8 * SECOND,
        );
        assert_eq!(
            events,
            vec![
                Event::Opened {
                    account: "a".into(),
                    pid: 10
                },
                Event::Opened {
                    account: "b".into(),
                    pid: 20
                },
            ]
        );
        assert_eq!(tracker.window_of("a"), Some(10));
        assert_eq!(tracker.window_of("b"), Some(20));
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn processes_older_than_the_launch_are_not_ours() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        // Someone's own window, opened ten minutes earlier.
        let events = tracker.poll(&[proc(5, T0 - 600 * SECOND)], T0 + SECOND);
        assert!(events.is_empty());
        assert_eq!(tracker.window_of("a"), None);
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn a_stable_window_that_vanishes_is_lost() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.poll(&[proc(10, T0 + SECOND)], T0 + 5 * SECOND);

        let later = T0 + 5 * SECOND + HANDOFF_GRACE + SECOND;
        let events = tracker.poll(&[], later);
        assert_eq!(
            events,
            vec![Event::Lost {
                account: "a".into()
            }]
        );
        assert!(tracker.is_idle());
    }

    #[test]
    fn a_young_window_that_vanishes_is_a_handoff_not_a_loss() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.poll(&[proc(10, T0 + SECOND)], T0 + 5 * SECOND);

        // The launcher process exits 10 s later and the game runs in pid 11.
        let events = tracker.poll(&[], T0 + 15 * SECOND);
        assert!(events.is_empty());
        assert_eq!(tracker.pending_count(), 1);

        let events = tracker.poll(&[proc(11, T0 + 14 * SECOND)], T0 + 17 * SECOND);
        assert_eq!(
            events,
            vec![Event::Opened {
                account: "a".into(),
                pid: 11
            }]
        );
        assert_eq!(tracker.window_of("a"), Some(11));
    }

    #[test]
    fn a_launch_without_a_window_is_given_up() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        assert!(tracker.poll(&[], T0 + GIVE_UP - SECOND).is_empty());
        let events = tracker.poll(&[], T0 + GIVE_UP);
        assert_eq!(
            events,
            vec![Event::NeverOpened {
                account: "a".into()
            }]
        );
        assert!(tracker.is_idle());
    }

    #[test]
    fn forgetting_before_a_deliberate_close_prevents_a_lost_event() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.poll(&[proc(10, T0 + SECOND)], T0 + 5 * SECOND);

        tracker.forget("a");
        let events = tracker.poll(&[], T0 + 5 * SECOND + HANDOFF_GRACE + SECOND);
        assert!(events.is_empty());
    }

    #[test]
    fn launching_an_account_again_replaces_its_old_entry() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.poll(&[proc(10, T0 + SECOND)], T0 + 5 * SECOND);
        assert_eq!(tracker.window_of("a"), Some(10));

        tracker.launched("a", T0 + 100 * SECOND);
        assert_eq!(tracker.window_of("a"), None);
        assert_eq!(tracker.pending_count(), 1);
        assert_eq!(tracker.live_count(), 0);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut tracker = Tracker::default();
        tracker.launched("a", T0);
        tracker.launched("b", T0);
        tracker.clear();
        assert!(tracker.is_idle());
    }

    #[test]
    fn relaunches_are_capped_per_account() {
        let mut limiter = RelaunchLimiter::default();
        for step in 0..RELAUNCH_LIMIT as u64 {
            assert!(limiter.allow("a", T0 + step * SECOND));
        }
        assert!(!limiter.allow("a", T0 + 10 * SECOND));
        // Another account has its own budget.
        assert!(limiter.allow("b", T0 + 10 * SECOND));
        // The budget comes back once the window has passed.
        assert!(limiter.allow("a", T0 + RELAUNCH_WINDOW + 10 * SECOND));
    }

    #[test]
    fn slots_count_running_and_pending_windows() {
        assert!(has_free_slot(50, 50, 0));
        assert!(has_free_slot(2, 0, 3));
        assert!(!has_free_slot(3, 0, 3));
        assert!(!has_free_slot(2, 1, 3));
        assert!(has_free_slot(0, 0, 1));
    }
}
