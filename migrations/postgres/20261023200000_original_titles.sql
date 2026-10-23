-- Original titles on demand: the work's original language, and the title a
-- provider gave in the library's language, so that `title` can switch
-- between it and `original_title` without fetching metadata again.
ALTER TABLE items
    ADD COLUMN original_language VARCHAR(8)  NULL,
    ADD COLUMN provider_title    VARCHAR(512) NULL;
