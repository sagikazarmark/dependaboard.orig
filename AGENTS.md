# Dependaboard — notes for agents

`README.md` is the operator's and user's document: how to run it, what each control does, and the *Live Acceptance* checklist. `spec.md` is the architecture: Restate entities, the storage trait, the error taxonomy, and what is deliberately out of the MVP. Read the section of each that touches the area you are about to change.

## Agent skills

### Issue tracker

Issues live in GitHub Issues on `sagikazarmark/dependaboard.orig`, one issue per commit, body in the *What to build / Decision needed / Acceptance criteria / Context / Blocked by* shape; the commit says `Closes #N`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage labels, unchanged; `question` additionally marks a ticket whose decisions are still open. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` and `docs/adr/` at the repo root, created lazily when a term or decision is actually resolved. See `docs/agents/domain.md`.
