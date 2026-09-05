-- Finished bulk actions, kept for audit. Restate holds a batch's progress only
-- for the workflow's retention; the BulkAction workflow writes the finished
-- batch here as its last step, and the dashboard's "Recent batches" reads it.
--
-- Batches are append-only: a batch id runs once, so a row is written once and
-- never updated. The targets name their pull requests in full, link included,
-- rather than referencing pull_requests: a merged pull request leaves the
-- projection, and the record must not go with it.

CREATE TABLE batches (
  batch_id     TEXT PRIMARY KEY,
  action       TEXT NOT NULL,    -- core::BulkActionKind display form
  requested_by TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  completed_at INTEGER NOT NULL,
  succeeded    INTEGER NOT NULL,
  rejected     INTEGER NOT NULL,
  failed       INTEGER NOT NULL
);
CREATE INDEX idx_batch_completed ON batches(completed_at DESC, batch_id DESC);

CREATE TABLE batch_targets (
  batch_id      TEXT NOT NULL REFERENCES batches(batch_id) ON DELETE CASCADE,
  position      INTEGER NOT NULL, -- the target's place in the batch, from 0
  repository_id INTEGER NOT NULL,
  owner         TEXT NOT NULL,
  repo          TEXT NOT NULL,
  number        INTEGER NOT NULL,
  title         TEXT NOT NULL,
  html_url      TEXT NOT NULL,
  outcome       TEXT NOT NULL,    -- core::TargetOutcome as JSON
  PRIMARY KEY (batch_id, position)
);
