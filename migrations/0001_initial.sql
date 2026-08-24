PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS repositories (
  repository_id   INTEGER PRIMARY KEY,
  installation_id INTEGER NOT NULL,
  owner           TEXT NOT NULL,
  repo            TEXT NOT NULL,
  merge_method    TEXT,
  synced_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_repo_install ON repositories(installation_id);

CREATE TABLE IF NOT EXISTS pull_requests (
  id             TEXT PRIMARY KEY,
  repository_id  INTEGER NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  owner          TEXT NOT NULL,
  repo           TEXT NOT NULL,
  number         INTEGER NOT NULL,
  title          TEXT NOT NULL,
  html_url       TEXT NOT NULL,
  dependency     TEXT,
  from_version   TEXT,
  to_version     TEXT,
  dependencies   TEXT NOT NULL DEFAULT '[]',
  update_type    TEXT NOT NULL,
  head_sha       TEXT NOT NULL,
  check_status   TEXT NOT NULL,
  mergeable      TEXT,
  labels         TEXT NOT NULL DEFAULT '[]',
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  synced_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pr_filter ON pull_requests(owner, check_status, update_type);
CREATE INDEX IF NOT EXISTS idx_pr_repo ON pull_requests(repository_id);
CREATE INDEX IF NOT EXISTS idx_pr_sha ON pull_requests(repository_id, head_sha);
CREATE INDEX IF NOT EXISTS idx_pr_order ON pull_requests(updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_pr_dependency ON pull_requests(dependency);
