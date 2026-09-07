//! Live refresh: how the dashboard stays current without the user's hand,
//! and how it knows when it no longer is.
//!
//! The read model changes behind the dashboard's back: webhooks land, the
//! hourly sweep runs, another user merges. The dashboard learns of it by
//! polling the projection's revision, one cheap read, and reloading the rows
//! only when the answer has moved. The same poll is what tells a manual sync
//! apart from nothing happening: while one is in flight the dashboard asks
//! every second, and the sync glyph spins until the pull requests' own
//! revision moves — the sweep writes every repository before it reaches a
//! pull request, and those writes are not what the glyph is waiting for.
//!
//! The poll is also the dashboard's word on its line to the server. Polls
//! that get no revision are counted, and enough in a row mean the rows on
//! screen are no longer being kept current, which the dashboard says rather
//! than leave a green dot over stale rows; the server refusing the
//! credentials is told apart, since only signing in again cures that — and
//! it ends the polling, and the clock with it. Every 401 the browser gets
//! carries the server's challenge, which a same-origin fetch turns into the
//! browser's own credential prompt; asking again every ten seconds would
//! prompt every ten seconds. The banner says to reload, and the reload is
//! what starts the refresh over.

use std::time::Duration;

use dependaboard_core::{ProjectionRevision, unix_seconds};
use dioxus::logger::tracing;
use dioxus::prelude::*;

use crate::api::load_projection_revision;
use crate::ui::dashboard_state::{Connection, DashboardState};
use crate::ui::{Fault, POLL_INTERVAL, fault, sleep};

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

/// How many polls in a row must go unanswered before the dashboard counts
/// itself disconnected. One is a blip; this many is an outage, and the rows
/// on screen are no longer being kept current. At the idle cadence it is
/// reached some twenty seconds into an outage, while a sync is followed
/// within a few.
pub(crate) const DISCONNECT_THRESHOLD: u32 = 3;

/// What the dashboard does at one tick of the poll interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing until the next tick.
    Wait,
    /// Ask the server for the projection's revision.
    Poll,
    /// Stop following the manual sync in flight and reload the rows anyway.
    Reload,
    /// Ask nothing more: the server has refused the credentials, and every
    /// further ask would be refused the same — and turned into a credential
    /// prompt by the browser. Only a reload of the page signs in again, and
    /// a reload starts the refresh over.
    Stop,
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
/// revision, whether an answer means the rows have moved, and what the
/// answers — and the polls that got none — say about the line to the server.
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
    /// Polls in a row that got no revision, since the last one that did.
    misses: u32,
    /// Whether one of those misses was the server refusing the credentials,
    /// which ends the refresh: nothing is asked after it.
    refused: bool,
}

impl Refresh {
    /// A refresh that has seen nothing yet and owes its first poll.
    pub(crate) fn new() -> Self {
        Self {
            since_poll: 0,
            owed: true,
            followed: 0,
            seen: None,
            misses: 0,
            refused: false,
        }
    }

    /// The line to the server, as the polls since the last answer say. A
    /// refusal of the credentials is definitive — the server was reached and
    /// said no — and outranks the count; the count says the rest.
    pub(crate) fn connection(&self) -> Connection {
        if self.refused {
            Connection::SignedOut
        } else if self.misses >= DISCONNECT_THRESHOLD {
            Connection::Disconnected
        } else {
            Connection::Online
        }
    }

