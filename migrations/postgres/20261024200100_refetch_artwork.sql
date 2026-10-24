-- With the artwork dropped from the database (previous migration), every
-- item is due a provider lookup at the next scan, which downloads its
-- artwork again (an item matched once is otherwise never looked up again).
-- Its textual metadata is kept: providers only fill what is missing.
UPDATE items SET remote_fetched_at = NULL;
