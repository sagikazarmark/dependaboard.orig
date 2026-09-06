# Dependabot Dashboard — MVP Architecture

Scope: one GitHub App installation, one org, single user. Filterable table of open
Dependabot PRs. Three bulk actions: merge, rebase, update branch.

---

## 1. Components

| Component | Runs on | Responsibility |
|---|---|---|
| **Web app** (Dioxus fullstack) | Worker *or* Axum | UI + server functions. Reads the read model, submits batches. |
| **Webhook handler** | Separate Worker (or an Axum route) | Verify HMAC, forward to `WebhookIngress.dispatch`, return 200. No routing logic. |
| **Restate services** | **Separate binary** | `PullRequest` object, `BulkAction` workflow, `WebhookIngress` service, `InstallationSync` / `RepoSync` objects. |
| **Read model** | libSQL (see below) | Queryable projection of PR state, behind a `PrStore` trait. |
| **Restate server** | Restate Cloud (prod) / Docker (local) | Durable execution, keyed concurrency, retries. |

Splitting the Restate service into its own binary is the right call — it gets a full
tokio runtime, octocrab works unmodified, and the durable-execution layer stops being
coupled to the Workers lifecycle. It does have one knock-on effect, below.

### Why a separate read model

Restate object state is keyed — you can read one PR's state, not `WHERE update_type =
'patch' AND check_status = 'failure' ORDER BY updated_at`. The table needs cross-key
querying, so object handlers write through to `PrStore` after every state change.
Restate holds *in-flight action* truth; the store holds *queryable* truth.

---

## 2. Restate entities

**One rule that applies to every handler below:** every GitHub HTTP call and every
external database operation performed from a Restate handler goes inside `ctx.run`, so
its result is journaled and replay stays deterministic. Restate state access
(`ctx.get`/`ctx.set`) and Restate-to-Restate calls do not — they're already durable.
The pseudocode in this section elides the `ctx.run` wrappers for readability; the
implementation must not.

### `PullRequest` — virtual object, key `{repository_id}#{number}`

Keyed concurrency gives you the per-PR lock for free, and the object's own state gives
you action tracking in the same place. Don't split lock and state.

**Key on the immutable repository id, not `owner/repo`.** GitHub hands you a numeric
repository id in every payload, and it survives renames and transfers; `owner/repo` does
not. Keep `owner` and `repo` in object state as display/routing metadata. Same for the
SQL primary key. Free now, a migration later.

**State**

```rust
struct PrState {
    snapshot: Option<PrSnapshot>,  // None once closed(); sha, title, dep, update_type, checks
    history: VecDeque<ActionLog>,  // bounded, last ~20
    last_synced_at: Option<u64>,   // when the last full sync COMPLETED; debounce anchor
    sync_pending: bool,            // a trailing debounced sync is already scheduled
}
```

**`sync` fetches; it is not handed a snapshot.** The webhook handler can't cheaply build
a `PrSnapshot` — update type needs the head commit's metadata block, and check status needs a
rollup across two separate GitHub mechanisms. Making the handler construct one either
duplicates that logic in the Worker or produces partial snapshots. So the handler takes a
request and does the fetching itself, inside the Restate binary:

```rust
sync(SyncRequest {
    repository_id: u64,
    owner: String,
    repo: String,
    number: u64,
})
```

```
PullRequest.sync
  → debounce check (below); maybe return without fetching
  → one GraphQL query: pull request (title, state, author, labels, mergeability),
    head commit message (YAML metadata block → update type, §5),
    check runs + commit statuses + check suites, first 100 of each  (→ rollup, see §5b)
  → follow cursors only if a commit has more than 100 contexts or suites
  → build PrSnapshot → update object state → upsert read model
  → set last_synced_at
```

This was five or more REST reads (pull request, head commit, check runs, check suites,
combined status — several paginated) before it became one query; REST remains for the
mutations and the installation and pull-request listings.

One canonical sync path serves webhooks, reconciliation, initial import and manual
refresh. It also makes the webhook Worker trivially boring, which is the point.

**Handlers**

| Handler | Kind | Behaviour |
|---|---|---|
| `sync(SyncRequest)` | exclusive | Debounce, then fetch canonical state from GitHub, update state, write through to `PrStore`. Idempotent. |
| `closed(synced_before?)` | exclusive | PR merged or closed: clear the object's state keys, `delete_pr()` from the read model. A sweep's drain passes the fence the prune recorded — the instant its listing started; the object stands down if it has synced since (reopened behind the sweep). Webhooks and purges pass nothing: unconditional. |
| `merge(MergeRequest)` | exclusive | Guard `expected_sha == snapshot.sha`; `PUT /pulls/{n}/merge` with an explicit `merge_method`; on success `delete_pr()`. |
| `command(DependabotCommand)` | exclusive | Post `@dependabot <cmd>` + attribution footer. **User token required** — see §6. Fire-and-forget. |
| `update_branch()` | exclusive | `PUT /pulls/{n}/update-branch` with `expected_head_sha`. App-identity alternative to rebase; on success, one-way self-send of `sync` so the row catches up before the webhook does. |
| `status()` | **shared** | Read-only, for UI drill-down without blocking actions. |

**Concurrency.** The exclusive handlers serialise per PR, so "only one action at a time"
is structural — you don't implement it. And because v1 has no awaited async gap, keyed
concurrency is the *whole* locking story. No `in_flight` field, no awakeables, no timeout
logic.

**Debounce `sync`, or event storms eat the rate budget.** Every `check_run` `created`
and `completed` event routes to a full canonical sync — one GraphQL query today, 4+ REST
calls when this was written. One push to a PR with a 20-job matrix fires ~40 `check_run`
events plus `check_suite` plus possibly `status` events, so a single push costs 40+
serialized full syncs (≈ 160+ API calls over REST). Bulk-rebase 50 PRs — the feature this
tool exists for — and the resulting Dependabot pushes generate thousands of syncs within
minutes, against the same rate budget everything else in this section works to protect.
Keyed concurrency serialises those syncs; it does not collapse them — all 40 queued
invocations run to completion.

The fix is ~20 lines at the top of `sync`:

```
if now - last_synced_at < DEBOUNCE (~20s):
    if !sync_pending:
        sync_pending = true
        delayed one-way self-send: PullRequest.sync in DEBOUNCE   // same key
    return                          // skip the fetch entirely
else:
    sync_pending = false
    ... full fetch as normal; set last_synced_at on completion
```

The trailing scheduled sync is not optional: without it, the *last* `check_run.completed`
of a storm gets swallowed and the PR sits at Pending until the hourly sweep. Leading sync
plus one trailing sync collapses any storm into two fetches. These flags are coalescing
state, not locking — the keyed-concurrency story above is unchanged.

**Rebase is a comment with a nicer name.** `command` posts the comment and returns. What
happens next arrives through the normal webhook path: Dependabot pushes, `check_suite`
fires, `sync` updates the row, the table shows the new check status. That's the progress
indicator, and it's one you were going to build anyway.

**Optimistic merge.** Carry `expected_sha` from the row the user saw in the table. If the
head moved between render and click, fail that PR in the batch rather than merging
something unreviewed. Cheap, and it's the failure mode that actually bites in bulk UIs.

**Optional: re-verify green inside `merge`.** The read model is deliberately eventually
consistent, so a cached `check_status` is a rendering hint, not an authorization. If your
invariant is genuinely "bulk merge never merges a non-green PR", re-fetch the rollup
immediately before the call:

```
merge(expected_sha)
  → fetch PR;      assert head_sha == expected_sha
  → fetch rollup;  assert Success
  → PUT /pulls/{n}/merge
```

The cost is two extra API calls per merge inside an already rate-limit-bounded fan-out.
Skip it if branch protection is your authoritative gate — GitHub enforces required checks
server-side regardless. Add it if your notion of green is stricter than the repo's
required checks, which it may well be.

The MVP skips it and says so where it matters: the merge confirmation counts the selected
rows whose cached rollup is not green and tells the user that count is the read model's
last word, not a gate. The gate is branch protection.

### `BulkAction` — workflow, key = batch UUIDv7

Workflow rather than object: a batch has a definite lifecycle and you want to query its
progress from the UI.

