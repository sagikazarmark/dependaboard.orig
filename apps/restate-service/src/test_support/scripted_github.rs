//! A [`GithubApi`] that answers from a script, so handler logic that takes the trait can run
//! against the GitHub answers a test chose, and the test can see what was asked of GitHub.

use std::{collections::VecDeque, sync::Mutex};

use async_trait::async_trait;
use dependaboard_core::{
    CommandRequest, MergeMethod, MergeRequest, PrRecord, RepoRecord, SyncRequest,
    UpdateBranchRequest,
};
use dependaboard_github::{GithubApi, GithubError};

/// One call made of GitHub, with what it asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GithubCall {
    FetchSnapshot(SyncRequest),
    ListInstallationRepositories,
    ListDependabotPrs {
        owner: String,
        repo: String,
        repository_id: u64,
    },
    Merge {
        request: MergeRequest,
        merge_method: Option<MergeMethod>,
    },
    PostCommand(CommandRequest),
    UpdateBranch(UpdateBranchRequest),
}

/// Answers each kind of call from its own queue, in the order the test scripted them, and
/// records every call made. A call with nothing left to answer it is a test bug and panics
/// naming the call.
pub(crate) struct ScriptedGithub {
    installation_id: u64,
    snapshots: Answers<Result<Option<PrRecord>, GithubError>>,
    installation_repositories: Answers<Result<Vec<RepoRecord>, GithubError>>,
    dependabot_prs: Answers<Result<Vec<SyncRequest>, GithubError>>,
    merges: Answers<Result<String, GithubError>>,
    commands: Answers<Result<String, GithubError>>,
    branch_updates: Answers<Result<String, GithubError>>,
    calls: Mutex<Vec<GithubCall>>,
}

/// The scripted answers for one kind of call, handed out in order.
struct Answers<T>(Mutex<VecDeque<T>>);

impl<T> Default for Answers<T> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<T> Answers<T> {
    fn push(&self, answer: T) {
        self.0.lock().unwrap().push_back(answer);
    }

    fn pop(&self) -> Option<T> {
        self.0.lock().unwrap().pop_front()
    }
}

impl ScriptedGithub {
    pub(crate) fn new(installation_id: u64) -> Self {
        Self {
            installation_id,
            snapshots: Answers::default(),
            installation_repositories: Answers::default(),
            dependabot_prs: Answers::default(),
            merges: Answers::default(),
            commands: Answers::default(),
            branch_updates: Answers::default(),
            calls: Mutex::default(),
        }
    }

    pub(crate) fn on_fetch_snapshot(&self, answer: Result<Option<PrRecord>, GithubError>) -> &Self {
        self.snapshots.push(answer);
        self
    }

    pub(crate) fn on_list_installation_repositories(
        &self,
        answer: Result<Vec<RepoRecord>, GithubError>,
    ) -> &Self {
        self.installation_repositories.push(answer);
        self
    }

    pub(crate) fn on_list_dependabot_prs(
        &self,
        answer: Result<Vec<SyncRequest>, GithubError>,
    ) -> &Self {
        self.dependabot_prs.push(answer);
        self
    }

    pub(crate) fn on_merge(&self, answer: Result<String, GithubError>) -> &Self {
        self.merges.push(answer);
        self
    }

    pub(crate) fn on_post_command(&self, answer: Result<String, GithubError>) -> &Self {
        self.commands.push(answer);
        self
    }

    pub(crate) fn on_update_branch(&self, answer: Result<String, GithubError>) -> &Self {
        self.branch_updates.push(answer);
        self
    }

    /// Every call made so far, in order.
    pub(crate) fn calls(&self) -> Vec<GithubCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Records `call` and hands back its next scripted answer.
    fn answer<T>(&self, call: GithubCall, answers: &Answers<T>) -> T {
        self.calls.lock().unwrap().push(call.clone());
        answers
            .pop()
            .unwrap_or_else(|| panic!("ScriptedGithub has no answer scripted for {call:?}"))
    }
}

#[async_trait]
impl GithubApi for ScriptedGithub {
    fn installation_id(&self) -> u64 {
        self.installation_id
    }

