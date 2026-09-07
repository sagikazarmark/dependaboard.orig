//! Following a manual sync of one pull request to its end. The sync is
//! one-way: Restate takes the request and answers with nothing, so the
//! dashboard polls the pull request's durable state until the completion id
//! it was handed is recorded there, then reads the refreshed row back. The
//! drawer's **Sync** runs one; **Retry rejected** runs one per target, all
//! at once.
//!
//! The polls stop when the row is read, when the pull request is gone, when
//! a minute has passed without the completion landing — or when the server
//! refuses the credentials the polls carry, since asking again would only be
//! refused again, and the browser turns each refusal into a credential
//! prompt. Nor is a sync started from a page the server has already refused.

use std::fmt;
use std::time::Duration;

use dependaboard_core::{PrKey, PrRecord, PrState};

use crate::api::{load_pr_projection, load_pr_status, request_pr_sync};
use crate::ui::dashboard_state::DashboardState;
use crate::ui::{Fault, POLL_INTERVAL, logged_fault, sleep};

/// How long a manual sync is waited for before it is given up on.
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// [`SYNC_TIMEOUT`] in polls.
const SYNC_POLLS: u64 = SYNC_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs();

/// The server as the sync sees it, so the flow can be driven by a script in
/// tests. A call that failed says what it said about the line to the
/// server, since a refusal of the credentials ends the sync where any other
/// fault is waited out.
pub(crate) trait SyncGateway {
    /// Queues the sync; the completion id to wait for.
    async fn request(&mut self) -> Result<String, Fault>;
    /// The pull request's durable state; `None` once the pull request is no
    /// longer in the dashboard.
    async fn status(&mut self) -> Result<Option<PrState>, Fault>;
    /// The pull request's row; `None` once it is no longer in the dashboard.
    async fn projection(&mut self) -> Result<Option<PrRecord>, Fault>;
    /// Waits one poll interval.
    async fn tick(&mut self);
}

/// The sync of the pull request `key` names through the server functions,
/// on the page whose line to the server `state` carries. A page the server
/// has already refused — a poll got the 401 — is not asked on behalf of: the
/// answer would be the same refusal, with a credential prompt for it, so
/// each call is handed the refusal as if it had asked.
pub(crate) struct ServerSync {
    pub(crate) key: PrKey,
    pub(crate) state: DashboardState,
}

impl SyncGateway for ServerSync {
    async fn request(&mut self) -> Result<String, Fault> {
        if self.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        request_pr_sync(self.key.repository_id, self.key.number)
            .await
            .map_err(|error| logged_fault(&error))
    }

    async fn status(&mut self) -> Result<Option<PrState>, Fault> {
        if self.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        load_pr_status(self.key.repository_id, self.key.number)
            .await
            .map_err(|error| logged_fault(&error))
    }

    async fn projection(&mut self) -> Result<Option<PrRecord>, Fault> {
        if self.state.signed_out() {
            return Err(Fault::SignedOut);
        }
        load_pr_projection(self.key.repository_id, self.key.number)
            .await
            .map_err(|error| logged_fault(&error))
    }

    async fn tick(&mut self) {
        sleep(POLL_INTERVAL).await;
    }
}

/// Why a sync did not yield the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SyncFailure {
    /// Restate was not asked: the request failed; carries the fault.
    NotQueued(Fault),
    /// Restate took the sync, and a poll after it failed; carries the last
    /// poll's fault.
    Unconfirmed(Fault),
    /// Restate took the sync, and it was not seen to complete within
    /// [`SYNC_TIMEOUT`].
    TimedOut,
}

impl SyncFailure {
    /// Whether the server refused the credentials, of the request or of a
    /// poll after it: the page is signed out, and nothing more is asked of
    /// the server until it is reloaded.
    pub(crate) fn signed_out(&self) -> bool {
        matches!(
            self,
            Self::NotQueued(Fault::SignedOut) | Self::Unconfirmed(Fault::SignedOut)
        )
    }
}

