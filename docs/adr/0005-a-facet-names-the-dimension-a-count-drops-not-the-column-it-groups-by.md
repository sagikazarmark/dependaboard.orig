---
status: accepted
---

# A `Facet` names the dimension a count drops, not the column it groups by

`facet_counts` builds four statements in a row, and each one pairs a `Facet::` argument with
SQL written beside it: `facets.rs:23-28` scopes by `Facet::Checks` and groups by
`p.check_status`, `:30-35` scopes by `Facet::UpdateTypes` and groups by `p.update_type`,
`:39-48` scopes by `Facet::Labels` and groups by `lbl.value` over `json_each(p.labels)`, and
`:50-51` scopes by `Facet::Repositories` and hands the clause to `repository_facets`. An
architecture review read the two halves as one fact written twice, observed that swapping two
`Facet::` arguments compiles, and proposed moving the grouping onto the enum — a
`Facet::grouping()` the statement builder reads — so that a mispairing could not be written.
It cited `StoreStepKind` as precedent, on the strength of that type's own sentence: "one
choice rather than two, because the retry budget and the reading of a failure only make sense
together". We keep the pairing, and this ADR is here so the proposal is not made a third time.

The first thing to know is that the proposal does not do what it says. A reviewer built it,
and with the grouping moved onto `Facet` the swap still compiles, because nothing about the
`Facet` argument reaches the type of the result. `enum_counts` (`facets.rs:122-135`, the
function the proposal called `enum_facet`) is generic in `T: FromStr + Ord` and takes the SQL
as a `&str`; its `T` is fixed by where the value lands, not by the facet that built the
statement. `FacetCounts` (`crates/core/src/lib.rs:568-573`) has four differently-typed fields
— `checks: BTreeMap<CheckStatus, u64>`, `update_types: BTreeMap<UpdateType, u64>`,
`labels: Vec<LabelFacet>` and `repositories: Vec<RepoFacet>` — and the struct literal at
`facets.rs:53-58` is what infers `T` for each call. Write `Facet::UpdateTypes` where
`Facet::Checks` belongs and the `checks` field still forces `T = CheckStatus`; the code
compiles, asks SQLite for `p.update_type`, and fails at run time in `stored_enum` with
`StoreError::CorruptEnum("minor")`, because the two vocabularies happen to be disjoint. A
five-hundred from a live read is not the compile error that was promised, and it is strictly
worse than what the swap costs today, which is a wrong scope caught by tests.

The second thing is that binding would open a hole where there is none. The repository facet
is not a `GROUP BY` fragment over `pull_requests p` at all; it is `repository_facets`
(`facets.rs:86-117`), and it differs on four axes at once. It selects `FROM repositories r`,
so the table is different. It uses the facet clause as a derived table —
`LEFT JOIN (SELECT p.repository_id, COUNT(*) ... {where_sql} GROUP BY p.repository_id) c`
— so that a repository with no matching pull request is still listed, at zero. It binds a
parameter of its own past the clause's, `scoped.bind(integer(installation_id)?)` at
`facets.rs:91`, because it is the one facet that reads `repositories` directly and so the one
that needs its own tenancy predicate; the doc comment at `:81-85` records that this is how a
tenancy hole showed, and `ScopedFilter`'s own comment (`scope.rs:17-26`) records why the
clause inside that derived table may name only `p`. And it decodes seven columns, not two —
the six of `REPO_COLUMNS` through `repo_from_row`, plus the count at `REPO_EXTRA_INDEX` — under
an `ORDER BY r.owner COLLATE NOCASE, r.repo COLLATE NOCASE` that no other facet has. No
`grouping()` can answer for it, so the binding needs a fourth arm that answers with nothing.

That arm is the hole. Today a mistyped `Facet::Repositories` at `facets.rs:39` compiles —
`facet_scope` takes any `Facet` — but it cannot reach the statement, because the labels SQL is
written at the call site and says `json_each(p.labels) ... GROUP BY lbl.value` in plain sight;
all the typo can do is leave the label filter in the scope, which
`choosing_a_value_never_narrows_the_facet_it_was_chosen_in` fails on. Under the binding that
same argument would also decide the statement, and the empty arm would hand back an empty
label list, which no assertion about grouping can tell from a scope that genuinely carries no
labels. So the binding would trade a wrong answer a test already names for a silent one.

The third thing is that the two jobs do not co-vary, and the repository already says so in
prose. `without_facet` (`filter.rs:152-161`) names a `PrFilter` field to clear, and nothing
more. What the facet then counts is a different question with, for repositories, a
deliberately different answer: the filter matches repositories **by name**, through a
subquery on `repositories` (`filter.rs:43-54`), while the facet counts **by
`repository_id`** (`facets.rs:100`). The comment at `filter.rs:44-51` explains the
divergence — a rename reaches the `repositories` row on the next installation sweep while
each pull request keeps the `owner`/`repo` it was last synced under, so matching the pull
request's own copy would offer a count whose rows the filter cannot find — and
`a_renamed_repository_lists_the_rows_its_facet_still_counts` is the regression test that holds
the two ends together: the sidebar counts the renamed repository at three, and clicking it
must list three. That is a documented non-identity between "the dimension to drop" and "the
column to count". A single enum arm would have to hold both halves of it and would read as
though they were one.

