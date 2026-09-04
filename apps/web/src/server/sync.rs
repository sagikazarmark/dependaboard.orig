//! The installation the dashboard is bound to and the synthetic ingress
//! events its manual sync requests are dispatched as.

use dependaboard_core::{DASHBOARD_SYNC_ACTION, WebhookEvent};
use dioxus::prelude::ServerFnError;

pub(crate) fn github_installation_id() -> Result<u64, ServerFnError> {
    std::env::var("GITHUB_INSTALLATION_ID")
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID is not configured"))?
        .parse::<u64>()
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID must be an integer"))
}

pub(crate) fn manual_pr_sync_event(
    installation_id: u64,
    repository_id: u64,
    owner: String,
    repo: String,
    number: u64,
    observed_sha: String,
    completion_id: String,
) -> WebhookEvent {
    WebhookEvent {
        event: "pull_request".to_owned(),
        action: Some(DASHBOARD_SYNC_ACTION.to_owned()),
        installation_id: Some(installation_id),
        repository_id: Some(repository_id),
        owner: Some(owner),
        repo: Some(repo),
        number: Some(number),
        sha: Some(observed_sha),
        pull_requests: Vec::new(),
        sync_completion_id: Some(completion_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_pull_request_sync_uses_the_dashboard_ingress_action() {
        let event = manual_pr_sync_event(
            42,
            7,
            "acme".to_owned(),
            "api".to_owned(),
            9,
            "abc123".to_owned(),
            "sync-123".to_owned(),
        );

        assert_eq!(event.event, "pull_request");
        assert_eq!(event.action.as_deref(), Some(DASHBOARD_SYNC_ACTION));
        assert_eq!(event.installation_id, Some(42));
        assert_eq!(event.repository_id, Some(7));
        assert_eq!(event.owner.as_deref(), Some("acme"));
        assert_eq!(event.repo.as_deref(), Some("api"));
        assert_eq!(event.number, Some(9));
        assert_eq!(event.sha.as_deref(), Some("abc123"));
        assert_eq!(event.sync_completion_id.as_deref(), Some("sync-123"));
    }
}
