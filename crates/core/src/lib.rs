use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
    time::Duration,
};

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
/// How many pull requests one bulk action may take. The dashboard resolves
/// "select all matching" to a page of this size, so it must fit in a page.
pub const MAX_BATCH_TARGETS: usize = 100;
const _: () = assert!(
    MAX_BATCH_TARGETS <= MAX_PAGE_SIZE as usize,
    "a batch's worth of rows must fit in one page"
);
/// How far back the dashboard's list of finished batches can be asked to go
/// in one read. Every batch is on record; this bounds one answer, not the
/// record.
pub const MAX_RECENT_BATCHES: u32 = 200;
/// How many of the ranked labels the dashboard's label facet shows.
pub const LABEL_FACET_LIMIT: usize = 8;
/// How many labels a dashboard row shows before folding the rest into a count.
pub const ROW_LABEL_LIMIT: usize = 2;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// A pull request a prune has removed from the read model whose object has not yet
/// been told: one row of the store's retirement outbox.
///
/// The prune writes it in the same transaction as the delete, so the keys survive a
/// step whose result Restate lost; the sweep drains them afterwards and sends each
/// its `closed`. `synced_before` is the fence the prune ran under — the instant the
/// sweep's listing started, or `None` for a purge, whose close is unconditional. It
/// is recorded, not recomputed when drained: an object synced since that instant was
/// reopened behind the sweep and keeps its state, however late the close arrives.
/// `id` grows with every row queued, so acknowledging up to one acknowledges exactly
/// what was read before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retirement {
    pub id: u64,
    pub key: PrKey,
    pub synced_before: Option<u64>,
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

    /// Whether the rollup passed. The dashboard's "green": a rendering hint
    /// read from the projection, not an authorization to merge, which only
    /// branch protection gives.
    pub fn is_green(self) -> bool {
        matches!(self, Self::Success)
    }
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

/// What the dashboard narrows the pull requests by. Every field is one the
/// dashboard's controls can set and its URL carries; the store applies them
/// all together.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFilter {
    pub query: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub update_types: Vec<UpdateType>,
    #[serde(default)]
    pub check_statuses: Vec<CheckStatus>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// One dependency by name, case-insensitively: a pull request updating it
    /// alone, or as one of a group.
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

/// One entry of the repository facet: a repository and how many of its open
/// pull requests the facet's scope (see [`FacetCounts`]) takes in. Every
/// repository the read model knows is listed, so a count of zero is possible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFacet {
    pub repository: RepoRecord,
    pub count: u64,
}

/// Sidebar facet counts, scoped to a [`PrFilter`].
///
/// Each facet counts the pull requests that match every part of the filter
/// except its own dimension: the check facet ignores `check_statuses`, the
/// update-type facet `update_types`, the label facet `labels`, and the
/// repository facet `repos`. So a facet's count says how many pull requests
/// choosing that value alone would show, given the other filters in force,
/// and the values already chosen keep their counts instead of hiding the
/// alternatives.
///
/// `checks` and `update_types` are closed sets, so they are keyed by their
/// enums: the UI walks `CheckStatus::ALL` / `UpdateType::ALL` and looks each
/// one up, treating an absent key as zero. Their map order carries no meaning.
///
/// `labels` is an open set ranked by popularity, so it is an ordered sequence:
/// descending count, ties broken by case-insensitive label name. Consumers
/// that truncate must keep this order rather than re-sorting by key.
///
/// `repositories` lists every repository, in owner then name order compared
/// case-insensitively, so the UI can group them by owner without re-sorting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacetCounts {
    pub checks: BTreeMap<CheckStatus, u64>,
    pub update_types: BTreeMap<UpdateType, u64>,
    pub labels: Vec<LabelFacet>,
    pub repositories: Vec<RepoFacet>,
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

/// One page of the dashboard's rows for a filter and cursor. `total` counts
/// every row the filter matches, not just this page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardPage {
    pub rows: Vec<PrRecord>,
    pub total: u64,
    pub next_cursor: Option<String>,
}

/// What the dashboard shows around the rows for a filter: the facet counts
/// scoped to it and when the read model last heard from GitHub. It does not
/// depend on the page cursor, so paging through a filter need not recompute
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSummary {
    pub facets: FacetCounts,
    /// The newest `synced_at` of any pull request in the read model, whatever
    /// the filter; absent until the first sync lands.
    pub last_synced_at: Option<u64>,
}

