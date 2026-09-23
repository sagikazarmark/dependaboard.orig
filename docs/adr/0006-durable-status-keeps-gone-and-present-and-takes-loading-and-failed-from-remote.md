---
status: accepted
---

# `DurableStatus` keeps `Gone` and `Present`, and takes `Loading` and `Failed` from `Remote`

An architecture review read `DurableStatus` (`apps/web/src/ui/detail_drawer.rs`) as
`Remote<Option<PrState>>` (`apps/web/src/ui/dashboard_state.rs`) wearing different clothes —
`Loading` for `Loading`, `Failed` for `Failed`, `Gone` for `Loaded(None)`, `Present` for
`Loaded(Some(_))` — with `from_resource` repeating by hand a mapping `Remote` already owns. It
proposed deleting the enum for a type alias, and reported the deletion test as passing:
complexity vanishes. The two arms it is right about have since been shared; the two it is
wrong about stay. This ADR is here because the proposal is easy to make again and the code no
longer shows why it was refused.

`Gone` is not a spelling of `Loaded(None)`; it is the name of a fixed defect. Before
`1f8fe56` — *"retire pull-request object state when reconcile or purge prunes its row"* — a
reconcile or a purge deleted the projection row and left the `PullRequest` object's state
standing, and the drawer could not tell an answer still in flight from an object holding
nothing. That commit's own summary of the user-visible half is the sentence this variant
exists to keep true: *"The drawer's durable-state section now tells 'still loading' from 'the
object holds nothing', so a closed-while-offline PR reads as no longer open instead of
spinning forever."* A pull request closed while a webhook was missed spun a spinner
indefinitely. The test that pins it pins it by that name —
`an_empty_status_answer_means_the_state_is_gone_not_still_loading` — and it asserts exactly
the distinction an alias would ask a reader to reconstruct from `Loaded(None)` against
`Loading`.

The name is also shared vocabulary inside the one file, at two layers that a reader has to
keep apart. `OpenPr::Gone(Box<PrRecord>)` is the *row* leaving the read model — merged,
closed, or its repository gone — kept as the drawer last saw it so the drawer can say which
pull request it was; `settled` mints it, the drawer takes it as the `gone` prop, and shows the
row while withholding every action. `DurableStatus::Gone` is the Restate *object* holding
nothing. The two meet where `gone` is one of the two reactive keys — with `row.synced_at` —
that re-fire the status resource, because both mean something happened to the pull request
that Restate may have recorded. Behind that second `Gone` is a back-end concept with its own
name: `apps/restate-service/src/retirement.rs` drains the retirement outbox, `Retirement` is
the queued key, and `pending_retirements` is what a prune leaves behind for the sweep to tell
the object with. `Gone` is what that machinery looks like from the drawer. `Loaded(None)` is
what it looks like from nowhere.

`OpenPr` is the precedent, and it is in the same file. It is a three-variant status enum over
an asynchronous read — `Loading(PrKey)`, `Loaded(Box<PrRecord>)`, `Gone(Box<PrRecord>)` — and
it was deliberately not made a `Remote`, because `Loading` carries the key it is waiting on
and `Gone` carries the last row seen. Nobody has proposed collapsing that one. The reason
`DurableStatus` attracts the proposal and `OpenPr` does not is that `DurableStatus`'s extra
information happens to be expressible as `Option`, which makes the arms line up on the page —
and lining up on the page is not the same as saying the same thing, which is the trap ADR-0003
described in another vocabulary and this one meets again.

The alias does not fit on three further counts, each checkable. `Remote`'s own documentation
says it is "what the dashboard has heard from the read model in answer to one question", and
every instantiation keeps to that — `PageStatus`, `SummaryStatus`, `CapabilitiesStatus`,
`BatchesStatus`, `Remote<Vec<RepoRecord>>`. The durable state is not a read of the read model:
`load_pr_status_in` (`apps/web/src/api.rs:521`) uses the projection only as an installation
gate and then asks Restate. The answer comes from the object, not the projection, and the
`None` that becomes `Gone` can come from either. Second, `DurableStatus` would use neither of
`Remote`'s two methods: `loaded()` has nine call sites across the crate and `map()` has one,
and the durable state is at none of them — its single consumer matches all four arms
exhaustively. The alias would inherit an interface it does not call. Third, at this
instantiation the inherited interface is actively misleading: `loaded()` would return
`Option<&Option<PrState>>`, so `.loaded().is_some()` reads as "did it load?" and answers true
for the gone case — which is the defect of `1f8fe56` reintroduced as a tempting one-liner.