    /// One poll interval has passed with the tab `visible` or not and a
    /// manual sync in flight (`syncing`) or not. Once the credentials have
    /// been refused every tick says to stop, whatever the two are doing.
    pub(crate) fn tick(&mut self, visible: bool, syncing: bool) -> Step {
        if self.refused {
            return Step::Stop;
        }
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
    ///
    /// An answer after a poll that got none puts the line back online and
    /// says the rows moved whatever the revision: a read of the rows that
    /// failed in the gap is not retried by anything else, and it is the
    /// reload that clears the failure from the screen. A refusal of the
    /// credentials is not put back: nothing is asked after one, so no answer
    /// comes, and the page that signs in again is a fresh one.
    pub(crate) fn observe(&mut self, revision: ProjectionRevision) -> Moved {
        let restored = self.misses > 0;
        self.misses = 0;
        let moved = match self.seen {
            Some(seen) => Moved {
                projection: restored || seen.projection != revision.projection,
                pull_requests: seen.pull_requests != revision.pull_requests,
            },
            None => Moved {
                projection: restored,
                pull_requests: false,
            },
        };
        self.seen = Some(revision);
        moved
    }

    /// A poll got no revision, and `fault` is why; what the line to the
    /// server is now. The credentials being refused signs out at once,
    /// whatever the count stood at, and for good: every tick from then on is
    /// a [`Step::Stop`]. Any other miss counts towards
    /// [`DISCONNECT_THRESHOLD`], and the polls go on.
    pub(crate) fn miss(&mut self, fault: &Fault) -> Connection {
        self.misses += 1;
        self.refused |= *fault == Fault::SignedOut;
        self.connection()
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

/// Drives `now`, the dashboard's clock in Unix seconds: brought up to date
/// once a [`CLOCK_INTERVAL`] while the tab shows, and the moment it shows
/// again after being hidden, so nothing it dates is left saying what it said
/// before the tab was left. It stops with the polls once the server has
/// refused the credentials: nothing on the page is being kept current from
/// then on, and the times it dates are the page's as it was left, until the
/// reload the banner asks for.
pub(crate) fn use_clock(mut now: Signal<u64>, visible: ReadSignal<bool>, state: DashboardState) {
    let mut was_showing = use_hook(|| CopyValue::new(true));
    use_effect(move || {
        let showing = visible();
        let was_showing = was_showing.replace(showing);
        if showing && !was_showing && !state.signed_out() {
            now.set(unix_seconds());
        }
    });
    use_future(move || async move {
        loop {
            sleep(CLOCK_INTERVAL).await;
            if state.signed_out() {
                break;
            }
            if *visible.peek() {
                now.set(unix_seconds());
            }
        }
    });
}

/// Keeps the rows current: asks for the projection's revision as [`Refresh`]
/// schedules it, reloads the rows when it has moved, and ends the manual
/// sync in flight, if any, when the pull requests' own revision has: that is
/// the sweep reaching the pull requests, which is what the sync glyph was
/// spinning for. The first tick is taken at once, alongside the first read
/// of the rows, so the revision remembered is the one they were read at.
///
/// The polls are also the dashboard's word on its line to the server: each
/// answer dates the rows and puts the line online, each miss is counted, and
/// the state is told when the count says the line is down or the server
/// says the credentials are no longer good. The first answer after a miss
/// reloads the rows, which is what clears a read that failed in the gap.
///
/// A refusal of the credentials ends the refresh. [`Refresh`] says to stop
/// once its own poll was refused; and a step that would reach the server is
/// not taken once the state says the page is signed out on any poll's word —
/// a batch being followed asks every second, so its poll is often the one
/// refused first. Either way the answer would be another refusal, and
/// another credential prompt. The reload the banner asks for starts it over.
pub(crate) fn use_live_refresh(mut state: DashboardState, visible: ReadSignal<bool>) {
    use_future(move || async move {
        let mut refresh = Refresh::new();
        loop {
            match refresh.tick(*visible.peek(), state.syncing()) {
                Step::Wait => {}
                Step::Poll | Step::Reload if state.signed_out() => break,
                Step::Poll => match load_projection_revision().await {
                    Ok(revision) => {
                        let moved = refresh.observe(revision);
                        if moved.pull_requests {
                            state.end_sync();
                        }
                        if moved.projection {
                            state.reload();
                        }
                        state.poll_answered(unix_seconds());
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the projection's revision could not be read");
                        state.poll_missed(refresh.miss(&fault(&error)));
                    }
                },
                Step::Reload => {
                    state.end_sync();
                    state.reload();
                }
                Step::Stop => break,
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

    /// The revision the server answers with its counters at `projection`
    /// and `pull_requests`.
    fn at(projection: u64, pull_requests: u64) -> ProjectionRevision {
        ProjectionRevision {
            projection,
            pull_requests,
        }
    }

    /// An answer that moved nothing.
    fn still() -> Moved {
        Moved {
            projection: false,
            pull_requests: false,
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

        assert_eq!(
            refresh.observe(at(5, 2)),
            still(),
            "the first answer is only remembered"
        );
        assert_eq!(refresh.observe(at(5, 2)), still());

        assert_eq!(
            refresh.observe(at(6, 2)),
            Moved {
                projection: true,
                pull_requests: false,
            },
            "a repository row alone"
        );
        assert_eq!(refresh.observe(at(6, 2)), still());

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

    /// One poll going unanswered is a blip; [`DISCONNECT_THRESHOLD`] in a row
    /// is an outage, and the dashboard says so. An answer in between starts
    /// the count over. The first answer after any miss says the rows moved
    /// whatever the revision, since a read that failed in the gap is not
    /// retried by anything else; and it puts the line back online.
    #[test]
    fn polls_unanswered_in_a_row_disconnect_and_the_next_answer_reconnects_and_reloads() {
        let mut refresh = Refresh::new();
        refresh.observe(at(5, 2));
        assert_eq!(refresh.connection(), Connection::Online);

        for miss in 1..DISCONNECT_THRESHOLD {
            assert_eq!(
                refresh.miss(&Fault::Unreachable),
                Connection::Online,
                "miss {miss} of {DISCONNECT_THRESHOLD}"
            );
        }
        assert_eq!(
            refresh.observe(at(5, 2)),
            Moved {
                projection: true,
                pull_requests: false,
            },
            "the rows are reloaded after a miss, though the revision stands"
        );
        assert_eq!(refresh.connection(), Connection::Online);

        for _ in 0..DISCONNECT_THRESHOLD - 1 {
            refresh.miss(&Fault::Unreachable);
        }
        assert_eq!(
            refresh.miss(&Fault::Refused("The read model is unavailable".to_owned())),
            Connection::Disconnected,
            "a refused read of the revision is a miss like any other"
        );
        assert_eq!(refresh.connection(), Connection::Disconnected);
        assert_eq!(
            refresh.miss(&Fault::Unreachable),
            Connection::Disconnected,
            "and stays so"
        );

        assert_eq!(
            refresh.observe(at(6, 3)),
            Moved {
                projection: true,
                pull_requests: true,
            }
        );
        assert_eq!(refresh.connection(), Connection::Online);
        assert_eq!(refresh.observe(at(6, 3)), still());
    }

    /// The server answering 401 is definitive — it was reached, and refused
    /// the credentials the browser holds — so one is enough, whatever the
    /// count stood at, and there is no asking again: every 401 the browser
    /// gets carries a challenge it turns into a credential prompt, and only
    /// the reload the banner asks for signs in again. From the refusal on
    /// the refresh says to stop, whatever the tab and the sync are doing.
    #[test]
    fn one_refusal_of_the_credentials_signs_out_at_once_and_nothing_is_asked_after_it() {
        let mut refresh = Refresh::new();
        assert_eq!(refresh.tick(true, true), Step::Poll);

        assert_eq!(refresh.miss(&Fault::SignedOut), Connection::SignedOut);
        assert_eq!(refresh.connection(), Connection::SignedOut);

        for (visible, syncing) in [(true, false), (true, true), (false, false), (false, true)] {
            for tick in 1..=REFRESH_TICKS + 1 {
                assert_eq!(
                    refresh.tick(visible, syncing),
                    Step::Stop,
                    "tick {tick} (visible: {visible}, syncing: {syncing})"
                );
            }
        }
        assert_eq!(refresh.connection(), Connection::SignedOut);
    }

    /// A line that is down is asked again: the first answer after a miss is
    /// what clears the banner and reloads the rows, and a dead server answers
    /// nothing that the browser would turn into a prompt.
    #[test]
    fn a_disconnected_line_is_still_polled() {
        let mut refresh = Refresh::new();
        assert_eq!(refresh.tick(true, false), Step::Poll);
        for _ in 0..DISCONNECT_THRESHOLD {
            refresh.miss(&Fault::Unreachable);
        }
        assert_eq!(refresh.connection(), Connection::Disconnected);

        wait(&mut refresh, REFRESH_TICKS - 1, true, false);
        assert_eq!(refresh.tick(true, false), Step::Poll);
    }
}
