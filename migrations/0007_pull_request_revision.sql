-- A second counter beside the projection's, moved by the pull request rows
-- alone. The dashboard spins its sync glyph until the sweep it asked for
-- reaches the pull requests, and the projection's counter cannot tell it
-- when: a sweep writes every repository row before it fetches a single pull
-- request, and each of those writes moves it. The new counter is the subset
-- the glyph follows; the projection's keeps moving on every row, so the rows
-- and the sidebar — whose repositories are read from their own table — still
-- reload on any change.
--
-- The pull request triggers are recreated to move both counters at once. The
-- repository triggers of 0004 stand as they are. If a later migration
-- rebuilds pull_requests, recreate these three: DROP TABLE takes them with it.

ALTER TABLE projection_revision ADD COLUMN pull_requests INTEGER NOT NULL DEFAULT 0;

DROP TRIGGER projection_revision_pr_insert;
DROP TRIGGER projection_revision_pr_update;
DROP TRIGGER projection_revision_pr_delete;

CREATE TRIGGER projection_revision_pr_insert AFTER INSERT ON pull_requests
BEGIN
  UPDATE projection_revision
  SET revision = revision + 1, pull_requests = pull_requests + 1
  WHERE id = 1;
END;

CREATE TRIGGER projection_revision_pr_update AFTER UPDATE ON pull_requests
BEGIN
  UPDATE projection_revision
  SET revision = revision + 1, pull_requests = pull_requests + 1
  WHERE id = 1;
END;

CREATE TRIGGER projection_revision_pr_delete AFTER DELETE ON pull_requests
BEGIN
  UPDATE projection_revision
  SET revision = revision + 1, pull_requests = pull_requests + 1
  WHERE id = 1;
END;
