-- Where a batch came from and what it acted on, for the audit trail to say more
-- than that a merge happened. A batch queued from a finished batch's "Retry
-- rejected" names that batch, so the two records point at each other; a target
-- keeps the head it was sent against — the one the guard checked — so a merge
-- says what it was a merge *of*, which for a squash or rebase merge the merge
-- commit cannot say. The merge commit itself rides in the target's outcome JSON,
-- as `succeeded { detail, merge_sha }`: it is part of what succeeding was.
--
-- Every column is nullable and unset on the rows already here: a batch from
-- before this migration retried nothing anyone recorded, and sent its targets
-- against heads nobody kept. The link is not a foreign key: it is the browser's
-- word, and a record that names a batch the projection never kept is a dangling
-- link, not a reason to lose the record that names it.

ALTER TABLE batches ADD COLUMN retried_from TEXT;
ALTER TABLE running_batches ADD COLUMN retried_from TEXT;
ALTER TABLE batch_targets ADD COLUMN head_sha TEXT;
