-- More metadata providers: ratings by source (JSON list), AniDB and AniList ids.
ALTER TABLE items
    ADD COLUMN ratings    TEXT        NULL,
    ADD COLUMN anidb_id   VARCHAR(32) NULL,
    ADD COLUMN anilist_id VARCHAR(32) NULL;

-- Subtitles downloaded from a provider (OpenSubtitles), stored as WebVTT:
-- libraries stay read-only.
CREATE TABLE subtitles (
    id               BIGSERIAL    PRIMARY KEY,
    item_id          BIGINT       NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    language         VARCHAR(8)   NOT NULL,
    source           VARCHAR(32)  NOT NULL,
    source_id        VARCHAR(64)  NOT NULL,
    release_name     VARCHAR(512) NULL,
    hearing_impaired SMALLINT     NOT NULL DEFAULT 0,
    forced           SMALLINT     NOT NULL DEFAULT 0,
    -- Matched by the file's hash, not only by title.
    hash_match       SMALLINT     NOT NULL DEFAULT 0,
    content          TEXT         NOT NULL,
    -- `auto`, or the user who downloaded it.
    created_by       VARCHAR(255) NOT NULL,
    created_at       BIGINT       NOT NULL,
    UNIQUE (item_id, source, source_id)
);
CREATE INDEX subtitles_item ON subtitles (item_id);

-- Automatic searches per item and language, so that a scan does not search
-- again what was found, nor before `scanner.retry_unmatched` what was not.
CREATE TABLE subtitle_searches (
    item_id     BIGINT     NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    language    VARCHAR(8) NOT NULL,
    searched_at BIGINT     NOT NULL,
    found       SMALLINT   NOT NULL,
    PRIMARY KEY (item_id, language)
);
