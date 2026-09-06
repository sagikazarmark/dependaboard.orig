//! The `DashboardIngress` service: the manual refreshes the dashboard asks for, addressed by
//! name rather than disguised as GitHub deliveries, and the word on what this deployment
//! can do.

use dependaboard_core::{Capabilities, ManualSyncRequest, SyncRequest};
use restate_sdk::prelude::*;

use crate::{
    handler::{HandlerOutcome, traced, traced_read},
    installation_sync::InstallationSyncClient,
    pull_request::{PullRequestClient, request_key},
};

pub(crate) struct DashboardIngress {
    pub(crate) installation_id: u64,
    /// What the service resolved at startup that decides which actions the dashboard
    /// offers; fixed for the life of the process.
    pub(crate) capabilities: Capabilities,
}

impl HandlerOutcome for Json<Capabilities> {
    fn outcome(&self) -> String {
        if self.0.rebase_enabled {
            "rebase enabled".to_owned()
        } else {
            "rebase disabled".to_owned()
        }
    }
}

#[restate_sdk::service]
impl DashboardIngress {
    /// What this deployment can do, so the dashboard offers only the actions the service
    /// would not refuse. Answered from the settings resolved at startup: no side effect,
    /// nothing to journal, and asked once per page, so it completes quietly.
    #[handler]
    async fn capabilities(&self, _ctx: Context<'_>) -> HandlerResult<Json<Capabilities>> {
        let installation_id = self.installation_id.to_string();
        traced_read("DashboardIngress/capabilities", &installation_id, async {
            Ok(Json::from(self.capabilities))
        })
        .await
    }

    /// Reconciles the whole installation now, without waiting for the scheduler's next sweep.
    #[handler]
    async fn sync_installation(&self, ctx: Context<'_>) -> HandlerResult<()> {
        let installation_id = self.installation_id.to_string();
        traced(
            "DashboardIngress/sync_installation",
            &installation_id,
            async {
                ctx.object_client::<InstallationSyncClient>(installation_id.clone())
                    .sync_now()
                    .send();
                Ok(())
            },
        )
        .await
    }

    /// Refreshes one pull request's snapshot and records the request's completion id on it,
    /// so the dashboard can tell this refresh from the webhook syncs around it.
    #[handler]
    async fn sync_pull_request(
        &self,
        ctx: Context<'_>,
        request: Json<ManualSyncRequest>,
    ) -> HandlerResult<()> {
        let request = manual_sync(request.into_inner());
        let key = request_key(&request).to_string();
        traced("DashboardIngress/sync_pull_request", &key, async {
            ctx.object_client::<PullRequestClient>(key.clone())
                .sync(Json::from(request))
                .send();
            Ok(())
        })
        .await
    }
}

/// The canonical sync a manual refresh becomes. Someone is waiting on it, so it does not
/// queue behind the webhook debounce, and it names the completion id the dashboard polls for.
fn manual_sync(request: ManualSyncRequest) -> SyncRequest {
    SyncRequest {
        repository_id: request.repository_id,
        owner: request.owner,
        repo: request.repo,
        number: request.number,
        bypass_debounce: true,
        completion_id: Some(request.completion_id),
    }
}

#[cfg(test)]
mod tests {
    use restate_sdk::service::Discoverable;

    use super::*;

    #[test]
    fn dashboard_ingress_is_reachable_through_ingress() {
        let discovery = <DashboardIngress as Discoverable>::discover();
        assert_ne!(discovery.ingress_private, Some(true));

        for handler_name in ["sync_installation", "sync_pull_request", "capabilities"] {
            let handler = discovery
                .handlers
                .iter()
                .find(|handler| handler.name.as_str() == handler_name)
                .unwrap_or_else(|| panic!("missing DashboardIngress/{handler_name}"));
            assert_ne!(handler.ingress_private, Some(true), "{handler_name}");
        }
    }

    #[test]
    fn a_manual_sync_bypasses_the_debounce_and_carries_its_completion_id() {
        let request = ManualSyncRequest {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            completion_id: "sync-123".to_owned(),
        };

        assert_eq!(
            manual_sync(request),
            SyncRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                number: 9,
                bypass_debounce: true,
                completion_id: Some("sync-123".to_owned()),
            }
        );
    }
}
