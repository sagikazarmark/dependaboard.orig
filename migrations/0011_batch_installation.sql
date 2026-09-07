-- Which installation a batch ran for, so two deployments sharing one store do
-- not read each other's batches. The BulkAction workflow stamps its own
-- installation on the running listing and the finished record as it writes
-- them; every batch read filters on it, as the pull request reads are held to
-- the installation through the repository row.
--
-- The rows already here carry none. A finished batch is attributed through its
-- targets' repositories where they are still in `repositories` — a deployment
-- holds one installation, so every target of a batch is that installation's.
-- A batch whose repositories were all purged since, and a running listing,
-- which has no targets to go through, are left NULL, and NULL never satisfies
-- `installation_id = ?`: a row nobody can be sure of is shown to nobody, and
-- kept rather than dropped. Nothing is derived from `batch_targets` at read
-- time: the record is meant to outlive the repositories it names.

ALTER TABLE batches ADD COLUMN installation_id INTEGER;
ALTER TABLE running_batches ADD COLUMN installation_id INTEGER;

UPDATE batches SET installation_id = (
  SELECT MIN(r.installation_id)
  FROM batch_targets t
  JOIN repositories r ON r.repository_id = t.repository_id
  WHERE t.batch_id = batches.batch_id
);
