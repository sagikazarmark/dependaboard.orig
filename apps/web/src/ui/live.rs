//! Live refresh: how the dashboard stays current without the user's hand.
//!
//! The read model changes behind the dashboard's back: webhooks land, the
//! hourly sweep runs, another user merges. The dashboard learns of it by
//! polling the projection's revision, one cheap read, and reloading the rows
//! only when the answer has moved. The same poll is what tells a manual sync
//! apart from nothing happening: while one is in flight the dashboard asks
//! every second, and the sync glyph spins until the pull requests' own
//! revision moves — the sweep writes every repository before it reaches a
//! pull request, and those writes are not what the glyph is waiting for.

use std::time::Duration;

use dependaboard_core::{ProjectionRevision, unix_seconds};
use dioxus::logger::tracing;
use dioxus::prelude::*;

use crate::api::load_projection_revision;
use crate::ui::dashboard_state::DashboardState;
use crate::ui::{POLL_INTERVAL, sleep};

/// How often the dashboard asks whether the projection has moved while it is
/// not waiting on anything in particular.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// How long a manual sync is followed, asked about every poll interval,
/// before the dashboard stops waiting for it and reloads anyway. A sync that
/// reaches the pull requests moves their revision within seconds; one that
/// does not — nothing reached the store, or the installation has no pull
/// requests for it to reach — is not going to.
pub(crate) const SYNC_FOLLOW_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the relative times are brought up to date.
pub(crate) const CLOCK_INTERVAL: Duration = Duration::from_secs(60);

/// [`REFRESH_INTERVAL`] in polls.
const REFRESH_TICKS: u32 = (REFRESH_INTERVAL.as_secs() / POLL_INTERVAL.as_secs()) as u32;

/// [`SYNC_FOLLOW_TIMEOUT`] in polls.
const SYNC_FOLLOW_TICKS: u32 = (SYNC_FOLLOW_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs()) as u32;

/// What the dashboard does at one tick of the poll interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing until the next tick.
    Wait,
    /// Ask the server for the projection's revision.
    Poll,
    /// Stop following the manual sync in flight and reload the rows anyway.
    Reload,
}

/// What an answer to the poll said had moved since the answer before it.
/// The two are read apart rather than ranked: a pull request row moves both
/// counters, so the second seldom moves alone — but a database reset can
/// leave either where it was while moving the other, and each flag is still
/// right about its own rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Moved {
    /// A row of the read model changed: the rows must be reloaded.
    pub(crate) projection: bool,
    /// A pull request row changed: a manual sync in flight has reached the
    /// pull requests, which is what it was being followed for.
    pub(crate) pull_requests: bool,
}

/// The dashboard's refresh, decided one tick at a time: when to ask for the
/// revision, and whether an answer means the rows have moved.
///
/// Nothing is asked while the tab is hidden, and the first tick after it
/// shows asks at once. While a manual sync is in flight every tick asks, for
/// up to [`SYNC_FOLLOW_TIMEOUT`] of watching; otherwise one tick in
/// [`REFRESH_INTERVAL`] does.
#[derive(Debug)]
pub(crate) struct Refresh {
    /// Ticks since the last poll.
    since_poll: u32,
    /// Whether a poll is owed the moment the tab shows: at the start, and
    /// after time spent hidden.
    owed: bool,
    /// Visible ticks spent following the manual sync in flight.
    followed: u32,
    /// The revision the server last answered.
    seen: Option<ProjectionRevision>,
}

impl Refresh {
    /// A refresh that has seen nothing yet and owes its first poll.
    pub(crate) fn new() -> Self {
        Self {
            since_poll: 0,
            owed: true,
            followed: 0,
            seen: None,
        }
    }

