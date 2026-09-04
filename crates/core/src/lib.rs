use std::{cmp::Ordering, collections::BTreeMap, fmt, str::FromStr, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[cfg(feature = "dependabot-metadata")]
mod dependabot_metadata;
#[cfg(feature = "dependabot-metadata")]
pub use dependabot_metadata::parse_dependabot_metadata;

pub const DEPENDABOT_LOGIN: &str = "dependabot[bot]";
pub const DEFAULT_PAGE_SIZE: u32 = 50;
pub const MAX_PAGE_SIZE: u32 = 100;
pub const MAX_BATCH_TARGETS: usize = 100;
/// How long a projected row is trusted after we last fetched it (`synced_at`).
/// Older rows are flagged stale in the UI and count as "needs attention".
/// Shorter than the hourly reconcile sweep so a missed sweep is visible.
pub const STALE_AFTER: Duration = Duration::from_secs(45 * 60);

/// Seconds since the Unix epoch on the host clock. Uses `web_time` so the same
/// helper compiles for the browser (wasm32) and native targets.
pub fn unix_seconds() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl UserId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrKey {
    pub repository_id: u64,
    pub number: u64,
}

impl PrKey {
    pub fn new(repository_id: u64, number: u64) -> Self {
        Self {
            repository_id,
            number,
        }
    }
}

impl fmt::Display for PrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repository_id, self.number)
    }
}

#[derive(Debug, Error)]
#[error("invalid pull request key: {0}")]
pub struct ParsePrKeyError(String);

impl FromStr for PrKey {
    type Err = ParsePrKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (repository_id, number) = value
            .split_once('#')
            .ok_or_else(|| ParsePrKeyError(value.to_owned()))?;
        Ok(Self {
            repository_id: repository_id
                .parse()
                .map_err(|_| ParsePrKeyError(value.to_owned()))?,
            number: number
                .parse()
                .map_err(|_| ParsePrKeyError(value.to_owned()))?,
        })
    }
}

/// Ordered by severity: `Unknown < Patch < Minor < Major`. `Ord` is derived
/// from a severity rank rather than declaration order so that `.max()`,
/// sorting and [`highest_update_type`] all agree on the "worst" update.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateType {
    Major,
    Minor,
    Patch,
    #[default]
    Unknown,
}

impl UpdateType {
    pub const ALL: [Self; 4] = [Self::Major, Self::Minor, Self::Patch, Self::Unknown];

    /// The single source of truth for `Ord`; compare values instead of
    /// calling this directly.
    fn rank(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Patch => 1,
            Self::Minor => 2,
            Self::Major => 3,
        }
    }
}

impl Ord for UpdateType {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for UpdateType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for UpdateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Patch => "patch",
            Self::Unknown => "unknown",
        })
    }
}

impl FromStr for UpdateType {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "major" => Self::Major,
            "minor" => Self::Minor,
            "patch" => Self::Patch,
            "unknown" => Self::Unknown,
            _ => return Err(ParseEnumError(value.to_owned())),
        })
    }
}

/// `Ord` follows declaration order and exists only so the status can key a
/// `BTreeMap`; it says nothing about severity. Use `rollup_checks` for that.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Success,
    Failure,
    Pending,
    #[default]
    None,
}

impl CheckStatus {
    pub const ALL: [Self; 4] = [Self::Success, Self::Failure, Self::Pending, Self::None];
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Pending => "pending",
            Self::None => "none",
        })
    }
}

impl FromStr for CheckStatus {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "success" => Self::Success,
            "failure" => Self::Failure,
            "pending" => Self::Pending,
            "none" => Self::None,
            _ => return Err(ParseEnumError(value.to_owned())),
        })
    }
}

#[derive(Debug, Error)]
#[error("unknown enum value: {0}")]
pub struct ParseEnumError(String);

/// GitHub's REST `mergeable_state` vocabulary for a pull request.
///
/// `Unknown` is a catch-all: GitHub reports it while mergeability is still
/// being computed, and any value this crate does not recognise folds into it
/// so that a new upstream state never breaks a sync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeable {
    Clean,
    Dirty,
    Blocked,
    Behind,
    Unstable,
    Draft,
    HasHooks,
    #[default]
    #[serde(other)]
    Unknown,
}

