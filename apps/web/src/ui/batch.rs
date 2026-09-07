//! The bulk-action flow after confirmation: submit the batch to Restate, then
//! follow its progress until it completes. A batch the dashboard knows only by
//! id — after a reload, or picked from the audit view — is followed the same
//! way, without the submission, once the projection has been asked what it
//! holds of it: a finished record is the batch as it stands and needs no
//! follow, a running listing vouches for the batch as a receipt would, and
//! only an id the projection has never heard of is Restate's alone to answer
//! for.
//!
//! A batch is never declared lost: it is durable in Restate, and a target may
//! legitimately sit for hours behind GitHub's retry and rate-limit budgets. The
//! follow reports since when the batch has gone unchanged and whether its polls
//! are being answered, and leaves saying so to the drawer. It ends only when the
//! batch completes, when Restate would not take it, when Restate has never
//! had progress for the id it was given — or when the server refuses the
//! credentials the polls carry, since asking again would only be refused
//! again, and the browser turns each refusal into a credential prompt; the
//! batch carries on, and the reload that signs in again picks it up.

use std::time::Duration;

use dependaboard_core::{
    BatchProgress, BatchReceipt, BulkActionKind, PrKey, PrTarget, ProjectedBatch, RunningBatch,
    SubmittedTarget, unix_seconds,
};

use crate::api::{load_batch_progress, load_batch_projection, submit_batch};
use crate::ui::dashboard_state::DashboardState;
use crate::ui::{Fault, POLL_INTERVAL, logged_fault, sleep};

/// How many times a submission is tried, one poll interval apart, before the
/// batch is reported as not submitted — unless the server refuses the
/// credentials, which is not tried again.
pub(crate) const SUBMIT_ATTEMPTS: u32 = 5;

/// How long a batch followed by id alone, which the projection has never heard
/// of, may go with Restate answering that it has no progress for it before the
/// id is taken to name no batch. A stale link, or one from another
/// deployment, is given up in this time; a live batch answers its first poll.
/// Only answers count: a poll the server did not answer says nothing about
/// the batch, so an outage stretches the time rather than spending it. A
/// batch the dashboard submitted itself is not held to this at all: Restate
/// took it, and a service that is down or deploying starts it late, not
/// never. Nor is one the projection lists as running: the workflow wrote the
/// listing, so Restate has the batch.
pub(crate) const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// [`ATTACH_TIMEOUT`] in answered polls.
const ATTACH_LIMIT: u32 = (ATTACH_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs()) as u32;

/// How long a batch may stand unchanged before the pill and the drawer say
/// so. A healthy GitHub call answers in seconds, and a round of three lands
/// its first within them; a minute without a change is a call being retried,
/// a rate limit being waited out, or a workflow that has stopped for a reason
/// of its own — and the dashboard cannot tell which: the workflow publishes
/// progress only as verdicts land, so every kind of stall looks the same from
/// here, and the dashboard says only for how long the batch has stood. It
/// does not guess when that will end: one call's budgets run to hours — four
/// half-hour retry budgets and three rate-limit waits of up to an hour, for
/// the guard read and again for the mutation — and the batch is durable for
/// all of them.
pub(crate) const WAITING_NOTICE_AFTER: Duration = Duration::from_secs(60);

/// The server as the follow sees it, so the follow can be driven by a script
/// in tests. A call that failed says what it said about the line to the
/// server, since a refusal of the credentials ends the follow where any
/// other fault is waited out.
pub(crate) trait BatchGateway {
    async fn progress(&mut self) -> Result<Option<BatchProgress>, Fault>;
    /// Waits one poll interval.
    async fn tick(&mut self);
    /// The wall clock, in Unix seconds.
    fn now(&self) -> u64;
}

/// A [`BatchGateway`] with a batch to submit.
pub(crate) trait SubmitGateway: BatchGateway {
    async fn submit(&mut self) -> Result<BatchReceipt, Fault>;
}

/// A [`BatchGateway`] for a batch known by id alone, which the projection is
/// asked about before Restate is.
pub(crate) trait AttachGateway: BatchGateway {
    /// What the projection holds of the batch: its finished record, its
    /// listing while it runs, or nothing for an id it has never heard of.
    async fn projected(&mut self) -> Result<Option<ProjectedBatch>, Fault>;
}

