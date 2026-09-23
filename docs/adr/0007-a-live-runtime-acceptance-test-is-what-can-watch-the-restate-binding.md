---
status: accepted
---

# A live-runtime acceptance test is the only thing that can watch the Restate binding, and it is worth building for the handful of lines that need it

Three mutation sweeps over `bulk_action`, `installation_sync`, `repo_sync` and
`retirement` left about twenty survivors in one place: the `Restate*` effects adapters and
the `#[restate_sdk::object]` handler bodies. Nothing in `cargo test` can reach them.
ADR-0004 explains why for the webhook dispatch — `ContextClient` is sealed behind a
`pub(crate)` supertrait, the generated clients hold a private `&ContextInternal`, and
`ContextInternal::new` is `pub(super)` — and the same wall stands in front of every other
adapter in the tier. The effects traits are drawn deliberately above that wall so the
decisions can be tested; what is left below it is the wiring, and the wiring is exactly
what no fake can watch.

Two of those survivors are severe. `schedule_tick` re-arms the sweep with
`.send_after(self.interval)`, and `.send()` is a one-token slip that compiles, passes, and
runs the installation sweep in a tight loop against the GitHub API. `RestateReconcileEffects`
used to be built by naming `owner` and `repo` at two call sites, and transposing them is a
404 on every listing of every repository — that one is closed now, by extracting the
mapping into a value a test can hold, but the shape of the hazard is general and the
extraction does not reach the delay, the state keys or the generation handed to each call.

So the question is whether the binding is worth standing a runtime up for, and the way to
answer it was to try. It works, it is not expensive, and it catches what it was supposed to
catch.

The harness is: Restate from `compose.yaml`; the service binary with `GITHUB_API_URL`
pointed at a stub that answers a token and an installation with no repositories;
`LIBSQL_URL` on a temporary file; and `RECONCILE_INTERVAL_SECONDS` turned down from an hour
to two seconds, which is what makes a schedule observable inside a test rather than inside
an afternoon. The service registers itself at `POST /deployments` on the admin port and
arms its own scheduler at startup, so the chain runs without anything being driven through
the ingress. Counting the stub's listings over twelve seconds then measures the sweep's
cadence directly.

With `send_after(2s)`: six sweeps in twelve seconds. With `send()`: **two hundred and
thirty-three**. That is the unthrottled loop, and it is not subtle — no threshold needs
tuning, and a test asserting "at most a handful" separates the two by a factor of forty.
Standing the whole thing up, from `docker compose up` to a number, takes under a minute on
an already-built binary.

Two things learned in the building of it are worth writing down, because both cost a
confusing measurement before they were understood. Restate's virtual object state is
durable across service restarts, so a second run against the same object key finds the
scheduler already armed, answers `AlreadyArmed`, and sends no tick — a test must either use
a fresh key per case or bring the runtime up on a fresh volume. And chains left armed by an
earlier case go on ticking into a later one: a baseline that should have read six read
seventeen, because three objects from three runs were all sweeping at once. Isolation here
is the volume, not the process.

What this does not become is a second test suite. Everything the effects traits already
cover stays where it is — those tests are fast, they are the ones that say what the
handlers decide, and they run on every change. The live test is for the wiring beneath
them, which is a small and slow-moving surface: the delay a chain re-arms with, the keys
its state is written under, the identifiers handed to each call, and the one-way sends a
route becomes. A handful of cases, run where a minute is affordable — on a pull request
rather than on a push, or nightly — is the whole of it.

It is not built here. What this ADR records is that it can be, what it costs, and what it
is for; the sweep's cadence is the first case to write, and the second is the one that
reads a virtual object's state back through the admin API to pin the keys it was written
under.

## Considered options

- *Leave the binding untested and rely on review.* Rejected: it is the layer where a
  one-token slip is invisible to the compiler and to the whole suite, and two of the slips
  available there — an unthrottled loop against a third party, and a 404 on every listing
  in the fleet — are outages rather than defects. Three sweeps found them by accident; a
  fourth reviewer might not.
- *Widen the effects traits until the adapters are thin enough not to matter.* Rejected,
  and already partly done where it is honest: the value-construction half of an adapter can
  be extracted and tested, as `Addressed` and `submission` were. But the delay, the state
  keys and the object a send is addressed to are not values the caller supplies — they are
  what the adapter is — and a trait that covered them would need one method per send, which
  is the match again, typed. ADR-0004 rejects that shape for the dispatch and it is the
  same shape here.
- *Test against a mock of the Restate protocol rather than the runtime.* Rejected: the
  properties worth asserting are Restate's own — that a delayed send arrives after the
  delay and not before, that state survives a restart, that a journaled send is not rolled
  back. A mock of the protocol would be a second implementation of the thing under test,
  and it would agree with whatever we believed when we wrote it.
