//! The stateless ingress services: `WebhookIngress` routes verified GitHub deliveries to the
//! object that owns them, and `SchedulerIngress` lets the bootstrap arm the installation's
//! reconcile chain.

use dependaboard_core::{DASHBOARD_SYNC_ACTION, PrKey, SyncRequest, SyncShaRequest, WebhookEvent};
use restate_sdk::prelude::*;
use thiserror::Error;

use crate::{
    handler::{HandlerOutcome, traced},
    installation_sync::InstallationSyncClient,
    pull_request::{PullRequestClient, request_key},
    repo_sync::RepoSyncClient,
};

pub(crate) struct WebhookIngress {
    pub(crate) installation_id: u64,
}

pub(crate) struct SchedulerIngress {
    pub(crate) installation_id: u64,
}

#[restate_sdk::service]
impl SchedulerIngress {
    #[handler]
    async fn start(&self, ctx: Context<'_>) -> HandlerResult<()> {
        let installation_id = self.installation_id.to_string();
        traced("SchedulerIngress/start", &installation_id, async {
            ctx.object_client::<InstallationSyncClient>(installation_id.clone())
                .start()
                .send();
            Ok(())
        })
        .await
    }
}

#[restate_sdk::service]
impl WebhookIngress {
    /// Routes a verified GitHub delivery to the object that owns it.
    ///
    /// The web edge acknowledges kinds with no arm in `route_webhook` before they reach
    /// Restate, and its copy of this table (`route_delivery` in the web app)
    /// must be kept in step: adding a kind to `route_webhook` without adding it there
    /// means the edge silently swallows it.
    #[handler]
    async fn dispatch(&self, ctx: Context<'_>, event: Json<WebhookEvent>) -> HandlerResult<()> {
        let event = event.into_inner();
        // A service has no object key; the delivery kind is what identifies this invocation.
        let delivery = format!("{}.{}", event.event, event.action.as_deref().unwrap_or("-"));
        traced("WebhookIngress/dispatch", &delivery, async {
            // A rejection is the handler's failure, so `traced` logs it with its reason;
            // the message carries the ids needed to find the delivery in GitHub.
            let route = route_webhook(self.installation_id, &event)
                .map_err(|rejection| TerminalError::new(rejection.to_string()))?;
            send_webhook_route(&ctx, &route);
            Ok(route)
        })
        .await
        .map(|_| ())
    }
}

/// Where a webhook delivery goes, decided before anything is sent so the decision can be
/// tested and logged without a Restate context.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WebhookRoute {
    ClosePullRequest(PrKey),
    SyncPullRequest(SyncRequest),
    SyncSha(SyncShaRequest),
    Installation {
        installation_id: u64,
        action: InstallationLifecycleAction,
    },
    /// A routed kind whose action has no arm: `check_suite.requested`,
    /// `check_run.rerequested`, `pull_request.assigned`, and the like. Kinds the edge
    /// acknowledges never get this far.
    Ignore,
}

impl HandlerOutcome for WebhookRoute {
    fn outcome(&self) -> String {
        match self {
            Self::ClosePullRequest(key) => format!("sent PullRequest/{key}.closed"),
            Self::SyncPullRequest(request) => {
                format!("sent PullRequest/{}.sync", request_key(request))
            }
            Self::SyncSha(request) => {
                format!("sent RepoSync/{}.sync_sha", request.repository_id)
            }
            Self::Installation {
                installation_id,
                action,
            } => {
                let handler = match action {
                    InstallationLifecycleAction::Start => "start",
                    InstallationLifecycleAction::SyncNow => "sync_now",
                    InstallationLifecycleAction::Pause => "pause",
                    InstallationLifecycleAction::Purge => "purge",
                };
                format!("sent InstallationSync/{installation_id}.{handler}")
            }
            Self::Ignore => "ignored".to_owned(),
        }
    }
}

/// Why a delivery can never be routed, however often Restate retried it.
///
/// Rendered into the terminal failure and the handler's log line, so it must carry the ids
/// an operator needs to find the offending delivery in GitHub.
#[derive(Debug, Error, PartialEq, Eq)]
enum WebhookRejection {
    #[error("webhook installation {actual} does not match configured installation {expected}")]
    ForeignInstallation { expected: u64, actual: u64 },
    #[error("{kind} webhook is missing {}", fields.join(", "))]
    MissingFields {
        kind: &'static str,
        fields: Vec<&'static str>,
    },
}

fn route_webhook(
    installation_id: u64,
    event: &WebhookEvent,
) -> Result<WebhookRoute, WebhookRejection> {
    if let Some(actual) = event
        .installation_id
        .filter(|actual| *actual != installation_id)
    {
        return Err(WebhookRejection::ForeignInstallation {
            expected: installation_id,
            actual,
        });
    }
    match event.event.as_str() {
        "pull_request" => route_pull_request(event),
        "check_suite" if event.action.as_deref() == Some("completed") => route_sha(event),
        "check_run" if matches!(event.action.as_deref(), Some("created" | "completed")) => {
            route_sha(event)
        }
        "status" => route_sha(event),
        "installation" => route_installation(event),
        "installation_repositories" => Ok(match event.installation_id {
            Some(installation_id) => WebhookRoute::Installation {
                installation_id,
                action: InstallationLifecycleAction::SyncNow,
            },
            None => WebhookRoute::Ignore,
        }),
        _ => Ok(WebhookRoute::Ignore),
    }
}