/// A batch known by id alone, followed through the server functions, on the
/// page whose line to the server `state` carries.
pub(crate) struct ServerFollow {
    pub(crate) batch_id: String,
    pub(crate) state: DashboardState,
}

impl BatchGateway for ServerFollow {
    /// A page the server has already refused — the live refresh's poll got
    /// the 401 — is not asked on behalf of: the answer would be the same
    /// refusal, with a credential prompt for it, so the follow is handed the
    /// refusal as if it had asked.
    async fn progress(&mut self) -> Result<Option<BatchProgress>, Fault> {
        if self.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        load_batch_progress(self.batch_id.clone())
            .await
            .map_err(|error| logged_fault(&error))
    }

    async fn tick(&mut self) {
        sleep(POLL_INTERVAL).await;
    }

    fn now(&self) -> u64 {
        unix_seconds()
    }
}

impl AttachGateway for ServerFollow {
    /// Not asked from a page the server has already refused, as
    /// [`ServerFollow::progress`] does not ask from one.
    async fn projected(&mut self) -> Result<Option<ProjectedBatch>, Fault> {
        if self.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        load_batch_projection(self.batch_id.clone())
            .await
            .map_err(|error| logged_fault(&error))
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
    async fn progress(&mut self) -> Result<Option<BatchProgress>, Fault> {
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
    /// Not submitted from a page the server has already refused, as
    /// [`ServerFollow::progress`] does not ask from one.
    async fn submit(&mut self) -> Result<BatchReceipt, Fault> {
        if self.follow.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        let targets = self.targets.iter().map(SubmittedTarget::from).collect();
        submit_batch(self.follow.batch_id.clone(), self.action, targets)
            .await
            .map_err(|error| logged_fault(&error))
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
    /// Why the last poll was not answered, while the polls are failing. A
    /// refusal of the credentials is the last poll's word for good: the
    /// follow ends on it.
    pub(crate) trouble: Option<Fault>,
    /// The projection's word on a batch followed by id alone, which is asked
    /// before Restate is: what the drawer can say of the batch until Restate
    /// answers, and which of the ways of finding a batch this follow is on.
    pub(crate) listing: Listing,
}

/// What the projection said of a batch followed by id alone. A finished
/// record is not a case here: it is taken as the batch's `progress`, and the
/// follow ends on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Listing {
    /// Not asked, or not answered yet. A batch the dashboard queued itself is
    /// never asked after: it has its own snapshot to show.
    #[default]
    Unasked,
    /// The projection could not be read — the store was away, or the line to
    /// the server — so it has no word on the batch, and Restate is asked as it
    /// always was, within [`ATTACH_TIMEOUT`].
    Unreadable,
    /// The projection has never heard of the id, running or finished: a stale
    /// or foreign link, or a batch so new the workflow has not listed it yet.
    /// Restate is the second word, and is given [`ATTACH_TIMEOUT`] to give it.
    Unlisted,
    /// The projection lists the batch as running. The workflow wrote the
    /// listing itself, after it published its first progress, so Restate has
    /// the batch and is asked where it stands for as long as it takes.
    Running(RunningBatch),
}

impl Listing {
    /// The action the batch runs, as far as the projection has said: the pill
    /// and the drawer name it in place of "batch" once it is known.
    pub(crate) fn action(&self) -> Option<BulkActionKind> {
        match self {
            Self::Running(listing) => Some(listing.action),
            Self::Unasked | Self::Unreadable | Self::Unlisted => None,
        }
    }
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
            listing: Listing::Unasked,
        }
    }

    /// A batch the dashboard knows by id alone and is asking after.
    pub(crate) fn attaching(batch_id: &str, now: u64) -> Self {
        Self {
            batch_id: batch_id.to_owned(),
            progress: None,
            heard: false,
            since: now,
            trouble: None,
            listing: Listing::Unasked,
        }
    }

