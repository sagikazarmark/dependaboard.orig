---
status: accepted
---

# The installation is a parameter of every projection read, not a property of the reader

`4492d63` gave six of `ProjectionReader`'s seven methods a leading `installation_id`, and
the web edge passes `ServerState`'s field to each of them. An architecture review read that
repetition as friction and proposed binding it once — `reader.for_installation(id)`, a
handle the edge holds — so that a read could not forget its tenancy. We keep the parameter.

The projection is genuinely multi-installation, and only the web deployment is not.
`ProjectionWriter` names installations outright (`purge_installation`,
`replace_installation_repos`), the reader answers `Foreign` for a key belonging to another,
and the store's own tests read two installations from one database to prove each is shown
only its own. What is single is the deployment's configuration, and that is already held
exactly once, in `ServerState`; binding would move a `u64` that has one home into a second
type that forwards seven methods minus that `u64` — an interface as large as its
implementation, which is to say no depth at all. It would need a second trait with one
implementation to hold it, which is a hypothetical seam, and `projection_revision` could
not travel with it — it is deployment-wide on purpose — so the edge would still hold two
handles at the end of it.

The parameter also carries what binding would hide. A reader of the trait sees tenancy in
every signature that has it, and sees the one exception in the signature that does not:
`projection_revision`'s missing argument is what makes its documented exception legible at
a glance, to a person or an agent reading only the trait.

The reason to write this down rather than leave it to taste is the trap under the obvious
next step. Binding does **not** make `ProjectedPr::Foreign` unreachable: the `PrKey` still
arrives from a browser and can still name another installation's row. But a binding reads
as though it does, and someone who believes it will fold `Foreign` into `Absent` as a state
the types have ruled out — turning "this deployment never showed that pull request" into
"no longer in the dashboard", which is a different sentence, a different meaning, and one
`README.md` promises. That regression is reachable by a change that looks like tidying, and
it is the whole cost of getting this wrong.

None of this is an argument that the repetition is pleasant. If it ever does bite, the
shape to reach for is the third option below, not the first.

## Considered options

- *A bound projection from the reader (`reader.for_installation(id)`).* Rejected: shallow —
  it restates the reader's method set minus one argument, and must repeat every invariant
  the reader documents or callers lose them. It needs `Arc<Self>` as the receiver to be
  object-safe and held, forcing the store's own tests to wrap a bare `LibSqlPrStore` in an
  `Arc`; it cannot carry `projection_revision`; and it does not deliver the claim made for
  it, since an unscoped read does not stop compiling — with both traits in scope on the
  concrete store, `store.get_pr(&key)` still resolves to `ProjectionWriter::get_pr`.
- *An `InstallationId` newtype in `crates/core`, taken by the six reads.* Rejected: it
  addresses confusion — a bare `u64` transposable with `repository_id` — rather than the
  repetition that prompted the review, and that confusion has not bitten. 267 mentions
  across 30 files is a large edit for a hazard nothing has met.
- *A web-side adapter: `struct Installation { store, id }` with six delegating methods in
  `apps/web/src/server/`.* Rejected for now, and the one to revisit: it buys the same
  brevity at the edge for about fifty lines, with no second trait, no object-safety
  gymnastics, and `crates/store` and `spec.md` §4 untouched. Nothing today asks for it —
  seven call sites each passing one field is not yet a cost — but if the edge grows more
  reads, this is the change to make, and it does not disturb the contract this ADR is about.
