-- A counter that moves whenever a row of the read model changes, so the
-- dashboard can learn that the projection has moved with one cheap read
-- instead of reloading the rows to compare them. MAX(synced_at) cannot do
-- this: a merged or closed pull request is a plain DELETE, which leaves no
-- newer stamp behind.
--
-- The triggers keep the counter, so no writer can forget to: a repository
-- delete that cascades to its pull requests moves it as surely as an upsert.
-- If a later migration rebuilds either table (as 0002 did), recreate its
-- triggers: DROP TABLE takes them with it.

CREATE TABLE projection_revision (
  id       INTEGER PRIMARY KEY CHECK (id = 1),
  revision INTEGER NOT NULL
);

INSERT INTO projection_revision (id, revision) VALUES (1, 0);

CREATE TRIGGER projection_revision_pr_insert AFTER INSERT ON pull_requests
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER projection_revision_pr_update AFTER UPDATE ON pull_requests
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER projection_revision_pr_delete AFTER DELETE ON pull_requests
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER projection_revision_repo_insert AFTER INSERT ON repositories
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER projection_revision_repo_update AFTER UPDATE ON repositories
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER projection_revision_repo_delete AFTER DELETE ON repositories
BEGIN
  UPDATE projection_revision SET revision = revision + 1 WHERE id = 1;
END;
