-- The retirement outbox: the pull requests a prune has removed from the read
-- model whose objects have not yet been told. A sweep deletes rows inside a
-- Restate step, and a step's result can be lost after its effect has landed:
-- the delete commits, the process dies before the journal takes the returned
-- keys, and the re-run finds nothing left to delete. The objects that just
-- lost their rows would keep serving a snapshot nobody else has. So every
-- prune writes the keys it removed here, in the same transaction as the
-- delete, and the sweep drains the table afterwards, sending each its
-- `closed` and acknowledging what it read.
--
-- synced_before is the fence the prune ran under — the instant the sweep's
-- listing started — and NULL for a purge, whose close is unconditional. It is
-- recorded rather than recomputed when drained: an object synced since that
-- instant was reopened behind the sweep and must keep its state, however late
-- the close arrives.
--
-- AUTOINCREMENT keeps the ids monotonic even after rows are deleted, so a
-- drain that read up to some id can acknowledge with `id <= ?`: anything
-- queued since carries a higher one and stays.

CREATE TABLE pull_request_retirements (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  repository_id INTEGER NOT NULL,
  number        INTEGER NOT NULL,
  synced_before INTEGER
);