fn route_pull_request(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let (Some(repository_id), Some(owner), Some(repo), Some(number)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.number,
    ) else {
        return Err(missing_fields(
            "pull_request",
            [
                ("repository_id", event.repository_id.is_none()),
                ("owner", event.owner.is_none()),
                ("repo", event.repo.is_none()),
                ("number", event.number.is_none()),
            ],
        ));
    };
    Ok(match event.action.as_deref() {
        Some("closed") => WebhookRoute::ClosePullRequest(PrKey::new(repository_id, number)),
        action if pull_request_action_requests_sync(action) => {
            WebhookRoute::SyncPullRequest(SyncRequest {
                repository_id,
                owner: owner.clone(),
                repo: repo.clone(),
                number,
                bypass_debounce: action == Some(DASHBOARD_SYNC_ACTION),
                completion_id: event.sync_completion_id.clone(),
            })
        }
        _ => WebhookRoute::Ignore,
    })
}

fn route_sha(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let (Some(repository_id), Some(owner), Some(repo), Some(sha)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.sha.as_ref(),
    ) else {
        return Err(missing_fields(
            "commit",
            [
                ("repository_id", event.repository_id.is_none()),
                ("owner", event.owner.is_none()),
                ("repo", event.repo.is_none()),
                ("sha", event.sha.is_none()),
            ],
        ));
    };
    Ok(WebhookRoute::SyncSha(SyncShaRequest {
        repository_id,
        owner: owner.clone(),
        repo: repo.clone(),
        sha: sha.clone(),
        pull_requests: event.pull_requests.clone(),
    }))
}

fn route_installation(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let Some(installation_id) = event.installation_id else {
        return Err(missing_fields("installation", [("installation_id", true)]));
    };
    Ok(
        match installation_lifecycle_action(event.action.as_deref()) {
            Some(action) => WebhookRoute::Installation {
                installation_id,
                action,
            },
            None => WebhookRoute::Ignore,
        },
    )
}