The fourth thing is that `StoreStepKind` is not the precedent it was taken for. It binds two
halves of a single decision that is incoherent apart — a budget that never gives up, under a
reading that ends the step on the store's word, gives up after all — and it is consumed by a
generic helper, `run_store_step`, with two implementations and twelve call sites across five
handler modules. The kind travels; that is why it must carry both halves. `without_facet` is
perfectly coherent alone — it clears a field — and it has exactly one production call site,
`facet_scope` at `facets.rs:70`, reached from four `Facet::` arguments that all sit within
twenty-eight lines of one another inside one function. Nothing travels. There is no call site
at a distance to protect from picking the halves apart, because there is no distance.

The fifth thing is that the hazard the proposal was aimed at is already caught. Mutating each
of the six pairwise swaps of the four `Facet::` arguments in `facet_counts` kills every one
of them against the existing end-to-end facet tests — one or two failing tests per swap, each
naming the dimension that went wrong. A change that the type system would have to be rebuilt
to reject is a change the suite rejects today, in the language of the sidebar rather than the
language of inference.

What that same mutation sweep did find were two real survivors in this module, and they were
not mispairings. The update-type statement could `GROUP BY p.check_status` with its `SELECT`
left alone — SQLite's bare-column rule then fills the selected column from an arbitrary row
of each group, so the sidebar answers with genuine update types against check-status groups:
plausible words, wrong numbers, no error anywhere — and the same statement could lose its
`without_facet` call entirely, so that choosing *Major* in the sidebar made Minor and Patch
vanish from it and the user could never widen back, which is the one thing this module's
opening paragraph promises does not happen. Both left all 74 store tests green. The cause was
the fixture, not the pairing: `facet_fixture` differs in the *values* of each dimension and
not in how each dimension *partitions* the rows, so under every filter its tests apply the
update-type facet was left with a single group — and with one group, `GROUP BY` any column
answers the same. `f5ec0e9` fixed it with `crosscut_fixture`, six pull requests cut into two
check groups, three update types, two repositories of unequal size and two labels, each
dimension keeping a count multiset the others cannot imitate, and two tests on it:
`every_facet_groups_by_the_column_it_names` pins all four pairings and
`choosing_a_value_never_narrows_the_facet_it_was_chosen_in` pins the `without_facet` call once
per statement. Both mutants now die. No production line changed — the statements were already
right, and nothing had said so.

That is the whole shape of it. The defect the binding was proposed against was already
covered by tests before the proposal was written; the defects that actually existed in this
module were untouched by it, and would have survived it unchanged, because a `grouping()`
method cannot tell a fixture to partition its rows. The reason to record this rather than
leave it to taste is that the proposal is attractive on sight and expensive to disprove: it
took building the thing to find that the swap still compiles.

## Considered options

- *Binding the grouping to `Facet` (a `grouping()` method).* Rejected: it does not deliver
  what it claims — the swap still compiles, since `enum_counts`'s `T` comes from the
  `FacetCounts` field the value lands in and never from the facet — and it needs a fourth arm
  answering with nothing for `Facet::Repositories`, which is counted by `repository_facets`
  over a different table, through a derived table, with a parameter of its own and a
  seven-column decode. That arm turns one mistyped argument into an empty label sidebar with
  no error at all. It would also move FROM-lists, a `json_each` join and an
  `ORDER BY COUNT(*) DESC, lbl.value COLLATE NOCASE` into `filter.rs`, whose stated job
  (`filter.rs:1-2`) is the `WHERE` clause and the parameters it binds.
- *A walked table of `(Facet, key, from, order)` inside `facets.rs`.* Rejected: it cannot be
  walked once. The three grouped results go to three differently-typed fields —
  `BTreeMap<CheckStatus, u64>`, `BTreeMap<UpdateType, u64>` and `Vec<LabelFacet>` — so the
  loop needs a second match on the facet to decide where each row set lands, with an
  `unreachable!` for the repository arm that the table cannot describe. Two matches and a
  panic arm in place of four statements that each say what they count, and a mispairing is
  still not a compile error.
- *A marker struct per facet, with a trait binding it to its result type.* The one shape that
  does make the swap `error[E0308]`: `Checks` and `UpdateTypes` as zero-sized types with an
  associated `Output`, so passing the wrong marker no longer type-checks rather than failing
  in `stored_enum` at run time. Not taken now — it covers only the two enum facets, since
  `Repositories` cannot join a trait about `GROUP BY` fragments and `Labels` has an `ORDER BY`
  of its own, so it buys a compile error for one of six swaps that the suite already kills.
  Record it as the shape to reach for if this is ever revisited, and let the trigger be a
  fifth facet arriving — a hand-paired statement added by someone who did not write the other
  four — not another review of the four that exist.
