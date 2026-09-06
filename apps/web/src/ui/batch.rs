//! The bulk-action flow after confirmation: submit the batch to Restate, then
//! follow its progress until it completes. A batch the dashboard knows only by
//! id — after a reload, or picked from the audit view — is followed the same
//! way, without the submission.
//!
//! A batch is never declared lost: it is durable in Restate, and a target may
//! legitimately sit for hours behind GitHub's retry and rate-limit budgets. The
//! follow reports since when the batch has gone unchanged and whether its polls
//! are being answered, and leaves saying so to the drawer. It ends only when the
//! batch completes, when Restate would not take it, or when Restate has never
//! had progress for the id it was given.

use std::time::Duration;

use dependaboard_core::{BatchProgress, BulkActionKind, PrTarget, SubmittedTarget, unix_seconds};

use crate::api::{load_batch_progress, submit_batch};
use crate::ui::{POLL_INTERVAL, sleep, user_facing};

/// How many times a submission is tried, one poll interval apart, before the
/// batch is reported as not submitted.
pub(crate) const SUBMIT_ATTEMPTS: u32 = 5;

/// How long a batch followed by id alone may go with Restate answering that it
/// has no progress for it before the id is taken to name no batch. A stale
/// link, or one from another deployment, is given up in this time; a live
/// batch answers its first poll. Only answers count: a poll the server did not
/// answer says nothing about the batch, so an outage stretches the time rather
/// than spending it. A batch the dashboard submitted itself is not held to
/// this at all: Restate took it, and a service that is down or deploying
/// starts it late, not never.
pub(crate) const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// [`ATTACH_TIMEOUT`] in answered polls.
const ATTACH_LIMIT: u32 = (ATTACH_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs()) as u32;

/// The server as the follow sees it, so the follow can be driven by a script
/// in tests. Errors are already in their user-facing form.
pub(crate) trait BatchGateway {
    async fn progress(&mut self) -> Result<Option<BatchProgress>, String>;
    /// Waits one poll interval.
    async fn tick(&mut self);
    /// The wall clock, in Unix seconds.
    fn now(&self) -> u64;
}

/// A [`BatchGateway`] with a batch to submit.
pub(crate) trait SubmitGateway: BatchGateway {
    async fn submit(&mut self) -> Result<(), String>;
}

/// A batch known by id alone, followed through the server functions.
pub(crate) struct ServerFollow {
    pub(crate) batch_id: String,
}

impl BatchGateway for ServerFollow {
    async fn progress(&mut self) -> Result<Option<BatchProgress>, String> {
        load_batch_progress(self.batch_id.clone())
            .await
            .map_err(|error| user_facing(&error))
    }

    async fn tick(&mut self) {
        sleep(POLL_INTERVAL).await;
    }

    fn now(&self) -> u64 {
        unix_seconds()
    }
}

/// One batch to submit, driven through the server functions: a
/// [`ServerFollow`] with what to submit. The targets are kept in full so the
/// drawer can show them before the first progress arrives; the server is sent
/// only what it takes the browser's word on, the key and the head the user
/// saw.
pub(crate) struct ServerBatch {
    pub(crate) follow: ServerFollow,
    pub(crate) action: BulkActionKind,
    pub(crate) targets: Vec<PrTarget>,
}

impl BatchGateway for ServerBatch {
    async fn progress(&mut self) -> Result<Option<BatchProgress>, String> {
        self.follow.progress().await
    }

    async fn tick(&mut self) {
        self.follow.tick().await;
    }

    fn now(&self) -> u64 {
        self.follow.now()
    }
}

impl SubmitGateway for ServerBatch {
    async fn submit(&mut self) -> Result<(), String> {
        let targets = self.targets.iter().map(SubmittedTarget::from).collect();
        submit_batch(self.follow.batch_id.clone(), self.action, targets)
            .await
            .map_err(|error| user_facing(&error))
    }
}

/// What the dashboard knows of the batch it follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Followed {
    pub(crate) batch_id: String,
    /// Where the batch stands: Restate's last word once it has given one,
    /// before that the dashboard's own snapshot of what it queued, and nothing
    /// for a batch followed by id alone until Restate answers.
    pub(crate) progress: Option<BatchProgress>,
    /// Whether `progress` is Restate's word rather than the dashboard's own.
    pub(crate) heard: bool,
    /// Unix seconds when `progress` last changed, or the follow began.
    pub(crate) since: u64,
    /// Why the last poll was not answered, while the polls are failing.
    pub(crate) trouble: Option<String>,
}