    /// How long the batch has stood unchanged, against `now`.
    pub(crate) fn unchanged_for(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.since))
    }

    /// Whether the batch has stood still long enough to be said so: not while
    /// it changed within [`WAITING_NOTICE_AFTER`], not once it has run to the
    /// end, and not while there is no progress to date — a batch known by id
    /// alone is being asked after, not waited on. The pill and the drawer
    /// both read this, so they say the batch stands at the same moment; how
    /// long it has stood is [`since`](Self::since), which each puts in words.
    pub(crate) fn stands_still(&self, now: u64) -> bool {
        self.progress.is_some()
            && !self.completed()
            && self.unchanged_for(now) >= WAITING_NOTICE_AFTER
    }

    /// Whether the batch has run to the end, as far as the dashboard has heard.
    pub(crate) fn completed(&self) -> bool {
        self.progress
            .as_ref()
            .is_some_and(|progress| progress.completed)
    }

    /// Whether the server refused the credentials the last poll carried,
    /// which is where the follow stopped: the batch is no longer being asked
    /// after, and will not be until the page is reloaded.
    pub(crate) fn signed_out(&self) -> bool {
        self.trouble == Some(Fault::SignedOut)
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

    /// Drops the targets `keys` name from the dashboard's own snapshot: the
    /// server left them out of the batch, so Restate will never report them.
    /// Restate's word, once heard, already counts only what it runs, and is
    /// left alone.
    fn leave_out(&mut self, keys: &[PrKey]) {
        if self.heard {
            return;
        }
        if let Some(progress) = self.progress.as_mut() {
            progress
                .targets
                .retain(|item| !keys.contains(&item.target.key()));
        }
    }

    /// The targets the dashboard queued that `progress`, Restate's word on
    /// the batch, does not run: the receipt a lost response would have
    /// carried, read off what Restate has instead.
    fn left_out_of(&self, progress: &BatchProgress) -> Vec<PrKey> {
        let Some(queued) = self.progress.as_ref().filter(|_| !self.heard) else {
            return Vec::new();
        };
        queued
            .targets
            .iter()
            .map(|item| item.target.key())
            .filter(|key| {
                !progress
                    .targets
                    .iter()
                    .any(|item| item.target.key() == *key)
            })
            .collect()
    }

    /// Notes how the last poll went, if that is news.
    fn answered(&mut self, trouble: Option<Fault>) -> bool {
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
    /// Restate never accepted the batch; carries the last submission fault.
    NotSubmitted(Fault),
    /// Restate has no progress for the batch and never had any while it was
    /// followed: the id names no batch it has run, or one whose workflow has
    /// been retired since.
    Unknown,
    /// The server refused the credentials the polls carried, so the follow
    /// stopped: the batch carries on in Restate, and the dashboard asks after
    /// it again once the page is reloaded and signed in.
    SignedOut,
}

/// Submits `followed` and follows it, calling `report` with each change in
/// what is known of it, until it completes or cannot be submitted.
/// `on_accepted` is called once, when Restate is known to have the batch,
/// with the receipt: which of the targets submitted the server left out of
/// it. A round trip that failed after Restate took the batch has no receipt
/// to hand over, so the receipt is read off Restate's progress instead: what
/// the dashboard queued that Restate does not run was left out.
pub(crate) async fn run_batch<G: SubmitGateway>(
    gateway: &mut G,
    mut followed: Followed,
    mut report: impl FnMut(&Followed),
    on_accepted: impl FnOnce(BatchReceipt),
) -> BatchOutcome {
    match submit(gateway).await {
        Ok(Taken::Accepted(receipt)) => {
            if !receipt.left_out.is_empty() {
                followed.leave_out(&receipt.left_out);
                report(&followed);
            }
            on_accepted(receipt);
        }
        Ok(Taken::Running(progress)) => {
            // Read against the dashboard's own snapshot, before Restate's
            // word replaces it.
            let receipt = BatchReceipt {
                left_out: followed.left_out_of(&progress),
            };
            let now = gateway.now();
            if followed.hear(progress, now) {
                report(&followed);
            }
            on_accepted(receipt);
            if let Some(progress) = followed.progress.as_ref().filter(|it| it.completed) {
                return BatchOutcome::Completed(progress.clone());
            }
        }
        Err(error) => return BatchOutcome::NotSubmitted(error),
    }
    poll(gateway, followed, true, report).await
}

/// Follows `followed`, a batch known by id alone, calling `report` with each
/// change in what is known of it, until it completes or Restate proves never
/// to have had it. The projection is asked first: a batch it holds finished is
/// shown from the record and the follow ends on it, without a poll — the
/// workflow may have been retired days ago, and the record is the batch as it
/// stands. One it lists as running is polled as a batch Restate is known to
/// have, for as long as it takes. One it has never heard of is Restate's to
/// vouch for, within [`ATTACH_TIMEOUT`]; so is one the projection could not be
/// asked about, with the fault passed on as a failing poll's is — except a
/// refusal of the credentials, which ends the follow as it ends a poll.
pub(crate) async fn follow_batch<G: AttachGateway>(
    gateway: &mut G,
    mut followed: Followed,
    mut report: impl FnMut(&Followed),
) -> BatchOutcome {
    let known = match gateway.projected().await {
        Ok(Some(ProjectedBatch::Finished(record))) => {
            let progress = BatchProgress::from(record);
            followed.hear(progress.clone(), gateway.now());
            report(&followed);
            return BatchOutcome::Completed(progress);
        }
        Ok(Some(ProjectedBatch::Running(listing))) => {
            followed.listing = Listing::Running(listing);
            report(&followed);
            true
        }
        Ok(None) => {
            followed.listing = Listing::Unlisted;
            report(&followed);
            false
        }
        Err(fault) => {
            followed.listing = Listing::Unreadable;
            followed.answered(Some(fault));
            report(&followed);
            if followed.signed_out() {
                return BatchOutcome::SignedOut;
            }
            false
        }
    };
    poll(gateway, followed, known, report).await
}

/// Polls until the batch completes. `known` says Restate is known to have the
/// batch — it took the submission, the projection lists it as running, or it
/// has reported progress — in which case a poll it answers with nothing is
/// waited out, however many: the workflow has not started yet, or has been
/// retired, and neither is a reason to say the batch is lost. A batch not
/// known to Restate is given up as [`Unknown`] once [`ATTACH_LIMIT`] answered
/// polls have found nothing. A poll that fails is noted and the next is taken,
/// and counts for nothing either way: the batch is durable, and the line to
/// the server is reported over the page in its own right. The one fault not
/// waited out is the server refusing the credentials: the next poll would be
/// refused the same, and each refusal the browser gets it turns into a
/// credential prompt, so the follow ends as [`SignedOut`], with the refusal
/// reported as the last poll's word.
///
/// [`Unknown`]: BatchOutcome::Unknown
/// [`SignedOut`]: BatchOutcome::SignedOut
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
        if followed.signed_out() {
            return BatchOutcome::SignedOut;
        }
        if let Some(progress) = followed.progress.as_ref().filter(|it| it.completed) {
            return BatchOutcome::Completed(progress.clone());
        }
    }
}