The decisive count is size, and it runs the opposite way from the proposal's premise.
Measured on this target: `DurableStatus` is 24 bytes, because rustc packs its four variants
into `Failed(String)`'s three words; `PrState` is 384 — an `Option<PrRecord>` of 312 alone,
plus two `Vec`s, an `Option<u64>` and a `bool` — so `Remote<Option<PrState>>` is 384 too.
Every other `Remote<T>` payload in the crate is a handful of words: `Capabilities` is 1 byte,
`DashboardPage` 56, `DashboardSummary` 112, and none comes near
`clippy::large_enum_variant`'s 200-byte threshold. `PrState` sails past it. The lint is
warn-by-default, there is no `clippy.toml` in the repository and no `allow` anywhere in it,
and CI denies warnings: `cargo clippy --workspace --all-targets --no-default-features
--features dependaboard-web/server -- -D warnings`, in `README.md`'s *Development
Verification* block and run on a pull request as the `ci:clippy` check in
`.dagger/modules/ci/main.dang`. That lint is *why* the `Box` in `Present` is there. And it
would not fire through the alias — clippy checks the non-monomorphic definition of
`Remote<T>`, where `Loaded(T)` has no size to complain about, so `Remote<Option<PrState>>`
passes silently while making every value of the type sixteen times wider. The collapse does
not satisfy the lint. It blinds it, and it blinds it at the one instantiation the lint had
something to say about.

The deletion test was run the wrong way round. Deleting the enum removes its declaration, its
four variants, and the two arms of `from_resource` that map `Loaded(None)` and
`Loaded(Some(_))`. What has to be re-spelled is everything else the name touches: the
construction, the prop type, the four match arms, the test helper's signature, and six
spellings across the three tests — eleven sites, each of which then says `Remote::Loaded(None)`
where it now says `Gone`, and each of which then needs a reader to know what the `None` means.
Two lines of mapping go; the sentence they were carrying is copied out to eleven places.
Complexity moves outward and the measurement stops at the file where it left.

What is in the code is the half of the proposal that was right. As of `9fb45eb`,
`DurableStatus::from_resource` is written over `Remote::from_faulted`: it matches on the
`Remote` and keeps only the two arms that are this drawer's own, so that nothing yet is
loading, and a fault is its words, are written once for every read the dashboard makes.
`Gone`, `Present` and the `Box` survive. It landed as a consequence rather than as a tidy-up:
guarding the resource reads changed the drawer's resource from
`Result<Option<PrState>, ServerFnError>` to `Result<Option<PrState>, Fault>`, which is the
type `Remote::from_faulted` takes and the reason the shared half became shareable at all —
`Remote::from_resource`, the `ServerFnError` constructor, lost its last caller in that commit
and is gone. The remaining hand-copying is two arms that say something no other read says.

## Considered options

- *`type DurableStatus = Remote<Option<Box<PrState>>>`.* Rejected: it does not compile.
  `Remote::from_faulted` takes `Option<&Result<T, Fault>>`, and the drawer's resource is a
  `Result<Option<PrState>, Fault>` — `T` is forced by the resource's own type to
  `Option<PrState>`, so the boxed alias is simply a different type. Keeping the `Box` would
  need an adaptor at the call site that reads the resource out and boxes it, cloning
  `PrState` — 384 bytes — twice per render of the drawer, to hold a shape the constructor
  will not produce.
- *`type DurableStatus = Remote<Option<PrState>>` (unboxed).* Rejected: it compiles and the
  tests pass, which is the whole of the evidence offered for it. It takes the type from 24
  bytes to 384 at every site that holds one, silences `clippy::large_enum_variant` by hiding
  the payload behind a generic the lint cannot see through, and renames the distinction
  `1f8fe56` was written to create into `Loaded(None)`, which the next reader has to look up.
- *Keeping the enum and deriving `from_resource` from `Remote`'s.* **Accepted**, and what the
  code does as of `9fb45eb`. The two arms every read of the server shares are written once in
  `Remote::from_faulted`; the two that belong to this drawer — the object holding nothing
  after a retirement, and the state it holds when it holds one — keep their names and their
  `Box` in the file that uses them.