    /// One poll interval has passed with the tab `visible` or not and a
    /// manual sync in flight (`syncing`) or not.
    pub(crate) fn tick(&mut self, visible: bool, syncing: bool) -> Step {
        if !syncing {
            self.followed = 0;
        }
        if !visible {
            self.owed = true;
            return Step::Wait;
        }
        if syncing {
            self.followed += 1;
            if self.followed > SYNC_FOLLOW_TICKS {
                self.followed = 0;
                return Step::Reload;
            }
        }
        self.since_poll += 1;
        if self.owed || syncing || self.since_poll >= REFRESH_TICKS {
            self.since_poll = 0;
            self.owed = false;
            Step::Poll
        } else {
            Step::Wait
        }
    }

    /// Takes in the revision the server answered; which of its counters
    /// differ from the last answer, in which case the rows they count have
    /// moved. The first answer is only remembered: it is asked for as the
    /// rows are, so it is what they show, give or take a write that lands
    /// between the two reads — which the next write, the hourly sweep's at
    /// the latest, brings in.
    pub(crate) fn observe(&mut self, revision: ProjectionRevision) -> Moved {
        let moved = match self.seen {
            Some(seen) => Moved {
                projection: seen.projection != revision.projection,
                pull_requests: seen.pull_requests != revision.pull_requests,
            },
            None => Moved {
                projection: false,
                pull_requests: false,
            },
        };
        self.seen = Some(revision);
        moved
    }
}

