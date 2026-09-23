use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

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

/// The names Restate registers this deployment's handlers under.
///
/// The Restate service declares them — `#[restate_sdk::service]` and its siblings take a
/// service's name from the type and a handler's from the method — and the web edge
/// addresses them over the ingress. That makes one contract held in two binaries, and
/// nothing but a matching string joins them: an invocation sent to a name Restate does
/// not have is refused by the ingress, not by a compiler. Naming them here gives both
/// sides the same word to compile against — the edge builds every ingress path from
/// these (`apps/web/src/server/restate.rs`), and each service's discovery test asserts
/// what it registered against them, so a rename that misses one end fails a test here
/// rather than a request in production.
///
/// Only what the edge addresses is named: the handlers reachable through the ingress
/// (spec §3b). A private handler is one service's business and stays spelled where it is
/// declared. A keyed invocation takes the key between the two names —
/// `BulkAction/{batch_id}/run` — so a service and its handlers are named apart rather
/// than as one path; how the key is encoded into the path is the edge's business.
pub mod restate {
    /// The workflow one batch of bulk actions runs as, keyed by the batch id.
    pub const BULK_ACTION: &str = "BulkAction";
    /// Runs the batch: the submission the dashboard enqueues under the batch id.
    pub const BULK_ACTION_RUN: &str = "run";
    /// Where the batch stands, as the workflow holds it: the shared read the progress
    /// pill polls.
    pub const BULK_ACTION_PROGRESS: &str = "progress";

    /// The virtual object that owns one pull request, keyed by its [`crate::PrKey`].
    pub const PULL_REQUEST: &str = "PullRequest";
    /// The durable state the object holds: the shared read the detail drawer polls, and
    /// the object's one public handler.
    pub const PULL_REQUEST_STATUS: &str = "status";

    /// The service the dashboard asks for what this deployment can do, and for the
    /// refreshes it wants now rather than at the next sweep.
    pub const DASHBOARD_INGRESS: &str = "DashboardIngress";
    /// What this deployment can do, as the service resolved it at startup.
    pub const DASHBOARD_CAPABILITIES: &str = "capabilities";
    /// Reconciles the whole installation now: the dashboard's global **Sync**.
    pub const DASHBOARD_SYNC_INSTALLATION: &str = "sync_installation";
    /// Refreshes one pull request now: the drawer's **Sync**.
    pub const DASHBOARD_SYNC_PULL_REQUEST: &str = "sync_pull_request";

    /// The service verified GitHub deliveries are routed by.
    pub const WEBHOOK_INGRESS: &str = "WebhookIngress";
    /// Routes one verified delivery to the object that owns it: the sole entry point the
    /// webhook edge has.
    pub const WEBHOOK_DISPATCH: &str = "dispatch";
}

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
///
/// The `Display` form is a persisted token, not a label you may reword: the
/// store writes it to `pull_requests.update_type` and reads it back with
/// `FromStr`, the dashboard puts it in the `type=` query parameter of every
/// shareable link, and the UI shows it as the chip text. Renaming a variant
/// orphans stored rows and breaks links people have already sent.
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
/// `BTreeMap`; it says nothing about severity. Severity is the GitHub client's
/// to decide, in `dependaboard_github`'s rollup.
///
/// The `Display` form is a persisted token, not a label you may reword: the
/// store writes it to `pull_requests.check_status` and reads it back with
/// `FromStr`, the dashboard puts it in the `check=` query parameter of every
/// shareable link, and the UI shows it as the filter chip text. Renaming a
/// variant orphans stored rows and breaks links people have already sent.
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

/// One pull request as the projection holds it, and as Restate journals it.
///
/// It carries no installation of its own: whose a pull request is, is its
/// repository's row's to say (`RepoRecord::installation_id`), and the store
/// answers with the repository the row hangs off rather than with anything a
/// caller wrote here. `id` is `{repository_id}#{number}`, which the store
/// derives when it writes the row and [`PrRecord::key`] derives when it reads
/// one, so neither takes the field's word for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRecord {
    pub id: String,
    pub repository_id: u64,
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

/// The `Display` form is a persisted token, not a label you may reword: the
/// store writes it to `repositories.merge_method` and reads it back with
/// `FromStr`, and the GitHub client sends the same string as the
/// `merge_method` field of `PUT /pulls/{n}/merge`, where GitHub's own
/// vocabulary fixes it.
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

/// This enum carries two deliberately different string vocabularies; do not
/// unify them. Serde's is `snake_case` (`update_branch`) and is what the wire
/// format uses. `Display`/`FromStr` spell the same variant `update branch`,
/// **with a space**, and that is the form persisted in `batches.action` (see
/// spec.md §4's schema). Re-spelling `Display` to match serde would make every
/// batch row already on disk unreadable, so the space is load-bearing;
/// `a_bulk_action_kind_reads_back_from_its_display_form` pins it.
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
    /// The batch this one retries the rejected targets of, when it was queued from a
    /// finished batch's **Retry rejected**: the id of that batch. Every target here was
    /// rejected there, so the record of the one names the record of the other.
    #[serde(default)]
    pub retried_from: Option<String>,
}