/// Where the read model stands: two counters the store moves as rows change,
/// cheap enough to poll. A dashboard acts on an answer only where it differs
/// from the one it last saw, and the two answer different questions: whether
/// anything it shows may have moved, and whether a sync has reached the pull
/// requests — which the repositories a sweep writes first cannot say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRevision {
    /// Moves whenever any row of the read model changes, however it changes.
    pub projection: u64,
    /// Moves whenever a pull request row changes, however it changes; a
    /// repository row changing on its own leaves it where it is.
    pub pull_requests: u64,
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
    /// `PUT /pulls/{n}/update-branch`: merges the base into the head under the App's
    /// identity, the alternative to a rebase when no user token is configured.
    UpdateBranch,
}

impl fmt::Display for BulkActionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::UpdateBranch => "update branch",
        })
    }
}

impl FromStr for BulkActionKind {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "merge" => Self::Merge,
            "rebase" => Self::Rebase,
            "update branch" => Self::UpdateBranch,
            _ => return Err(ParseEnumError(value.to_owned())),
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
    pub fn key(&self) -> PrKey {
        PrKey::new(self.repository_id, self.number)
    }
}

/// A target as the browser submits it: the pull request, and the head the
/// user saw when they decided. That is all the browser gets a say in. The
/// rest of a [`PrTarget`] — whose repository it is, what it is called, where
/// it links — the web API fills in from the projection, so a batch and the
/// record it leaves for audit carry the dashboard's word on the target, not
/// the client's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedTarget {
    pub repository_id: u64,
    pub number: u64,
    pub expected_sha: String,
}

impl SubmittedTarget {
    pub fn key(&self) -> PrKey {
        PrKey::new(self.repository_id, self.number)
    }
}

impl From<&PrTarget> for SubmittedTarget {
    fn from(target: &PrTarget) -> Self {
        Self {
            repository_id: target.repository_id,
            number: target.number,
            expected_sha: target.expected_sha.clone(),
        }
    }
}

/// What the browser gets back for a batch submission Restate took. The batch
/// runs over every target the projection vouched for; `left_out` names the
/// ones it could not — pull requests no longer in the dashboard, merged or
/// closed between the selection and the click — which the batch does not
/// carry. They are named by key alone, since the projection has nothing else
/// to say about them; the browser still holds what it asked for and is the
/// one to say which pull requests these were.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReceipt {
    pub left_out: Vec<PrKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkRequest {
    pub action: BulkActionKind,
    pub targets: Vec<PrTarget>,
    pub user_id: UserId,
}

impl BulkRequest {
    /// Whether this request may run as batch `batch_id`. The web API checks it before
    /// asking Restate, and the workflow checks it again on its way in, so a batch that
    /// reaches Restate by another route is held to the same rules.
    pub fn validate(&self, batch_id: &str) -> Result<(), InvalidBatch> {
        validate_batch(batch_id, self.targets.iter().map(PrTarget::key))
    }
}

/// Whether pull requests `keys` may run as batch `batch_id`: the id is a UUIDv7, the
/// count is within bounds, and no pull request is named twice. Takes the keys alone so
/// the web API can hold a submission to the rules before it resolves a single target.
pub fn validate_batch(
    batch_id: &str,
    keys: impl IntoIterator<Item = PrKey>,
) -> Result<(), InvalidBatch> {
    if !valid_batch_id(batch_id) {
        return Err(InvalidBatch::BatchId);
    }
    let keys: Vec<PrKey> = keys.into_iter().collect();
    if keys.is_empty() || keys.len() > MAX_BATCH_TARGETS {
        return Err(InvalidBatch::TargetCount);
    }
    let mut seen = BTreeSet::new();
    if keys.iter().any(|key| !seen.insert(key)) {
        return Err(InvalidBatch::DuplicateTargets);
    }
    Ok(())
}

/// Why a batch request cannot run. Each message is meant for the user who submitted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum InvalidBatch {
    #[error("batch id must be a UUIDv7")]
    BatchId,
    #[error("batch must contain between 1 and {MAX_BATCH_TARGETS} targets")]
    TargetCount,
    #[error("batch contains duplicate pull requests")]
    DuplicateTargets,
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
    StaleSha {
        expected: String,
        actual: String,
    },
    NotMergeable,
    MergeMethodDisallowed,
    Forbidden,
    NotFound,
    /// The deployment has no user identity to post `@dependabot` commands under: a
    /// rebase is refused before GitHub is asked anything. Merge and update branch
    /// run as the App and are unaffected.
    NoUserToken,
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
            Self::NoUserToken => {
                f.write_str("no GitHub user token is configured for @dependabot commands")
            }
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