impl Mergeable {
    pub const ALL: [Self; 8] = [
        Self::Clean,
        Self::Dirty,
        Self::Blocked,
        Self::Behind,
        Self::Unstable,
        Self::Draft,
        Self::HasHooks,
        Self::Unknown,
    ];

    /// Maps a raw GitHub `mergeable_state` value. Total: unrecognised input
    /// becomes [`Mergeable::Unknown`].
    pub fn from_github_state(value: &str) -> Self {
        match value {
            "clean" => Self::Clean,
            "dirty" => Self::Dirty,
            "blocked" => Self::Blocked,
            "behind" => Self::Behind,
            "unstable" => Self::Unstable,
            "draft" => Self::Draft,
            "has_hooks" => Self::HasHooks,
            _ => Self::Unknown,
        }
    }

    /// True when GitHub reports a base/head merge conflict (`dirty`). Other
    /// non-clean states (`blocked`, `behind`, `unstable`, ...) are not
    /// conflicts and are resolvable without a rebase.
    pub fn is_conflicting(self) -> bool {
        matches!(self, Self::Dirty)
    }

    /// Field-level deserializer that also accepts JSON `null` (the shape the
    /// field had when it was `Option<String>`), folding it into `Unknown`.
    fn deserialize_lenient<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Option::<Self>::deserialize(deserializer)?.unwrap_or_default())
    }
}

impl fmt::Display for Mergeable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Blocked => "blocked",
            Self::Behind => "behind",
            Self::Unstable => "unstable",
            Self::Draft => "draft",
            Self::HasHooks => "has_hooks",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckSignal {
    Pass,
    Fail,
    Pending,
}

pub fn rollup_checks(signals: impl IntoIterator<Item = CheckSignal>) -> CheckStatus {
    let mut saw_pass = false;
    let mut saw_pending = false;
    for signal in signals {
        match signal {
            CheckSignal::Fail => return CheckStatus::Failure,
            CheckSignal::Pending => saw_pending = true,
            CheckSignal::Pass => saw_pass = true,
        }
    }
    if saw_pending {
        CheckStatus::Pending
    } else if saw_pass {
        CheckStatus::Success
    } else {
        CheckStatus::None
    }
}

pub fn check_signal(status: Option<&str>, conclusion: Option<&str>) -> Option<CheckSignal> {
    if let Some(conclusion) = conclusion {
        return match conclusion {
            "success" | "neutral" | "skipped" => Some(CheckSignal::Pass),
            "failure" | "timed_out" | "action_required" | "cancelled" | "stale"
            | "startup_failure" => Some(CheckSignal::Fail),
            _ => None,
        };
    }
    match status {
        Some("queued" | "in_progress" | "waiting" | "pending" | "requested" | "expected") => {
            Some(CheckSignal::Pending)
        }
        Some("startup_failure") => Some(CheckSignal::Fail),
        _ => None,
    }
}

/// What one commit status contributes, by its state. `expected` is a required context
/// that has not reported yet, which the truth table counts as pending.
pub fn status_signal(state: &str) -> Option<CheckSignal> {
    match state {
        "success" => Some(CheckSignal::Pass),
        "failure" | "error" => Some(CheckSignal::Fail),
        "pending" | "expected" => Some(CheckSignal::Pending),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyUpdate {
    pub name: String,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub update_type: UpdateType,
}

pub fn highest_update_type(updates: &[DependencyUpdate]) -> UpdateType {
    updates
        .iter()
        .map(|dependency| dependency.update_type)
        .max()
        .unwrap_or(UpdateType::Unknown)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    pub repository_id: u64,
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
    /// The method merges in this repository use when the configured
    /// preference is disallowed there, resolved at the last repository sync.
    /// `None` means the preference is allowed, or the row predates this field.
    #[serde(default)]
    pub merge_method: Option<MergeMethod>,
    pub synced_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRecord {
    pub id: String,
    pub repository_id: u64,
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub html_url: String,
    pub dependency: Option<String>,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub dependencies: Vec<DependencyUpdate>,
    pub update_type: UpdateType,
    pub head_sha: String,
    pub check_status: CheckStatus,
    /// Snapshots persisted in Restate before this was an enum may carry
    /// `null` or omit the field; both read as [`Mergeable::Unknown`].
    #[serde(default, deserialize_with = "Mergeable::deserialize_lenient")]
    pub mergeable: Mergeable,
    pub labels: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub synced_at: u64,
}

impl PrRecord {
    pub fn key(&self) -> PrKey {
        PrKey::new(self.repository_id, self.number)
    }

    /// Whether this projection was last fetched more than [`STALE_AFTER`]
    /// before `now` (Unix seconds). A `now` earlier than `synced_at` is fresh.
    pub fn is_stale(&self, now: u64) -> bool {
        now.saturating_sub(self.synced_at) > STALE_AFTER.as_secs()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFilter {
    pub query: Option<String>,
    pub owner: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub update_types: Vec<UpdateType>,
    #[serde(default)]
    pub check_statuses: Vec<CheckStatus>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub dependency: Option<String>,
    #[serde(default)]
    pub needs_attention: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCursor {
    pub updated_at: u64,
    pub id: String,
}

impl PageCursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor is serializable"))
    }

    pub fn decode(value: &str) -> Result<Self, CursorError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| CursorError::Invalid)?;
        serde_json::from_slice(&bytes).map_err(|_| CursorError::Invalid)
    }
}

#[derive(Debug, Error)]
pub enum CursorError {
    #[error("invalid page cursor")]
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub limit: u32,
    pub after: Option<String>,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: DEFAULT_PAGE_SIZE,
            after: None,
        }
    }
}

