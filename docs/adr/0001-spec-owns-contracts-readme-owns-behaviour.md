---
status: accepted
---

# `spec.md` owns the contracts; `README.md` owns what the user can observe

By #65 nobody could tell which document a ticket should edit: half the behaviour shipped
over two days was written up in the README only, while `spec.md` trailed the code in the
contracts it does describe. We split ownership the way the work had in fact been going.
`spec.md` owns the architecture — Restate entities and handler contracts, the call flow,
ingress visibility, the storage trait and schema, the error taxonomy and retry budgets,
the GitHub App constraint, and what is out of the MVP; `README.md` owns what a user or
operator can observe — how to run it, the environment, what each control does and says,
and the *Live Acceptance* checklist. Each names a mechanism the other owns and points
across rather than restating it. A ticket edits the document that owns what it changes,
in the ticket's commit — both when it changes both — and its acceptance criteria say which;
the issue stays the decision log, the documents carry the outcome.

## Considered options

- *`spec.md` owns everything.* Rejected: the spec would become the place a user reads to
  learn what a button does, and its contract sections would be buried in behaviour prose.
- *The README owns everything, the spec is frozen history.* Rejected: a Restate visibility
  table and a storage trait have no place in a quickstart, and a frozen spec is a spec that
  lies.