/// Where a bulk action stands: one state per target and the running tally.
///
/// A target that fails terminally is recorded and the batch carries on, so the tally has
/// three terminal columns and the batch is complete once every target is in one of them.
/// The batch itself has no failure of its own: the reason a target failed lives on that
/// target. (State retained from before this held a batch-level `failure`; it is ignored.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchProgress {
    pub batch_id: String,
    pub action: BulkActionKind,
    pub targets: Vec<TargetProgress>,
    pub completed: bool,
    pub succeeded: u64,
    pub rejected: u64,
    #[serde(default)]
    pub failed: u64,
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
            failed: 0,
        }
    }
    /// The target's action has been sent.
    pub fn start(&mut self, key: &PrKey) {
        self.set_state(key, TargetProgressState::Running);
    }

    /// The target's action completed: GitHub did it, or said no for good.
    pub fn record(&mut self, key: &PrKey, outcome: ActionOutcome) {
        let state = match outcome {
            ActionOutcome::Succeeded { detail } => TargetProgressState::Succeeded { detail },
            ActionOutcome::Rejected { reason } => TargetProgressState::Rejected { reason },
        };
        self.set_state(key, state);
    }

    /// The target's action failed terminally without an outcome; `detail` says why.
    pub fn record_failure(&mut self, key: &PrKey, detail: impl Into<String>) {
        self.set_state(
            key,
            TargetProgressState::Failed {
                detail: detail.into(),
            },
        );
    }

    /// How many targets have reached a terminal state, whichever one.
    pub fn settled(&self) -> u64 {
        self.succeeded + self.rejected + self.failed
    }

    /// The targets GitHub, or the guard, said no to, each with the reason, in batch order.
    pub fn rejected_targets(&self) -> impl Iterator<Item = (&PrTarget, &RejectReason)> {
        self.targets.iter().filter_map(|item| match &item.state {
            TargetProgressState::Rejected { reason } => Some((&item.target, reason)),
            _ => None,
        })
    }

    /// Moves the target to `state` and keeps the tally in step with it. A key the batch
    /// does not contain changes nothing: the tally must only ever count targets. Nor does
    /// a target that has already settled: its verdict is final, and counting a second one
    /// would let the tally reach the target count while a target is still queued.
    fn set_state(&mut self, key: &PrKey, state: TargetProgressState) {
        let Some(target) = self
            .targets
            .iter_mut()
            .find(|target| target.target.key() == *key)
        else {
            return;
        };
        if target.state.outcome().is_some() {
            return;
        }
        match &state {
            TargetProgressState::Succeeded { .. } => self.succeeded += 1,
            TargetProgressState::Rejected { .. } => self.rejected += 1,
            TargetProgressState::Failed { .. } => self.failed += 1,
            TargetProgressState::Queued | TargetProgressState::Running => {}
        }
        target.state = state;
        self.completed = self.settled() == self.targets.len() as u64;
    }

    /// This batch as the projection keeps it once it has run: who asked for it, when it
    /// started and finished, the tally, and every target's verdict in batch order.
    /// `None` while any target is still queued or running: there is no record to keep
    /// of a batch that has not finished.
    pub fn completed_record(
        &self,
        requested_by: UserId,
        started_at: u64,
        completed_at: u64,
    ) -> Option<BatchRecord> {
        let targets = self
            .targets
            .iter()
            .map(|item| {
                Some(BatchTargetRecord {
                    repository_id: item.target.repository_id,
                    owner: item.target.owner.clone(),
                    repo: item.target.repo.clone(),
                    number: item.target.number,
                    title: item.target.title.clone(),
                    html_url: item.target.html_url.clone(),
                    outcome: item.state.outcome()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(BatchRecord {
            batch_id: self.batch_id.clone(),
            action: self.action,
            requested_by,
            started_at,
            completed_at,
            succeeded: self.succeeded,
            rejected: self.rejected,
            failed: self.failed,
            targets,
        })
    }
}

/// How one target's action ended, for good: GitHub did it, GitHub or the guard said no,
/// or the round trip failed terminally. [`TargetProgressState`] without the two states a
/// target passes through on the way here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetOutcome {
    Succeeded { detail: String },
    Rejected { reason: RejectReason },
    Failed { detail: String },
}

impl TargetProgressState {
    /// The verdict this state is, if it is one; `None` while the target is queued or
    /// running.
    pub fn outcome(&self) -> Option<TargetOutcome> {
        match self {
            Self::Queued | Self::Running => None,
            Self::Succeeded { detail } => Some(TargetOutcome::Succeeded {
                detail: detail.clone(),
            }),
            Self::Rejected { reason } => Some(TargetOutcome::Rejected {
                reason: reason.clone(),
            }),
            Self::Failed { detail } => Some(TargetOutcome::Failed {
                detail: detail.clone(),
            }),
        }
    }
}

impl From<TargetOutcome> for TargetProgressState {
    fn from(outcome: TargetOutcome) -> Self {
        match outcome {
            TargetOutcome::Succeeded { detail } => Self::Succeeded { detail },
            TargetOutcome::Rejected { reason } => Self::Rejected { reason },
            TargetOutcome::Failed { detail } => Self::Failed { detail },
        }
    }
}

/// One target of a finished batch as the projection keeps it. The pull request is
/// named in full, link included, because the row it came from may be gone by the time
/// anyone reads this: a merged pull request leaves the projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchTargetRecord {
    pub repository_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub html_url: String,
    pub outcome: TargetOutcome,
}

/// A finished bulk action as the projection keeps it, for the audit view: what was
/// asked, by whom, when it ran, how it went in all, and how each target went. Restate
/// forgets a workflow after its retention; this does not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    pub batch_id: String,
    pub action: BulkActionKind,
    pub requested_by: UserId,
    /// Unix seconds when the workflow started running the batch.
    pub started_at: u64,
    /// Unix seconds when the workflow found every target settled and finished the batch.
    pub completed_at: u64,
    pub succeeded: u64,
    pub rejected: u64,
    pub failed: u64,
    /// Every target in batch order, each with its verdict.
    pub targets: Vec<BatchTargetRecord>,
}

