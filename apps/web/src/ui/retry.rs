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

use dependaboard_core::{BatchProgress, PrRecord, PrTarget, RejectReason};
use futures_util::future::join_all;

use crate::api::request_pr_sync;
use crate::ui::pr_sync::wait_for_pr_sync_completion;
use crate::ui::user_facing;

/// The server as the retry sees it, so the flow can be driven by a script in
/// tests. Errors are already in their user-facing form.
pub(crate) trait RefreshGateway {
    /// Syncs `target`'s pull request again and reads its row back; `None`
    /// once the pull request is no longer in the dashboard.
    async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, String>;
}

/// The refresh as the drawer's per-PR **Sync** does it, through the server
/// functions: queue the sync, wait for its completion id, read the row.
pub(crate) struct ServerRefresh;

impl RefreshGateway for ServerRefresh {
    async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, String> {
        let completion_id = request_pr_sync(target.repository_id, target.number)
            .await
            .map_err(|error| user_facing(&error))?;
        wait_for_pr_sync_completion(target.repository_id, target.number, completion_id).await
    }
}

/// The rejected targets once they have been refreshed: the rows to submit,
/// carrying their current head SHAs, and the targets left out, each with why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Refreshed {
    pub(crate) rows: Vec<PrRecord>,
    pub(crate) left_out: Vec<LeftOut>,
}

impl Refreshed {
    /// What to tell the user about the targets left out; nothing if none were.
    pub(crate) fn notice(&self) -> Option<String> {
        left_out_notice("the retry", &self.left_out)
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
pub(crate) async fn refresh_rejected<G: RefreshGateway>(
    gateway: &G,
    progress: &BatchProgress,
) -> Refreshed {
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
    let refreshed = join_all(to_refresh.iter().map(|target| gateway.refresh(target))).await;
    let mut rows = Vec::new();
    for (target, answer) in to_refresh.into_iter().zip(refreshed) {
        match answer {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => left_out.push(LeftOut::no_longer_open(target)),
            Err(error) => left_out.push(LeftOut {
                target: target.clone(),
                why: LeftOutReason::CouldNotRefresh(error),
            }),
        }
    }
    Refreshed { rows, left_out }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::BTreeMap;

    use dependaboard_core::{ActionOutcome, BulkActionKind, PrKey, RejectReason};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::{grouped_row, off_page_row, serde_row};

    /// A server whose answer to each target's refresh is scripted by key. A
    /// target the script does not name fails its refresh, so a target that
    /// should never have been refreshed shows up as left out with that error.
    struct Scripted(BTreeMap<PrKey, Result<Option<PrRecord>, String>>);

    impl Scripted {
        fn new(
            answers: impl IntoIterator<Item = (PrRecord, Result<Option<PrRecord>, String>)>,
        ) -> Self {
            Self(
                answers
                    .into_iter()
                    .map(|(row, answer)| (pr_target(&row).key(), answer))
                    .collect(),
            )
        }
    }

    impl RefreshGateway for Scripted {
        async fn refresh(&self, target: &PrTarget) -> Result<Option<PrRecord>, String> {
            self.0.get(&target.key()).cloned().unwrap_or_else(|| {
                Err(format!("{} was not expected to be refreshed", target.key()))
            })
        }
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

        let refreshed = refresh_rejected(&gateway, &one_of_each()).await;

        assert_eq!(
            refreshed,
            Refreshed {
                rows: vec![moved_on(&serde_row())],
                left_out: vec![],
            }
        );
        assert_eq!(refreshed.notice(), None);
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

        let refreshed = refresh_rejected(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
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

        let refreshed = refresh_rejected(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
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

        let refreshed = refresh_rejected(&gateway, &progress).await;

        assert_eq!(
            refreshed,
            Refreshed {
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
    /// open, has none.
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

        let refreshed = refresh_rejected(&gateway, &progress).await;

        assert_eq!(refreshed.rows, vec![]);
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
            (serde_row(), Err("Restate is unavailable".to_owned())),
        ]);

        let refreshed = refresh_rejected(&gateway, &progress).await;

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
}
