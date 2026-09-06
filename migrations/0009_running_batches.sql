-- Bulk actions the BulkAction workflow is running. Restate holds where a batch
-- stands, but only a dashboard that knows the batch id can ask; this row is how
-- the dashboard's "Recent batches" lists a batch that has not finished, so a tab
-- that lost one, or was never following it, can find it and follow it.
--
-- The workflow writes the row as its first step, and the finished record's write
-- takes it away in the same transaction (see 0005_batches.sql). It says only that
-- the batch is running, what was asked, by whom, since when, and over how many
-- pull requests; how it is going is read from the workflow's progress.

CREATE TABLE running_batches (
  batch_id     TEXT PRIMARY KEY,
  action       TEXT NOT NULL,    -- core::BulkActionKind display form
  requested_by TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  target_count INTEGER NOT NULL
);
