//! Retrying the targets a batch rejected. A rejection is GitHub's, or the
//! guard's, last word on the request as it was sent, most often because the
//! head had moved; so before the targets go out again each is synced and its
//! row read back, and the new batch carries the head SHAs the dashboard now
//! shows rather than the ones the batch was rejected over. Only rejections a
//! fresh attempt can cure are sent again: one over the configuration — the
//! identity GitHub refuses, the merge method the repository disallows, the
//! user token the deployment has none of — would be rejected the same way
//! whatever the head, and is left out with that said.
//!
//! The notice that names the targets left out is shared with the submission
//! itself, which leaves out a target the projection no longer has by the
//! time the batch is submitted; both name the pull request with its
//! repository, in one voice.

use dependaboard_core::{BatchProgress, BulkActionKind, PrRecord, PrTarget, RejectReason};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use crate::ui::PendingAction;
use crate::ui::dashboard_state::DashboardState;
use crate::ui::pr_sync::{ServerSync, SyncFailure, sync_pr};

/// The server as the retry sees it, so the flow can be driven by a script in
/// tests. A refresh that failed says why, since a refusal of the credentials
/// ends the retry where any other failure leaves the one target out.
pub(crate) trait RefreshGateway {
    /// Syncs `target`'s pull request again and reads its row back; `None`
    /// once the pull request is no longer in the dashboard.
    async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, SyncFailure>;
}

/// The refresh as the drawer's per-PR **Sync** does it, through the server
/// functions, on the page whose line to the server `state` carries: queue
/// the sync, wait for its completion id, read the row. Not asked from a page
/// the server has already refused, as [`ServerSync`] is not.
pub(crate) struct ServerRefresh {
    pub(crate) state: DashboardState,
}

impl RefreshGateway for ServerRefresh {
    async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, SyncFailure> {
        let mut sync = ServerSync {
            key: target.key(),
            state: self.state,
        };
        sync_pr(&mut sync, || {}).await
    }
}

/// The retry was given up before anything was queued: the server refused
/// the credentials of one target's sync, or of a poll after it. Every other
/// target's would be refused the same, and so would the batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SignedOut;

/// The rejected targets of a batch once they have been refreshed: which batch,
/// and of what kind, the rows to submit, carrying their current head SHAs, and
/// the targets left out, each with why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Refreshed {
    /// The batch whose rejected targets these are.
    pub(crate) batch_id: String,
    pub(crate) action: BulkActionKind,
    pub(crate) rows: Vec<PrRecord>,
    pub(crate) left_out: Vec<LeftOut>,
}

impl Refreshed {
    /// What to tell the user about the targets left out; nothing if none were.
    pub(crate) fn notice(&self) -> Option<String> {
        left_out_notice("the retry", &self.left_out)
    }

    /// The batch to queue: one of the same kind over the refreshed rows, naming
    /// the batch it retries so the record of the one points at the record of the
    /// other. `None` when every target was left out: there is nothing to queue.
    pub(crate) fn retry(&self) -> Option<PendingAction> {
        if self.rows.is_empty() {
            return None;
        }
        Some(PendingAction {
            action: self.action,
            rows: self.rows.clone(),
            retried_from: Some(self.batch_id.clone()),
        })
    }
}

/// What to tell the user about the targets left out of `what` — "the retry",
/// "the batch" — each named with its repository and why; nothing if none
/// were.
pub(crate) fn left_out_notice(what: &str, left_out: &[LeftOut]) -> Option<String> {
    if left_out.is_empty() {
        return None;
    }
    let entries = left_out
        .iter()
        .map(|left_out| {
            let target = &left_out.target;
            let why = match &left_out.why {
                LeftOutReason::NoLongerOpen => "is no longer open".to_owned(),
                LeftOutReason::WouldBeRejectedAgain(reason) => {
                    format!("would be rejected again ({reason})")
                }
                LeftOutReason::CouldNotRefresh(error) => {
                    format!("could not be refreshed ({error})")
                }
            };
            format!("{}/{}#{} {why}", target.owner, target.repo, target.number)
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!("Left out of {what}: {entries}."))
}

/// A target a retry, or a batch, does not carry, and why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LeftOut {
    pub(crate) target: PrTarget,
    pub(crate) why: LeftOutReason,
}

impl LeftOut {
    pub(crate) fn no_longer_open(target: &PrTarget) -> Self {
        Self {
            target: target.clone(),
            why: LeftOutReason::NoLongerOpen,
        }
    }
}