    async fn fetch_snapshot(&self, request: &SyncRequest) -> Result<Option<PrRecord>, GithubError> {
        self.answer(GithubCall::FetchSnapshot(request.clone()), &self.snapshots)
    }

    async fn list_installation_repositories(&self) -> Result<Vec<RepoRecord>, GithubError> {
        self.answer(
            GithubCall::ListInstallationRepositories,
            &self.installation_repositories,
        )
    }

    async fn list_dependabot_prs(
        &self,
        owner: &str,
        repo: &str,
        repository_id: u64,
    ) -> Result<Vec<SyncRequest>, GithubError> {
        self.answer(
            GithubCall::ListDependabotPrs {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                repository_id,
            },
            &self.dependabot_prs,
        )
    }

    async fn merge(
        &self,
        request: &MergeRequest,
        merge_method: Option<MergeMethod>,
    ) -> Result<String, GithubError> {
        self.answer(
            GithubCall::Merge {
                request: request.clone(),
                merge_method,
            },
            &self.merges,
        )
    }

    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError> {
        self.answer(GithubCall::PostCommand(request.clone()), &self.commands)
    }

    async fn update_branch(&self, request: &UpdateBranchRequest) -> Result<String, GithubError> {
        self.answer(
            GithubCall::UpdateBranch(request.clone()),
            &self.branch_updates,
        )
    }
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{DependabotCommand, UserId};

    use super::*;
    use crate::test_support::{snapshot, target};

    fn sync_request() -> SyncRequest {
        SyncRequest {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            bypass_debounce: false,
            completion_id: None,
        }
    }

    #[tokio::test]
    async fn every_call_answers_from_its_script_in_order_and_is_recorded() {
        let github = ScriptedGithub::new(1);
        github
            .on_fetch_snapshot(Ok(Some(snapshot())))
            .on_fetch_snapshot(Ok(None))
            .on_list_installation_repositories(Ok(Vec::new()))
            .on_list_dependabot_prs(Ok(vec![sync_request()]))
            .on_merge(Ok("merged".to_owned()))
            .on_post_command(Err(GithubError::Config("no user token".to_owned())))
            .on_update_branch(Ok("updated".to_owned()));
        let merge = MergeRequest {
            batch_id: "batch".to_owned(),
            target: target(),
        };
        let command = CommandRequest {
            batch_id: "batch".to_owned(),
            target: target(),
            user_id: UserId::new("dashboard"),
            command: DependabotCommand::Rebase,
        };
        let update = UpdateBranchRequest {
            batch_id: "batch".to_owned(),
            target: target(),
        };

        assert_eq!(
            github.fetch_snapshot(&sync_request()).await.unwrap(),
            Some(snapshot())
        );
        assert_eq!(
            github.fetch_snapshot(&sync_request()).await.unwrap(),
            None,
            "the second answer follows the first"
        );
        assert_eq!(
            github.list_installation_repositories().await.unwrap(),
            Vec::new()
        );
        assert_eq!(
            github.list_dependabot_prs("acme", "api", 7).await.unwrap(),
            vec![sync_request()]
        );
        assert_eq!(
            github
                .merge(&merge, Some(MergeMethod::Rebase))
                .await
                .unwrap(),
            "merged"
        );
        assert!(matches!(
            github.post_command(&command).await,
            Err(GithubError::Config(_))
        ));
        assert_eq!(github.update_branch(&update).await.unwrap(), "updated");
        assert_eq!(
            github.calls(),
            vec![
                GithubCall::FetchSnapshot(sync_request()),
                GithubCall::FetchSnapshot(sync_request()),
                GithubCall::ListInstallationRepositories,
                GithubCall::ListDependabotPrs {
                    owner: "acme".to_owned(),
                    repo: "api".to_owned(),
                    repository_id: 7,
                },
                GithubCall::Merge {
                    request: merge,
                    merge_method: Some(MergeMethod::Rebase),
                },
                GithubCall::PostCommand(command),
                GithubCall::UpdateBranch(update),
            ]
        );
    }
}