fn missing_fields(
    kind: &'static str,
    fields: impl IntoIterator<Item = (&'static str, bool)>,
) -> WebhookRejection {
    WebhookRejection::MissingFields {
        kind,
        fields: fields
            .into_iter()
            .filter_map(|(field, missing)| missing.then_some(field))
            .collect(),
    }
}

fn send_webhook_route(ctx: &Context<'_>, route: &WebhookRoute) {
    match route {
        WebhookRoute::ClosePullRequest(key) => {
            ctx.object_client::<PullRequestClient>(key.to_string())
                .closed()
                .send();
        }
        WebhookRoute::SyncPullRequest(request) => {
            ctx.object_client::<PullRequestClient>(request_key(request).to_string())
                .sync(Json::from(request.clone()))
                .send();
        }
        WebhookRoute::SyncSha(request) => {
            ctx.object_client::<RepoSyncClient>(request.repository_id.to_string())
                .sync_sha(Json::from(request.clone()))
                .send();
        }
        WebhookRoute::Installation {
            installation_id,
            action,
        } => {
            let client = ctx.object_client::<InstallationSyncClient>(installation_id.to_string());
            match action {
                InstallationLifecycleAction::Start => {
                    client.start().send();
                }
                InstallationLifecycleAction::SyncNow => {
                    client.sync_now().send();
                }
                InstallationLifecycleAction::Pause => {
                    client.pause().send();
                }
                InstallationLifecycleAction::Purge => {
                    client.purge().send();
                }
            }
        }
        WebhookRoute::Ignore => {}
    }
}

fn pull_request_action_requests_sync(action: Option<&str>) -> bool {
    matches!(
        action,
        Some(
            "opened"
                | "reopened"
                | "synchronize"
                | "edited"
                | "labeled"
                | "unlabeled"
                | DASHBOARD_SYNC_ACTION
        )
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallationLifecycleAction {
    Start,
    SyncNow,
    Pause,
    Purge,
}

fn installation_lifecycle_action(action: Option<&str>) -> Option<InstallationLifecycleAction> {
    match action {
        Some("created") => Some(InstallationLifecycleAction::SyncNow),
        Some("unsuspend") => Some(InstallationLifecycleAction::Start),
        Some("suspend") => Some(InstallationLifecycleAction::Pause),
        Some("deleted") => Some(InstallationLifecycleAction::Purge),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_webhooks_map_to_distinct_lifecycle_actions() {
        assert_eq!(
            installation_lifecycle_action(Some("created")),
            Some(InstallationLifecycleAction::SyncNow)
        );
        assert_eq!(
            installation_lifecycle_action(Some("unsuspend")),
            Some(InstallationLifecycleAction::Start)
        );
        assert_eq!(
            installation_lifecycle_action(Some("suspend")),
            Some(InstallationLifecycleAction::Pause)
        );
        assert_eq!(
            installation_lifecycle_action(Some("deleted")),
            Some(InstallationLifecycleAction::Purge)
        );
        assert_eq!(
            installation_lifecycle_action(Some("new_permissions_accepted")),
            None
        );
    }

    #[test]
    fn dashboard_action_requests_a_pull_request_sync() {
        assert!(pull_request_action_requests_sync(Some(
            DASHBOARD_SYNC_ACTION
        )));
        assert!(!pull_request_action_requests_sync(Some("closed")));
        assert!(!pull_request_action_requests_sync(None));
    }

    /// A delivery as the web edge forwards it, for installation 1 and repository 7.
    fn delivery(event: &str, action: Option<&str>) -> WebhookEvent {
        WebhookEvent {
            event: event.to_owned(),
            action: action.map(str::to_owned),
            installation_id: Some(1),
            repository_id: Some(7),
            owner: Some("acme".to_owned()),
            repo: Some("api".to_owned()),
            number: Some(9),
            sha: Some("abc123".to_owned()),
            pull_requests: Vec::new(),
            sync_completion_id: None,
        }
    }

    #[test]
    fn deliveries_for_another_installation_are_rejected_naming_both_ids() {
        let mut event = delivery("pull_request", Some("opened"));
        event.installation_id = Some(5);

        let rejection = route_webhook(1, &event).unwrap_err();

        assert_eq!(
            rejection.to_string(),
            "webhook installation 5 does not match configured installation 1"
        );
    }

    #[test]
    fn deliveries_without_routing_fields_are_rejected_naming_the_missing_ones() {
        let mut pull = delivery("pull_request", Some("opened"));
        pull.repository_id = None;
        pull.number = None;
        assert_eq!(
            route_webhook(1, &pull).unwrap_err().to_string(),
            "pull_request webhook is missing repository_id, number"
        );

        let mut check = delivery("check_run", Some("completed"));
        check.sha = None;
        assert_eq!(
            route_webhook(1, &check).unwrap_err().to_string(),
            "commit webhook is missing sha"
        );

        let mut installation = delivery("installation", Some("suspend"));
        installation.installation_id = None;
        assert_eq!(
            route_webhook(1, &installation).unwrap_err().to_string(),
            "installation webhook is missing installation_id"
        );
    }

    #[test]
    fn a_dashboard_sync_routes_to_the_pull_request_bypassing_the_debounce() {
        let mut event = delivery("pull_request", Some(DASHBOARD_SYNC_ACTION));
        event.sync_completion_id = Some("completion-1".to_owned());

        assert_eq!(
            route_webhook(1, &event).unwrap(),
            WebhookRoute::SyncPullRequest(SyncRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                number: 9,
                bypass_debounce: true,
                completion_id: Some("completion-1".to_owned()),
            })
        );
        assert!(matches!(
            route_webhook(1, &delivery("pull_request", Some("synchronize"))).unwrap(),
            WebhookRoute::SyncPullRequest(request) if !request.bypass_debounce
        ));
        assert_eq!(
            route_webhook(1, &delivery("pull_request", Some("closed"))).unwrap(),
            WebhookRoute::ClosePullRequest(PrKey::new(7, 9))
        );
    }

    #[test]
    fn commit_deliveries_route_to_the_repository_with_the_pull_requests_github_named() {
        let mut event = delivery("check_run", Some("completed"));
        event.pull_requests = vec![9, 12];

        assert_eq!(
            route_webhook(1, &event).unwrap(),
            WebhookRoute::SyncSha(SyncShaRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                sha: "abc123".to_owned(),
                pull_requests: vec![9, 12],
            })
        );
    }

    #[test]
    fn installation_deliveries_route_to_the_installation_object() {
        assert_eq!(
            route_webhook(1, &delivery("installation", Some("suspend"))).unwrap(),
            WebhookRoute::Installation {
                installation_id: 1,
                action: InstallationLifecycleAction::Pause,
            }
        );
        assert_eq!(
            route_webhook(1, &delivery("installation_repositories", Some("added"))).unwrap(),
            WebhookRoute::Installation {
                installation_id: 1,
                action: InstallationLifecycleAction::SyncNow,
            }
        );
    }

    #[test]
    fn routed_kinds_with_unhandled_actions_are_ignored_not_rejected() {
        for (event, action) in [
            ("pull_request", "assigned"),
            ("check_suite", "requested"),
            ("check_run", "rerequested"),
            ("installation", "new_permissions_accepted"),
        ] {
            assert_eq!(
                route_webhook(1, &delivery(event, Some(action))).unwrap(),
                WebhookRoute::Ignore,
                "{event}.{action}"
            );
        }
    }
}
