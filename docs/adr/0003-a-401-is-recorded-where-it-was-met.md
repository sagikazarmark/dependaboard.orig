---
status: accepted
---

# A 401 is recorded where it was met, by the guard every call to the server already passes through

`DashboardState::guarded` gated outgoing calls on a refusal the page already knew about and
recorded none of its own. So a 401 met by the global **Sync**, or by **Select all N
matching**, was toasted and left the line `Connection::Online`: the banner waited for the
revision poll to meet a refusal of its own — up to ten seconds with the tab showing, and
indefinitely while it was hidden, since `Refresh::tick` answers `Step::Wait` when the tab is
not visible and `use_clock` stops only on `state.signed_out()`. `README.md` has always
promised the opposite — "one is enough, the banner says to reload the page to sign in again,
and every failed call says the same" — and one meant one poll's, not one call's. Two flows
had already written the missing half by hand: the drawer's **Sync** and the batch follow
both called `poll_missed(Connection::SignedOut)` after a refusal they had been handed.

The fix is in the guard: a call that was made and came back `Fault::SignedOut` writes
`Connection::SignedOut` before the fault is handed back. Every call the page makes to the
server goes through `guarded` — ten call sites, in four gateway adapters and three free
functions — so the rule is now written once, for the class, and the banner rises from
whichever call meets the 401 first. The README needed no edit; the code now says what it
said.

An architecture review proposed something else, and this ADR is here so it is not proposed
again: a new module owning "the page's line to the server", gathering `Connection`,
`signed_out`, `guarded`, `poll_answered` and `poll_missed` behind one type so that the eight
spellings of "signed out" would become one. It is the shape ADR-0002 rejected, for the same
reason. The single entry point it would create already exists, and is the thing being fixed
here; `connection` and `refreshed_at` are read by the status bar and the banner and are set
by `DashboardFixture`, so they cannot leave the state that the components hold; and the
"gateway traits" it would give tests are four adapters over `DashboardState` and three free
functions, each already faked in its own tests by a scripted gateway that never touches the
state at all, so not one fake would change. What is left is a type that restates its
contents minus nothing — an interface as large as its implementation.

The eight spellings are layers, not synonyms, and reading them as duplication is what makes
the module look attractive. There are three of them. `Fault::SignedOut` is the wire: what a
401 from the auth edge reads as, minted in one place from the status. `Connection::SignedOut`
and `DashboardState::signed_out` are the page's line to the server: one value, one question
about it, read by the banner, the footer and every call the guard lets out. The rest are each
flow's own terminal outcome — `SyncFailure::signed_out` for one pull request's sync,
`BatchOutcome::SignedOut` and `Followed::signed_out` for a batch being followed, `SignedOut`
for a retry given up whole, `BatchEffects::signed_out` for what such a flow then asks of the
page — and the live refresh spells it differently again, as `Refresh::refused` and
`Step::Stop`. That third layer is what lets those flows be pure and gateway-tested: `sync_pr`,
`follow_batch`, `refresh_rejected` and `conclude` are driven in their tests by scripted
gateways and a fake effects sink, with no `DashboardState`, no signals and no VirtualDom in
sight, and each says in its own vocabulary how it ended. Collapsing them onto the page's line
would put the page's state inside every one of those tests to buy a word.

The trap is under the fix itself, and is the reason the guard's new line is not the whole
change. `use_live_refresh` read a refusal with `Err(Fault::SignedOut) if state.signed_out() =>
break`, whose guard distinguished "the guard refused without asking" from "asked and got a
401", deliberately letting the second fall through to `state.poll_missed(refresh.miss(&fault))`.
Once `guarded` records, `state.signed_out()` is true by the time that arm is tested, so a
made-and-refused poll would take the `break` — and `refresh.miss` is the only thing that sets
`Refresh::refused`, which is the only thing that makes `tick` answer `Step::Stop`. The refresh
would have ended with no refusal of its own and `Step::Stop` would have been reachable only
from its unit test. So the question is asked before the guard is, on the `Step::Poll` arm
itself, and the arm below is now only ever a poll that was made. The observable behaviour is
unchanged: the loop ends either way, the banner is up either way, and
`a_signed_out_page_is_not_polled_and_ends_the_refresh` and
`one_refusal_of_the_credentials_signs_out_at_once_and_nothing_is_asked_after_it` still pin what
they pinned.

The drawer's hand-rolled recorder is gone, since `ServerSync` asks through the guard and every
`SyncFailure::signed_out` it can produce has passed through it. The batch follow's stayed:
`BatchEffects::signed_out` is called by `conclude` and `queue_retry`, which are pure functions
over an effects sink, and deleting the call would delete tested behaviour of the flow rather
than a redundant line.

## Considered options

- *A module owning the page's line to the server.* Rejected: it restates what it wraps, and
  cannot take `connection` or `refreshed_at` with it — the status bar, the banner and
  `DashboardFixture` read and write them on the state the components share. The single entry
  point it promises is `guarded`, which already exists and already has every call site; the
  fakes it promises already exist, per flow, and would not change. ADR-0002 rejected the same
  shape for the same reason.
- *Recording the refusal at the two call sites that diverged — `top_bar`'s `sync` and
  `pr_table`'s `matching` — as the drawer and the batch follow already did.* Rejected: it
  fixes two doors of ten and leaves the next call added to the page to remember the rule, or
  not. The defect was not that two sites were missing a line; it was that the rule was written
  at the sites at all.
- *One spelling of "signed out" across all three layers.* Rejected: the flows would take the
  page's state as a dependency to say how they ended, and their tests — scripted gateways, a
  fake effects sink, no VirtualDom — would have to mount a dashboard to assert an outcome.
  The names are long precisely because each one says which layer is talking.
- *Recording inside `guarded` and letting the live refresh break on any refusal.* Rejected:
  observably identical, but it strands `Refresh::refused` and `Step::Stop`, which no
  production path would then reach — a refresh that cannot say it was refused is one bug away
  from polling again.