/// Why a target is not in the retry, or the batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LeftOutReason {
    /// The pull request was closed or merged: rejected as not found, gone by
    /// the time it was refreshed, or gone from the projection by the time the
    /// batch was submitted.
    NoLongerOpen,
    /// The rejection was over the configuration, not the head, and a fresh
    /// attempt would meet it again; carries the reason.
    WouldBeRejectedAgain(RejectReason),
    /// The refresh itself failed; carries the error.
    CouldNotRefresh(String),
}

/// Whether a fresh attempt can cure `reason`. A head that moved is cured by
/// sending the head the dashboard shows now; GitHub judges mergeability anew
/// on every attempt, so a base that has since moved or checks that have since
/// finished can cure that too. The identity GitHub refuses, the merge
/// method the repository disallows, and the user token the deployment lacks
/// are the same whatever the head, and a pull request rejected as not found
/// is closed or merged with its row gone.
fn worth_retrying(reason: &RejectReason) -> bool {
    match reason {
        RejectReason::StaleSha { .. } | RejectReason::NotMergeable => true,
        RejectReason::Forbidden
        | RejectReason::MergeMethodDisallowed
        | RejectReason::NotFound
        | RejectReason::NoUserToken => false,
    }
}

/// Whether `progress` has anything for a retry to take: it has run to the
/// end and rejected a target for a reason a fresh attempt can cure. A batch
/// whose rejections are all over the configuration, or all of pull requests
/// no longer open, has nothing left to send again.
pub(crate) fn can_retry(progress: &BatchProgress) -> bool {
    progress.completed
        && progress
            .rejected_targets()
            .any(|(_, reason)| worth_retrying(reason))
}