/// How Restate came to have the batch.
enum Taken {
    /// The submission was answered: a clean acceptance, with its receipt.
    Accepted(BatchReceipt),
    /// A round trip failed after Restate had already taken the batch; carries
    /// the progress that proved it.
    Running(BatchProgress),
}

/// Gets the batch accepted, or gives up after [`SUBMIT_ATTEMPTS`] with the
/// last fault. A refusal of the credentials — of the submission, or of the
/// ask whether a failed one was taken — is given up at once: it came from
/// the auth edge, before any function ran, so Restate has nothing to ask
/// after, and every further call would be refused the same, with a
/// credential prompt each time.
async fn submit<G: SubmitGateway>(gateway: &mut G) -> Result<Taken, Fault> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match gateway.submit().await {
            Ok(receipt) => return Ok(Taken::Accepted(receipt)),
            Err(Fault::SignedOut) => return Err(Fault::SignedOut),
            Err(error) => error,
        };
        // A failed round trip does not mean Restate did not take the batch;
        // the workflow's own progress is the authority, and a batch that is
        // running must not be submitted again.
        match gateway.progress().await {
            Ok(Some(progress)) => return Ok(Taken::Running(progress)),
            Err(Fault::SignedOut) => return Err(Fault::SignedOut),
            Ok(None) | Err(_) => {}
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

    use dependaboard_core::{
        ActionOutcome, BatchRecord, BulkActionKind, PrTarget, ProjectedBatch, RunningBatch, UserId,
    };

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::grouped_row;

    /// A server whose answers are scripted: each call pops the next answer,
    /// and the last one repeats once the script runs out. The projection's
    /// word is one answer, asked once; it is that the projection has never
    /// heard of the batch unless the test says otherwise. The clock reads the
    /// epoch plus one second per tick, as the real one would.
    struct Scripted {
        submits: VecDeque<Result<BatchReceipt, Fault>>,
        progress: VecDeque<Result<Option<BatchProgress>, Fault>>,
        projected: Result<Option<ProjectedBatch>, Fault>,
        submit_calls: u32,
        progress_calls: u32,
        ticks: u32,
    }

    /// The fake clock's reading at the first tick, in Unix seconds.
    const EPOCH: u64 = 1_700_000_000;

    impl Scripted {
        fn new(
            submits: impl IntoIterator<Item = Result<BatchReceipt, Fault>>,
            progress: impl IntoIterator<Item = Result<Option<BatchProgress>, Fault>>,
        ) -> Self {
            Self {
                submits: submits.into_iter().collect(),
                progress: progress.into_iter().collect(),
                projected: Ok(None),
                submit_calls: 0,
                progress_calls: 0,
                ticks: 0,
            }
        }

        /// With `projected` as the projection's word on the batch.
        fn projecting(self, projected: Result<Option<ProjectedBatch>, Fault>) -> Self {
            Self { projected, ..self }
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
        async fn submit(&mut self) -> Result<BatchReceipt, Fault> {
            self.submit_calls += 1;
            next_or_repeat(&mut self.submits)
        }
    }

    impl AttachGateway for Scripted {
        async fn projected(&mut self) -> Result<Option<ProjectedBatch>, Fault> {
            self.projected.clone()
        }
    }

    impl BatchGateway for Scripted {
        async fn progress(&mut self) -> Result<Option<BatchProgress>, Fault> {
            self.progress_calls += 1;
            next_or_repeat(&mut self.progress)
        }

        async fn tick(&mut self) {
            self.ticks += 1;
        }

        fn now(&self) -> u64 {
            EPOCH + u64::from(self.ticks)
        }
    }

    fn unavailable() -> Fault {
        Fault::Refused("Restate is unavailable".to_owned())
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

    async fn run(gateway: &mut Scripted) -> (BatchOutcome, Vec<Followed>, Option<BatchReceipt>) {
        let followed = Followed::queued("batch-1", BulkActionKind::Merge, &targets(), EPOCH);
        let mut reported = Vec::new();
        let mut accepted = None;
        let outcome = run_batch(
            gateway,
            followed,
            |followed| {
                reported.push(followed.clone());
            },
            |receipt| accepted = Some(receipt),
        )
        .await;
        (outcome, reported, accepted)
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

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::NotSubmitted(unavailable()));
        assert_eq!(gateway.submit_calls, SUBMIT_ATTEMPTS);
        assert!(reported.is_empty());
        assert_eq!(
            accepted, None,
            "a batch Restate never took was not accepted"
        );
    }

    /// A submission the server refused the credentials of never reached
    /// Restate — the auth edge refuses before any function runs — and trying
    /// again, or asking whether the batch is running, would be refused the
    /// same, each time with a credential prompt. It is given up at once as
    /// not submitted, for that reason.
    #[tokio::test]
    async fn a_submission_refused_the_credentials_is_given_up_at_once_without_asking_after_the_batch()
     {
        let mut gateway = Scripted::new([Err(Fault::SignedOut)], [Ok(Some(after(1)))]);

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::NotSubmitted(Fault::SignedOut));
        assert_eq!(gateway.submit_calls, 1, "the refusal is not tried again");
        assert_eq!(
            gateway.progress_calls, 0,
            "nor is the batch asked after, which would be refused the same"
        );
        assert!(reported.is_empty());
        assert_eq!(accepted, None);
    }

    /// The credentials may be refused on the way to finding out whether a
    /// failed submission was taken: that is the same refusal, and trying the
    /// submission again would meet it again, with a prompt. It is given up
    /// there and then.
    #[tokio::test]
    async fn a_refusal_while_asking_whether_a_failed_submission_was_taken_gives_it_up_too() {
        let mut gateway = Scripted::new([Err(unavailable())], [Err(Fault::SignedOut)]);

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::NotSubmitted(Fault::SignedOut));
        assert_eq!(gateway.submit_calls, 1);
        assert_eq!(gateway.progress_calls, 1);
        assert!(reported.is_empty());
        assert_eq!(accepted, None);
    }

    /// The batch as Restate runs it when the server left the second fixture
    /// target out: the first alone.
    fn without_the_second() -> BatchProgress {
        BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets()[..1])
    }

    /// [`without_the_second`], run to the end.
    fn done_without_the_second() -> BatchProgress {
        let mut done = without_the_second();
        done.record(
            &targets()[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        );
        done
    }

    /// The server took the batch over the targets the projection still had
    /// and named the one it did not. That is an acceptance, not a failure:
    /// the batch is not submitted again, the target left out is handed back
    /// at once for the dashboard to name, and the dashboard's own snapshot
    /// of the batch drops it, so the pill and drawer count what Restate runs.
    #[tokio::test]
    async fn a_target_the_server_left_out_is_handed_back_and_the_batch_followed_without_it() {
        let receipt = BatchReceipt {
            left_out: vec![targets()[1].key()],
        };
        let mut gateway =
            Scripted::new([Ok(receipt.clone())], [Ok(Some(done_without_the_second()))]);

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(
            gateway.submit_calls, 1,
            "a partial acceptance is not retried"
        );
        assert_eq!(accepted, Some(receipt));
        assert_eq!(outcome, BatchOutcome::Completed(done_without_the_second()));
        assert_eq!(
            (reported[0].heard, reported[0].progress.clone()),
            (false, Some(without_the_second())),
            "the dashboard's own snapshot is reported without the target left out"
        );
    }

    /// A round trip that failed after Restate took the batch carries no
    /// receipt; Restate's progress is the word on which targets it runs, and
    /// a target the submission named that Restate does not was left out.
    #[tokio::test]
    async fn a_batch_already_running_names_what_was_left_out_by_what_restate_runs() {
        let mut gateway = Scripted::new(
            [Err(unavailable())],
            [
                Ok(Some(without_the_second())),
                Ok(Some(done_without_the_second())),
            ],
        );

        let (outcome, _, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(done_without_the_second()));
        assert_eq!(
            accepted,
            Some(BatchReceipt {
                left_out: vec![targets()[1].key()],
            })
        );
    }

    #[tokio::test]
    async fn a_failed_round_trip_whose_batch_is_already_running_is_not_retried() {
        let mut gateway = Scripted::new(
            [Err(unavailable())],
            [Ok(Some(after(1))), Ok(Some(after(2)))],
        );

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(gateway.submit_calls, 1);
        assert_eq!(heard(&reported), [after(1), after(2)]);
        assert_eq!(
            accepted,
            Some(BatchReceipt::default()),
            "Restate runs every target, so none was left out"
        );
    }

    /// Restate took the batch; a service that is down or deploying starts it
    /// late, not never. The dashboard waits as long as it takes, well past
    /// what it would give a batch it knows by id alone.
    #[tokio::test]
    async fn a_submitted_batch_restate_has_not_started_is_waited_for() {
        let progress =
            repeat(Ok(None), 4 * ATTACH_LIMIT).chain([Ok(Some(queued())), Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([Ok(BatchReceipt::default())], progress);

        let (outcome, reported, accepted) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(heard(&reported), [queued(), after(2)]);
        assert_eq!(accepted, Some(BatchReceipt::default()));
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
        let mut gateway = Scripted::new([Ok(BatchReceipt::default())], progress);

        let (outcome, reported, _) = run(&mut gateway).await;

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
        let mut gateway = Scripted::new([Ok(BatchReceipt::default())], progress);

        let (outcome, reported, _) = run(&mut gateway).await;

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

    /// A poll the server refused the credentials for is the last one taken.
    /// Every further poll would be refused the same, and each 401 the
    /// browser gets carries a challenge it turns into a credential prompt;
    /// the batch carries on in Restate without being asked after, and the
    /// follow ends saying so, over the progress last heard, so the drawer can
    /// say why the batch is no longer followed.
    #[tokio::test]
    async fn a_poll_refused_the_credentials_is_the_last_one_taken_and_the_follow_ends_saying_so() {
        let progress = [
            Ok(Some(after(1))),
            Err(Fault::SignedOut),
            Ok(Some(after(2))),
        ];
        let mut gateway = Scripted::new([Ok(BatchReceipt::default())], progress);

        let (outcome, reported, _) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::SignedOut);
        assert_eq!(
            gateway.progress_calls, 2,
            "nothing is asked after the refusal"
        );
        assert_eq!(
            reported
                .last()
                .map(|followed| (followed.trouble.clone(), followed.progress.clone())),
            Some((Some(Fault::SignedOut), Some(after(1)))),
            "the last word is the refusal, over the progress last heard"
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
        assert_eq!(
            reported
                .iter()
                .find(|followed| followed.heard)
                .map(|followed| followed.since),
            Some(EPOCH + 3),
            "the first word from Restate is dated by when it was heard"
        );
    }

    /// A link may name a batch Restate never ran, or one it has retired, and
    /// the projection has never heard of either. That is said at once, so the
    /// drawer can say which case it is in; Restate is the second word, and the
    /// dashboard gives the batch up as unknown once the attach budget is
    /// spent, rather than ask forever after a batch that is not there.
    #[tokio::test]
    async fn a_batch_nobody_has_heard_of_is_said_to_be_and_given_up_as_unknown() {
        let mut gateway = Scripted::new([], [Ok(None)]).projecting(Ok(None));

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Unknown);
        assert_eq!(gateway.ticks, ATTACH_LIMIT);
        assert_eq!(
            reported
                .iter()
                .map(|followed| (followed.listing.clone(), followed.progress.clone()))
                .collect::<Vec<_>>(),
            [(Listing::Unlisted, None)],
            "the projection's word is reported once; the empty polls are not news"
        );
    }

    /// Polls that fail are not polls that found nothing: a server that cannot
    /// be reached says nothing about whether Restate has the batch.
    #[tokio::test]
    async fn a_batch_followed_by_id_is_not_given_up_while_the_polls_fail() {
        let progress = repeat(Err(unavailable()), 2 * ATTACH_LIMIT).chain([Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([], progress);

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        let troubled = reported
            .iter()
            .find(|followed| followed.trouble.is_some())
            .expect("the failing polls are reported");
        assert_eq!(troubled.trouble, Some(unavailable()));
        assert_eq!(troubled.progress, None);
    }

    /// The one word on whether a batch has stood still, which the pill and the
    /// drawer both take so they say so at the same moment: not while the
    /// batch changed within the notice, however long the follow has run; yes
    /// from the notice on; not once it has finished, however long ago; and
    /// not for a batch with no progress to date.
    #[test]
    fn a_batch_stands_still_once_unchanged_for_the_notice_unless_finished_or_never_heard() {
        let notice = WAITING_NOTICE_AFTER.as_secs();
        let running = Followed {
            batch_id: "batch-1".to_owned(),
            progress: Some(after(1)),
            heard: true,
            since: EPOCH,
            trouble: None,
            listing: Listing::Unasked,
        };

        assert!(!running.stands_still(EPOCH + notice - 1));
        assert!(
            running.stands_still(EPOCH + notice),
            "the notice is the first second the batch is said to stand"
        );
        assert!(running.stands_still(EPOCH + 47 * 60));

        let finished = Followed {
            progress: Some(after(2)),
            ..running.clone()
        };
        assert!(
            !finished.stands_still(EPOCH + 24 * 3600),
            "a finished batch waits on nothing"
        );

        let queued = Followed::queued("batch-1", BulkActionKind::Merge, &targets(), EPOCH);
        assert!(
            queued.stands_still(EPOCH + 3 * 60),
            "the dashboard's own snapshot stands as Restate's word does"
        );

        let attaching = Followed::attaching("batch-1", EPOCH);
        assert!(
            !attaching.stands_still(EPOCH + 3 * 60),
            "a batch with no progress yet has nothing that could stand"
        );
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

    /// [`after`]`(2)` as the projection recorded it once the workflow
    /// finished: `alice` asked for it, it ran for thirty seconds.
    fn recorded() -> BatchRecord {
        after(2)
            .completed_record(UserId::new("alice"), EPOCH - 30, EPOCH)
            .expect("every target has settled")
    }

    /// [`after`]`(2)` as it reads back from the record: the record does not
    /// keep the head each target was sent against, so the targets carry none.
    fn read_back() -> BatchProgress {
        let mut progress = after(2);
        for item in &mut progress.targets {
            item.target.expected_sha.clear();
        }
        progress
    }

    /// A link eight days on names a batch Restate has long retired, and the
    /// projection has held the finished record the whole time. The record is
    /// the batch as it stands, and the follow ends on it at once: Restate is
    /// not asked, and nothing is waited for.
    #[tokio::test]
    async fn a_batch_the_projection_has_finished_is_shown_from_the_record_without_asking_restate() {
        let mut gateway = Scripted::new([], [Ok(None)])
            .projecting(Ok(Some(ProjectedBatch::Finished(recorded()))));

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(read_back()));
        assert_eq!(gateway.progress_calls, 0, "Restate is not asked");
        assert_eq!(gateway.ticks, 0, "and nothing is waited for");
        assert_eq!(
            reported
                .iter()
                .map(|followed| (followed.heard, followed.progress.clone(), followed.since))
                .collect::<Vec<_>>(),
            [(true, Some(read_back()), EPOCH)],
            "the record is reported once, as the word on the batch"
        );
    }

    /// The listing the workflow wrote when it started [`after`]`(0)`.
    fn listed() -> RunningBatch {
        RunningBatch {
            batch_id: "batch-1".to_owned(),
            action: BulkActionKind::Merge,
            requested_by: UserId::new("alice"),
            started_at: EPOCH - 30,
            target_count: 2,
        }
    }

    /// The workflow lists the batch as running itself, after it has published
    /// its first progress, so the listing is the projection's word that
    /// Restate has the batch — as good as a submission receipt. It is polled
    /// for as long as Restate takes, well past what an id nobody has heard of
    /// is given; and what the listing says of the batch is reported at once,
    /// so the drawer has something to say before Restate answers.
    #[tokio::test]
    async fn a_batch_the_projection_lists_as_running_is_polled_without_the_give_up() {
        let progress = repeat(Ok(None), 2 * ATTACH_LIMIT).chain([Ok(Some(after(2)))]);
        let mut gateway =
            Scripted::new([], progress).projecting(Ok(Some(ProjectedBatch::Running(listed()))));

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(
            (reported[0].listing.clone(), reported[0].progress.clone()),
            (Listing::Running(listed()), None),
            "the listing is the first word, before any progress"
        );
        assert_eq!(heard(&reported), [after(2)]);
    }

    /// The projection is the first word, not the only one: a read of it that
    /// fails says nothing about the batch, and Restate is asked as it always
    /// was, under the budget for an id nobody has vouched for. The fault is
    /// passed on for the drawer, as a failing poll is.
    #[tokio::test]
    async fn a_projection_that_cannot_be_read_leaves_the_batch_to_restate() {
        let store_down = Fault::Refused("The read model is unavailable".to_owned());
        let mut gateway = Scripted::new([], [Ok(None)]).projecting(Err(store_down.clone()));

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Unknown);
        assert_eq!(gateway.ticks, ATTACH_LIMIT);
        assert_eq!(
            reported
                .iter()
                .map(|followed| (followed.listing.clone(), followed.trouble.clone()))
                .collect::<Vec<_>>(),
            [
                (Listing::Unreadable, Some(store_down)),
                (Listing::Unreadable, None)
            ],
            "the fault is reported when it happens and again when the polls are answered"
        );
    }

    /// A refusal of the credentials on the projection read is the same
    /// refusal every poll would meet, with a credential prompt each; the
    /// follow ends there, before a single poll, saying so.
    #[tokio::test]
    async fn a_projection_read_refused_the_credentials_ends_the_follow_before_any_poll() {
        let mut gateway = Scripted::new([], [Ok(Some(after(2)))]).projecting(Err(Fault::SignedOut));

        let (outcome, reported) = attach(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::SignedOut);
        assert_eq!(gateway.progress_calls, 0);
        assert_eq!(
            reported
                .last()
                .map(|followed| (followed.trouble.clone(), followed.progress.clone())),
            Some((Some(Fault::SignedOut), None))
        );
    }
}