/// The record as the progress it was written from — the inverse of
/// [`BatchProgress::completed_record`]: every target settled by its verdict, the batch
/// complete, the tally as recorded. So a dashboard that reads a finished batch from the
/// projection shows it as it would have shown Restate's last word on it. The head each
/// target was sent against is not kept in the record, so the targets carry none;
/// nothing that reads a finished batch needs it.
impl From<BatchRecord> for BatchProgress {
    fn from(record: BatchRecord) -> Self {
        Self {
            batch_id: record.batch_id,
            action: record.action,
            targets: record
                .targets
                .into_iter()
                .map(|target| TargetProgress {
                    target: PrTarget {
                        repository_id: target.repository_id,
                        owner: target.owner,
                        repo: target.repo,
                        number: target.number,
                        expected_sha: String::new(),
                        title: target.title,
                        html_url: target.html_url,
                    },
                    state: target.outcome.into(),
                })
                .collect(),
            completed: true,
            succeeded: record.succeeded,
            rejected: record.rejected,
            failed: record.failed,
        }
    }
}

/// A bulk action the workflow is running, as the projection keeps it while it
/// runs: what was asked, by whom, when it started, and how many pull requests it
/// spans. Enough for the audit view to list the batch beside the finished ones
/// and for a dashboard that lost it to follow it again; where it stands is
/// Restate's word, read through the workflow's progress. Written as the
/// workflow's first step and taken away by the finished record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningBatch {
    pub batch_id: String,
    pub action: BulkActionKind,
    pub requested_by: UserId,
    /// Unix seconds when the workflow started running the batch.
    pub started_at: u64,
    /// How many pull requests the batch was asked to act on.
    pub target_count: u64,
}

/// The batches the audit view lists: the ones running and the ones most
/// recently finished, each newest first. Two lists rather than one, since a
/// running batch has no verdicts yet and is found in Restate, not read here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchList {
    pub running: Vec<RunningBatch>,
    pub finished: Vec<BatchRecord>,
}

/// What the projection holds of one batch, asked for by id: its finished
/// record, or its listing while it runs. A batch is one or the other, never
/// both — the record's write takes the listing away — and an id the
/// projection has never heard of is neither, which is a batch just queued
/// that the workflow has not listed yet, or no batch at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectedBatch {
    /// Listed as running by the workflow's first step; where it stands is Restate's
    /// word, read through the workflow's progress.
    Running(RunningBatch),
    /// Recorded as finished by the workflow's last step, targets and verdicts included.
    Finished(BatchRecord),
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

