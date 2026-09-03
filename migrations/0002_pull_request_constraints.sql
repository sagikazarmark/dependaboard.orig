-- SQLite's ALTER TABLE can neither add a constraint nor change a column's
-- collation, so this rebuilds pull_requests the documented way: create the
-- new shape, copy, drop the old table, rename, recreate the indexes
-- (https://www.sqlite.org/lang_altertable.html#otheralter).
--
-- Changes against 0001:
--   * dependency is COLLATE NOCASE. Package names compare case-insensitively
--     everywhere, and idx_pr_dependency inherits the column's collation, so
--     the case-insensitive dependency filter can use it instead of scanning.
--   * idx_pr_number makes (repository_id, number) unique. The id is
--     "{repository_id}#{number}", so the pair must be exactly as unique as
--     the id that is derived from it.
--   * idx_pr_repo (repository_id) is not recreated. idx_pr_sha and
--     idx_pr_number both start with repository_id and cover that lookup.
--
-- Dropping pull_requests with foreign keys on is safe: it is only ever the
-- child side of a constraint, so the implicit DELETE cannot violate one.
-- (PRAGMA foreign_keys could not be toggled inside the migration's
-- transaction anyway.)

CREATE TABLE pull_requests_new (
  id             TEXT PRIMARY KEY,
  repository_id  INTEGER NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  owner          TEXT NOT NULL,
  repo           TEXT NOT NULL,
  number         INTEGER NOT NULL,
  title          TEXT NOT NULL,
  html_url       TEXT NOT NULL,
  dependency     TEXT COLLATE NOCASE,
  from_version   TEXT,
  to_version     TEXT,
  dependencies   TEXT NOT NULL DEFAULT '[]',
  update_type    TEXT NOT NULL,
  head_sha       TEXT NOT NULL,
  check_status   TEXT NOT NULL,
  mergeable      TEXT, -- GitHub REST mergeable_state vocabulary (core::Mergeable); NULL reads as unknown
  labels         TEXT NOT NULL DEFAULT '[]',
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  synced_at      INTEGER NOT NULL
);

INSERT INTO pull_requests_new (
  id, repository_id, owner, repo, number, title, html_url, dependency,
  from_version, to_version, dependencies, update_type, head_sha,
  check_status, mergeable, labels, created_at, updated_at, synced_at
)
SELECT
  id, repository_id, owner, repo, number, title, html_url, dependency,
  from_version, to_version, dependencies, update_type, head_sha,
  check_status, mergeable, labels, created_at, updated_at, synced_at
FROM pull_requests;

DROP TABLE pull_requests;
ALTER TABLE pull_requests_new RENAME TO pull_requests;

CREATE INDEX idx_pr_filter ON pull_requests(owner, check_status, update_type);
CREATE UNIQUE INDEX idx_pr_number ON pull_requests(repository_id, number);
CREATE INDEX idx_pr_sha ON pull_requests(repository_id, head_sha);
CREATE INDEX idx_pr_order ON pull_requests(updated_at DESC, id DESC);
CREATE INDEX idx_pr_dependency ON pull_requests(dependency);