impl Page {
    pub fn normalized_limit(&self) -> u32 {
        self.limit.clamp(1, MAX_PAGE_SIZE)
    }
}

/// One entry of the ranked label facet: a label and how many open pull
/// requests carry it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelFacet {
    pub label: String,
    pub count: u64,
}

/// Sidebar facet counts for the whole read model.
///
/// `checks` and `update_types` are closed sets, so they are keyed by their
/// enums: the UI walks `CheckStatus::ALL` / `UpdateType::ALL` and looks each
/// one up, treating an absent key as zero. Their map order carries no meaning.
///
/// `labels` is an open set ranked by popularity, so it is an ordered sequence:
/// descending count, ties broken by case-insensitive label name. Consumers
/// that truncate must keep this order rather than re-sorting by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacetCounts {
    pub checks: BTreeMap<CheckStatus, u64>,
    pub update_types: BTreeMap<UpdateType, u64>,
    pub labels: Vec<LabelFacet>,
}

impl FacetCounts {
    pub fn check_count(&self, status: CheckStatus) -> u64 {
        self.checks.get(&status).copied().unwrap_or_default()
    }

    pub fn update_type_count(&self, update_type: UpdateType) -> u64 {
        self.update_types
            .get(&update_type)
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardPage {
    pub rows: Vec<PrRecord>,
    pub total: u64,
    pub next_cursor: Option<String>,
    pub repositories: Vec<RepoRecord>,
    pub facets: FacetCounts,
    pub last_synced_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    #[default]
    Squash,
    Rebase,
}

impl fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        })
    }
}