/// Reports whether the tab is showing, now and on every change. The body
/// never returns: returning would close the channel the listener sends on.
const VISIBILITY_SCRIPT: &str = r#"
    const report = () => dioxus.send(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", report);
    report();
    await new Promise(() => {});
"#;

/// Whether the tab is showing, as the browser reports it. Where there is no
/// browser to ask — the server's render — the tab counts as showing.
pub(crate) fn use_visibility() -> ReadSignal<bool> {
    let mut visible = use_signal(|| true);
    use_future(move || async move {
        let mut eval = document::eval(VISIBILITY_SCRIPT);
        while let Ok(showing) = eval.recv::<bool>().await {
            if *visible.peek() != showing {
                visible.set(showing);
            }
        }
    });
    visible.into()
}

/// The dashboard's clock, in Unix seconds: brought up to date once a
/// [`CLOCK_INTERVAL`] while the tab shows, and the moment it shows again
/// after being hidden, so nothing it dates is left saying what it said
/// before the tab was left.
pub(crate) fn use_clock(visible: ReadSignal<bool>) -> ReadSignal<u64> {
    let mut now = use_signal(unix_seconds);
    let mut was_showing = use_hook(|| CopyValue::new(true));
    use_effect(move || {
        let showing = visible();
        if showing && !was_showing.replace(showing) {
            now.set(unix_seconds());
        }
    });
    use_future(move || async move {
        loop {
            sleep(CLOCK_INTERVAL).await;
            if *visible.peek() {
                now.set(unix_seconds());
            }
        }
    });
    now.into()
}

/// Keeps the rows current: asks for the projection's revision as [`Refresh`]
/// schedules it, reloads the rows when it has moved, and ends the manual
/// sync in flight, if any, when the pull requests' own revision has: that is
/// the sweep reaching the pull requests, which is what the sync glyph was
/// spinning for. The first tick is taken at once, alongside the first read
/// of the rows, so the revision remembered is the one they were read at. An
/// answer that does not come is noted and waited out; the next tick asks
/// again.
pub(crate) fn use_live_refresh(mut state: DashboardState, visible: ReadSignal<bool>) {
    use_future(move || async move {
        let mut refresh = Refresh::new();
        loop {
            match refresh.tick(*visible.peek(), state.syncing()) {
                Step::Wait => {}
                Step::Poll => match load_projection_revision().await {
                    Ok(revision) => {
                        let moved = refresh.observe(revision);
                        if moved.pull_requests {
                            state.end_sync();
                        }
                        if moved.projection {
                            state.reload();
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the projection's revision could not be read");
                    }
                },
                Step::Reload => {
                    state.end_sync();
                    state.reload();
                }
            }
            sleep(POLL_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(refresh: &mut Refresh, ticks: u32, visible: bool, syncing: bool) {
        for tick in 1..=ticks {
            assert_eq!(
                refresh.tick(visible, syncing),
                Step::Wait,
                "tick {tick} of {ticks} (visible: {visible}, syncing: {syncing})"
            );
        }
    }

    #[test]
    fn the_first_tick_polls_and_then_one_in_every_refresh_interval_does() {
        let mut refresh = Refresh::new();

        assert_eq!(refresh.tick(true, false), Step::Poll);
        wait(&mut refresh, REFRESH_TICKS - 1, true, false);
        assert_eq!(refresh.tick(true, false), Step::Poll);
        wait(&mut refresh, REFRESH_TICKS - 1, true, false);
        assert_eq!(refresh.tick(true, false), Step::Poll);
    }

    /// A hidden tab is not looked at, so nothing is asked on its behalf; the
    /// moment it shows again the dashboard catches up, whatever the cadence
    /// would otherwise have said.
    #[test]
    fn nothing_is_asked_while_the_tab_is_hidden_and_showing_it_asks_at_once() {
        let mut refresh = Refresh::new();
        assert_eq!(refresh.tick(true, false), Step::Poll);
        wait(&mut refresh, 2, true, false);

        wait(&mut refresh, 3, false, false);

        assert_eq!(refresh.tick(true, false), Step::Poll);
        wait(&mut refresh, REFRESH_TICKS - 1, true, false);
        assert_eq!(refresh.tick(true, false), Step::Poll);
    }

    #[test]
    fn a_manual_sync_is_asked_about_every_tick_until_its_budget_and_then_reloaded_anyway() {
        let mut refresh = Refresh::new();

        for tick in 1..=SYNC_FOLLOW_TICKS {
            assert_eq!(refresh.tick(true, true), Step::Poll, "tick {tick}");
        }
        assert_eq!(refresh.tick(true, true), Step::Reload);

        // The dashboard ended the sync as it reloaded; the idle cadence
        // takes over.
        wait(&mut refresh, REFRESH_TICKS - 1, true, false);
        assert_eq!(refresh.tick(true, false), Step::Poll);
    }

    /// The budget is time spent watching: a sync started and then left in a
    /// hidden tab is still followed when the tab shows again.
    #[test]
    fn hidden_ticks_neither_ask_about_a_sync_nor_count_against_its_budget() {
        let mut refresh = Refresh::new();
        for _ in 0..SYNC_FOLLOW_TICKS - 1 {
            assert_eq!(refresh.tick(true, true), Step::Poll);
        }

        wait(&mut refresh, 5, false, true);

        assert_eq!(refresh.tick(true, true), Step::Poll);
        assert_eq!(refresh.tick(true, true), Step::Reload);
    }

    /// The rows were read moments before the first answer, so it is only
    /// remembered. Any other answer afterwards — a lower one too, which is a
    /// database that was reset — means the rows have moved. The two counters
    /// are told apart: a repository written on its own moves the rows, and
    /// says nothing of a sync having reached the pull requests.
    #[test]
    fn the_first_revision_is_remembered_and_any_other_one_says_which_rows_moved() {
        let mut refresh = Refresh::new();
        let at = |projection, pull_requests| ProjectionRevision {
            projection,
            pull_requests,
        };
        let still = Moved {
            projection: false,
            pull_requests: false,
        };

        assert_eq!(
            refresh.observe(at(5, 2)),
            still,
            "the first answer is only remembered"
        );
        assert_eq!(refresh.observe(at(5, 2)), still);

        assert_eq!(
            refresh.observe(at(6, 2)),
            Moved {
                projection: true,
                pull_requests: false,
            },
            "a repository row alone"
        );
        assert_eq!(refresh.observe(at(6, 2)), still);

        assert_eq!(
            refresh.observe(at(7, 3)),
            Moved {
                projection: true,
                pull_requests: true,
            },
            "a pull request row"
        );

        assert_eq!(
            refresh.observe(at(1, 0)),
            Moved {
                projection: true,
                pull_requests: true,
            },
            "a database that was reset"
        );
    }
}