```
run(BulkRequest { action, targets: Vec<PrTarget>, user_id })
  ├─ for each target (bounded concurrency; merges grouped per repo, see below):
  │     ctx.object_client::<PullRequest>(key).call(action)
  │       → ActionOutcome, or a TerminalError the callee gave up with
  │     write the outcome into workflow state AS IT COMPLETES
  ├─ terminal state: Completed { succeeded, rejected, failed }
  └─ ctx.run: PrStore::record_batch(kind, requester, started/completed at, per-target verdicts)

progress() -> BatchProgress    // shared handler, UI polls this
```

`action` is one of `merge`, `rebase`, `update_branch`, each dispatched to the
`PullRequest` handler of the same name (`rebase` to `command`). The per-target semantics
are the same for all three — the `expected_sha` guard, `Succeeded`/`Rejected`/`Failed`
verdicts, progress written as each lands — and only the scheduling differs, see "Bound the
fan-out" below.

**A target that fails terminally does not stop the batch.** The callee's `TerminalError`
is recorded against that target as `Failed` with its reason and the batch carries on;
the workflow completes with the tally, never with an error, once any target was
attempted — the recording step that follows retries until the store takes it rather
than giving up, so a store outage delays the workflow's end without failing it. One pull
request's problem is not a reason to leave the other ninety-nine
queued. This holds for configuration-wide fatals too (bad credentials, a 404 on a
resource never read): every target gets its own verdict rather than the batch aborting
on a guess about which failures are shared, so the drawer shows the same reason on each.

**Update progress per target, not after `collect`.** If state is only written once the
whole fan-out finishes, `progress()` returns nothing useful for the entire duration of
the batch — which is exactly when the user is watching it. Write each outcome as it
lands.

**The finished batch is written to libSQL, once.** Workflow state answers "where is the
batch now"; it is cleared after the workflow's retention (seven days here), and then the
batch is gone from Restate for good. So the last step of `run` writes the finished batch
to the projection — kind, requester, when it started and finished, the tally, and every
target's verdict with the pull request named in full and linked — inside `ctx.run`, so
a replay does not write it again, against a store write that keeps the first record for
a batch id, so a retry of the step does not either. The step's retries are unbounded:
nothing later would redo this write, so giving up would lose the record for good. The
dashboard's **Batches** button lists what was written, newest first; that list is the
audit view, and it does not care how old a batch is. A merged pull request leaves
`pull_requests`, so the targets are copied rather than referenced.

**Retrying rejected targets is a new batch, refreshed first.** A rejection is the last
word on the request *as it was sent*; most often the head moved, and the fix is to send
the target again with the head it has now. The drawer offers **Retry rejected** once a
batch has finished with a rejected target a fresh attempt can cure. The UI syncs each
such pull request
through `DashboardIngress.sync_pull_request` — all at once, awaiting each completion id
as the drawer's own **Sync** does — reads the rows back, and queues them as a new batch
of the same kind under a fresh UUIDv7: the old id has run and cannot run again. Only
`StaleSha` and `NotMergeable` are worth the trip: the head is what moved, and GitHub
judges mergeability anew on every attempt. `Forbidden` and `MergeMethodDisallowed` are
over the configuration, the same whatever the head, and are left out unrefreshed with
that said, or the retry would only reject them again and write a second audit row each. A
target rejected as `NotFound`, or whose row is gone by the time it is refreshed, is left
out; so is one whose refresh failed, since sending it with the SHA it was just rejected
over would only reject it again. The targets left out are named in a notice, and the rest
go on. The refresh can take a while; a retry that finds another batch queued in the
meantime stands down rather than take the drawer from the one running. The confirmation
dialog's promise that moved pull requests are "rejected, not silently retried against new
code" stands: the retry is the user's, and the SHAs it carries are the ones the dashboard
shows at the time. Nothing here is a Restate concern: the retry composes the public
handlers the dashboard already uses.

### Error taxonomy — decide this before writing any handler

This is the most important implementation detail on the page. Restate's default retry
policy is exponential backoff up to 70 attempts, then **pause** the invocation for manual
resumption. So if a handler propagates an octocrab error verbatim, a permanent GitHub
rejection — stale SHA, disallowed merge method, insufficient permissions — doesn't become
`rejected += 1` in the batch summary. It burns 70 retries over minutes and then parks the
invocation, and the UI shows the batch as forever in-flight.

Two distinct concepts, and they must not be conflated:

```rust
/// The handler's return value. Both variants are SUCCESSFUL completions
/// of the handler — Restate does not retry either.
enum ActionOutcome {
    Succeeded { detail: String },
    Rejected  { reason: RejectReason },   // GitHub said no, permanently
}

enum RejectReason {
    StaleSha { expected: Sha, actual: Sha },
    NotMergeable,
    MergeMethodDisallowed,
    Forbidden,          // e.g. the Dependabot-command refusal
    NotFound,           // PR closed/deleted underneath us
}
```

Everything else — 5xx, connection resets, 403 with `retry-after`, secondary rate limits —
is an *error*, propagated so Restate retries it. And anything genuinely unrecoverable
that isn't a per-target rejection should be a `TerminalError`, so it fails fast rather
than pausing.

Rule of thumb: if retrying the identical request could plausibly succeed later, it's an
error; if it never could, it's a `Rejected` outcome.

**Classify on the whole response, not the status code.** A status-code lookup table is
the obvious implementation and it's wrong in both directions:

- **Rate limiting is not a distinct status.** Primary limiting arrives as 403 *or* 429
  with `x-ratelimit-remaining: 0`; secondary limiting also as 403 *or* 429, and
  `Retry-After` is **not guaranteed to be present**. GitHub's guidance is to read the
  rate-limit headers and back off regardless. A rule keyed on "403 + Retry-After" misses
  the common case and burns the whole retry budget on something that would have succeeded.
- **404 is overloaded.** GitHub deliberately returns 404 rather than 403 for resources
  that exist but which your token can't see, to avoid leaking their existence. Treating
  every 404 as `Rejected::NotFound` silently converts a permissions misconfiguration into
  "that PR is gone" across a whole batch — with no signal that anything is wrong.

So write one function, and give it everything:

```rust
fn classify(
    status:  StatusCode,
    headers: &HeaderMap,      // x-ratelimit-remaining, x-ratelimit-reset, retry-after
    body:    &GitHubError,    // message + documentation_url carry the real reason
    op:      Operation,       // merge / comment / update_branch — context changes meaning
) -> Classification          // Retryable { after } | Rejected(RejectReason) | Fatal
```

Rough shape: any status with `x-ratelimit-remaining: 0` or a secondary-limit message is
`Retryable` regardless of code; 5xx and transport errors are `Retryable`; 409, and 405
with a merge-method message, are `Rejected` — *except* the base-branch 405 below; 404 on
an operation you've just successfully read is `Rejected`, but 404 on a resource never
read is a configuration `Fatal`. "Read" includes the object's own snapshot: a mutation
handler passes the snapshot's presence to its GitHub step exactly as `sync` does, so a
pull request that has gone since the table showed it is `Rejected(NotFound)` — skipped by
retry, explained by the drawer — not a failure that asks for the operator. The same
read-first logic governs a refused write: a
403 on a merge or branch update *after* the pull request was read with the same
credentials is `Rejected(Forbidden)` — branch protection, a repository the installation
can see but not push to — while a 401 is `Fatal` whatever came before it, because the
client has already refreshed the token and retried once by the time it surfaces. A
`Fatal` fails the `PullRequest` handler terminally; inside a batch that is one target
marked failed, not an aborted batch (see `BulkAction` above). Of the DB constraint
violations, only the foreign key one (see the FK race in `InstallationSync`) is
`Retryable`: a primary key or unique violation is a programming error that a fresh
attempt would hit again, so it is `Fatal`.

**One 405 is special-cased, and it's the one bulk merging hits most.**
`PUT /pulls/{n}/merge` returns 405 `"Base branch was modified. Review and try the merge
again."` transiently when merges land in quick succession on the same base branch — which
is exactly what a batch with `MAX_CONCURRENT: 3` and several green PRs in one repo does
to itself. The retry succeeds. Classify that message as `Retryable`, not
`Rejected::NotMergeable`, or a perfectly normal bulk merge reports a summary full of
false failures it caused itself. It's also an argument for grouping merge targets by
repository in the fan-out and running same-repo merges sequentially — cross-repo merges
can't collide on a base branch.

### GitHub mutations are at-least-once, not exactly-once

