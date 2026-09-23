---
status: accepted
---

# The webhook dispatch sends through the Restate context directly, with no effects trait between, because the SDK seals the context away

`WebhookIngress::dispatch` decides a delivery's route once and then acts on it twice. The
decision is `route_webhook` (`ingress.rs:131`), a pure function from an installation id and a
`WebhookEvent` to a `WebhookRoute`, exhaustive on `DeliveryKind` on purpose and held by six
tests. The two acts follow it: `send_webhook_route` (`ingress.rs:259`) makes the call, and
`HandlerOutcome for WebhookRoute` (`ingress.rs:89`) builds the line `traced` logs, which tells
an operator where the delivery went. The `InstallationLifecycleAction` → handler mapping is
written out in both — as four one-way sends at `ingress.rs:279`–`:292`, and as four handler
names at `:103`–`:108` — so the same four-way choice appears twice, once typed and once as
strings, with nothing linking them.

An architecture review proposed something else, and this ADR is here so it is not proposed
again: a `WebhookDispatchEffects` trait, with a production adapter over the context and a
recording fake, so that the dispatch could be driven from a test the way the rest of this
tier is driven. It is the right instinct and the wrong place, and the reason is not taste.
The argument has two legs, and it needs both.

The first leg is the sealing. `ContextClient` — the trait that owns `object_client`, and so
owns every call this service makes to another object — is declared at
`restate-sdk-0.11.1/src/context/mod.rs:601` with `private::SealedContext<'ctx>` as its
supertrait, and is given to every type that has that supertrait by the blanket impl at
`:725`. `SealedContext` lives in `pub(crate) mod private` at `:1240` and is implemented
exactly five times, all within that module, for the SDK's own contexts. A downstream crate
cannot name it, so it cannot implement it, so it cannot acquire `ContextClient`, so no type
we could write is a context as far as the compiler is concerned. This matters here rather
than in the abstract because the shape a fake context would have to take already exists in
this codebase and is already generic: `close_pull_request` (`pull_request.rs:707`) takes
`ctx: &impl ContextClient<'ctx>` and is called both by the dispatch and by the retirement
drain. Generic over the context is precisely the cheap seam — no new trait, no adapter, no
methods to keep in step — and the sealing is exactly the wall it stops at: the generic
parameter has five possible arguments and we may write none of them.

Two further doors are locked behind the same wall, and they are worth naming so that nobody
spends an afternoon rattling them. The generated clients hold the context privately: the
macro emits `ctx: &'ctx ContextInternal` as an unmarked field, and the only constructor is
`IntoObjectClient::create_client`, whose signature demands that same `&ContextInternal`. So a
fake cannot hand itself an `InstallationSyncClient` either. And `ContextInternal` cannot be
minted outside the SDK: its only constructor is `pub(super) fn new`, called once, by the
endpoint. The conclusion is flat and worth stating plainly: what `send_webhook_route` sends
cannot be observed by any test in this repository, by any arrangement of traits, without a
live Restate runtime on the other end of it.

That is the first leg, and on its own it does not settle the matter — which is worth saying,
because it is tempting to stop there. A wrapper trait *above* the sends needs no fake
context at all, so sealing does not forbid one. The second leg is what such a wrapper would
be worth, and here the comparison with the rest of the tier is instructive, because the shape
is used seven times and every one of them is doing something the dispatch is not. They are
`PullRequestEffects` (`pull_request.rs:124`), `RepoReconcileEffects` (`repo_sync.rs:25`),
`InstallationSyncEffects` (`installation_sync.rs:334`), `SchedulerTickEffects` (`:281`),
`BulkActionEffects` (`bulk_action.rs:55`), `RetirementEffects` (`retirement.rs:32`) and
`GithubStepEffects` (`github.rs:210`). What makes each earn its keep is not that it wraps
effects — it is that it stands under a driver that *branches*. Each of those drivers reads a
clock, a durable state, a listing or a settled GitHub answer through the trait and then
decides something on the strength of it, and a recording fake scripts those answers and asks
what the driver then did.