impl fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotQueued(fault) | Self::Unconfirmed(fault) => fault.fmt(f),
            Self::TimedOut => write!(
                f,
                "the sync did not complete within {} seconds",
                SYNC_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Queues a sync of the pull request `gateway` names and follows it to its
/// end: calls `on_queued` once Restate has taken it, then polls until the
/// completion id is recorded and reads the row back. `Ok(None)` means the
/// pull request is no longer in the dashboard: the sync found it closed, or
/// it went while the sync was awaited.
///
/// A poll that fails is noted and the next is taken, and the last one's
/// fault is the word when the polls run out. The one fault not waited out is
/// the server refusing the credentials — of the request, or of either call a
/// poll makes: the next would be refused the same, and each refusal the
/// browser gets it turns into a credential prompt, so the sync is given up
/// there and then, for that reason.
pub(crate) async fn sync_pr<G: SyncGateway>(
    gateway: &mut G,
    on_queued: impl FnOnce(),
) -> Result<Option<PrRecord>, SyncFailure> {
    let completion_id = gateway.request().await.map_err(SyncFailure::NotQueued)?;
    on_queued();
    let mut last_fault = None;
    for _ in 0..SYNC_POLLS {
        gateway.tick().await;
        let fault = match gateway.status().await {
            Ok(state) if sync_id_completed(state.as_ref(), &completion_id) => {
                match gateway.projection().await {
                    Ok(row) => return Ok(row),
                    Err(fault) => fault,
                }
            }
            Ok(_) => match gateway.projection().await {
                Ok(None) => return Ok(None),
                Ok(Some(_)) => {
                    last_fault = None;
                    continue;
                }
                Err(fault) => fault,
            },
            Err(fault) => fault,
        };
        if fault == Fault::SignedOut {
            return Err(SyncFailure::Unconfirmed(fault));
        }
        last_fault = Some(fault);
    }
    Err(last_fault.map_or(SyncFailure::TimedOut, SyncFailure::Unconfirmed))
}

fn sync_id_completed(state: Option<&PrState>, completion_id: &str) -> bool {
    state.is_some_and(|state| {
        state
            .completed_sync_ids
            .iter()
            .any(|completed| completed == completion_id)
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::ui::test_support::{next_or_repeat, serde_row};

    /// A server whose answers are scripted: each poll pops the next answer,
    /// and the last one repeats once the script runs out. The request is
    /// answered once.
    struct Scripted {
        request: Result<String, Fault>,
        status: VecDeque<Result<Option<PrState>, Fault>>,
        projection: VecDeque<Result<Option<PrRecord>, Fault>>,
        request_calls: u32,
        status_calls: u32,
        projection_calls: u32,
        ticks: u32,
    }

    impl Scripted {
        fn new(
            request: Result<String, Fault>,
            status: impl IntoIterator<Item = Result<Option<PrState>, Fault>>,
            projection: impl IntoIterator<Item = Result<Option<PrRecord>, Fault>>,
        ) -> Self {
            Self {
                request,
                status: status.into_iter().collect(),
                projection: projection.into_iter().collect(),
                request_calls: 0,
                status_calls: 0,
                projection_calls: 0,
                ticks: 0,
            }
        }
    }

    impl SyncGateway for Scripted {
        async fn request(&mut self) -> Result<String, Fault> {
            self.request_calls += 1;
            self.request.clone()
        }

        async fn status(&mut self) -> Result<Option<PrState>, Fault> {
            self.status_calls += 1;
            next_or_repeat(&mut self.status)
        }

        async fn projection(&mut self) -> Result<Option<PrRecord>, Fault> {
            self.projection_calls += 1;
            next_or_repeat(&mut self.projection)
        }

        async fn tick(&mut self) {
            self.ticks += 1;
        }
    }

    /// The completion id the scripted server hands out.
    const SYNC: &str = "sync-123";

    /// The durable state with `SYNC` recorded as complete.
    fn completed() -> PrState {
        let mut state = PrState::default();
        state.complete_sync(SYNC.to_owned());
        state
    }

    /// The durable state before `SYNC` has landed: another sync's id only.
    fn pending() -> PrState {
        let mut state = PrState::default();
        state.complete_sync("sync-456".to_owned());
        state
    }

    /// [`serde_row`] as the sync refreshed it: a new head SHA.
    fn refreshed() -> PrRecord {
        PrRecord {
            head_sha: "def456-fresh".to_owned(),
            ..serde_row()
        }
    }

    async fn run(gateway: &mut Scripted) -> (Result<Option<PrRecord>, SyncFailure>, u32) {
        let mut queued = 0;
        let outcome = sync_pr(gateway, || queued += 1).await;
        (outcome, queued)
    }

    /// The sync is one-way, so the dashboard learns it has landed from the
    /// pull request's durable state: the poll that finds the completion id
    /// recorded is the one that reads the row back, and the row is the one
    /// the sync wrote. Until then the row is read only to see the pull
    /// request is still there.
    #[tokio::test]
    async fn a_sync_is_queued_and_the_row_read_back_once_its_completion_is_recorded() {
        let mut gateway = Scripted::new(
            Ok(SYNC.to_owned()),
            [Ok(Some(pending())), Ok(Some(completed()))],
            [Ok(Some(serde_row())), Ok(Some(refreshed()))],
        );

        let (outcome, queued) = run(&mut gateway).await;

        assert_eq!(outcome, Ok(Some(refreshed())));
        assert_eq!(
            queued, 1,
            "the caller hears once that Restate took the sync"
        );
        assert_eq!(
            (gateway.request_calls, gateway.ticks, gateway.status_calls),
            (1, 2, 2),
            "one request, then a poll interval before each of the two polls"
        );
    }

    /// A poll the server refused the credentials for is the last one taken.
    /// Every further poll would be refused the same, and each 401 the
    /// browser gets carries a challenge it turns into a credential prompt;
    /// the sync carries on in Restate without being waited for, and the wait
    /// ends saying so, so the drawer can tell the page it is signed out.
    #[tokio::test]
    async fn a_status_poll_refused_the_credentials_is_the_last_one_taken() {
        let mut gateway = Scripted::new(
            Ok(SYNC.to_owned()),
            [Err(Fault::SignedOut), Ok(Some(completed()))],
            [Ok(Some(refreshed()))],
        );

        let (outcome, queued) = run(&mut gateway).await;

        assert_eq!(outcome, Err(SyncFailure::Unconfirmed(Fault::SignedOut)));
        assert_eq!(queued, 1);
        assert_eq!(
            gateway.status_calls, 1,
            "nothing is asked after the refusal"
        );
        assert_eq!(gateway.projection_calls, 0);
        assert_eq!(gateway.ticks, 1);
    }

    /// The row is read on every poll — for the row once the completion has
    /// landed, and before that to see the pull request is still there — and
    /// a refusal on that read is the same refusal, ending the wait where a
    /// refused status poll does.
    #[tokio::test]
    async fn a_row_read_refused_the_credentials_ends_the_wait_whether_the_sync_has_landed_or_not() {
        let mut landed = Scripted::new(
            Ok(SYNC.to_owned()),
            [Ok(Some(completed()))],
            [Err(Fault::SignedOut), Ok(Some(refreshed()))],
        );

        let (outcome, _) = run(&mut landed).await;

        assert_eq!(outcome, Err(SyncFailure::Unconfirmed(Fault::SignedOut)));
        assert_eq!((landed.status_calls, landed.projection_calls), (1, 1));

        let mut not_yet = Scripted::new(
            Ok(SYNC.to_owned()),
            [Ok(Some(pending())), Ok(Some(completed()))],
            [Err(Fault::SignedOut), Ok(Some(refreshed()))],
        );

        let (outcome, _) = run(&mut not_yet).await;

        assert_eq!(outcome, Err(SyncFailure::Unconfirmed(Fault::SignedOut)));
        assert_eq!((not_yet.status_calls, not_yet.projection_calls), (1, 1));
    }

    /// A request the server refused never reached Restate — the auth edge
    /// refuses before any function runs — so there is no sync to wait for,
    /// and it is given up as not queued, for that reason, without a poll and
    /// without trying again, which would only prompt again. A request that
    /// failed any other way is given up the same: the sync is one-way, and a
    /// request that was not answered was not taken.
    #[tokio::test]
    async fn a_request_that_fails_is_not_queued_and_nothing_is_polled() {
        for fault in [Fault::SignedOut, Fault::Unreachable] {
            let mut gateway = Scripted::new(
                Err(fault.clone()),
                [Ok(Some(completed()))],
                [Ok(Some(refreshed()))],
            );

            let (outcome, queued) = run(&mut gateway).await;

            assert_eq!(outcome, Err(SyncFailure::NotQueued(fault.clone())));
            assert_eq!(queued, 0, "{fault:?}: the caller never hears it was queued");
            assert_eq!(
                (gateway.request_calls, gateway.status_calls, gateway.ticks),
                (1, 0, 0),
                "{fault:?}: the request is not tried again, and nothing is polled"
            );
        }
    }

    /// A poll that fails any other way says nothing about the sync: Restate
    /// has it, and the line to the server is reported over the page in its
    /// own right. The next poll is taken, and the answer when it comes is
    /// the row.
    #[tokio::test]
    async fn a_poll_that_fails_otherwise_is_waited_out() {
        let mut gateway = Scripted::new(
            Ok(SYNC.to_owned()),
            [
                Err(Fault::Unreachable),
                Err(Fault::Refused("Restate is unavailable".to_owned())),
                Ok(Some(completed())),
            ],
            [Ok(Some(refreshed()))],
        );

        let (outcome, _) = run(&mut gateway).await;

        assert_eq!(outcome, Ok(Some(refreshed())));
        assert_eq!(gateway.status_calls, 3);
    }

    /// The pull request may close while the sync is awaited — the sync
    /// itself may find it closed — and its row goes with it. That is the end
    /// of the wait, said as such, rather than a minute of polling for a
    /// completion that will never be read.
    #[tokio::test]
    async fn a_pull_request_gone_while_the_sync_is_awaited_ends_the_wait() {
        let mut gateway = Scripted::new(Ok(SYNC.to_owned()), [Ok(None)], [Ok(None)]);

        let (outcome, _) = run(&mut gateway).await;

        assert_eq!(outcome, Ok(None));
        assert_eq!(gateway.ticks, 1);
    }

    /// A sync not seen to complete within the timeout is given up with the
    /// last poll's word: a fault, if the last poll failed, since it may be
    /// why the completion was not seen; otherwise that the time ran out.
    #[tokio::test]
    async fn a_sync_not_seen_to_complete_in_time_is_given_up_with_the_last_polls_word() {
        let mut quiet = Scripted::new(
            Ok(SYNC.to_owned()),
            [Ok(Some(pending()))],
            [Ok(Some(serde_row()))],
        );

        let (outcome, _) = run(&mut quiet).await;

        assert_eq!(outcome, Err(SyncFailure::TimedOut));
        assert_eq!(u64::from(quiet.ticks), SYNC_POLLS);
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "the sync did not complete within 60 seconds"
        );

        let unavailable = Fault::Refused("Restate is unavailable".to_owned());
        let mut failing = Scripted::new(
            Ok(SYNC.to_owned()),
            [Ok(Some(pending())), Err(unavailable.clone())],
            [Ok(Some(serde_row()))],
        );

        let (outcome, _) = run(&mut failing).await;

        assert_eq!(outcome, Err(SyncFailure::Unconfirmed(unavailable)));
        assert_eq!(u64::from(failing.ticks), SYNC_POLLS);
        assert_eq!(outcome.unwrap_err().to_string(), "Restate is unavailable");
    }

    #[test]
    fn pull_request_sync_completes_only_for_its_request_id() {
        let mut state = PrState::default();
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-456".to_owned());
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-123".to_owned());
        assert!(sync_id_completed(Some(&state), "sync-123"));
    }
}
