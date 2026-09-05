//! Following a manual sync of one pull request to its end. The sync is
//! one-way: Restate takes the request and answers with nothing, so the
//! dashboard polls the pull request's durable state until the completion id
//! it was handed is recorded there, then reads the refreshed row back.

use std::time::Duration;

use dependaboard_core::{PrRecord, PrState};

use crate::api::{load_pr_projection, load_pr_status};
use crate::ui::{POLL_INTERVAL, sleep, user_facing};

/// How long a manual sync is waited for before it is given up on.
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// [`SYNC_TIMEOUT`] in polls.
const SYNC_POLLS: u64 = SYNC_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs();

/// Waits for the sync `completion_id` names to land, then reads the row back.
/// `Ok(None)` means the pull request is no longer in the dashboard: the sync
/// found it closed, or it went while the sync was awaited.
pub(crate) async fn wait_for_pr_sync_completion(
    repository_id: u64,
    number: u64,
    completion_id: String,
) -> Result<Option<PrRecord>, String> {
    let mut last_error = None;
    for _ in 0..SYNC_POLLS {
        sleep(POLL_INTERVAL).await;
        match load_pr_status(repository_id, number).await {
            Ok(state) if sync_id_completed(state.as_ref(), &completion_id) => {
                match load_pr_projection(repository_id, number).await {
                    Ok(row) => return Ok(row),
                    Err(error) => last_error = Some(user_facing(&error)),
                }
            }
            Ok(_) => match load_pr_projection(repository_id, number).await {
                Ok(None) => return Ok(None),
                Ok(Some(_)) => last_error = None,
                Err(error) => last_error = Some(user_facing(&error)),
            },
            Err(error) => last_error = Some(user_facing(&error)),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        format!(
            "the sync did not complete within {} seconds",
            SYNC_TIMEOUT.as_secs()
        )
    }))
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
    use super::*;

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