Several of them do also record bare one-way sends — `send_sync`, `schedule_sync`,
`reconcile_repository`, `persist`, `schedule_tick`, `close_pull_request` — so a void method is
not disqualifying by itself, and an argument that leaned on "the dispatch is fire-and-forget"
would be answering the wrong objection. Those assertions are worth making because the decision
that led to each of them was taken inside the function under test. The dispatch has no such
function. Its decision was taken by `route_webhook`, which is pure, returns a value, and is
already pinned six ways. What is left below it is seven `.send()` calls and nothing else — and
`.send()` yields a `SendHandle` that the dispatch drops unread, so there is not even a value a
fake could lie about. A `RecordedWebhookDispatch` would hold a list of routes it was handed,
and the assertion would read that `route_webhook`'s answer was passed along unchanged: the
match echoed back to the test that supplied it.

The shape of such a trait makes the same point from the other side. A single method taking an
`InstallationLifecycleAction` would not remove the duplicated mapping; it would move the
four-way match down into the adapter, which is the one layer no test can reach, and the fake
would record an action rather than a destination — precisely the distinction the review wanted
held. To record destinations, the trait needs one method per destination, and there are seven:
close a pull request, sync a pull request, sync a commit's repository, and start, sync now,
pause or purge an installation. Seven methods, chosen by a match in the caller, is the match
again with the arms renamed, plus an adapter, plus a fake, plus the standing obligation to add
a method whenever `WebhookRoute` or `InstallationLifecycleAction` grows an arm — an interface
as large as its implementation, which ADR-0002 and ADR-0003 both rejected under their own
circumstances, and which buys here an assertion that the caller called what it decided to call.

What was done instead, as of `bc4a28e`, is to pin the half that is reachable.
`the_completion_line_names_the_object_and_handler_each_route_was_sent_to` walks all eight
completion lines — the three per-kind ones, the four lifecycle ones, and `ignored` — asserting
each against `outcome`, so the service names, the handler names and the key formats are now
written in two places that must agree rather than one that nothing watched. The walk's list
could itself age behind the enums, so it carries two do-nothing exhaustive matches: a variant
added to either enum stops the test compiling until it is named. The mutation that prompted
all this — the `Pause` and `Purge` arms of `outcome` swapped, both real handlers on the same
object, the line reading perfectly ordinarily to an operator asking why an installation was
torn down — now fails.

The cost is to be stated honestly rather than argued away. That test covers the *claim* and
not the *send*. If someone swaps the two `client` arms at `ingress.rs:286` and `:289` instead,
the suite stays green, the log goes on saying `pause`, and a suspended installation is purged.
Nothing in this repository can catch that, and — per the sealing above — nothing could be built
that would, short of standing a Restate runtime up in the test suite and reading the journal
back. Two things make that an acceptable place to stop. The first is that the surface is small
and static: seven typed method calls, each checked by the compiler against a real generated
client, so the failure mode is confined to choosing the wrong one of four sibling handlers
rather than calling something that does not exist. The second is that the two matches now sit
thirty lines apart in one file, each documented as the other's mirror, which is the strongest
link this language will give without a runtime. If a live-runtime acceptance test ever exists
for other reasons, the dispatch is the first thing to point it at.

## Considered options

- *A `WebhookDispatchEffects` trait with a production adapter and a recording fake.* Rejected:
  the fake would record a decision `route_webhook` had already made and returned, so it could
  only echo the match back to the test that supplied it. The trait cannot be made generic over
  the context instead — `ContextClient`'s supertrait `private::SealedContext`
  (`restate-sdk-0.11.1/src/context/mod.rs:601`, `:1242`) is `pub(crate)` and implemented only
  for the SDK's five contexts, the generated clients hold a private `&ContextInternal`, and
  `ContextInternal::new` is `pub(super)`. One method taking an `InstallationLifecycleAction`
  would push the four-way match into the untested adapter; one method per destination is seven
  methods, which is the match again, typed.
- *`send_webhook_route` returning the line it sent, deleting `HandlerOutcome for WebhookRoute`.*
  Recommended by two of three reviewers, and rejected on the third's ground, which decided the
  matter: it moves a string that is testable today into a function no test can call, so the
  single source of truth it creates is a source no assertion can read, and net coverage falls.
  The duplication is real; removing it by merging the tested half into the untested one is not
  a repair.
- *A shared `installation_handler(action) -> &str` called by both matches.* Rejected: Restate
  clients dispatch by typed method, not by name — `client.pause()` and `client.purge()` are
  distinct generated methods, and there is no call-by-string to hand the answer to — so the
  send side cannot call it. The "one function both call" would be called by one, and the other
  would keep its match.