/// What this deployment can do, as `DashboardIngress.capabilities` answers the dashboard:
/// the settings the Restate service resolved at startup that decide which actions are on
/// offer. The service is the one process that holds the credentials, so it is the one
/// that can say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Whether a user identity is configured to post `@dependabot rebase` under. Without
    /// one the service rejects every rebase as [`RejectReason::NoUserToken`] and the
    /// dashboard withholds **Request rebase**; merge and update branch run as the App
    /// and are always on offer.
    pub rebase_enabled: bool,
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
        // The read that verified the target went through with the same credentials, so
        // GitHub is refusing this write on this pull request (branch protection, a
        // repository the installation can see but not push to), not the configuration.
        // A 401 is different: the client has already refreshed the token and retried
        // once, so bad credentials are bad for every target and stay fatal below.
        (403, Operation::Merge | Operation::UpdateBranch) if known_resource => {
            Classification::Rejected(RejectReason::Forbidden)
        }
        (405, Operation::Merge) if message.contains("merge method") => {
            Classification::Rejected(RejectReason::MergeMethodDisallowed)
        }
        (405 | 409 | 422, Operation::Merge) => Classification::Rejected(RejectReason::NotMergeable),
        (422, Operation::UpdateBranch) => Classification::Rejected(RejectReason::NotMergeable),
        // Refused before anything was read: the credentials or the installation's access
        // are wrong for every target, not just this one.
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
            repositories: vec![RepoFacet {
                repository: RepoRecord {
                    repository_id: 7,
                    installation_id: 1,
                    owner: "acme".to_owned(),
                    repo: "api".to_owned(),
                    merge_method: None,
                    synced_at: 0,
                },
                count: 5,
            }],
        };

        let json = serde_json::to_string(&facets).unwrap();
        assert!(json.contains(r#""success":3"#), "{json}");
        assert!(json.contains(r#""failure":1"#), "{json}");
        assert!(json.contains(r#""update_types":{"minor":4}"#), "{json}");
        assert!(
            json.contains(r#""labels":[{"label":"rust","count":4},{"label":"go","count":2}"#),
            "{json}"
        );
        assert!(
            json.contains(r#""repositories":[{"repository":{"repository_id":7"#),
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
            repositories: Vec::new(),
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

    /// "Green" is the dashboard's word for a passing rollup. Pending is not
    /// green yet, and no checks at all is not green either: nothing has
    /// vouched for the head.
    #[test]
    fn only_a_passing_rollup_is_green() {
        for status in CheckStatus::ALL {
            assert_eq!(
                status.is_green(),
                status == CheckStatus::Success,
                "{status:?}"
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
    fn a_bulk_action_kind_travels_in_snake_case_and_reads_as_plain_words() {
        // The wire form is what the dashboard posts to `BulkAction/run`; the display form
        // is what it puts in the confirm dialog title and the progress pill.
        let kinds = [
            (BulkActionKind::Merge, r#""merge""#, "merge"),
            (BulkActionKind::Rebase, r#""rebase""#, "rebase"),
            (
                BulkActionKind::UpdateBranch,
                r#""update_branch""#,
                "update branch",
            ),
        ];
        for (kind, wire, display) in kinds {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<BulkActionKind>(wire).unwrap(), kind);
            assert_eq!(kind.to_string(), display);
        }
    }

    #[test]
    fn batch_ids_are_uuid_v7() {
        let id = new_batch_id();
        assert!(valid_batch_id(&id));
        assert!(!valid_batch_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!valid_batch_id("not-a-uuid"));
    }

    fn bulk_request(targets: Vec<PrTarget>) -> BulkRequest {
        BulkRequest {
            action: BulkActionKind::Merge,
            targets,
            user_id: UserId::new("alice"),
        }
    }

    /// The one check both the web API and the workflow run before taking a batch: the id
    /// is a UUIDv7, the target count is within bounds, and no pull request is named twice.
    #[test]
    fn a_batch_request_is_checked_for_its_id_its_size_and_repeated_targets() {
        let full = (1..=MAX_BATCH_TARGETS as u64).map(batch_target).collect();
        assert_eq!(bulk_request(full).validate(&new_batch_id()), Ok(()));
        assert_eq!(
            bulk_request(vec![batch_target(1)]).validate("550e8400-e29b-41d4-a716-446655440000"),
            Err(InvalidBatch::BatchId)
        );
        assert_eq!(
            bulk_request(Vec::new()).validate(&new_batch_id()),
            Err(InvalidBatch::TargetCount)
        );
        let too_many = (1..=MAX_BATCH_TARGETS as u64 + 1)
            .map(batch_target)
            .collect();
        assert_eq!(
            bulk_request(too_many).validate(&new_batch_id()),
            Err(InvalidBatch::TargetCount)
        );
        assert_eq!(
            bulk_request(vec![batch_target(1), batch_target(2), batch_target(1)])
                .validate(&new_batch_id()),
            Err(InvalidBatch::DuplicateTargets)
        );
    }

    #[test]
    fn an_invalid_batch_says_what_is_wrong_with_it() {
        assert_eq!(
            InvalidBatch::BatchId.to_string(),
            "batch id must be a UUIDv7"
        );
        assert_eq!(
            InvalidBatch::TargetCount.to_string(),
            "batch must contain between 1 and 100 targets"
        );
        assert_eq!(
            InvalidBatch::DuplicateTargets.to_string(),
            "batch contains duplicate pull requests"
        );
    }

    /// A target for pull request `number` in repository 7.
    fn batch_target(number: u64) -> PrTarget {
        PrTarget {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number,
            expected_sha: "abc123".to_owned(),
            title: format!("Bump dependency {number}"),
            html_url: String::new(),
        }
    }

    #[test]
    fn a_batch_completes_once_every_target_has_settled_including_the_failed_ones() {
        let targets = [batch_target(1), batch_target(2), batch_target(3)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);

        progress.start(&targets[0].key());
        assert_eq!(progress.targets[0].state, TargetProgressState::Running);

        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        );
        progress.record_failure(
            &targets[1].key(),
            "GitHub mutation failed with HTTP 500: boom",
        );
        assert!(
            !progress.completed,
            "one target is still queued, so the batch is not over"
        );

        progress.record(
            &targets[2].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );

        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (1, 1, 1)
        );
        assert_eq!(
            progress.targets[1].state,
            TargetProgressState::Failed {
                detail: "GitHub mutation failed with HTTP 500: boom".to_owned()
            }
        );
        assert!(progress.completed);
    }

    /// A verdict is final. Were a second one counted, the tally would reach the target
    /// count before the last target had settled, and the batch would read as complete
    /// with a target still queued.
    #[test]
    fn a_target_already_settled_keeps_its_first_verdict_and_is_counted_once() {
        let targets = [batch_target(1), batch_target(2)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        );

        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        progress.record_failure(&targets[0].key(), "boom");
        progress.start(&targets[0].key());

        assert_eq!(
            progress.targets[0].state,
            TargetProgressState::Succeeded {
                detail: "merged".to_owned()
            }
        );
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (1, 0, 0)
        );
        assert!(
            !progress.completed,
            "the second target is still queued; a double count must not finish the batch"
        );
    }

    #[test]
    fn an_outcome_for_a_pull_request_outside_the_batch_changes_nothing() {
        let targets = [batch_target(1)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);

        progress.record_failure(&batch_target(99).key(), "boom");

        assert_eq!(progress.settled(), 0);
        assert!(
            !progress.completed,
            "a stray outcome must not count towards the batch's own targets"
        );
    }

    #[test]
    fn the_rejected_targets_are_listed_with_their_reasons_and_nothing_else_is() {
        let targets = [batch_target(1), batch_target(2), batch_target(3)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        progress.record_failure(&targets[1].key(), "boom");
        progress.record(
            &targets[2].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotFound,
            },
        );

        let rejected = progress
            .rejected_targets()
            .map(|(target, reason)| (target.number, reason.clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            rejected,
            [(1, RejectReason::Forbidden), (3, RejectReason::NotFound)]
        );
    }

    /// The record the projection keeps is the finished batch: who asked, when it ran,
    /// the tally, and each target's verdict in batch order. Until the last target has
    /// settled there is no record to keep.
    #[test]
    fn a_finished_batch_becomes_a_record_with_every_targets_verdict_and_not_before() {
        let targets = [batch_target(1), batch_target(2), batch_target(3)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        let requester = UserId::new("alice");
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        assert_eq!(
            progress.completed_record(requester.clone(), 100, 160),
            None,
            "one target is still queued"
        );

        progress.record_failure(&targets[2].key(), "boom");

        assert_eq!(
            progress.completed_record(requester, 100, 160),
            Some(BatchRecord {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                started_at: 100,
                completed_at: 160,
                succeeded: 1,
                rejected: 1,
                failed: 1,
                targets: vec![
                    BatchTargetRecord {
                        repository_id: 7,
                        owner: "acme".to_owned(),
                        repo: "api".to_owned(),
                        number: 1,
                        title: "Bump dependency 1".to_owned(),
                        html_url: String::new(),
                        outcome: TargetOutcome::Succeeded {
                            detail: "merged".to_owned()
                        },
                    },
                    BatchTargetRecord {
                        repository_id: 7,
                        owner: "acme".to_owned(),
                        repo: "api".to_owned(),
                        number: 2,
                        title: "Bump dependency 2".to_owned(),
                        html_url: String::new(),
                        outcome: TargetOutcome::Rejected {
                            reason: RejectReason::Forbidden
                        },
                    },
                    BatchTargetRecord {
                        repository_id: 7,
                        owner: "acme".to_owned(),
                        repo: "api".to_owned(),
                        number: 3,
                        title: "Bump dependency 3".to_owned(),
                        html_url: String::new(),
                        outcome: TargetOutcome::Failed {
                            detail: "boom".to_owned()
                        },
                    },
                ],
            })
        );
    }

    /// The store keeps the kind in its display form, as it keeps every enum, so the
    /// display form must read back.
    #[test]
    fn a_bulk_action_kind_reads_back_from_its_display_form() {
        for kind in [
            BulkActionKind::Merge,
            BulkActionKind::Rebase,
            BulkActionKind::UpdateBranch,
        ] {
            assert_eq!(kind.to_string().parse::<BulkActionKind>().unwrap(), kind);
        }
        assert!("update_branch".parse::<BulkActionKind>().is_err());
    }

    #[test]
    fn batch_progress_retained_before_failed_targets_were_counted_still_deserializes() {
        // Workflow state is retained for seven days; progress written before the
        // `failed` counter existed carried a batch-level `failure` instead.
        let progress: BatchProgress = serde_json::from_value(serde_json::json!({
            "batch_id": "batch-1",
            "action": "merge",
            "targets": [],
            "completed": true,
            "succeeded": 2,
            "rejected": 0,
            "failure": "Terminal error [500]: GitHub mutation failed"
        }))
        .unwrap();

        assert_eq!(progress.failed, 0);
        assert!(progress.completed);
        let serialized = serde_json::to_value(&progress).unwrap();
        assert!(serialized.get("failure").is_none());
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

    #[test]
    fn a_write_refused_on_a_pull_request_just_read_is_that_targets_rejection() {
        // The read that verified the target went through with the same credentials, so
        // the refusal is about this pull request, not the configuration: one merge
        // forbidden by branch protection must not fail the other ninety-nine.
        let response = GithubErrorResponse {
            status: 403,
            message: "Resource not accessible by integration".to_owned(),
            ..Default::default()
        };
        for operation in [Operation::Merge, Operation::UpdateBranch] {
            assert_eq!(
                classify_github_error(&response, operation, true, 1_000),
                Classification::Rejected(RejectReason::Forbidden),
                "403 on {operation} of a known pull request"
            );
        }
    }

    #[test]
    fn bad_credentials_are_a_configuration_failure_even_on_a_pull_request_just_read() {
        // The client has already refreshed the token and retried once before a 401 gets
        // here, so the fresh token was refused too: the credentials are wrong for every
        // target, and the one just read is no exception.
        let response = GithubErrorResponse {
            status: 401,
            message: "Bad credentials".to_owned(),
            ..Default::default()
        };
        for operation in [Operation::Merge, Operation::UpdateBranch] {
            assert_eq!(
                classify_github_error(&response, operation, true, 1_000),
                Classification::Fatal,
                "401 on {operation} of a known pull request"
            );
        }
    }

    #[test]
    fn a_write_refused_before_anything_was_read_is_a_configuration_failure() {
        for status in [401, 403] {
            let response = GithubErrorResponse {
                status,
                message: "Bad credentials".to_owned(),
                ..Default::default()
            };
            for operation in [Operation::Merge, Operation::UpdateBranch, Operation::Read] {
                assert_eq!(
                    classify_github_error(&response, operation, false, 1_000),
                    Classification::Fatal,
                    "{status} on {operation} with nothing verified"
                );
            }
        }
    }
}