/// Refreshes the targets `progress` rejected, all at once, and sorts them into
/// the rows to submit again and the targets left out. A target rejected as
/// not found is left out unrefreshed: its pull request is closed or merged
/// and its row already gone. So is one rejected over the configuration: a
/// refresh changes nothing GitHub would judge differently.
///
/// One failure is not a target's alone: the server refusing the credentials
/// of any refresh. Every other target's sync and poll would be refused the
/// same, each with a credential prompt, and so would the batch the retry
/// queues; so the retry is given up at the first refusal, as [`SignedOut`],
/// with the refreshes still in flight dropped before they ask again.
pub(crate) async fn refresh_rejected<G: RefreshGateway>(
    gateway: &G,
    progress: &BatchProgress,
) -> Result<Refreshed, SignedOut> {
    let mut left_out = Vec::new();
    let mut to_refresh = Vec::new();
    for (target, reason) in progress.rejected_targets() {
        if *reason == RejectReason::NotFound {
            left_out.push(LeftOut::no_longer_open(target));
        } else if !worth_retrying(reason) {
            left_out.push(LeftOut {
                target: target.clone(),
                why: LeftOutReason::WouldBeRejectedAgain(reason.clone()),
            });
        } else {
            to_refresh.push(target);
        }
    }
    // The answers come in as they land; they are sorted back into the batch's
    // order once all are in, so the retry lists its targets as the batch did.
    let mut refreshing: FuturesUnordered<_> = to_refresh
        .into_iter()
        .enumerate()
        .map(|(order, target)| async move { (order, target, gateway.refresh(target).await) })
        .collect();
    let mut answered = Vec::new();
    while let Some((order, target, answer)) = refreshing.next().await {
        if answer.as_ref().is_err_and(SyncFailure::signed_out) {
            return Err(SignedOut);
        }
        answered.push((order, target, answer));
    }
    answered.sort_by_key(|(order, ..)| *order);
    let mut rows = Vec::new();
    for (_, target, answer) in answered {
        match answer {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => left_out.push(LeftOut::no_longer_open(target)),
            Err(failure) => left_out.push(LeftOut {
                target: target.clone(),
                why: LeftOutReason::CouldNotRefresh(failure.to_string()),
            }),
        }
    }
    Ok(Refreshed {
        batch_id: progress.batch_id.clone(),
        action: progress.action,
        rows,
        left_out,
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use dependaboard_core::{ActionOutcome, BulkActionKind, PrKey, RejectReason};

    use super::*;
    use crate::ui::test_support::{grouped_row, half_done_merge, off_page_row, serde_row};
    use crate::ui::{Fault, PendingAction, pr_target};

    /// A server whose answer to each target's refresh is scripted by key, and
    /// which remembers which targets it was asked to refresh and which it
    /// answered, each in order. A target the script does not name fails its
    /// refresh, so a target that should never have been refreshed shows up as
    /// left out with that error. An answer can be held back a number of
    /// turns, so the answers come in an order of the test's choosing rather
    /// than the targets' — or not at all, if the refresh is dropped first.
    struct Scripted {
        answers: BTreeMap<PrKey, Result<Option<PrRecord>, SyncFailure>>,
        held_back: BTreeMap<PrKey, u32>,
        asked: RefCell<Vec<PrKey>>,
        answered: RefCell<Vec<PrKey>>,
    }

    impl Scripted {
        fn new(
            answers: impl IntoIterator<Item = (PrRecord, Result<Option<PrRecord>, SyncFailure>)>,
        ) -> Self {
            Self {
                answers: answers
                    .into_iter()
                    .map(|(row, answer)| (pr_target(&row).key(), answer))
                    .collect(),
                held_back: BTreeMap::new(),
                asked: RefCell::new(Vec::new()),
                answered: RefCell::new(Vec::new()),
            }
        }

        /// With `row`'s answer held back `turns` turns of the executor.
        fn holding_back(mut self, row: &PrRecord, turns: u32) -> Self {
            self.held_back.insert(pr_target(row).key(), turns);
            self
        }
    }

    impl RefreshGateway for Scripted {
        async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, SyncFailure> {
            let key = target.key();
            self.asked.borrow_mut().push(key.clone());
            for _ in 0..self.held_back.get(&key).copied().unwrap_or(0) {
                tokio::task::yield_now().await;
            }
            self.answered.borrow_mut().push(key.clone());
            self.answers.get(&key).cloned().unwrap_or_else(|| {
                Err(SyncFailure::Unconfirmed(Fault::Refused(format!(
                    "{key} was not expected to be refreshed"
                ))))
            })
        }
    }

    /// The refresh failing on the server's side: Restate away.
    fn unavailable() -> SyncFailure {
        SyncFailure::Unconfirmed(Fault::Refused("Restate is unavailable".to_owned()))
    }

    /// [`refresh_rejected`] on a page the server lets in.
    async fn refresh(gateway: &Scripted, progress: &BatchProgress) -> Refreshed {
        refresh_rejected(gateway, progress)
            .await
            .expect("no refresh was refused the credentials")
    }

    /// `row` as the store shows it after another push: a new head SHA.
    fn moved_on(row: &PrRecord) -> PrRecord {
        PrRecord {
            head_sha: format!("{}-fresh", row.head_sha),
            ..row.clone()
        }
    }

    fn stale(row: &PrRecord) -> RejectReason {
        RejectReason::StaleSha {
            expected: row.head_sha.clone(),
            actual: moved_on(row).head_sha,
        }
    }

    /// A completed merge of the three fixture rows: the grouped one merged,
    /// the serde one rejected for a moved head, the off-page one failed.
    fn one_of_each() -> BatchProgress {
        let targets = [
            pr_target(&grouped_row()),
            pr_target(&serde_row()),
            pr_target(&off_page_row()),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: stale(&serde_row()),
            },
        );
        progress.record_failure(&targets[2].key(), "GitHub mutation failed with HTTP 500");
        assert!(progress.completed);
        progress
    }

    #[tokio::test]
    async fn only_the_rejected_targets_are_refreshed_and_come_back_with_their_current_heads() {
        let gateway = Scripted::new([
            (grouped_row(), Ok(Some(moved_on(&grouped_row())))),
            (serde_row(), Ok(Some(moved_on(&serde_row())))),
            (off_page_row(), Ok(Some(moved_on(&off_page_row())))),
        ]);

        let refreshed = refresh(&gateway, &one_of_each()).await;

        assert_eq!(
            refreshed,
            Refreshed {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                rows: vec![moved_on(&serde_row())],
                left_out: vec![],
            }
        );
        assert_eq!(refreshed.notice(), None);
    }

    /// What a retry queues is a batch of the same kind over the refreshed rows that
    /// names the batch it retries, so the record of the one points at the record of
    /// the other and the audit view can link them each way. A retry with nothing
    /// left to send queues no batch.
    #[tokio::test]
    async fn the_retry_is_a_batch_of_the_same_kind_that_names_the_batch_it_retries() {
        let gateway = Scripted::new([(serde_row(), Ok(Some(moved_on(&serde_row()))))]);

        let refreshed = refresh(&gateway, &one_of_each()).await;

        assert_eq!(
            refreshed.retry(),
            Some(PendingAction {
                action: BulkActionKind::Merge,
                rows: vec![moved_on(&serde_row())],
                retried_from: Some("batch-1".to_owned()),
            })
        );

        let nothing_left = Refreshed {
            rows: Vec::new(),
            ..refreshed
        };
        assert_eq!(nothing_left.retry(), None);
    }

    /// A batch rejected two targets: one for a moved head, one as not found.
    /// The pull request rejected as not found is closed or merged already, and
    /// its row deleted, so there is nothing to refresh, let alone retry.
    #[tokio::test]
    async fn a_target_rejected_as_not_found_is_left_out_without_being_refreshed() {
        let targets = [pr_target(&serde_row()), pr_target(&off_page_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: stale(&serde_row()),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotFound,
            },
        );
        // Only the moved one is scripted: refreshing the other is an error.
        let gateway = Scripted::new([(serde_row(), Ok(Some(moved_on(&serde_row()))))]);

        let refreshed = refresh(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                rows: vec![moved_on(&serde_row())],
                left_out: vec![LeftOut {
                    target: pr_target(&off_page_row()),
                    why: LeftOutReason::NoLongerOpen,
                }],
            }
        );
        assert_eq!(
            refreshed.notice().as_deref(),
            Some("Left out of the retry: acme/web#13 is no longer open.")
        );
    }

    /// The same voice names the targets a batch was submitted without: a pull
    /// request the projection no longer had at submit is named with its
    /// repository, as the retry names the ones it leaves out, rather than by
    /// a bare number.
    #[test]
    fn a_target_left_out_of_a_batch_is_named_with_its_repository() {
        let left_out = [
            LeftOut::no_longer_open(&pr_target(&serde_row())),
            LeftOut::no_longer_open(&pr_target(&off_page_row())),
        ];

        assert_eq!(
            left_out_notice("the batch", &left_out).as_deref(),
            Some(
                "Left out of the batch: acme/web#12 is no longer open; acme/web#13 is no longer open."
            )
        );
        assert_eq!(left_out_notice("the batch", &[]), None);
    }

    /// A rejected pull request may have been merged or closed since: the
    /// refresh finds no row for it. It is left out the same way as one
    /// rejected as not found, and the retry carries on with the rest.
    #[tokio::test]
    async fn a_target_gone_by_the_time_it_is_refreshed_is_left_out_as_no_longer_open() {
        let targets = [pr_target(&serde_row()), pr_target(&off_page_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: stale(&serde_row()),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );
        let gateway = Scripted::new([
            (serde_row(), Ok(None)),
            (off_page_row(), Ok(Some(moved_on(&off_page_row())))),
        ]);

        let refreshed = refresh(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                rows: vec![moved_on(&off_page_row())],
                left_out: vec![LeftOut {
                    target: pr_target(&serde_row()),
                    why: LeftOutReason::NoLongerOpen,
                }],
            }
        );
    }

    /// A rejection over the configuration is not over the head: the identity
    /// GitHub refused and the merge method the repository disallows are the
    /// same on the next attempt, so refreshing and resubmitting would only
    /// reject them again and write a second audit row each. They are left
    /// out unrefreshed, the notice says why in GitHub's terms, and the ones
    /// a fresh head can cure go on.
    #[tokio::test]
    async fn a_target_rejected_over_the_configuration_is_left_out_without_being_refreshed() {
        let targets = [
            pr_target(&grouped_row()),
            pr_target(&serde_row()),
            pr_target(&off_page_row()),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: stale(&serde_row()),
            },
        );
        progress.record(
            &targets[2].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::MergeMethodDisallowed,
            },
        );
        // Only the moved one is scripted: refreshing either other is an error.
        let gateway = Scripted::new([(serde_row(), Ok(Some(moved_on(&serde_row()))))]);

        let refreshed = refresh(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                rows: vec![moved_on(&serde_row())],
                left_out: vec![
                    LeftOut {
                        target: pr_target(&grouped_row()),
                        why: LeftOutReason::WouldBeRejectedAgain(RejectReason::Forbidden),
                    },
                    LeftOut {
                        target: pr_target(&off_page_row()),
                        why: LeftOutReason::WouldBeRejectedAgain(
                            RejectReason::MergeMethodDisallowed
                        ),
                    },
                ],
            }
        );
        assert_eq!(
            refreshed.notice().as_deref(),
            Some(
                "Left out of the retry: acme/api#9 would be rejected again (the configured \
                 identity is not allowed to perform this action); acme/web#13 would be \
                 rejected again (the repository disallows this merge method)."
            )
        );
    }

    /// The offer follows the same rule as the refresh: a finished batch with
    /// a rejection a fresh attempt can cure has one; a batch whose rejections
    /// are all over the configuration, or all of pull requests no longer
    /// open, has none. Nor has a batch still running, whatever it has
    /// rejected so far, or one that finished with a target failed and none
    /// rejected: a failure was the round trip's, not the request's, and is
    /// not what the retry is for.
    #[test]
    fn a_retry_is_offered_only_for_rejections_a_fresh_attempt_can_cure() {
        fn finished_with(reason: RejectReason) -> BatchProgress {
            let targets = [pr_target(&serde_row())];
            let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
            progress.record(&targets[0].key(), ActionOutcome::Rejected { reason });
            assert!(progress.completed);
            progress
        }

        assert!(can_retry(&finished_with(stale(&serde_row()))));
        assert!(can_retry(&finished_with(RejectReason::NotMergeable)));
        assert!(!can_retry(&finished_with(RejectReason::Forbidden)));
        assert!(!can_retry(&finished_with(
            RejectReason::MergeMethodDisallowed
        )));
        assert!(!can_retry(&finished_with(RejectReason::NotFound)));
        assert!(
            !can_retry(&finished_with(RejectReason::NoUserToken)),
            "a rebase without a user token is refused the same way every time"
        );

        let targets = [pr_target(&serde_row()), pr_target(&off_page_row())];
        let mut running = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        running.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );
        assert!(!running.completed);
        assert!(
            !can_retry(&running),
            "a batch still running is not offered a retry yet"
        );

        let mut failed = half_done_merge();
        failed.record_failure(
            &pr_target(&serde_row()).key(),
            "GitHub mutation failed with HTTP 500",
        );
        assert!(failed.completed);
        assert!(
            !can_retry(&failed),
            "a failed target is not a rejected one, and a batch with nothing rejected has \
             nothing to retry"
        );
    }

    /// A rebase asked of a deployment without a user token is a rejection
    /// over the configuration: the notice says so in words that name what
    /// is missing, and the target is not refreshed.
    #[tokio::test]
    async fn a_rebase_rejected_for_want_of_a_user_token_is_left_out_saying_so() {
        let targets = [pr_target(&serde_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Rebase, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NoUserToken,
            },
        );
        let gateway = Scripted::new([]);

        let refreshed = refresh(&gateway, &progress).await;

        assert_eq!(refreshed.rows, vec![]);
        assert_eq!(refreshed.retry(), None, "nothing is left to queue");
        assert_eq!(
            refreshed.notice().as_deref(),
            Some(
                "Left out of the retry: acme/web#12 would be rejected again (no GitHub user \
                 token is configured for @dependabot commands)."
            )
        );
    }

    /// A refresh that fails is not a reason to send that target out with the
    /// head SHA it was just rejected over, nor to hold the others back: it is
    /// left out with its error and the rest go on. The notice lists every
    /// target left out, each with its own reason.
    #[tokio::test]
    async fn a_target_whose_refresh_fails_is_left_out_with_the_error_and_the_rest_go_on() {
        let targets = [
            pr_target(&grouped_row()),
            pr_target(&serde_row()),
            pr_target(&off_page_row()),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::UpdateBranch, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: stale(&grouped_row()),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );
        progress.record(
            &targets[2].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotFound,
            },
        );
        let gateway = Scripted::new([
            (grouped_row(), Ok(Some(moved_on(&grouped_row())))),
            (serde_row(), Err(unavailable())),
        ]);

        let refreshed = refresh(&gateway, &progress).await;

        assert_eq!(refreshed.rows, vec![moved_on(&grouped_row())]);
        assert_eq!(
            refreshed.left_out,
            vec![
                LeftOut {
                    target: pr_target(&off_page_row()),
                    why: LeftOutReason::NoLongerOpen,
                },
                LeftOut {
                    target: pr_target(&serde_row()),
                    why: LeftOutReason::CouldNotRefresh("Restate is unavailable".to_owned()),
                },
            ]
        );
        assert_eq!(
            refreshed.notice().as_deref(),
            Some(
                "Left out of the retry: acme/web#13 is no longer open; \
                 acme/web#12 could not be refreshed (Restate is unavailable)."
            )
        );
    }

    /// Every target of `batch-1` rejected as not mergeable: three refreshes
    /// to run.
    fn three_rejected() -> BatchProgress {
        let targets = [
            pr_target(&grouped_row()),
            pr_target(&serde_row()),
            pr_target(&off_page_row()),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        for target in &targets {
            progress.record(
                &target.key(),
                ActionOutcome::Rejected {
                    reason: RejectReason::NotMergeable,
                },
            );
        }
        progress
    }

    /// A refresh the server refused the credentials of is not a target to
    /// leave out and go on without: every other target's sync and poll would
    /// be refused the same, each with a credential prompt, and so would the
    /// batch the retry queues. The retry is given up there and then, and
    /// nothing is queued. On a page already signed out the first target's
    /// request is refused without asking, before a poll of anything, and the
    /// other targets are not started at all — whether the refusal is of the
    /// request or of a poll after it.
    #[tokio::test]
    async fn a_retry_from_a_page_already_signed_out_is_given_up_with_nothing_more_started() {
        for refusal in [
            SyncFailure::NotQueued(Fault::SignedOut),
            SyncFailure::Unconfirmed(Fault::SignedOut),
        ] {
            let gateway = Scripted::new([
                (grouped_row(), Err(refusal.clone())),
                (serde_row(), Ok(Some(moved_on(&serde_row())))),
                (off_page_row(), Ok(Some(moved_on(&off_page_row())))),
            ]);

            let outcome = refresh_rejected(&gateway, &three_rejected()).await;

            assert_eq!(outcome, Err(SignedOut), "{refusal:?}");
            assert_eq!(
                *gateway.asked.borrow(),
                vec![pr_target(&grouped_row()).key()],
                "nothing is asked after the refusal ({refusal:?})"
            );
        }
    }

    /// The password is rotated while the targets are being refreshed: every
    /// refresh is out already, and the first poll refused ends them all. The
    /// waits still in flight are dropped where they stand, so none of them
    /// asks again, and none answers.
    #[tokio::test]
    async fn a_refusal_while_the_targets_are_being_refreshed_ends_every_wait_in_flight() {
        let gateway = Scripted::new([
            (grouped_row(), Ok(Some(moved_on(&grouped_row())))),
            (serde_row(), Err(SyncFailure::Unconfirmed(Fault::SignedOut))),
            (off_page_row(), Ok(Some(moved_on(&off_page_row())))),
        ])
        .holding_back(&grouped_row(), 3)
        .holding_back(&serde_row(), 1)
        .holding_back(&off_page_row(), 3);

        let outcome = refresh_rejected(&gateway, &three_rejected()).await;

        assert_eq!(outcome, Err(SignedOut));
        assert_eq!(
            gateway.asked.borrow().len(),
            3,
            "every refresh was under way when the refusal came"
        );
        assert_eq!(
            *gateway.answered.borrow(),
            vec![pr_target(&serde_row()).key()],
            "the refused one is the only one to answer: the rest were dropped mid-wait"
        );
    }

    /// The targets are refreshed all at once, and their syncs land in
    /// whatever order GitHub answers; the retry still lists them as the batch
    /// it retries did, so the drawer reads the same down both.
    #[tokio::test]
    async fn the_refreshed_rows_keep_the_batchs_order_whatever_order_the_answers_came_in() {
        let gateway = Scripted::new([
            (grouped_row(), Ok(Some(moved_on(&grouped_row())))),
            (serde_row(), Ok(Some(moved_on(&serde_row())))),
            (off_page_row(), Ok(Some(moved_on(&off_page_row())))),
        ])
        .holding_back(&grouped_row(), 2)
        .holding_back(&serde_row(), 1);

        let refreshed = refresh(&gateway, &three_rejected()).await;

        assert_eq!(
            refreshed.rows,
            vec![
                moved_on(&grouped_row()),
                moved_on(&serde_row()),
                moved_on(&off_page_row()),
            ]
        );
        assert_eq!(
            *gateway.answered.borrow(),
            vec![
                pr_target(&off_page_row()).key(),
                pr_target(&serde_row()).key(),
                pr_target(&grouped_row()).key(),
            ],
            "the answers did come in the other order"
        );
    }
}
