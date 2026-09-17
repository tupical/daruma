-- Git work context of a task (branch / head commit / MR / repo) as a JSON
-- object, client-reported and replaced whole. NULL for tasks without one;
-- history is not backfilled — agents used to write this into comments.
ALTER TABLE tasks ADD COLUMN git_context TEXT;
