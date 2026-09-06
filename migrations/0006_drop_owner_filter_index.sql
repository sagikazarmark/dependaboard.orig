-- idx_pr_filter led with owner, for a filter on the owner alone that the
-- dashboard never offered: its repository tree selects an owner's repositories
-- by name, which the store answers with the owner/repo expression, not this
-- index. With the owner filter gone from PrFilter nothing constrains the
-- index's first column, so no query can use it, and every write paid for it.

DROP INDEX idx_pr_filter;