impl FromStr for MergeMethod {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "merge" => Self::Merge,
            "squash" => Self::Squash,
            "rebase" => Self::Rebase,
            _ => return Err(ParseEnumError(value.to_owned())),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkActionKind {
    Merge,
    Rebase,
}

impl fmt::Display for BulkActionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrTarget {
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub expected_sha: String,
    pub title: String,
    #[serde(default)]
    pub html_url: String,
}

impl PrTarget {
    pub fn key(&self) -> String {
        PrKey::new(self.repository_id, self.number).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkRequest {
    pub action: BulkActionKind,
    pub targets: Vec<PrTarget>,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub batch_id: String,
    pub target: PrTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub batch_id: String,
    pub target: PrTarget,
    pub user_id: UserId,
    pub command: DependabotCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateBranchRequest {
    pub batch_id: String,
    pub target: PrTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependabotCommand {
    Rebase,
}

impl fmt::Display for DependabotCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("rebase")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Succeeded { detail: String },
    Rejected { reason: RejectReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    StaleSha { expected: String, actual: String },
    NotMergeable,
    MergeMethodDisallowed,
    Forbidden,
    NotFound,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleSha { expected, actual } => {
                write!(
                    f,
                    "head moved from {} to {}",
                    short_sha(expected),
                    short_sha(actual)
                )
            }
            Self::NotMergeable => f.write_str("GitHub reports this pull request is not mergeable"),
            Self::MergeMethodDisallowed => {
                f.write_str("the repository disallows this merge method")
            }
            Self::Forbidden => {
                f.write_str("the configured identity is not allowed to perform this action")
            }
            Self::NotFound => f.write_str("the pull request was closed or no longer exists"),
        }
    }
}

fn short_sha(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetProgressState {
    Queued,
    Running,
    Succeeded { detail: String },
    Rejected { reason: RejectReason },
    Failed { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetProgress {
    pub target: PrTarget,
    pub state: TargetProgressState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchProgress {
    pub batch_id: String,
    pub action: BulkActionKind,
    pub targets: Vec<TargetProgress>,
    pub completed: bool,
    pub succeeded: u64,
    pub rejected: u64,
    pub failure: Option<String>,
}

impl BatchProgress {
    pub fn queued(
        batch_id: impl Into<String>,
        action: BulkActionKind,
        targets: &[PrTarget],
    ) -> Self {
        Self {
            batch_id: batch_id.into(),
            action,
            targets: targets
                .iter()
                .cloned()
                .map(|target| TargetProgress {
                    target,
                    state: TargetProgressState::Queued,
                })
                .collect(),
            completed: false,
            succeeded: 0,
            rejected: 0,
            failure: None,
        }
    }

    pub fn record(&mut self, key: &str, outcome: ActionOutcome) {
        let state = match outcome {
            ActionOutcome::Succeeded { detail } => {
                self.succeeded += 1;
                TargetProgressState::Succeeded { detail }
            }
            ActionOutcome::Rejected { reason } => {
                self.rejected += 1;
                TargetProgressState::Rejected { reason }
            }
        };
        if let Some(target) = self
            .targets
            .iter_mut()
            .find(|target| target.target.key() == key)
        {
            target.state = state;
        }
        self.completed = self.succeeded + self.rejected == self.targets.len() as u64;
    }

    pub fn fail(&mut self, detail: impl Into<String>) {
        self.failure = Some(detail.into());
        self.completed = true;
    }
}

pub fn new_batch_id() -> String {
    Uuid::now_v7().to_string()
}

pub fn valid_batch_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.get_version_num() == 7)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// Manual refreshes should not wait behind webhook storm coalescing.
    #[serde(default)]
    pub bypass_debounce: bool,
    #[serde(default)]
    pub completion_id: Option<String>,
}

/// What the dashboard asks `DashboardIngress.sync_pull_request` for: a refresh of one pull
/// request, reported back under `completion_id` once its snapshot is current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSyncRequest {
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// The id the dashboard polls `PullRequest.status` for, in `completed_sync_ids`.
    pub completion_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncShaRequest {
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub sha: String,
    #[serde(default)]
    pub pull_requests: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionLog {
    pub at: u64,
    pub action: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrState {
    pub snapshot: Option<PrRecord>,
    pub history: Vec<ActionLog>,
    pub last_synced_at: Option<u64>,
    pub sync_pending: bool,
    #[serde(default)]
    pub completed_sync_ids: Vec<String>,
}

impl PrState {
    pub fn push_history(&mut self, entry: ActionLog) {
        self.history.push(entry);
        if self.history.len() > 20 {
            self.history.drain(..self.history.len() - 20);
        }
    }

    pub fn complete_sync(&mut self, completion_id: String) {
        if !self.completed_sync_ids.contains(&completion_id) {
            self.completed_sync_ids.push(completion_id);
        }
        if self.completed_sync_ids.len() > 20 {
            self.completed_sync_ids
                .drain(..self.completed_sync_ids.len() - 20);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub event: String,
    pub action: Option<String>,
    pub installation_id: Option<u64>,
    pub repository_id: Option<u64>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub number: Option<u64>,
    pub sha: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    Merge,
    Comment,
    UpdateBranch,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Merge => "merge",
            Self::Comment => "comment",
            Self::UpdateBranch => "update_branch",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubErrorResponse {
    pub status: u16,
    pub message: String,
    pub documentation_url: Option<String>,
    pub rate_limit_remaining: Option<u64>,
    pub rate_limit_reset: Option<u64>,
    pub retry_after_seconds: Option<u64>,
}

/// How long to back off from a secondary rate limit that carries no `Retry-After`.
///
/// GitHub's guidance is to wait at least one minute before retrying in that case.
pub const DEFAULT_RATE_LIMIT_WAIT_SECONDS: u64 = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classification {
    /// Transient: retrying the identical request with backoff may succeed.
    Retryable,
    /// GitHub asked us to back off; do not retry before `until` (unix seconds).
    RateLimited {
        until: u64,
    },
    Rejected(RejectReason),
    Fatal,
}

/// Classifies a failed GitHub response into an outcome for the caller.
///
/// `now` is the caller's unix-seconds clock, so the classification is a pure function of
/// its inputs: journal it once and replay it verbatim rather than recomputing it.
pub fn classify_github_error(
    response: &GithubErrorResponse,
    operation: Operation,
    known_resource: bool,
    now: u64,
) -> Classification {
    let message = response.message.to_ascii_lowercase();
    let is_rate_limited = response.rate_limit_remaining == Some(0)
        || response.retry_after_seconds.is_some()
        || message.contains("secondary rate limit")
        || message.contains("rate limit exceeded")
        || (response.status == 429);
    if is_rate_limited {
        let until = response
            .retry_after_seconds
            .map(|after| now.saturating_add(after))
            .or(response.rate_limit_reset)
            .unwrap_or_else(|| now.saturating_add(DEFAULT_RATE_LIMIT_WAIT_SECONDS));
        return Classification::RateLimited { until };
    }
    if response.status >= 500 {
        return Classification::Retryable;
    }
    if operation == Operation::Merge
        && response.status == 405
        && message.contains("base branch was modified")
    {
        return Classification::Retryable;
    }
    match (response.status, operation) {
        (404, _) if known_resource => Classification::Rejected(RejectReason::NotFound),
        (404, _) => Classification::Fatal,
        (403, Operation::Comment) => Classification::Rejected(RejectReason::Forbidden),
        (405, Operation::Merge) if message.contains("merge method") => {
            Classification::Rejected(RejectReason::MergeMethodDisallowed)
        }
        (405 | 409 | 422, Operation::Merge) => Classification::Rejected(RejectReason::NotMergeable),
        (422, Operation::UpdateBranch) => Classification::Rejected(RejectReason::NotMergeable),
        (401 | 403, _) => Classification::Fatal,
        _ => Classification::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_key_round_trips() {
        let key = PrKey::new(42, 7);
        assert_eq!(key.to_string().parse::<PrKey>().unwrap(), key);
    }

    #[test]
    fn user_id_is_a_transparent_string() {
        let user_id = UserId::new("dashboard");
        assert_eq!(serde_json::to_string(&user_id).unwrap(), r#""dashboard""#);
        assert_eq!(
            serde_json::from_str::<UserId>(r#""dashboard""#).unwrap(),
            user_id
        );
    }

    #[test]
    fn major_is_the_maximum_update_type_of_a_mixed_collection() {
        // `Ord` must agree with severity so that sorting, `.max()` and
        // `highest_update_type` all pick the same "worst" update.
        let mixed = [
            UpdateType::Minor,
            UpdateType::Unknown,
            UpdateType::Major,
            UpdateType::Patch,
        ];
        assert_eq!(mixed.into_iter().max(), Some(UpdateType::Major));
        assert_eq!(mixed.into_iter().min(), Some(UpdateType::Unknown));

        let mut sorted = mixed;
        sorted.sort();
        assert_eq!(
            sorted,
            [
                UpdateType::Unknown,
                UpdateType::Patch,
                UpdateType::Minor,
                UpdateType::Major,
            ]
        );
    }

    #[test]
    fn facet_counts_round_trip_through_json_in_ranked_order() {
        // Facets cross the server-function boundary as JSON, so enum keys must
        // survive as object keys and the label ranking must not be re-sorted.
        let facets = FacetCounts {
            checks: BTreeMap::from([(CheckStatus::Success, 3), (CheckStatus::Failure, 1)]),
            update_types: BTreeMap::from([(UpdateType::Minor, 4)]),
            labels: vec![
                LabelFacet {
                    label: "rust".to_owned(),
                    count: 4,
                },
                LabelFacet {
                    label: "go".to_owned(),
                    count: 2,
                },
                LabelFacet {
                    label: "dependencies".to_owned(),
                    count: 1,
                },
            ],
        };

        let json = serde_json::to_string(&facets).unwrap();
        assert!(json.contains(r#""success":3"#), "{json}");
        assert!(json.contains(r#""failure":1"#), "{json}");
        assert!(json.contains(r#""update_types":{"minor":4}"#), "{json}");
        assert!(
            json.contains(r#""labels":[{"label":"rust","count":4},{"label":"go","count":2}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<FacetCounts>(&json).unwrap(), facets);
    }

    #[test]
    fn facet_count_lookups_treat_an_absent_enum_key_as_zero() {
        let facets = FacetCounts {
            checks: BTreeMap::from([(CheckStatus::Success, 3)]),
            update_types: BTreeMap::from([(UpdateType::Minor, 4)]),
            labels: Vec::new(),
        };

        assert_eq!(facets.check_count(CheckStatus::Success), 3);
        assert_eq!(facets.check_count(CheckStatus::Pending), 0);
        assert_eq!(facets.update_type_count(UpdateType::Minor), 4);
        assert_eq!(facets.update_type_count(UpdateType::Major), 0);
    }

    #[test]
    fn rollup_uses_failure_pending_success_none_precedence() {
        assert_eq!(rollup_checks([]), CheckStatus::None);
        assert_eq!(rollup_checks([CheckSignal::Pass]), CheckStatus::Success);
        assert_eq!(
            rollup_checks([CheckSignal::Pass, CheckSignal::Pending]),
            CheckStatus::Pending
        );
        assert_eq!(
            rollup_checks([CheckSignal::Pending, CheckSignal::Fail]),
            CheckStatus::Failure
        );
    }

    #[test]
    fn all_documented_check_values_are_classified() {
        for value in ["success", "neutral", "skipped"] {
            assert_eq!(check_signal(None, Some(value)), Some(CheckSignal::Pass));
        }
        for value in [
            "failure",
            "timed_out",
            "action_required",
            "cancelled",
            "stale",
            "startup_failure",
        ] {
            assert_eq!(check_signal(None, Some(value)), Some(CheckSignal::Fail));
        }
        for value in [
            "queued",
            "in_progress",
            "waiting",
            "pending",
            "requested",
            "expected",
        ] {
            assert_eq!(check_signal(Some(value), None), Some(CheckSignal::Pending));
        }
        assert_eq!(status_signal("success"), Some(CheckSignal::Pass));
        for value in ["failure", "error"] {
            assert_eq!(status_signal(value), Some(CheckSignal::Fail));
        }
        for value in ["pending", "expected"] {
            assert_eq!(status_signal(value), Some(CheckSignal::Pending));
        }
    }

    #[test]
    fn all_documented_mergeable_states_are_classified() {
        for (value, expected) in [
            ("clean", Mergeable::Clean),
            ("dirty", Mergeable::Dirty),
            ("blocked", Mergeable::Blocked),
            ("behind", Mergeable::Behind),
            ("unstable", Mergeable::Unstable),
            ("draft", Mergeable::Draft),
            ("has_hooks", Mergeable::HasHooks),
            ("unknown", Mergeable::Unknown),
        ] {
            assert_eq!(Mergeable::from_github_state(value), expected, "{value}");
        }
    }

    #[test]
    fn undocumented_mergeable_states_fold_into_unknown() {
        for value in ["", "conflicting", "mergeable", "CLEAN", "some_future_state"] {
            assert_eq!(
                Mergeable::from_github_state(value),
                Mergeable::Unknown,
                "{value:?}"
            );
        }
    }

    #[test]
    fn only_dirty_counts_as_a_merge_conflict() {
        // GitHub reports a base/head conflict as `dirty`; every other state
        // (including `blocked` and `behind`) can be resolved without a rebase.
        for state in Mergeable::ALL {
            assert_eq!(
                state.is_conflicting(),
                state == Mergeable::Dirty,
                "{state:?}"
            );
        }
    }

    #[test]
    fn mergeable_display_is_the_github_vocabulary() {
        // The display form is what the store persists, so it must be exactly
        // the string GitHub emits for that state and must read back losslessly.
        assert_eq!(Mergeable::HasHooks.to_string(), "has_hooks");
        for state in Mergeable::ALL {
            assert_eq!(
                Mergeable::from_github_state(&state.to_string()),
                state,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_projection_is_stale_once_it_is_more_than_forty_five_minutes_old() {
        // The dashboard only trusts a row for 45 minutes after we last fetched
        // it (`synced_at`); the boundary itself is still fresh, one second
        // past it is stale, and a clock that runs behind never reports stale.
        let synced_at = 1_700_000_000;
        let record = PrRecord {
            id: "7#9".to_owned(),
            repository_id: 7,
            installation_id: 1,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            title: "Bump serde".to_owned(),
            html_url: "https://github.com/acme/api/pull/9".to_owned(),
            dependency: None,
            from_version: None,
            to_version: None,
            dependencies: Vec::new(),
            update_type: UpdateType::Unknown,
            head_sha: "abc123".to_owned(),
            check_status: CheckStatus::None,
            mergeable: Mergeable::Unknown,
            labels: Vec::new(),
            created_at: 0,
            updated_at: 0,
            synced_at,
        };

        assert!(!record.is_stale(synced_at));
        assert!(!record.is_stale(synced_at + 2_700));
        assert!(record.is_stale(synced_at + 2_701));
        assert!(!record.is_stale(synced_at - 1));
    }

    #[test]
    fn legacy_pr_record_json_deserializes_its_mergeable_field() {
        // Restate journals hold PrRecord snapshots written when `mergeable`
        // was `Option<String>`: a raw GitHub string, `null`, or (from before
        // the field existed at all) absent. None of them may poison a journal.
        fn record_with(mergeable: serde_json::Value) -> serde_json::Value {
            serde_json::json!({
                "id": "7#9",
                "repository_id": 7,
                "installation_id": 1,
                "owner": "acme",
                "repo": "api",
                "number": 9,
                "title": "Bump serde",
                "html_url": "https://github.com/acme/api/pull/9",
                "dependency": null,
                "from_version": null,
                "to_version": null,
                "dependencies": [],
                "update_type": "unknown",
                "head_sha": "abc123",
                "check_status": "none",
                "mergeable": mergeable,
                "labels": [],
                "created_at": 0,
                "updated_at": 0,
                "synced_at": 0
            })
        }
        for (raw, expected) in [
            (serde_json::json!("dirty"), Mergeable::Dirty),
            (serde_json::json!("some_future_state"), Mergeable::Unknown),
            (serde_json::json!(null), Mergeable::Unknown),
        ] {
            let record: PrRecord = serde_json::from_value(record_with(raw.clone()))
                .unwrap_or_else(|error| panic!("{raw}: {error}"));
            assert_eq!(record.mergeable, expected, "{raw}");
        }
        let mut absent = record_with(serde_json::Value::Null);
        absent.as_object_mut().unwrap().remove("mergeable");
        let record: PrRecord = serde_json::from_value(absent).unwrap();
        assert_eq!(record.mergeable, Mergeable::Unknown);
    }

    #[test]
    fn cursor_round_trips() {
        let cursor = PageCursor {
            updated_at: 123,
            id: "4#5".to_owned(),
        };
        assert_eq!(PageCursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    #[test]
    fn batch_ids_are_uuid_v7() {
        let id = new_batch_id();
        assert!(valid_batch_id(&id));
        assert!(!valid_batch_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!valid_batch_id("not-a-uuid"));
    }

    #[test]
    fn pr_target_url_defaults_for_existing_restate_data() {
        let target: PrTarget = serde_json::from_value(serde_json::json!({
            "repository_id": 7,
            "owner": "acme",
            "repo": "api",
            "number": 9,
            "expected_sha": "abc123",
            "title": "Bump serde"
        }))
        .unwrap();

        assert!(target.html_url.is_empty());
    }

    #[test]
    fn manual_sync_fields_default_for_existing_restate_data() {
        let request: SyncRequest = serde_json::from_value(serde_json::json!({
            "repository_id": 7,
            "owner": "acme",
            "repo": "api",
            "number": 9,
            "observed_sha": "abc123"
        }))
        .unwrap();
        assert!(!request.bypass_debounce);
        assert_eq!(request.completion_id, None);

        let state: PrState = serde_json::from_value(serde_json::json!({
            "snapshot": null,
            "history": [],
            "last_synced_at": 100,
            "sync_pending": false
        }))
        .unwrap();
        assert!(state.completed_sync_ids.is_empty());
    }

    #[test]
    fn webhook_events_journaled_with_the_retired_completion_id_still_deserialize() {
        // Dashboard refreshes used to travel as webhook events carrying `sync_completion_id`;
        // a `dispatch` journaled before they got their own service must still replay, and an
        // event forwarded today must not carry it.
        let event: WebhookEvent = serde_json::from_value(serde_json::json!({
            "event": "pull_request",
            "action": "synchronize",
            "installation_id": 42,
            "repository_id": 7,
            "owner": "acme",
            "repo": "api",
            "number": 9,
            "sha": "abc123",
            "pull_requests": [],
            "sync_completion_id": "sync-123"
        }))
        .unwrap();
        assert_eq!(
            event,
            WebhookEvent {
                event: "pull_request".to_owned(),
                action: Some("synchronize".to_owned()),
                installation_id: Some(42),
                repository_id: Some(7),
                owner: Some("acme".to_owned()),
                repo: Some("api".to_owned()),
                number: Some(9),
                sha: Some("abc123".to_owned()),
                pull_requests: Vec::new(),
            }
        );
        let serialized = serde_json::to_value(&event).unwrap();
        assert!(serialized.get("sync_completion_id").is_none());
    }

    #[test]
    fn sync_requests_journaled_with_the_retired_sha_hint_still_deserialize() {
        // `observed_sha` was never read; journal entries written before its removal must
        // still replay, and a request built today must not carry it.
        let request: SyncRequest = serde_json::from_value(serde_json::json!({
            "repository_id": 7,
            "owner": "acme",
            "repo": "api",
            "number": 9,
            "observed_sha": "abc123"
        }))
        .unwrap();
        assert_eq!(
            request,
            SyncRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                number: 9,
                bypass_debounce: false,
                completion_id: None,
            }
        );
        let serialized = serde_json::to_value(&request).unwrap();
        assert!(serialized.get("observed_sha").is_none());
    }

    #[test]
    fn rate_limits_are_recognised_regardless_of_status() {
        for status in [403, 429] {
            let response = GithubErrorResponse {
                status,
                message: "secondary rate limit".to_owned(),
                ..Default::default()
            };
            assert!(matches!(
                classify_github_error(&response, Operation::Comment, true, 1_000),
                Classification::RateLimited { .. }
            ));
        }
    }

    #[test]
    fn retry_after_sets_the_rate_limit_deadline_relative_to_now() {
        let response = GithubErrorResponse {
            status: 403,
            message: "slow down".to_owned(),
            retry_after_seconds: Some(30),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&response, Operation::Read, false, 1_000),
            Classification::RateLimited { until: 1_030 }
        );
    }

    #[test]
    fn a_primary_rate_limit_waits_for_the_advertised_reset() {
        let response = GithubErrorResponse {
            status: 403,
            message: "API rate limit exceeded for installation ID 1.".to_owned(),
            rate_limit_remaining: Some(0),
            rate_limit_reset: Some(4_600),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&response, Operation::Read, false, 1_000),
            Classification::RateLimited { until: 4_600 }
        );
    }

    #[test]
    fn a_secondary_rate_limit_without_headers_waits_one_minute() {
        // GitHub's guidance when Retry-After is absent is to wait at least a minute.
        let response = GithubErrorResponse {
            status: 403,
            message: "You have exceeded a secondary rate limit.".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&response, Operation::Comment, true, 1_000),
            Classification::RateLimited { until: 1_060 }
        );
    }

    #[test]
    fn server_errors_are_retryable_without_a_deadline() {
        let response = GithubErrorResponse {
            status: 503,
            message: "unavailable".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&response, Operation::Read, false, 1_000),
            Classification::Retryable
        );
    }

    #[test]
    fn merge_base_branch_405_is_retryable_but_method_405_is_rejected() {
        let transient = GithubErrorResponse {
            status: 405,
            message: "Base branch was modified. Review and try the merge again.".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&transient, Operation::Merge, true, 1_000),
            Classification::Retryable
        );

        let permanent = GithubErrorResponse {
            status: 405,
            message: "Merge method is not allowed".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&permanent, Operation::Merge, true, 1_000),
            Classification::Rejected(RejectReason::MergeMethodDisallowed)
        );
    }

    #[test]
    fn unknown_404_is_fatal_but_known_resource_is_gone() {
        let response = GithubErrorResponse {
            status: 404,
            message: "Not Found".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            classify_github_error(&response, Operation::Read, false, 1_000),
            Classification::Fatal
        );
        assert_eq!(
            classify_github_error(&response, Operation::Read, true, 1_000),
            Classification::Rejected(RejectReason::NotFound)
        );
    }
}