`ctx.run` journals the result *after* the closure runs. If the process dies in that
window, the replay re-executes the closure — the Rust SDK is explicit that the closure
may run more attempts than the journal records. So every GitHub mutation needs to be
safe to perform twice — and nothing may depend on a step's *return value* having been
seen, when the step's effect has happened either way. The store's prunes are the case in
point: a re-run `DELETE ... RETURNING` returns nothing, so the keys it removed are queued
in the same transaction instead (§2 `RepoSync`, "the prune's step result is not what
drives the closes").

- **Comments:** put a deterministic marker in the body — the hidden HTML comment you
  already wanted for provenance, containing the batch id. Before posting, list recent
  comments and skip if the marker is already there. The provenance feature and the
  idempotency mechanism are the same feature.
- **Merge:** on an ambiguous failure, re-fetch the PR. If it's already merged, that's
  `Succeeded`, not a failure — reporting a false negative on a merge that actually landed
  is worse than the duplicate call.
- **update_branch:** the `expected_head_sha` guard prevents performing it twice, but not a
  false failure: GitHub accepts the update and moves the SHA, the process dies before the
  journal write, the retry sends the now-stale expected SHA and is rejected. You'd report
  a failure for something that worked. Give it the same treatment as merge — on an
  ambiguous outcome, re-fetch and reconcile against reality. Low urgency, since this isn't
  the recommended rebase path.

**Completion semantics differ by action, deliberately.** An earlier draft claimed uniform
"issued = done" semantics for everything. That was wrong for merge: `PUT /pulls/{n}/merge`
is synchronous and tells you the real outcome — 200 on a merge, 409 when your SHA is
stale. Throwing that away makes the batch summary worse for no gain.

| Action | Batch success means | Eventual outcome |
|---|---|---|
| `merge` | GitHub actually merged it | — (terminal) |
| `command` (rebase etc.) | GitHub accepted the comment | arrives via webhook |
| `update_branch` | GitHub accepted the update | arrives via webhook |

This still avoids the machinery you didn't want — no awaited async gap for rebase, no
awakeables, no timeouts. Merge simply reports what the API already told it.

(GitHub does have an async `merge-async` endpoint. It's *required* for stacked PRs and
also handles ordinary ones, mainly as an escape hatch for merges slow enough to time out.
Dependabot PRs are neither stacked nor slow to merge, so the synchronous endpoint — which
hands you the outcome directly — is the right one here.)

**Bound the fan-out.** `for each target → concurrent` is fine for five PRs and reckless
for two hundred. GitHub's docs warn outright that creating comments too quickly triggers
secondary rate limiting, and their best-practice guidance for mutative requests is to
avoid concurrency and space requests out. Since the workflow is durable and asynchronous
anyway, slow is nearly free:

```rust
const MAX_CONCURRENT: usize = 3;   // comments: consider 1 + a short delay
```

Start conservative and raise it if you never see a 403 with a `retry-after`. For merge
batches, additionally group targets by repository and run same-repo merges sequentially
(concurrency applies *across* repos) — this sidesteps the base-branch 405 above. Branch
updates share the merge bound but not the grouping: `update-branch` writes only to the
pull request's own head branch, so two in one repository cannot collide and go out in the
same round, in batch order. Getting the App secondary-rate-limited across hundreds of
repos is a much worse failure than a batch taking two minutes.

**Generate the batch id client-side, before the call.** The id matters less as a format
than as an idempotency key. If the UI calls the server, the server mints an id, and the
response is lost to a dead connection, a retry mints a *second* id and runs the batch
twice. For merges that's mostly harmless (the PR is already gone); for `@dependabot
rebase` it posts duplicate commands. Generate the id in the UI, reuse it across retries,
and pass it through as the Restate workflow id — Restate then deduplicates for you,
because a workflow id can only run once.

**Set workflow retention deliberately.** Restate clears a workflow's state 24 hours after
`run` completes by default, after which `progress()` returns nothing. The retention here
is seven days, which covers the dashboard following a batch and reopening one it
followed recently; "what did last month's batch do" is answered from libSQL, where the
finished batch is written as the workflow's last step (see `BulkAction` above), not by
raising the retention further.

**Batch id: UUIDv7, not ULID.** Both are 128-bit with a 48-bit millisecond prefix, so the
time-ordering you want is identical — UUIDv7's canonical hex sorts lexicographically by
time exactly as ULID's base32 does. UUIDv7 wins on everything else: it's standardised
(RFC 9562) where ULID's spec is a README with underspecified same-millisecond
monotonicity; `uuid` is a crate you'll already have (`Uuid::now_v7()`); and it maps to a
native 16-byte type if you ever fall back to Postgres. The only loss is the shorter,
prettier 26-char string, and this id is only ever a Restate workflow key and a hidden
HTML marker — nobody reads it.

### `WebhookIngress` — service (unkeyed)

- `dispatch(WebhookEvent)` — the sole ingress-reachable entry point for webhooks. Maps
  event kind to one-way sends: `PullRequest.sync` / `PullRequest.closed`,
  `RepoSync.sync_sha`, `InstallationSync.sync_all`. No state, no GitHub calls, no keying.
- Public; everything it calls is private.
- Routes only what GitHub sends. The dashboard's manual refreshes used to be forged as
  `pull_request` events with a made-up action and pushed through here, which hid the
  manual-sync contract inside the webhook contract; they now have their own service.

### `DashboardIngress` — service (unkeyed)

- `sync_installation()` — the dashboard's global **Sync**: one-way
  `InstallationSync.sync_now` for the configured installation. No input; the service
  serves exactly one installation, as `SchedulerIngress.start` already assumes.
- `sync_pull_request(ManualSyncRequest)` — the drawer's per-PR **Sync**: one-way
  `PullRequest.sync` with the debounce bypassed (someone is waiting) and the request's
  `completion_id`, which `sync` records in `completed_sync_ids` so the drawer can poll
  `PullRequest.status` until its own refresh has landed.
- Public; called only by the web backend.

### `InstallationSync` — virtual object, key = installation id

Three handlers, deliberately separated — collapsing them is the bug described below.

| Handler | Does | Schedules? |
|---|---|---|
| `sync_now()` | one full reconciliation | **never** |
| `tick()` | **if `scheduler_started` is unset or the generation is stale: return — no sync, no reschedule.** Else schedule the next `tick` in 1h **first**, then `sync_now()` | yes |
| `start()` | if a tick is already pending for the current generation: return. Else set `scheduler_started` + `scheduler_tick_pending`, send `tick()` | only when no tick is pending |

- Service startup calls `start()`. Installation and repo-access webhooks call `sync_now()`.
  Only `tick()` ever creates a timer.
- Keyed on installation so two sweeps can't overlap.

**Re-arm before sweeping.** Restate commits journaled state writes and one-way sends as
they happen and never rolls them back when a handler fails terminally. If `tick()` swept
first and scheduled afterwards, a terminal sweep failure (a fatal GitHub 401/403, a
terminal store error, an operator cancelling the invocation) would end the chain with no
successor and nothing logged — until the next process restart called `start()`. So
`tick()` persists `scheduler_tick_pending`, schedules its successor, and only then sweeps;
a failed sweep is logged with the installation id and cause and the failure stays visible
on the `tick` invocation, but the next tick fires regardless.

**Why not one self-scheduling `sync_all`?** Because keyed concurrency *serialises*
invocations; it does not *deduplicate* them. Restate's ingress even documents that its
automatic idempotency keys don't dedupe across separate requests. So if `sync_all`
schedules its own successor and is also called from startup and from two installation
webhooks, you get three independent perpetual chains, each reconciling every hour,
forever — with no symptom except a triple API bill and triple the rate-limit pressure.
The `scheduler_started` flag in object state is what makes `start()` genuinely idempotent.

**The flag is also the kill switch — which is why `tick()` checks it first.** A scheduled
tick already exists inside Restate when `suspend` arrives, and `pause()` can't cheaply
cancel it. If `tick()` ran unconditionally, that orphaned timer would fire an hour later,
call `sync_now` against a suspended installation — burning the retry budget, the exact
failure the lifecycle table below exists to prevent — and reschedule itself. Then
`resume` → `start()` would see the flag unset and launch a *second* chain: the
duplicate-perpetual-chains bug again, reintroduced via suspend/resume. With the guard,
the orphaned tick wakes, sees `scheduler_started` unset, and dies silently; `pause()`
works as written and `resume` starts exactly one fresh chain.

**Order matters: repositories before pull requests.** `pull_requests.repository_id` is an
FK, so a newly installed repo whose `RepoSync` reaches `upsert_pr` before its parent row
exists will fail the insert. Sequence `sync_now` explicitly:

```
capture reconcile_start
enumerate ALL repos (complete, successful pagination; see below)
  → in one transaction: upsert every live RepoRecord
                        + retain_repos(live_ids, synced_before: reconcile_start)
                          → queues the PR keys the cascade removed for retirement,
                            fenced by reconcile_start
  → drain the retirement outbox: send PullRequest.closed(fence) to each, acknowledge
  → then fan out RepoSync.reconcile for the live set
```

Doing the upsert first also means renames and metadata changes land before PR
reconciliation reads them.

**Enumeration must notice when it moved under its own pages.** `GET
/installation/repositories` is offset-paged, and an installation with more than a page of
repositories can change between two page fetches: a repository removed before the page
boundary shifts the next page one position early, and the repository that was at the
boundary is never listed — for that sweep it is "gone", its rows are cascaded, and its
pull requests' objects retired. Every page carries `total_count`, so the client compares
it across pages, de-duplicates by id, and refuses the listing when the total moved or the
distinct ids do not add up to it; inside the sweep that is a retryable failure, and Restate
lists again from page one under the step's backoff. Residual, accepted: a removal and an
addition landing between the same two pages leave the total unchanged and can still hide
one repository; only a cursor-paged listing could rule that out.

One race survives the ordering: a `pull_request.opened` webhook for a freshly added repo
can reach `upsert_pr` before `sync_now`'s transaction has inserted the parent row. This
self-heals — the FK violation is an error inside `ctx.run`, Restate retries, and the repo
row lands well within the backoff window — but *only* if DB constraint failures classify
as `Retryable`. Leave a comment in the classifier saying so, so nobody "fixes" it into a
`TerminalError` later.

**Be authoritative about repositories, not just PRs.** If a repo is removed from the
installation, the next enumeration simply never calls `RepoSync` for it — so its PR rows
survive forever, invisible to every reconcile. `RepoSync` can't fix this; it only ever
sees repos it was told about. `InstallationSync` has to diff the live repo set and
cascade-delete the rest. Same completeness and timestamp rules as below: only after a
fully successful enumeration, and only rows synced before the enumeration started. And
the same object-state rule as `RepoSync.reconcile` below: `retain_repos` queues the PR
keys the cascade removed, and `sync_now` drains the queue and sends `PullRequest.closed`
to each, fenced by `reconcile_start`, so no object keeps serving a snapshot for a
repository the App no longer sees — and one re-synced since the sweep began, because
the repository was re-added behind its back, keeps its state.

**Installation lifecycle is not all one event.** Mapping every `installation.*` action to
"go enumerate GitHub" is wrong, because for half of them the App has just lost the access
that enumeration requires — a suspended installation can't reach the account's resources,
and a deleted one has no access at all. Those calls don't fail loudly; they burn the retry
budget and then park.

| Action | Handler | Why |
|---|---|---|
| `created`, `unsuspend`, `installation_repositories.*` | `sync_now()` | access exists; enumerate |
| `deleted` | `purge()` | delete this installation's repos + cascade, then drain the retirement outbox and send `PullRequest.closed` to every PR that went with them; no API call |
| `suspend` | `pause()` | clear `scheduler_started`, stop ticking; don't call GitHub |

`resume` after a suspension goes through `start()` again. These events are delivered to
every GitHub App automatically — they aren't in the subscription list because they can't
be subscribed to.

### `RepoSync` — virtual object, key = repository id

- `reconcile()` — capture `reconcile_start`, list open PRs authored by
  `dependabot[bot]`, fan out `PullRequest.sync`, **then delete read-model rows for this
  repo that weren't in the listing**. This is what makes reconciliation authoritative
  rather than additive-only.
- **A pruned row takes its object's state with it.** Deleting the row is only half the
  cleanup: the `PullRequest` object still holds the snapshot, and `status()` keeps
  serving it to the detail drawer indefinitely. `retain_prs` queues the keys it
  actually deleted; `reconcile` drains the queue and sends `PullRequest.closed` to each.
  Fan out from what was *deleted*, not from what was *absent from the listing*: a PR
  reopened mid-sweep and re-synced by its own webhook survives the `synced_at` guard
  below and must not be closed. That guard covers a webhook that lands *before* the
  delete; one that lands between the delete and the `closed` invocation would still be
  wiped, so the sweep's `closed` carries `reconcile_start` and the object stands down
  when its `last_synced_at >= reconcile_start` — the same boundary, applied on the
  object side. `sync_now` does the same over everything `retain_repos` cascaded when a
  repository left the installation, under the same fence: the App receives no webhooks
  for a repository it no longer sees, but the repository can be re-added and its pull
  requests re-synced before the close lands, and a listing that dropped it by mistake
  (the pagination shift above) must not cost them their history. `purge()` alone is
  unfenced: the App has lost the installation, so nothing can reopen those.
- **The prune's step result is not what drives the closes.** `ctx.run` is at-least-once
  (§"GitHub mutations are at-least-once"): the `DELETE ... RETURNING` can commit and the
  process die before the journal takes the returned keys, and the re-run then finds
  nothing left to delete. Driving `closed` from that return value would leave the
  objects that just lost their rows serving a snapshot nobody else has, indefinitely. So
  every prune — `retain_prs`, `retain_repos`, `purge_installation` — writes the keys it
  removed to `pull_request_retirements`, in the delete's own transaction, with the fence
  it ran under (`NULL` for a purge). The handler then drains the outbox in steps that are
  each safe to run again: read what is pending, send `closed` to each with the fence its
  row carries, acknowledge through the last id read. A re-run read sees the same rows; a
  duplicate `closed` is idempotent, and a fenced one is refused exactly where it should
  be; a re-run acknowledgement is a no-op. Ids are `AUTOINCREMENT`, so acknowledging
  `id <= last` covers exactly what was read and anything queued in between waits for the
  next drain. The fence is stored, not recomputed at drain time: a row pruned by a sweep
  that started at T1 and drained by one at T2 must carry T1, since a PR reopened and
  re-synced between the two has `last_synced_at >= T1` and keeps its state, where T2
  would retire it. Any drain drains everything pending — a `RepoSync` may tell an
  object another repository's sweep pruned — which is what makes a lost drain harmless:
  the next handler to prune anything finishes it.
- **`retain_prs` runs only after every page has been fetched successfully.** A listing
  that fails on page 3 of 5 must abort the whole reconcile, not treat two pages as the
  authoritative live set — otherwise a transient API error silently deletes most of a
  repo's rows. Collect the full set first, then diff; on any pagination error, bail
  before the delete and let Restate retry.
- **`retain_prs` must not delete rows written after the listing started.** The listing
  is a snapshot at T0; a PR opened at T1 lands in the read model via its own webhook
  sync at T2; a retain that only knows the T0 live set deletes it at T3 — and a quiet PR
  then stays invisible until the next sweep, an hour of absence from a triage tool.
  Scope the delete: `... AND synced_at < reconcile_start`. Rows upserted by concurrent
  webhook syncs carry a fresher `synced_at` and survive. The completeness rule above
  covers partial listings; this one covers concurrent writers — you need both.
- **One unsyncable PR must not abort the sweep.** A `PullRequest.sync` call only fails
  once the callee has failed terminally (retryable failures are retried inside the
  callee). Capture those failures instead of propagating them: attempt every listed PR,
  `warn!` each failure with its key and cause, run `retain_prs` over the full listing
  (the failed PR is still open on GitHub, so its row stays), and only then surface a
  single `TerminalError` naming the failed keys so the invocation is visible in Restate.
  Propagating early leaves that repo's closed PRs in the projection forever, and since
  `reconcile` is fire-and-forget from `InstallationSync` nobody sees why. The aggregate
  must be terminal, not retryable: the per-PR call results are already journaled, so a
  retry would replay the same failures and loop.
- `sync_sha(sha)` — resolve a commit SHA to PR(s) via the read model, fan out
  `PullRequest.sync`. Entry point for `status` and `check_suite` events (see §3).
- Keyed per repo, so repos reconcile in parallel without overlapping themselves.

The split costs almost nothing and is what makes stale-row cleanup natural — the
enumeration that finds live PRs is exactly the set you diff against.

---

## 3. Call flow

```
Table render      UI → server fn → PrStore::list(filter, page)

Select all        UI → server fn → PrStore::list(filter, page of MAX_BATCH_TARGETS)
matching               the selection is resolved server side, newest update first,
                       and capped at one batch's worth; the total comes back with the
                       rows so the UI can say when the filter matched more than it took

Bulk action       UI → server fn → Restate ingress
                       the UI names each target by key and the head SHA it saw; the
                       server fn resolves the rest — repository, title, link — from
                       PrStore::get_pr and refuses a target of another installation
                       POST /restate/send/BulkAction/{batch_id}/run
                     → workflow fans out to PullRequest objects
                     → objects call GitHub API, write through to PrStore
                     → workflow writes the finished batch: PrStore::record_batch

Progress          UI polls → server fn →
                       POST /restate/call/BulkAction/{batch_id}/progress

Recent batches    UI → server fn → PrStore::recent_batches(limit)
                       the finished batches, newest first, from the projection rather
                       than Restate, so they outlive the workflow retention

All ingress endpoints live under `/restate/`: `/restate/call/...` waits for the handler's
result, `/restate/send/...` returns as soon as the invocation is accepted. Use `send` for
`run` — a call would block the server function for the whole batch, which is deliberately
rate-limited and could be minutes. (The older unversioned paths and the `/send` suffix
still work, but Restate says new code should use these.)

Submitting the same workflow id twice fails with "Previously accepted" — which is exactly
the deduplication the client-generated batch id is there to exploit. Exploiting it means
the resend must not read as a failure: send the batch id as the request's
`idempotency-key` as well as the workflow key, and Restate answers a repeat with the
original acceptance instead of the refusal. The server function then has no error to
swallow and no message to sniff; a 409 that does arrive is a real one and is reported.
Otherwise the idempotency mechanism converts a recovered connection blip into a
user-visible error for a batch that is running fine.

Webhook           GitHub → /gh/webhook → verify HMAC
                     → WebhookIngress.dispatch(event)   (one-way, single entry point)

                  inside the Restate binary, dispatch routes:
                     pull_request.opened|reopened|synchronize|edited
                                  |labeled|unlabeled     → PullRequest.sync
                     pull_request.closed                 → PullRequest.closed
                     check_suite.completed               → RepoSync.sync_sha
                     check_run.created|completed         → RepoSync.sync_sha
                     status                              → RepoSync.sync_sha
                     installation.created|unsuspend      → InstallationSync.sync_now
                     installation_repositories.*         → InstallationSync.sync_now
                     installation.deleted                → InstallationSync.purge
                     installation.suspend                → InstallationSync.pause
                        (a dependabot push after a rebase is just another sync)
```

Webhooks are one-way sends (`send()`, not `call()`) so the handler returns 200 in
milliseconds and Restate owns the retries.

**Return 200 only after Restate has accepted the invocation.** One-way means Restate
doesn't wait for the *handler* to finish; it does not mean fire-and-forget from the
Worker. GitHub does not automatically redeliver failed deliveries, so a 200 returned
before the forward succeeded is a silently dropped event — recovered only by the hourly
sweep, an hour later. If the forward fails, return non-2xx and let it show up in the
App's delivery log. (This is a further argument for keeping reconciliation authoritative:
webhook delivery is best-effort by design.)

**The delivery id is the idempotency key.** GitHub never redelivers on its own, but a
delivery that timed out at 10 s is marked failed even when the forward had already been
accepted by Restate, and the fix for a failed delivery — **Redeliver** in the App's
delivery log, or a script driving the redelivery API — replays the request under the
same `X-GitHub-Delivery`. Every forward to `WebhookIngress.dispatch` sends that id as
Restate's `idempotency-key` header, so the replay attaches to the dispatch Restate
already accepted instead of creating a second one. Most downstream handlers would absorb
a duplicate anyway, but a duplicated `installation.created` re-enumerates every
repository, which is what this prevents. Dashboard-originated sends are not GitHub
deliveries and carry no key; they are minted fresh per click and dedup by other means
(the batch id as workflow key, the debounce in `PullRequest.sync`).

**Unroutable kinds stop at the edge.** The edge keeps a copy of dispatch's routing table
— the six event kinds above — and answers anything else (`ping`, `push`,
`issue_comment`, kinds GitHub adds later) with a 2xx before Restate is involved, since
the only thing the invocation would do is drop the event. The cost of the copy is that
the two tables can drift: a kind added to dispatch without being added to the edge is
acknowledged and never forwarded. Both sites carry a comment pointing at the other.

**One Restate entry point, not four.** An earlier draft had the webhook Worker calling
`PullRequest.sync` and `PullRequest.closed` directly — which contradicted §3b, where
those same handlers were marked private. Routing everything through a single
`WebhookIngress.dispatch` fixes more than the contradiction: the Worker stops needing to
know PullRequest's key format or handler shapes, and the event→handler routing lives in
Rust next to the octocrab types that already parse these payloads. The Worker's whole job
becomes verify-HMAC-and-forward, which is what you want from something running on the
edge.

**`unlabeled` too.** You store and filter on labels, so reacting only to `labeled`
leaves a removed label stale until the next hourly sweep.

**Commit-based events don't carry a PR number.** A `status` webhook gives you a
repository and a commit SHA. Its `branches` array is capped at ten entries and a matching
SHA isn't necessarily a branch head, so you cannot address a `PullRequest` object from it
directly — `status → PullRequest.sync` is unimplementable as an earlier draft drew it.
Route commit-keyed events through a resolution step instead:

```
RepoSync.sync_sha(repository_id, sha)
  → SELECT id FROM pull_requests WHERE repository_id = ? AND head_sha = ?
  → matches:    PullRequest.sync for each (one-way)
  → no matches: ignore
```

**No-match must mean ignore, not reconcile.** A repository emits `status` and `check_run`
events for every commit on every branch — the vast majority have nothing to do with a
Dependabot PR. Falling back to a full `reconcile()` on an unrecognised SHA turns ordinary
CI activity into a storm of repo-wide PR listings, which is precisely the rate-limit
failure mode §2 works to avoid. The hourly sweep already covers anything genuinely
missed.

If you later want to resolve unknown SHAs, use *list pull requests associated with a
commit* — a single narrow call — rather than a reconcile.

`check_suite` and `check_run` payloads *do* carry a `pull_requests` array — use it when
present, but it can be empty (forks) or hold more than one entry, so write that path for
zero-or-many rather than assuming exactly one. Falling back to the SHA lookup keeps one
code path for all three event types.

This needs an index on `(repository_id, head_sha)`.

(Per-PR event storms from `check_run` are absorbed by the sync debounce in §2 — the SHA
lookup itself is a cheap local query and needs no throttling.)

**`sync` decides whether the PR still belongs in the table.** GitHub does not guarantee
webhook delivery order. Without a guard you get: `closed` deletes the row → a late
`status` for the same SHA arrives → `sync` fetches, builds a snapshot, upserts → a merged
PR is resurrected into the dashboard and sits there until the hourly sweep. So make the
canonical fetch authoritative:

```
PullRequest.sync
  → GET pull request
  → if state != "open" || author != "dependabot[bot]":
        clear object state, delete_pr(), return
  → ... otherwise build snapshot and upsert as normal
```

`closed()` then becomes a fast path rather than the only defence, and reconciliation
stays the final backstop. Three layers, each cheap.

**Route `pull_request.closed` separately.** Merged and closed PRs must leave the read
model, or the dashboard accumulates dead rows forever. Since the product *is* an
open-PR dashboard, delete the row rather than keeping a `state = closed` column —
nothing queries it. The reconciliation sweep is the backstop for missed events.

**Subscribe to `status`, not just `check_suite`.** GitHub has two independent
mechanisms: the Checks API and classic commit statuses. A CI integration that reports
via commit statuses produces no `check_suite` event at all, so those PRs would show a
permanently empty check column. Both events do the same thing here — trigger the
canonical `sync`, which recomputes the rollup from both sources. Don't try to
incrementally patch check state from individual payloads.

---

## 3b. Restate ingress is not a public API

`POST /BulkAction/{id}/run` can merge repositories. Every service is reachable through
ingress by default, so this needs saying explicitly: the browser never talks to Restate.
Only the web backend and the webhook Worker do, authenticated with a Restate Cloud API
key.

`ingressPrivate` is available at **both service and handler level** — Restate documents
per-handler configuration alongside per-service. So the granularity isn't the constraint;
reachability is. Anything an external caller invokes must be public, which is why
`WebhookIngress` exists.

| Public (ingress-reachable) | Private (Restate-internal only) |
|---|---|
| `BulkAction.run`, `BulkAction.progress` | `PullRequest.*` |
| `WebhookIngress.dispatch` | `InstallationSync.*`, `RepoSync.*`, `TokenStore.*` |
| `DashboardIngress.sync_installation`, `DashboardIngress.sync_pull_request` | |

Marking whole services private is simpler to reason about than per-handler flags; reach
for handler-level only if you later need one shared handler exposed from an otherwise
private service.

---

## 4. Storage trait

> **Test this abstraction early.** `?Send` suits the single-threaded Workers runtime, but
> the Restate binary is a normal multi-threaded tokio process where `Send` futures are the
> norm — and it's now the primary writer. Forcing one boxed-future contract onto both
> runtimes may fight you. Two `cfg`-gated impls (or two traits) can be cleaner than one
> lowest-common-denominator signature. Cheap to find out in an afternoon; expensive to
> discover after the store layer is written.

```rust
#[async_trait(?Send)]   // ?Send: Workers runtime is single-threaded
pub trait PrStore {
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<()>;
    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>>;
    async fn delete_pr(&self, key: &PrKey) -> Result<()>;
    /// Reconciliation: drop rows for this repo not in `live` AND synced before the
    /// listing started. The `synced_before` guard keeps rows written by concurrent
    /// webhook syncs alive — without it, a PR opened mid-reconcile gets deleted.
    /// Returns the keys it deleted, and queues the same keys for retirement under
    /// `synced_before` in the delete's own transaction, so the caller need not trust
    /// this call's return value to reach them.
    async fn retain_prs(&self, repository_id: u64, live: &[u64], synced_before: u64) -> Result<Vec<PrKey>>;
    async fn list_prs(&self, f: &PrFilter, page: Page) -> Result<Vec<PrRecord>>;
    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<()>;
    /// Drop repos (and cascade their PRs) no longer in the installation.
    /// Same `synced_before` guard as retain_prs, for the same race. Returns the
    /// cascaded PR keys, queued for retirement under `synced_before`.
    async fn retain_repos(&self, installation_id: u64, live: &[u64], synced_before: u64) -> Result<Vec<PrKey>>;
    /// What `sync_now` calls: upsert every listed repo, then retain_repos over their
    /// ids, in one transaction, so a new repo's row is in place before its PRs arrive.
    async fn replace_installation_repos(&self, installation_id: u64, repos: &[RepoRecord], synced_before: u64) -> Result<Vec<PrKey>>;
    /// Drop every repo of a deleted installation and its PRs; returns the PR keys,
    /// queued for retirement with no fence: nothing can reopen them.
    async fn purge_installation(&self, installation_id: u64) -> Result<Vec<PrKey>>;
    /// Every pull request a prune removed and no drain has acknowledged, oldest first.
    async fn pending_retirements(&self) -> Result<Vec<Retirement>>;
    /// Forget every retirement up to and including `through`, the last one read; ids
    /// only grow, so anything queued since stays for the next drain.
    async fn acknowledge_retirements(&self, through: u64) -> Result<()>;
    /// Keep a finished batch for audit. A batch id already recorded is left as it
    /// was, so the workflow's recording step is safe to run again.
    async fn record_batch(&self, batch: &BatchRecord) -> Result<()>;
    /// The most recently finished batches, newest first, targets and verdicts included.
    async fn recent_batches(&self, limit: u32) -> Result<Vec<BatchRecord>>;
}

pub struct PrFilter {
    pub repos: Vec<String>,
    pub update_types: Vec<UpdateType>,     // Major | Minor | Patch | Unknown
    pub check_status: Option<CheckStatus>, // Success | Failure | Pending | None
    pub labels: Vec<String>,
    pub dependency: Option<String>,
}
```

**D1 is the wrong choice once Restate is a separate binary.** D1 is only comfortably
reachable from inside a Worker; a standalone process has to go through an awkward HTTP
API or bounce writes through the web app. Since both the Restate binary and the web app
need to write the read model, pick a database both can reach directly.

**libSQL/Turso is the natural fit.** Same SQLite dialect, so the "SQLite locally,
something else in prod" constraint becomes a connection-string change rather than a
second SQL implementation: a local file for dev, a remote endpoint for prod, reachable
from a native binary *and* a Worker. Keep the `PrStore` trait regardless — it costs
nothing and preserves the exit.

Fallback if you'd rather not add a dependency on Turso: plain Postgres. Loses the
local-file story, gains ubiquity.

**Schema**

```sql
CREATE TABLE repositories (
  repository_id   INTEGER PRIMARY KEY,   -- GitHub's immutable id
  installation_id INTEGER NOT NULL,
  owner           TEXT NOT NULL,
  repo            TEXT NOT NULL,
  merge_method    TEXT,                  -- per-repo override; NULL = global default
  synced_at       INTEGER NOT NULL
);
CREATE INDEX idx_repo_install ON repositories(installation_id);

CREATE TABLE pull_requests (
  id             TEXT PRIMARY KEY,   -- {repository_id}#{number}, immutable
  repository_id  INTEGER NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  owner          TEXT NOT NULL,      -- display/routing only; can change
  repo           TEXT NOT NULL,      -- display/routing only; can change
  number         INTEGER NOT NULL,
  title          TEXT NOT NULL,
  html_url       TEXT NOT NULL,
  dependency     TEXT,               -- NULL for grouped updates
  from_version   TEXT,               -- NULL for grouped updates
  to_version     TEXT,               -- NULL for grouped updates
  dependencies   TEXT NOT NULL DEFAULT '[]',  -- JSON: full updated-dependencies list
  update_type    TEXT,               -- major|minor|patch|unknown; highest in the group
  head_sha       TEXT NOT NULL,
  check_status   TEXT NOT NULL,
  mergeable      TEXT,               -- GraphQL mergeStateStatus lowercased (= REST mergeable_state):
                                     -- clean|dirty|blocked|behind|unstable|draft|has_hooks|unknown
                                     -- (core::Mergeable);
                                     -- `dirty` is the merge-conflict state; NULL reads as unknown
  labels         TEXT NOT NULL DEFAULT '[]',
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,   -- GitHub's updated_at
  synced_at      INTEGER NOT NULL    -- when WE last fetched; drives staleness UI
                                     -- and scopes reconciliation deletes (synced_before)
);
CREATE UNIQUE INDEX idx_pr_number ON pull_requests(repository_id, number);
CREATE INDEX idx_pr_sha        ON pull_requests(repository_id, head_sha);  -- webhook SHA lookup
CREATE INDEX idx_pr_order      ON pull_requests(updated_at DESC, id DESC); -- stable paging
CREATE INDEX idx_pr_dependency ON pull_requests(dependency);               -- the dependency filter

-- Finished bulk actions, for audit; append-only, written once per batch id by the
-- BulkAction workflow's last step. Not tied to pull_requests: a merged PR leaves that
-- table, and the record must not go with it.
CREATE TABLE batches (
  batch_id     TEXT PRIMARY KEY,   -- the workflow key, a UUIDv7
  action       TEXT NOT NULL,      -- merge | rebase | update branch
  requested_by TEXT NOT NULL,      -- the dashboard user who confirmed it
  started_at   INTEGER NOT NULL,
  completed_at INTEGER NOT NULL,
  succeeded    INTEGER NOT NULL,
  rejected     INTEGER NOT NULL,
  failed       INTEGER NOT NULL
);
CREATE INDEX idx_batch_completed ON batches(completed_at DESC, batch_id DESC);

CREATE TABLE batch_targets (
  batch_id      TEXT NOT NULL REFERENCES batches(batch_id) ON DELETE CASCADE,
  position      INTEGER NOT NULL,  -- batch order
  repository_id INTEGER NOT NULL,
  owner         TEXT NOT NULL,
  repo          TEXT NOT NULL,
  number        INTEGER NOT NULL,
  title         TEXT NOT NULL,
  html_url      TEXT NOT NULL,
  outcome       TEXT NOT NULL,     -- JSON: succeeded { detail } | rejected { reason } | failed { detail }
  PRIMARY KEY (batch_id, position)
);

-- The retirement outbox (§2 RepoSync): the pull requests a prune removed whose
-- objects have not yet been told. Written in the prune's transaction, drained by
-- the sweep afterwards. AUTOINCREMENT keeps ids monotonic across deletes, so a
-- drain acknowledges with `id <= last read` and anything queued since stays.
CREATE TABLE pull_request_retirements (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  repository_id INTEGER NOT NULL,
  number        INTEGER NOT NULL,
  synced_before INTEGER            -- the prune's fence; NULL for a purge's unconditional close
);
```

**Grouped updates need the list, not just the scalars.** The `updated-dependencies` block
can hold many entries (§5). Keep the scalar columns for the common single-dependency case
— they keep the single-dependency filter a plain column comparison (add an index on
`dependency` if that filter turns out to be hot) — but set them NULL when there's more
than one and put the full list in `dependencies`. Without this the dependency filter's
behaviour on grouped PRs is undefined, which is exactly the sort of thing that silently
hides PRs from a triage view. Decide now whether the filter matches *any* dependency in a
group (it probably should).

**`retain_repos` cascades.** With the FK above, deleting a repository row removes its PRs
in one statement — which is what makes the §2 installation-level reconciliation actually
enforceable rather than aspirational. The cascade is silent, though: a `DELETE FROM
repositories` cannot `RETURNING` the PR rows it takes with it. To report them, delete the
stale repositories' PRs first with `RETURNING`, then the repositories, then queue the keys
in `pull_request_retirements`, all inside the sync's transaction — the same shape
`purge_installation` uses. `retain_prs` is one `DELETE ... RETURNING` plus the same queue
insert, and opens a transaction for the pair.

**Keep `synced_at` separate from `updated_at`.** They answer different questions: one is
"when did this PR last change on GitHub", the other is "how much do I trust this row".
Only the second can tell you the sweep has stalled — and only the second can safely scope
a reconciliation delete.

**Order by `(updated_at DESC, id DESC)` and page by cursor, not offset.** A bulk-action
UI mutates the rows underneath the user constantly; offset pagination silently skips and
duplicates rows as things shift. The `id` tiebreak matters — `updated_at` alone is not
unique and the order becomes nondeterministic across pages.

---

## 5. Update type — parse the commit metadata block, treat it as a hint

Titles vary (`Bump x from 1 to 2`, `chore(deps): bump ...`, custom prefixes), so the
commit message is the better source. But it is **not** a set of git trailers — an earlier
draft got this wrong. Dependabot appends a YAML document delimited by `---` and `...`:

```
Bump org.jetbrains.kotlin:kotlin-reflect from 1.9.24 to 2.0.0

---
updated-dependencies:
- dependency-name: org.jetbrains.kotlin:kotlin-reflect
  dependency-type: direct:production
  update-type: version-update:semver-major
...
```

Parse the block between the `---` and `...` markers as YAML and read the
`updated-dependencies` list. Grouped updates produce multiple entries in that list — for
the MVP, render "N dependencies" and take the highest update type in the group. Semver-
diffing the title is the fallback when the block is absent or malformed.

(Note that on some ecosystems the summary line goes missing entirely and the commit
subject renders as bare `---`, so don't rely on the subject line either.)

**Treat `update_type` as display metadata, not truth.** There is a known Dependabot bug
where a PR opened as a minor bump and later rebased into a major bump keeps the stale
`update-type` in its commit metadata — which is also why `fetch-metadata` can report the
wrong type. For a dashboard you skim before clicking merge, that's an acceptable filter
and label. It is emphatically **not** a safe authorization predicate: if you later add
unattended auto-merge, do not let "metadata says patch" be the thing that permits the
merge. Re-derive the version delta from the PR itself for anything unattended.

---

## 5b. Check rollup — write the truth table, then unit-test it

`sync` merges two sources with different vocabularies into one four-value column, and the
collapse is where accidental behaviour creeps in.

Checks move through a *status* and, once `completed`, receive a *conclusion*. GitHub
treats `neutral` and `skipped` as successes for dependent checks; `stale` means the run
was marked stale by GitHub **because it took too long** — that's a failure, not a
supersession (an earlier draft had this backwards).

| Source value | Contributes |
|---|---|
| conclusion `success`, `neutral`, `skipped` | pass |
| conclusion `failure`, `timed_out`, `action_required`, `cancelled`, `stale` | fail |
| status `queued`, `in_progress`, `waiting`, `pending`, `requested`, `expected` | pending |
| suite status `startup_failure` | fail |
| commit status `success` | pass |
| commit status `failure`, `error` | fail |
| commit status `pending` | pending |

Rollup, in order:

```
any fail                  → Failure
else any pending          → Pending
else at least one pass    → Success
else (nothing reported)   → None
```

Failure beats pending deliberately: a red build stays visible in the filter instead of
hiding behind an unrelated queued job.

**Paginate the check runs.** GraphQL pages `statusCheckRollup.contexts` and `checkSuites`
at 100 (REST's *List check runs for a ref* returned 30 by default, max 100). A commit with
a matrix build easily exceeds a page, and the failure you care about is as likely to be on
page 2 as page 1 — so a single unpaginated read can render a red PR green. Follow every
cursor before rolling up. This is a one-line bug with a very bad blast radius in a
bulk-merge tool.

**Tell "no statuses" from "pending".** REST's combined-status endpoint reports `state:
pending` both when something is genuinely pending *and* when there are zero statuses —
so `state` alone collapses "no CI configured" and "CI hasn't finished", which mean
opposite things at merge time; there, `total_count == 0` was the tell. GraphQL lists the
status contexts individually, so each contributes on its own state and an empty list
contributes nothing, which is the same distinction without the counter.

**Subscribe to `check_run` as well.** With only `pull_request.synchronize` →
`check_suite.completed`, there's a window where `sync` runs before Actions has created
any check runs, records `None`, and nothing corrects it to `Pending` until the suite
finishes — so a freshly pushed PR looks like "no CI" for minutes. Handling `check_run`
`created` and `completed` closes that gap, and it needs no permission beyond the `checks:
read` you already have. Route it through `sync_sha` like the others. The per-event cost
is absorbed by the sync debounce (§2) — without that debounce, this subscription is what
turns a matrix build into an API-call storm, so the two ship together.

This is pure logic over an enum. Table-test it; it costs twenty minutes and it's the sort
of thing that's near-impossible to debug from a dashboard screenshot later.

---

## 6. GitHub App — and the constraint that shapes everything

### GitHub Apps cannot use Dependabot commands

`dependabot/dependabot-core#9147` (open since Feb 2024, unassigned): a Dependabot command
posted by a GitHub App holding repo write access is refused with

> Sorry, only users with push access can use that command.

This applies to every command, `rebase` included. The documented workaround is a token
belonging to a real user. **Verify this before anything else — it invalidates any design
where the App identity issues Dependabot commands.**

### Consequences

**Merge: use the REST endpoint, not `@dependabot merge`.**
`PUT /repos/{owner}/{repo}/pulls/{n}/merge` is an ordinary API call — installation token
works, Dependabot isn't involved. It also fits the workflow better: `@dependabot merge`
means "merge when CI goes green", but the table has already filtered to green, so what you
want is the synchronous call that returns a result you can put in the batch summary.

**Rebase: pick one.**

| Option | Identity | Trade-off |
|---|---|---|
| `@dependabot rebase` comment | **user token** | True rebase; Dependabot keeps managing the branch. Requires a user identity. |
| `PUT /pulls/{n}/update-branch` | installation | Merges base into head rather than rebasing. CI re-runs against current main — the actual goal. But Dependabot stops auto-rebasing that PR afterwards: it halts once extra commits land unless the commit message contains `[dependabot skip]`, and this endpoint can't set a commit message. |
| Git Data API force-push | installation | Full control including commit message. Considerably more work. |

**Recommended: hybrid auth.** Installation token for reads, webhooks and merges; a
user-scoped token *only* for posting Dependabot commands.

```
InstallationToken  →  list repos, list PRs, read checks, merge, update-branch
UserToken          →  @dependabot <command> comments
```

This makes "impersonation" load-bearing rather than a deferred nicety. For a single-user
MVP a fine-grained PAT is proportionate; user-to-server OAuth is the multi-user version of
the same mechanism.

> **Unverified.** PATs are confirmed to work. User-to-server tokens *should* behave
> identically since the comment is authored by the user — but this is documented nowhere.
> Test with a single comment before building on it.

### Setup

**Permissions:** `pull_requests: write`, `contents: write` (the merge endpoint writes to
the base branch), `checks: read`, `statuses: read`, `metadata: read`.

Note the API key is `statuses` — "Commit statuses" is only the display name in the App
settings UI.

**Subscribe to:** `pull_request`, `check_suite`, `check_run`, `status`.
`installation` and `installation_repositories` arrive automatically and aren't part of
the subscription list.

**Auth chain:** RS256 JWT signed with the App private key (10 min) → installation access
token (1 h) → cached, refreshed on 401.

**Merge method must be explicit.** `PUT /pulls/{n}/merge` takes `merge_method` of
`merge`, `squash` or `rebase`, and repository settings can disallow any of them. Leaving
it implicit means the resulting Git history depends on a per-repo default you never
looked at. For the MVP: one configured global default (`squash` is the sane pick for
dependency bumps), overridable per repo later. Reading `allow_squash_merge` /
`allow_merge_commit` / `allow_rebase_merge` off the repo and choosing deterministically
is the version that never 405s. (The one 405 you'll still see is the transient
base-branch-modified one — handled in the classifier, §2.)

Keep the comment attribution footer (`— via dependabot-dashboard (mark)`) from day one.
Retrofitting an audit trail into already-merged PRs isn't possible.

---

## 6b. Crate choices

**`octoevents` for the receiving edge.** It turns an untrusted request into a verified
`Envelope`: constant-time HMAC over `X-Hub-Signature-256`, the `X-GitHub-Delivery` and
content-type checks GitHub's contract requires, a body cap, and the status mapping to
answer with (`ResponseStatus::for_receive_error`). It stops there, which is the right
shape — routing stays in `WebhookIngress`, not in the transport.

The `Envelope` already carries the installation and repository probe every event shares,
so the handler only parses the per-event routing fields — PR number, head SHA, and the PRs
a check belongs to — with shallow serde structs of its own. That is `Envelope::parse`, not
the crate's optional `octocrab` feature: those four fields are shallow, and depending on
octocrab's beta webhook models to read them buys nothing.

**Do not hand-roll HMAC verification.** The comparison is the easy part; the delivery
contract around it (which failure is a `400` and which a `401`, refusing an unsigned
request before it occupies `body_limit` bytes, rejecting a form-encoded body, leaving
`ping` unrouted) is where a hand-rolled handler drifts from GitHub's expectations.

**Skip the App frameworks.** `octofer` and `octoapp` are Probot-shaped — octofer bundles
JWT generation, installation-token management, a built-in HTTP server with HMAC
verification, and event routing. Wrong shape here: they want to own the HTTP server and
the event loop, which collides with Restate owning orchestration. Both are also young and
thin. Take a receiving-edge crate and a plain API client; write the ~50 lines of glue
yourself.

**Wasm caveat.** `octoevents` is sans-I/O over `http` types and pulls no runtime, so the
receiving edge is not what would block a `wasm32` Worker. Check the API client instead:
that side is what pulls hyper/tokio.

---

## 6c. User tokens across a process boundary

The Restate service is a separate binary with no browser attached, so it can never obtain
a user token itself. This section is how one gets there.

### Issuance happens only in the web app

```
UI → GET https://github.com/login/oauth/authorize?client_id=…&state=…
   → user approves
   → GitHub redirects to your callback with ?code=…
   → POST https://github.com/login/oauth/access_token  (client_id + client_secret + code)
   → { access_token, expires_in, refresh_token, refresh_token_expires_in }
```

With expiring user tokens enabled (the default for new Apps) the access token lasts ~8
hours and the refresh token ~6 months; you renew with `grant_type=refresh_token`. The
refresh token rotates on every use.

### Never put the token in the Restate payload

Restate journals invocation inputs and `ctx.run()` return values durably. A token in a
workflow payload is persisted to the log, visible through the CLI and UI, and replayed on
recovery. Pass the **user id**; resolve the token inside the side effect.

```rust
// PullRequest::command
let comment_id = ctx.run("post-comment", || async {
    let token = tokens.user_token(user_id).await?;   // read, not journaled
    gh.issue_comment(&token, owner, repo, number, &body).await
}).await?;
```

Everything inside `ctx.run` is a side effect; only the returned value is journaled.
Return the comment id, never the credential.

### Refresh is the part that bites

GitHub refresh tokens are single-use. Two concurrent refreshes and the loser invalidates
the grant, dropping the user's authorization entirely — they have to re-authorize by hand.
You already have the tool for this: a `TokenStore` virtual object keyed by user id.

```rust
// serialise the refresh; returns unit so nothing sensitive crosses a journal boundary
ctx.object_client::<TokenStore>(user_id)
    .ensure_fresh()
    .call()               // NOT .send()
    .await?;
// only now read the fresh token from libSQL inside ctx.run, as above
```

**`.call()`, not `.send()`.** A Restate send is one-way: awaiting it confirms the message
was durably enqueued, not that the handler finished. With `.send()` the subsequent DB read
can race ahead of the refresh and pick up the stale token — intermittently, and only under
load, which is the worst kind of bug. `.call()` waits for completion. Returning `()` keeps
the token out of the journal either way.

Keyed concurrency then serialises the refresh for free.

### For the MVP, skip all of it

A fine-grained PAT is one env var in the Restate service. Put it behind the interface now
so the OAuth version drops in without touching call sites:

```rust
#[async_trait]
trait TokenProvider {
    async fn user_token(&self, user: UserId) -> Result<SecretString>;
}
```

`EnvPatProvider` today, `DbOAuthProvider` when there are real users. Store tokens
encrypted at rest in libSQL, not in Restate state — Restate state is for orchestration,
not secrets.

> **Resolved:** creating a PR conversation comment requires *either* `issues: write` or
> `pull_requests: write`. You already have the latter, so nothing to add.

---

## 7. Deliberately not in the MVP

- **Rebase progress tracking.** Deferred, with the research done. Three candidate signals
  when you come back to it:
  1. **Thumbs-up reaction** on the command comment — Dependabot's acknowledgement that it
     received the command. Reliable, cheap, available via the reactions API.
  2. **Push to the PR head** after the command comment — proof the rebase actually landed.
     Arrives as `pull_request.synchronize`, which you already handle.
  3. **The Dependabot Actions run.** Dependabot's compute moved to Actions, and GitHub
     states that Actions APIs and webhooks can detect failed runs. But the Dependabot REST
     API covers only alerts and secrets — there is no update-jobs endpoint — and nothing
     documents mapping a workflow run back to a specific PR. Treat as a spike, not a
     dependency. (Also: jobs can only be *triggered* from the Dependabot UI, not via API,
     so the comment command stays the only programmatic trigger.)

  Signals 1 and 2 are the load-bearing pair. Signal 3 is a bonus if the spike pans out.
- PR preview / check-log tailing
- Permission layers and audit-log UI (but *not* user-token auth — that moved into scope, see §6)
- App-submitted approvals + branch protection bypass
- Close, plain comments, arbitrary Dependabot commands
- Multi-org, multi-user

---

## 8. Resolve before you start

1. **Post one `@dependabot rebase` comment with a user token and confirm it's accepted.**
   Everything else is recoverable; this one determines whether bulk rebase — the feature
   that motivated the project — is reachable at all. Ten minutes of work, and it gates the
   auth design.
2. **Confirm the merge endpoint works with the installation token** on a real
   branch-protected repo, with an explicit `merge_method`. Validates the App-identity
   half of the hybrid.
3. **Are you sure you want a table before an exception queue?** Everything above assumes
   the table is the product. If in practice you skim it and merge every green patch bump
   without thinking, native auto-merge does that for free and the interesting product is
   the smaller list of PRs that *aren't* green. Worth checking against one week of real
   Dependabot traffic before committing to the bigger build.

Dioxus-on-Workers is no longer on this list — the Axum fallback removes the risk.
