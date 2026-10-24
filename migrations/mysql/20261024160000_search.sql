-- What an item is found by: its titles and its people's names, folded
-- (lower case, no accents), one per line. Filled when an item is written;
-- items written before this column are filled at startup.
ALTER TABLE items ADD COLUMN search_text TEXT NULL;
