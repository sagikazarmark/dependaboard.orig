-- Records, per repository, the merge method to use when the configured
-- preference is one the repository disallows. NULL means the preference is
-- allowed there (or the row has not been synced since this column existed),
-- so a merge falls back to the preference. Populated by the installation
-- repository sync from GitHub's allow_squash_merge / allow_merge_commit /
-- allow_rebase_merge flags.

ALTER TABLE repositories ADD COLUMN merge_method TEXT;