impl Followed {
    /// A batch the dashboard has just queued: its own snapshot of the targets
    /// stands in until Restate's first.
    pub(crate) fn queued(
        batch_id: &str,
        action: BulkActionKind,
        targets: &[PrTarget],
        now: u64,
    ) -> Self {
        Self {
            batch_id: batch_id.to_owned(),
            progress: Some(BatchProgress::queued(batch_id, action, targets)),
            heard: false,
            since: now,
            trouble: None,
        }
    }

    /// A batch the dashboard knows by id alone and is asking Restate about.
    pub(crate) fn attaching(batch_id: &str, now: u64) -> Self {
        Self {
            batch_id: batch_id.to_owned(),
            progress: None,
            heard: false,
            since: now,
            trouble: None,
        }
    }

    /// How long the batch has stood unchanged, against `now`.
    pub(crate) fn unchanged_for(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.since))
    }

    /// Whether the batch has run to the end, as far as the dashboard has heard.
    pub(crate) fn completed(&self) -> bool {
        self.progress
            .as_ref()
            .is_some_and(|progress| progress.completed)
    }

    /// Takes `progress` as Restate's latest word, if it is news.
    fn hear(&mut self, progress: BatchProgress, now: u64) -> bool {
        if self.heard && self.progress.as_ref() == Some(&progress) {
            return false;
        }
        self.progress = Some(progress);
        self.heard = true;
        self.since = now;
        true
    }

    /// Notes how the last poll went, if that is news.
    fn answered(&mut self, trouble: Option<String>) -> bool {
        if self.trouble == trouble {
            return false;
        }
        self.trouble = trouble;
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BatchOutcome {
    Completed(BatchProgress),
    /// Restate never accepted the batch; carries the last submission error.
    NotSubmitted(String),
    /// Restate has no progress for the batch and never had any while it was
    /// followed: the id names no batch it has run, or one whose workflow has
    /// been retired since.
    Unknown,
}

/// Submits `followed` and follows it, calling `report` with each change in
/// what is known of it, until it completes or cannot be submitted.
pub(crate) async fn run_batch<G: SubmitGateway>(
    gateway: &mut G,
    mut followed: Followed,
    mut report: impl FnMut(&Followed),
) -> BatchOutcome {
    match submit(gateway).await {
        Ok(Some(progress)) => {
            let now = gateway.now();
            if followed.hear(progress, now) {
                report(&followed);
            }
            if let Some(progress) = followed.progress.as_ref().filter(|it| it.completed) {
                return BatchOutcome::Completed(progress.clone());
            }
        }
        Ok(None) => {}
        Err(error) => return BatchOutcome::NotSubmitted(error),
    }
    poll(gateway, followed, true, report).await
}

/// Follows `followed`, a batch known by id alone, calling `report` with each
/// change in what is known of it, until it completes or Restate proves never
/// to have had it.
pub(crate) async fn follow_batch<G: BatchGateway>(
    gateway: &mut G,
    followed: Followed,
    report: impl FnMut(&Followed),
) -> BatchOutcome {
    poll(gateway, followed, false, report).await
}

/// Polls until the batch completes. `known` says Restate is known to have the
/// batch — it took the submission, or has reported progress — in which case a
/// poll it answers with nothing is waited out, however many: the workflow has
/// not started yet, or has been retired, and neither is a reason to say the
/// batch is lost. A batch not known to Restate is given up as [`Unknown`] once
/// [`ATTACH_LIMIT`] answered polls have found nothing. A poll that fails is
/// noted and the next is taken, and counts for nothing either way: the batch
/// is durable, and the line to the server is reported over the page in its
/// own right.
///
/// [`Unknown`]: BatchOutcome::Unknown
async fn poll<G: BatchGateway>(
    gateway: &mut G,
    mut followed: Followed,
    mut known: bool,
    mut report: impl FnMut(&Followed),
) -> BatchOutcome {
    let mut empty_polls = 0;
    loop {
        gateway.tick().await;
        let answer = gateway.progress().await;
        let mut changed = followed.answered(answer.as_ref().err().cloned());
        match answer {
            Ok(Some(progress)) => {
                known = true;
                changed |= followed.hear(progress, gateway.now());
            }
            Ok(None) if !known => {
                empty_polls += 1;
                if empty_polls == ATTACH_LIMIT {
                    return BatchOutcome::Unknown;
                }
            }
            Ok(None) | Err(_) => {}
        }
        if changed {
            report(&followed);
        }
        if let Some(progress) = followed.progress.as_ref().filter(|it| it.completed) {
            return BatchOutcome::Completed(progress.clone());
        }
    }
}

/// Gets the batch accepted. `Ok(None)` is a clean acceptance; `Ok(Some)` means
/// a round trip failed after Restate had already taken the batch, and carries
/// the progress that proved it.
async fn submit<G: SubmitGateway>(gateway: &mut G) -> Result<Option<BatchProgress>, String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match gateway.submit().await {
            Ok(()) => return Ok(None),
            Err(error) => error,
        };
        // A failed round trip does not mean Restate did not take the batch;
        // the workflow's own progress is the authority, and a batch that is
        // running must not be submitted again.
        if let Ok(Some(progress)) = gateway.progress().await {
            return Ok(Some(progress));
        }
        if attempt == SUBMIT_ATTEMPTS {
            return Err(error);
        }
        gateway.tick().await;
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::VecDeque;

    use dependaboard_core::{ActionOutcome, BulkActionKind, PrTarget};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::grouped_row;

    /// A server whose answers are scripted: each call pops the next answer,
    /// and the last one repeats once the script runs out. The clock reads the
    /// epoch plus one second per tick, as the real one would.
    struct Scripted {
        submits: VecDeque<Result<(), String>>,
        progress: VecDeque<Result<Option<BatchProgress>, String>>,
        submit_calls: u32,
        ticks: u32,
    }

    /// The fake clock's reading at the first tick, in Unix seconds.
    const EPOCH: u64 = 1_700_000_000;

    impl Scripted {
        fn new(
            submits: impl IntoIterator<Item = Result<(), String>>,
            progress: impl IntoIterator<Item = Result<Option<BatchProgress>, String>>,
        ) -> Self {
            Self {
                submits: submits.into_iter().collect(),
                progress: progress.into_iter().collect(),
                submit_calls: 0,
                ticks: 0,
            }
        }
    }

    fn next_or_repeat<T: Clone>(script: &mut VecDeque<T>) -> T {
        if script.len() > 1 {
            script.pop_front().unwrap()
        } else {
            script.front().cloned().expect("the script is not empty")
        }
    }

    impl SubmitGateway for Scripted {
        async fn submit(&mut self) -> Result<(), String> {
            self.submit_calls += 1;
            next_or_repeat(&mut self.submits)
        }
    }

    impl BatchGateway for Scripted {
        async fn progress(&mut self) -> Result<Option<BatchProgress>, String> {
            next_or_repeat(&mut self.progress)
        }

        async fn tick(&mut self) {
            self.ticks += 1;
        }

        fn now(&self) -> u64 {
            EPOCH + u64::from(self.ticks)
        }
    }

    fn unavailable() -> String {
        "Restate is unavailable".to_owned()
    }

    fn targets() -> Vec<PrTarget> {
        let mut second = grouped_row();
        second.number = 10;
        vec![pr_target(&grouped_row()), pr_target(&second)]
    }

    fn queued() -> BatchProgress {
        BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets())
    }

    fn after(steps: usize) -> BatchProgress {
        let mut progress = queued();
        for target in targets().iter().take(steps) {
            progress.record(
                &target.key(),
                ActionOutcome::Succeeded {
                    detail: "merged".to_owned(),
                },
            );
        }
        progress
    }

    fn repeat<T: Clone>(value: T, times: u32) -> impl Iterator<Item = T> {
        std::iter::repeat_n(value, times as usize)
    }

    /// What Restate said, from each report that carried its word.
    fn heard(reported: &[Followed]) -> Vec<BatchProgress> {
        reported
            .iter()
            .filter(|followed| followed.heard)
            .filter_map(|followed| followed.progress.clone())
            .collect()
    }

    async fn run(gateway: &mut Scripted) -> (BatchOutcome, Vec<Followed>) {
        let followed = Followed::queued("batch-1", BulkActionKind::Merge, &targets(), EPOCH);
        let mut reported = Vec::new();
        let outcome = run_batch(gateway, followed, |followed| {
            reported.push(followed.clone());
        })
        .await;
        (outcome, reported)
    }

    async fn attach(gateway: &mut Scripted) -> (BatchOutcome, Vec<Followed>) {
        let mut reported = Vec::new();
        let outcome = follow_batch(gateway, Followed::attaching("batch-1", EPOCH), |followed| {
            reported.push(followed.clone());
        })
        .await;
        (outcome, reported)
    }

    #[tokio::test]
    async fn a_submission_that_keeps_failing_is_given_up_after_the_attempt_budget() {
        let mut gateway = Scripted::new([Err(unavailable())], [Ok(None)]);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::NotSubmitted(unavailable()));
        assert_eq!(gateway.submit_calls, SUBMIT_ATTEMPTS);
        assert!(reported.is_empty());
    }

    #[tokio::test]
    async fn a_failed_round_trip_whose_batch_is_already_running_is_not_retried() {
        let mut gateway = Scripted::new(
            [Err(unavailable())],
            [Ok(Some(after(1))), Ok(Some(after(2)))],
        );

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(gateway.submit_calls, 1);
        assert_eq!(heard(&reported), [after(1), after(2)]);
    }

    /// Restate took the batch; a service that is down or deploying starts it
    /// late, not never. The dashboard waits as long as it takes, well past
    /// what it would give a batch it knows by id alone.
    #[tokio::test]
    async fn a_submitted_batch_restate_has_not_started_is_waited_for() {
        let progress =
            repeat(Ok(None), 4 * ATTACH_LIMIT).chain([Ok(Some(queued())), Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([Ok(())], progress);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(heard(&reported), [queued(), after(2)]);
    }

    /// Nothing has moved for as long as GitHub's budgets allow, and the
    /// dashboard does not stop following for it: the batch is durable, and
    /// a change comes when it comes. Each report says since when the batch
    /// has stood as it is, which is what the drawer dates its waiting from.
    #[tokio::test]
    async fn a_batch_that_stands_still_for_hours_is_followed_to_the_end_and_dated() {
        let three_hours = 3 * 60 * 60;
        let progress = [Ok(Some(after(1)))]
            .into_iter()
            .chain(repeat(Ok(Some(after(1))), three_hours))
            .chain([Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([Ok(())], progress);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(
            reported
                .iter()
                .map(|followed| followed.since - EPOCH)
                .collect::<Vec<_>>(),
            [1, u64::from(three_hours) + 2],
            "each change is dated by when it was heard"
        );
        assert_eq!(
            reported[0].unchanged_for(EPOCH + 1 + u64::from(three_hours)),
            Duration::from_secs(three_hours.into()),
            "and how long the batch has stood is measured from that date"
        );
    }

    /// A poll that fails says nothing about the batch. It is noted, so the
    /// drawer can say the server is not answering, and the next poll is
    /// taken; the note goes when the answers come back.
    #[tokio::test]
    async fn polls_that_fail_are_noted_and_the_follow_goes_on() {
        let progress = [
            Ok(Some(after(1))),
            Err(unavailable()),
            Err(unavailable()),
            Ok(Some(after(1))),
            Ok(Some(after(2))),
        ];
        let mut gateway = Scripted::new([Ok(())], progress);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(
            reported
                .iter()
                .map(|followed| (followed.trouble.clone(), followed.progress.clone()))
                .collect::<Vec<_>>(),
            [
                (None, Some(after(1))),
                (Some(unavailable()), Some(after(1))),
                (None, Some(after(1))),
                (None, Some(after(2))),
            ],
            "the trouble is reported once when it starts and once when it ends, over the progress last heard"
        );
    }

    /// After a reload the dashboard has the id and nothing else; Restate's
    /// first answer is the first thing it can show.
    #[tokio::test]
    async fn a_batch_followed_by_id_shows_what_restate_answers() {
        let mut gateway = Scripted::new(
            [],
            [Ok(None), Ok(None), Ok(Some(after(1))), Ok(Some(after(2)))],
        );

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(gateway.submit_calls, 0);
        assert_eq!(heard(&reported), [after(1), after(2)]);
        assert_eq!(reported[0].since, EPOCH + 3);
    }

    /// A link may name a batch Restate never ran, or one it has retired. The
    /// dashboard gives it up as unknown once the attach budget is spent,
    /// rather than ask forever after a batch that is not there.
    #[tokio::test]
    async fn a_batch_restate_has_never_had_is_given_up_as_unknown() {
        let mut gateway = Scripted::new([], [Ok(None)]);

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Unknown);
        assert_eq!(gateway.ticks, ATTACH_LIMIT);
        assert!(reported.is_empty());
    }

    /// Polls that fail are not polls that found nothing: a server that cannot
    /// be reached says nothing about whether Restate has the batch.
    #[tokio::test]
    async fn a_batch_followed_by_id_is_not_given_up_while_the_polls_fail() {
        let progress = repeat(Err(unavailable()), 2 * ATTACH_LIMIT).chain([Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([], progress);

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(reported[0].trouble, Some(unavailable()));
        assert_eq!(reported[0].progress, None);
    }

    /// Once Restate has spoken for the batch it is known to have it; polls
    /// that then find nothing are waited out, not counted against it.
    #[tokio::test]
    async fn a_batch_restate_has_spoken_for_is_not_given_up_when_it_falls_silent() {
        let progress = [Ok(Some(after(1)))]
            .into_iter()
            .chain(repeat(Ok(None), 2 * ATTACH_LIMIT))
            .chain([Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([], progress);

        let (outcome, _) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
    }
}