impl BulkRequest {
    /// Whether this request may run as batch `batch_id`. The web API checks it before
    /// asking Restate, and the workflow checks it again on its way in, so a batch that
    /// reaches Restate by another route is held to the same rules.
    pub fn validate(&self, batch_id: &str) -> Result<(), InvalidBatch> {
        validate_batch(
            batch_id,
            self.retried_from.as_deref(),
            self.targets.iter().map(PrTarget::key),
        )
    }
}

/// Whether pull requests `keys` may run as batch `batch_id`, retrying `retried_from` if
/// it names a batch: the id is a UUIDv7, so is the retried batch's when given, the count
/// is within bounds, and no pull request is named twice. Takes the keys alone so the web
/// API can hold a submission to the rules before it resolves a single target. The retried
/// batch is not looked up: the link is the browser's word, as the targets are, and a
/// record that names a batch the projection never kept is a dangling link, not a lost
/// record.
pub fn validate_batch(
    batch_id: &str,
    retried_from: Option<&str>,
    keys: impl IntoIterator<Item = PrKey>,
) -> Result<(), InvalidBatch> {
    if !valid_batch_id(batch_id) {
        return Err(InvalidBatch::BatchId);
    }
    if retried_from.is_some_and(|retried| !valid_batch_id(retried)) {
        return Err(InvalidBatch::RetriedFrom);
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
    #[error("the batch retried must be named by a UUIDv7")]
    RetriedFrom,
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

/// How one target's action ended once GitHub had its say: done, or refused for good.
///
/// A merge that landed carries the commit it made — GitHub's answer names it, and so
/// does the pull request once merged — as `merge_sha`; for a squash or rebase merge the
/// commit is not the head that was merged, so the record could not work it out later. A
/// rebase comment and a branch update produce nothing to name here: the one is a
/// request Dependabot carries out in its own time, the other an update GitHub only
/// accepts and finishes after it has answered. `None` for those, and for a merge
/// journaled before the commit was kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Succeeded {
        detail: String,
        #[serde(default)]
        merge_sha: Option<String>,
    },
    Rejected {
        reason: RejectReason,
    },
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

/// The first seven characters of `value`, how a commit is named in passing; `value` whole
/// when it is shorter.
pub fn short_sha(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetProgressState {
    Queued,
    Running,
    /// See [`ActionOutcome::Succeeded`] for `merge_sha`.
    Succeeded {
        detail: String,
        #[serde(default)]
        merge_sha: Option<String>,
    },
    Rejected {
        reason: RejectReason,
    },
    Failed {
        detail: String,
    },
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
            ActionOutcome::Succeeded { detail, merge_sha } => {
                TargetProgressState::Succeeded { detail, merge_sha }
            }
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

    /// This batch as the projection keeps it once it has run: the installation it ran
    /// for, who asked for it, which batch it retried if any, when it started and
    /// finished, the tally, and every target's verdict in batch order with the head it
    /// was sent against. `None` while any target is still queued or running: there is no
    /// record to keep of a batch that has not finished.
    pub fn completed_record(
        &self,
        installation_id: u64,
        requested_by: UserId,
        retried_from: Option<String>,
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
                    head_sha: Some(item.target.expected_sha.clone()),
                    outcome: item.state.outcome()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(BatchRecord {
            batch_id: self.batch_id.clone(),
            installation_id,
            action: self.action,
            requested_by,
            retried_from,
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
    /// See [`ActionOutcome::Succeeded`] for `merge_sha`.
    Succeeded {
        detail: String,
        #[serde(default)]
        merge_sha: Option<String>,
    },
    Rejected {
        reason: RejectReason,
    },
    Failed {
        detail: String,
    },
}

impl TargetProgressState {
    /// The commit the target's merge made, once it has: see
    /// [`ActionOutcome::Succeeded`]. `None` in every other state, and for a merge whose
    /// commit is unknown.
    pub fn merge_sha(&self) -> Option<&str> {
        match self {
            Self::Succeeded { merge_sha, .. } => merge_sha.as_deref(),
            Self::Queued | Self::Running | Self::Rejected { .. } | Self::Failed { .. } => None,
        }
    }

    /// The verdict this state is, if it is one; `None` while the target is queued or
    /// running.
    pub fn outcome(&self) -> Option<TargetOutcome> {
        match self {
            Self::Queued | Self::Running => None,
            Self::Succeeded { detail, merge_sha } => Some(TargetOutcome::Succeeded {
                detail: detail.clone(),
                merge_sha: merge_sha.clone(),
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
            TargetOutcome::Succeeded { detail, merge_sha } => Self::Succeeded { detail, merge_sha },
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
    /// The head the target was sent against: the one the user saw, which the guard
    /// checked the pull request still had before anything was done to it. Says what a
    /// merge was a merge *of*, where the merge commit says what it made — for a squash or
    /// rebase merge the two are different commits — and what a rejected or failed
    /// attempt was over. `None` on a record from before the head was kept.
    #[serde(default)]
    pub head_sha: Option<String>,
    pub outcome: TargetOutcome,
}

/// A finished bulk action as the projection keeps it, for the audit view: what was
/// asked, by whom, when it ran, how it went in all, and how each target went. Restate
/// forgets a workflow after its retention; this does not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    pub batch_id: String,
    /// The installation the batch ran for, stamped by the workflow that ran it. The
    /// store answers a batch read only within one installation, so two deployments
    /// sharing a store do not read each other's batches.
    pub installation_id: u64,
    pub action: BulkActionKind,
    pub requested_by: UserId,
    /// The batch this one was queued to retry the rejected targets of, by id, when it
    /// was; see [`BulkRequest::retried_from`]. The audit view links the two entries
    /// each way. `None` for a batch confirmed from the table, and on a record from
    /// before the link was kept.
    #[serde(default)]
    pub retried_from: Option<String>,
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
/// complete, the tally as recorded, each target at the head it was sent against. So a
/// dashboard that reads a finished batch from the projection shows it as it would have
/// shown Restate's last word on it. A record from before the heads were kept has none
/// to give, and its targets carry an empty one.
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
                        expected_sha: target.head_sha.unwrap_or_default(),
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
    /// See [`BatchRecord::installation_id`]: the same stamp, from the moment the batch
    /// is listed.
    pub installation_id: u64,
    pub action: BulkActionKind,
    pub requested_by: UserId,
    /// See [`BatchRecord::retried_from`]: the same link, from the moment the batch is
    /// listed, so the batch it retries can say it is being retried while this one runs.
    #[serde(default)]
    pub retried_from: Option<String>,
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

/// How far back the drawer's action trail reads. The history is durable state
/// a Restate object carries for as long as the pull request is open, so it is
/// bounded rather than appended to forever; what the bound answers is how much
/// of a pull request's story a person can still see.
const RETAINED_HISTORY: usize = 20;

/// How many manual syncs can be waiting to be acknowledged at once. A sync the
/// dashboard asks for carries a completion id, which the object records here
/// and the page polls for; an id that has fallen out of this window is a sync
/// the page can no longer be told completed. It is the same number as
/// [`RETAINED_HISTORY`] and not the same rule — one bounds what is read, the
/// other bounds what is still being waited on — so a change to the poll window
/// must not quietly shorten the trail.
const RETAINED_SYNC_IDS: usize = 20;

impl PrState {
    pub fn push_history(&mut self, entry: ActionLog) {
        self.history.push(entry);
        if self.history.len() > RETAINED_HISTORY {
            self.history.drain(..self.history.len() - RETAINED_HISTORY);
        }
    }

    pub fn complete_sync(&mut self, completion_id: String) {
        if !self.completed_sync_ids.contains(&completion_id) {
            self.completed_sync_ids.push(completion_id);
        }
        if self.completed_sync_ids.len() > RETAINED_SYNC_IDS {
            self.completed_sync_ids
                .drain(..self.completed_sync_ids.len() - RETAINED_SYNC_IDS);
        }
    }
}

/// The kind of GitHub delivery this deployment routes, named once for both ends.
///
/// The web edge reduces a verified delivery to a [`WebhookEvent`] and the Restate service
/// dispatches on it, so which kinds are routed is a contract held in two binaries. It used
/// to travel as a `String` matched against literals at both ends, with nothing but a
/// comment joining the two tables; naming the kinds makes the dispatcher's match
/// exhaustive, so a kind added here that it does not handle fails to compile rather than
/// being silently dropped at run time.
///
/// Hand-rolled rather than `octoevents::EventKind`, which is what the edge already has:
/// GitHub's vocabulary deliberately does not live in this crate (it left in "move what
/// GitHub means out of the shared contract crate"), and this crate compiles to wasm for the
/// browser bundle while `octoevents` is gated to the server build. Six names cost less than
/// that dependency. These are only the kinds we route; the App subscribes to what GitHub
/// sends, which is a longer list the edge reads with `EventKind`.
///
/// [`Other`](Self::Other) is what keeps the two binaries deployable apart. They roll out
/// separately, so the edge may forward a kind the dispatcher does not name yet; it decodes
/// into `Other` and is ignored, as it was when the field was a `String`, instead of failing
/// the invocation at deserialization. It costs nothing of the compile-time win, because a
/// kind we mean to route still needs a variant of its own.
///
/// The wire form is the word GitHub put in `X-GitHub-Event`, unchanged: `WebhookEvent`
/// crosses the Restate ingress as JSON and is durably journaled, so every spelling that
/// has ever been sent must still decode.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeliveryKind {
    PullRequest,
    CheckSuite,
    CheckRun,
    Status,
    Installation,
    InstallationRepositories,
    /// A kind this deployment does not route, kept as GitHub spelled it so the dispatch's
    /// log line still names the delivery it ignored.
    Other(String),
}

impl DeliveryKind {
    /// The word GitHub sends in `X-GitHub-Event`, which is also the wire form.
    pub fn as_str(&self) -> &str {
        match self {
            Self::PullRequest => "pull_request",
            Self::CheckSuite => "check_suite",
            Self::CheckRun => "check_run",
            Self::Status => "status",
            Self::Installation => "installation",
            Self::InstallationRepositories => "installation_repositories",
            Self::Other(kind) => kind,
        }
    }

    /// The one table of routed kinds; `None` is a kind that becomes [`Self::Other`].
    fn routed(value: &str) -> Option<Self> {
        Some(match value {
            "pull_request" => Self::PullRequest,
            "check_suite" => Self::CheckSuite,
            "check_run" => Self::CheckRun,
            "status" => Self::Status,
            "installation" => Self::Installation,
            "installation_repositories" => Self::InstallationRepositories,
            _ => return None,
        })
    }
}

impl From<&str> for DeliveryKind {
    fn from(value: &str) -> Self {
        Self::routed(value).unwrap_or_else(|| Self::Other(value.to_owned()))
    }
}

impl From<String> for DeliveryKind {
    fn from(value: String) -> Self {
        Self::routed(&value).unwrap_or(Self::Other(value))
    }
}

impl fmt::Display for DeliveryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for DeliveryKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Hand-written because `#[serde(other)]` only applies to unit variants, and the fallback
/// has to keep the word it did not recognise.
impl<'de> Deserialize<'de> for DeliveryKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub event: DeliveryKind,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_key_round_trips() {
        let key = PrKey::new(42, 7);
        assert_eq!(key.to_string().parse::<PrKey>().unwrap(), key);
    }

    /// A key arrives as text from places we do not control — a Restate object's
    /// key, a shared or hand-edited link — so every shape that is not
    /// `{repository_id}#{number}` has to be refused rather than read as some
    /// nearby key. A bare number is the one worth naming: it is what dropping
    /// the separator from a link leaves, and it must not be taken as a
    /// repository with pull request zero.
    #[test]
    fn anything_that_is_not_a_repository_and_a_number_is_not_a_pull_request_key() {
        for value in [
            "",                       // nothing at all
            "123",                    // no separator: a repository, or a number, but not both
            "4-5",                    // the wrong separator
            "#",                      // the separator alone
            "7#",                     // no number
            "#7",                     // no repository
            "acme#7",                 // a repository by name, not by id
            "7#seven",                // a number that is not one
            "7#-1",                   // and one that is signed
            " 7#1",                   // padded
            "7#1#2",                  // two separators: the number is "1#2"
            "7#1.0",                  // a number that is not an integer
            "18446744073709551616#1", // one past what a u64 holds
        ] {
            let error = value
                .parse::<PrKey>()
                .expect_err("this is not a pull request key");
            assert_eq!(
                error.to_string(),
                format!("invalid pull request key: {value}"),
                "the refusal names the whole value it was given, not the part that failed"
            );
        }
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

    /// The third party to the agreement [`UpdateType`] documents between
    /// `.max()`, sorting and this, and the only one that has to answer for an
    /// empty group: a grouped pull request is chipped with the worst update in
    /// it, and one whose dependencies we never parsed is `Unknown` rather than
    /// the worst of nothing.
    #[test]
    fn the_highest_update_type_is_the_worst_in_the_group_and_unknown_for_none() {
        fn bump(update_type: UpdateType) -> DependencyUpdate {
            DependencyUpdate {
                name: format!("dep-{update_type}"),
                from_version: Some("1.0.0".to_owned()),
                to_version: Some("2.0.0".to_owned()),
                update_type,
            }
        }

        assert_eq!(highest_update_type(&[]), UpdateType::Unknown);

        for only in UpdateType::ALL {
            assert_eq!(highest_update_type(&[bump(only)]), only, "{only:?}");
        }

        assert_eq!(
            highest_update_type(&[
                bump(UpdateType::Patch),
                bump(UpdateType::Major),
                bump(UpdateType::Minor),
            ]),
            UpdateType::Major
        );
        assert_eq!(
            highest_update_type(&[
                bump(UpdateType::Major),
                bump(UpdateType::Minor),
                bump(UpdateType::Patch),
            ]),
            UpdateType::Major,
            "which update is worst cannot depend on where it sits in the group"
        );
        assert_eq!(
            highest_update_type(&[bump(UpdateType::Unknown), bump(UpdateType::Patch)]),
            UpdateType::Patch,
            "an unparsed sibling must not outrank one whose severity we know"
        );

        // The agreement itself, on every pair rather than the few above:
        // sorting a group and taking its last is the same verdict.
        for first in UpdateType::ALL {
            for second in UpdateType::ALL {
                let mut ranked = [first, second];
                ranked.sort();
                assert_eq!(
                    highest_update_type(&[bump(first), bump(second)]),
                    ranked[1],
                    "{first:?} with {second:?}"
                );
            }
        }
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

    /// The display form is on disk (`pull_requests.update_type`) and in the
    /// `type=` parameter of every shared link, so the literals are pinned here
    /// rather than derived: a round trip alone would stay green through a
    /// rename of both halves.
    #[test]
    fn an_update_type_reads_back_from_its_display_form() {
        assert_eq!(UpdateType::Major.to_string(), "major");
        assert_eq!(UpdateType::Minor.to_string(), "minor");
        assert_eq!(UpdateType::Patch.to_string(), "patch");
        assert_eq!(UpdateType::Unknown.to_string(), "unknown");
        for update_type in UpdateType::ALL {
            assert_eq!(
                update_type.to_string().parse::<UpdateType>().unwrap(),
                update_type,
                "{update_type:?}"
            );
        }
    }

    /// As above, for `pull_requests.check_status` and the `check=` parameter.
    #[test]
    fn a_check_status_reads_back_from_its_display_form() {
        assert_eq!(CheckStatus::Success.to_string(), "success");
        assert_eq!(CheckStatus::Failure.to_string(), "failure");
        assert_eq!(CheckStatus::Pending.to_string(), "pending");
        assert_eq!(CheckStatus::None.to_string(), "none");
        for status in CheckStatus::ALL {
            assert_eq!(
                status.to_string().parse::<CheckStatus>().unwrap(),
                status,
                "{status:?}"
            );
        }
    }

    /// The display form is on disk (`repositories.merge_method`) and is also
    /// the string GitHub's merge endpoint expects, so these literals are
    /// GitHub's, not ours to rename.
    #[test]
    fn a_merge_method_reads_back_from_its_display_form() {
        assert_eq!(MergeMethod::Merge.to_string(), "merge");
        assert_eq!(MergeMethod::Squash.to_string(), "squash");
        assert_eq!(MergeMethod::Rebase.to_string(), "rebase");
        for method in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
            assert_eq!(
                method.to_string().parse::<MergeMethod>().unwrap(),
                method,
                "{method:?}"
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

    /// The same journals hold snapshots written when a `PrRecord` carried its
    /// repository's `installation_id` of its own. The field is gone — the
    /// store reads the installation from the repository row the pull request
    /// hangs off — and a record that still carries one must read as a record
    /// without it, not as a snapshot Restate can no longer replay.
    #[test]
    fn a_journaled_snapshot_that_still_carries_an_installation_deserializes_without_one() {
        let mut journaled = serde_json::json!({
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
            "mergeable": "clean",
            "labels": [],
            "created_at": 0,
            "updated_at": 0,
            "synced_at": 0
        });
        let record: PrRecord = serde_json::from_value(journaled.clone()).unwrap();

        assert_eq!(record.key(), PrKey::new(7, 9));
        journaled.as_object_mut().unwrap().remove("installation_id");
        assert_eq!(
            record,
            serde_json::from_value(journaled).unwrap(),
            "the key the old snapshot carried is ignored, not read into anything"
        );
    }

    #[test]
    fn cursor_round_trips() {
        let cursor = PageCursor {
            updated_at: 123,
            id: "4#5".to_owned(),
        };
        assert_eq!(PageCursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    /// A cursor travels in a URL, which means people share it, truncate it and
    /// edit it by hand. Nothing but a cursor we wrote may decode: whether
    /// base64 turns it away or the JSON inside does, the answer is the one
    /// refusal the web edge demotes into an invalid request, never a page read
    /// from somewhere the caller did not ask for.
    #[test]
    fn a_page_cursor_we_did_not_write_is_refused_rather_than_guessed_at() {
        let malformed = [
            // `after=` left empty
            String::new(),
            // outside base64's alphabet
            "not base64!".to_owned(),
            // a length base64 cannot hold
            "aaaaa".to_owned(),
            // base64, of something that is not JSON
            URL_SAFE_NO_PAD.encode("4#5"),
            // JSON, cut short
            URL_SAFE_NO_PAD.encode("{"),
            // no `id`
            URL_SAFE_NO_PAD.encode(r#"{"updated_at":123}"#),
            // no `updated_at`
            URL_SAFE_NO_PAD.encode(r#"{"id":"4#5"}"#),
            // an instant as text
            URL_SAFE_NO_PAD.encode(r#"{"updated_at":"123","id":"4#5"}"#),
            // an instant before the epoch
            URL_SAFE_NO_PAD.encode(r#"{"updated_at":-1,"id":"4#5"}"#),
            // the fields unnamed
            URL_SAFE_NO_PAD.encode(r#"["123","4#5"]"#),
        ];

        for value in malformed {
            let error = PageCursor::decode(&value)
                .expect_err("only a cursor this crate encoded may decode");
            assert!(
                matches!(error, CursorError::Invalid),
                "{value:?}: {error:?}"
            );
            assert_eq!(error.to_string(), "invalid page cursor");
        }
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
            retried_from: None,
        }
    }

    /// The one check both the web API and the workflow run before taking a batch: the id
    /// is a UUIDv7, the target count is within bounds, no pull request is named twice, and
    /// the batch it retries, if it names one, is a batch id too.
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
        let retry = BulkRequest {
            retried_from: Some(new_batch_id()),
            ..bulk_request(vec![batch_target(1)])
        };
        assert_eq!(retry.validate(&new_batch_id()), Ok(()));
        let retry_of_nothing = BulkRequest {
            retried_from: Some("batch-a".to_owned()),
            ..bulk_request(vec![batch_target(1)])
        };
        assert_eq!(
            retry_of_nothing.validate(&new_batch_id()),
            Err(InvalidBatch::RetriedFrom)
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
        assert_eq!(
            InvalidBatch::RetriedFrom.to_string(),
            "the batch retried must be named by a UUIDv7"
        );
    }

    /// A reject reason is the sentence the user is given for an action that did
    /// not happen — in the drawer's action log, in a finished batch's rejected
    /// list, and in what **Retry rejected** says it would send again — so the
    /// wording is pinned here rather than left to whoever next edits the match.
    /// `StaleSha` is the only arm carrying anything, and it names both heads
    /// through [`short_sha`]: forty hex characters twice would bury the
    /// sentence they are in.
    #[test]
    fn every_reject_reason_reads_as_a_sentence_and_a_moved_head_is_named_shortly() {
        assert_eq!(
            RejectReason::StaleSha {
                expected: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                actual: "fedcba9876543210fedcba9876543210fedcba98".to_owned(),
            }
            .to_string(),
            "head moved from 0123456 to fedcba9",
            "both heads are abbreviated, expected first"
        );
        assert_eq!(
            RejectReason::StaleSha {
                expected: "abc".to_owned(),
                actual: "def0".to_owned(),
            }
            .to_string(),
            "head moved from abc to def0",
            "a head already shorter than seven characters is given whole"
        );
        assert_eq!(
            RejectReason::NotMergeable.to_string(),
            "GitHub reports this pull request is not mergeable"
        );
        assert_eq!(
            RejectReason::MergeMethodDisallowed.to_string(),
            "the repository disallows this merge method"
        );
        assert_eq!(
            RejectReason::Forbidden.to_string(),
            "the configured identity is not allowed to perform this action"
        );
        assert_eq!(
            RejectReason::NotFound.to_string(),
            "the pull request was closed or no longer exists"
        );
        assert_eq!(
            RejectReason::NoUserToken.to_string(),
            "no GitHub user token is configured for @dependabot commands"
        );
    }

    /// How a commit is named in passing, wherever one is mentioned rather than
    /// linked. It takes bytes it did not choose — a head from GitHub, a sha
    /// from a journalled request — so the two edges are what matter: it never
    /// lengthens what it is given, and it answers for a value it cannot cut at
    /// seven instead of panicking on it.
    #[test]
    fn a_sha_is_named_by_its_first_seven_characters_or_given_whole_when_shorter() {
        assert_eq!(
            short_sha("0123456789abcdef0123456789abcdef01234567"),
            "0123456"
        );
        assert_eq!(short_sha("0123456"), "0123456");
        assert_eq!(short_sha("012345"), "012345");
        assert_eq!(short_sha("0"), "0");
        assert_eq!(short_sha(""), "");
        assert_eq!(
            short_sha("abcdef\u{e9}"),
            "abcdef\u{e9}",
            "a value whose seventh byte is mid-character comes back whole, not cut in half"
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
                merge_sha: None,
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
                merge_sha: None,
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
                detail: "merged".to_owned(),
                merge_sha: None,
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
    /// which batch it retried if any, the tally, and each target's verdict in batch order
    /// with the head it was sent against — and, for a merge that landed, the commit it
    /// made. Until the last target has settled there is no record to keep.
    #[test]
    fn a_finished_batch_becomes_a_record_with_every_targets_verdict_and_not_before() {
        let targets = [batch_target(1), batch_target(2), batch_target(3)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        let requester = UserId::new("alice");
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: Some("9f8e7d6c5b4a".to_owned()),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        assert_eq!(
            progress.completed_record(42, requester.clone(), Some("batch-0".to_owned()), 100, 160),
            None,
            "one target is still queued"
        );

        progress.record_failure(&targets[2].key(), "boom");

        assert_eq!(
            progress.completed_record(42, requester, Some("batch-0".to_owned()), 100, 160),
            Some(BatchRecord {
                batch_id: "batch-1".to_owned(),
                installation_id: 42,
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                retried_from: Some("batch-0".to_owned()),
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
                        head_sha: Some("abc123".to_owned()),
                        outcome: TargetOutcome::Succeeded {
                            detail: "merged".to_owned(),
                            merge_sha: Some("9f8e7d6c5b4a".to_owned()),
                        },
                    },
                    BatchTargetRecord {
                        repository_id: 7,
                        owner: "acme".to_owned(),
                        repo: "api".to_owned(),
                        number: 2,
                        title: "Bump dependency 2".to_owned(),
                        html_url: String::new(),
                        head_sha: Some("abc123".to_owned()),
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
                        head_sha: Some("abc123".to_owned()),
                        outcome: TargetOutcome::Failed {
                            detail: "boom".to_owned()
                        },
                    },
                ],
            })
        );
    }

    /// A finished batch opened from its record shows as Restate's last word on it would
    /// have: the record keeps the head each target was sent against and the commit each
    /// merge made, so reading it back as progress loses neither. A record from before the
    /// heads were kept reads back with none, as it was written.
    #[test]
    fn a_record_reads_back_as_the_progress_it_was_written_from_heads_and_merge_commits_included() {
        let targets = [batch_target(1), batch_target(2)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: Some("9f8e7d6c5b4a".to_owned()),
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );
        let record = progress
            .completed_record(42, UserId::new("alice"), None, 100, 160)
            .unwrap();

        assert_eq!(BatchProgress::from(record.clone()), progress);

        let before_heads_were_kept = BatchRecord {
            targets: record
                .targets
                .iter()
                .cloned()
                .map(|target| BatchTargetRecord {
                    head_sha: None,
                    ..target
                })
                .collect(),
            ..record
        };
        let read_back = BatchProgress::from(before_heads_were_kept);
        assert!(
            read_back
                .targets
                .iter()
                .all(|item| item.target.expected_sha.is_empty()),
            "{read_back:?}"
        );
    }

    /// Restate journals an action's outcome and keeps a batch's progress for seven days;
    /// the store keeps every target's outcome for good. All of them were written before a
    /// merge's commit was kept, and must still read, as a merge whose commit is unknown.
    /// Likewise a request, a listing, or a record from before a batch named the one it
    /// retried.
    #[test]
    fn outcomes_and_batches_from_before_the_merge_commit_and_retry_link_still_deserialize() {
        let outcome: ActionOutcome =
            serde_json::from_value(serde_json::json!({ "succeeded": { "detail": "merged" } }))
                .unwrap();
        assert_eq!(
            outcome,
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            }
        );
        let outcome: TargetOutcome =
            serde_json::from_value(serde_json::json!({ "succeeded": { "detail": "merged" } }))
                .unwrap();
        assert_eq!(
            outcome,
            TargetOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            }
        );
        let state: TargetProgressState =
            serde_json::from_value(serde_json::json!({ "succeeded": { "detail": "merged" } }))
                .unwrap();
        assert_eq!(
            state,
            TargetProgressState::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            }
        );

        let request: BulkRequest = serde_json::from_value(serde_json::json!({
            "action": "merge",
            "targets": [],
            "user_id": "alice"
        }))
        .unwrap();
        assert_eq!(request.retried_from, None);
        let running: RunningBatch = serde_json::from_value(serde_json::json!({
            "batch_id": "batch-1",
            "installation_id": 42,
            "action": "merge",
            "requested_by": "alice",
            "started_at": 100,
            "target_count": 2
        }))
        .unwrap();
        assert_eq!(running.retried_from, None);
        let target: BatchTargetRecord = serde_json::from_value(serde_json::json!({
            "repository_id": 7,
            "owner": "acme",
            "repo": "api",
            "number": 9,
            "title": "Bump serde",
            "html_url": "",
            "outcome": { "failed": { "detail": "boom" } }
        }))
        .unwrap();
        assert_eq!(target.head_sha, None);
    }

    /// The store keeps the kind in its display form, as it keeps every enum, so the
    /// display form must read back. The negative case is the point: serde spells the
    /// variant `update_branch`, `Display` spells it `update branch`, and `batches.action`
    /// holds the spaced form. Unifying the two vocabularies would silently fail to read
    /// every batch row already on disk, so this test refuses the serde spelling.
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
                event: DeliveryKind::PullRequest,
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

    /// The kind is a type here, but it crosses the Restate ingress as the word GitHub put
    /// in `X-GitHub-Event`: that wire form is GitHub's vocabulary and must stay exactly
    /// what the edge forwarded when the field was a `String`. A kind this deployment does
    /// not route keeps its own spelling instead of being flattened, so a delivery is still
    /// named by what it was in the dispatch's log line.
    #[test]
    fn every_delivery_kind_round_trips_the_word_github_sends() {
        for (kind, wire) in [
            (DeliveryKind::PullRequest, "pull_request"),
            (DeliveryKind::CheckSuite, "check_suite"),
            (DeliveryKind::CheckRun, "check_run"),
            (DeliveryKind::Status, "status"),
            (DeliveryKind::Installation, "installation"),
            (
                DeliveryKind::InstallationRepositories,
                "installation_repositories",
            ),
            (DeliveryKind::Other("push".to_owned()), "push"),
        ] {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(kind.to_string(), wire);
            assert_eq!(DeliveryKind::from(wire), kind);
            assert_eq!(
                serde_json::to_value(&kind).unwrap(),
                serde_json::json!(wire)
            );
            assert_eq!(
                serde_json::from_value::<DeliveryKind>(serde_json::json!(wire)).unwrap(),
                kind
            );
        }
    }

    /// The kind used to be a `String`, so every `dispatch` Restate has journaled carries
    /// the bare word; those invocations replay through today's deserializer and must still
    /// decode. The unrecognised kind is not hypothetical: the edge and the dispatcher are
    /// deployed apart, so during a rollout the edge may forward a kind this binary does
    /// not name yet, and a hard deserialization failure would fail the invocation before
    /// `route_webhook` could ignore it.
    #[test]
    fn webhook_events_journaled_with_the_kind_as_a_bare_word_still_deserialize() {
        let event: WebhookEvent = serde_json::from_str(
            r#"{"event":"pull_request","action":"synchronize","installation_id":42,"repository_id":7,"owner":"acme","repo":"api","number":9,"sha":"abc123","pull_requests":[]}"#,
        )
        .unwrap();
        assert_eq!(event.event, DeliveryKind::PullRequest);
        assert_eq!(event.action.as_deref(), Some("synchronize"));

        for (wire, kind) in [
            ("check_suite", DeliveryKind::CheckSuite),
            ("check_run", DeliveryKind::CheckRun),
            ("status", DeliveryKind::Status),
            ("installation", DeliveryKind::Installation),
            (
                "installation_repositories",
                DeliveryKind::InstallationRepositories,
            ),
            (
                "deployment_status",
                DeliveryKind::Other("deployment_status".to_owned()),
            ),
        ] {
            let event: WebhookEvent =
                serde_json::from_value(serde_json::json!({ "event": wire, "action": null }))
                    .unwrap();
            assert_eq!(event.event, kind, "{wire}");
            assert_eq!(
                serde_json::to_value(&event).unwrap()["event"],
                serde_json::json!(wire)
            );
        }
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

    /// The action trail a `PullRequest` object carries is durable state, kept
    /// for as long as the pull request is open, so it is bounded — and the end
    /// it is bounded from is what matters. A trail trimmed from the wrong end
    /// holds its length and shows a person the first twenty things that ever
    /// happened to a pull request instead of the last twenty, which reads as a
    /// drawer that stopped being told anything. So the entries are named, not
    /// counted: the oldest go, the newest stay, in the order they arrived.
    #[test]
    fn an_objects_history_keeps_the_newest_entries_and_drops_the_oldest() {
        let mut state = PrState::default();
        for at in 1..=(RETAINED_HISTORY as u64 + 5) {
            state.push_history(ActionLog {
                at,
                action: "sync".to_owned(),
                detail: format!("detail-{at}"),
            });
        }

        assert_eq!(state.history.len(), RETAINED_HISTORY);
        assert_eq!(
            state
                .history
                .iter()
                .map(|entry| entry.at)
                .collect::<Vec<_>>(),
            (6..=25).collect::<Vec<_>>(),
            "the five oldest went and the newest twenty stayed, in order"
        );
    }

    /// The completion ids an object remembers are the window a manual sync can
    /// still be acknowledged in: the dashboard asks for a sync under an id and
    /// then polls the object for it, so an id that has fallen out of this list
    /// is a sync the page can no longer be told finished. It is bounded from
    /// the same end as the history and for a different reason, and a repeat is
    /// not a new entry — a page that polls the same id twice must not cost
    /// another sync its place.
    #[test]
    fn completed_sync_ids_keep_the_newest_refreshes_and_a_repeat_costs_no_room() {
        let mut state = PrState::default();
        state.complete_sync("sync-1".to_owned());
        for _ in 0..3 {
            state.complete_sync("sync-1".to_owned());
        }
        assert_eq!(
            state.completed_sync_ids,
            ["sync-1"],
            "the same sync acknowledged again is the same sync"
        );

        for id in 2..=(RETAINED_SYNC_IDS as u64 + 3) {
            state.complete_sync(format!("sync-{id}"));
        }

        assert_eq!(state.completed_sync_ids.len(), RETAINED_SYNC_IDS);
        assert_eq!(
            state.completed_sync_ids.first().map(String::as_str),
            Some("sync-4"),
            "the three oldest went, the repeats having taken no room of their own"
        );
        assert_eq!(
            state.completed_sync_ids.last().map(String::as_str),
            Some("sync-23")
        );
    }
}
