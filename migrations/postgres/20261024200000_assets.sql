-- Artwork, downloaded subtitles and seek bar previews move to the metadata
-- directory (`metadata.directory`); their rows keep the files' tags.

-- Stored artwork is dropped: the next scans fetch or extract it again.
DELETE FROM images;
ALTER TABLE images DROP COLUMN data;

-- Downloaded subtitles keep their text until the next start moves it to a
-- file (downloading them again would spend the provider's quota).
ALTER TABLE subtitles ADD COLUMN content_tag VARCHAR(16) NULL;
ALTER TABLE subtitles ALTER COLUMN content DROP NOT NULL;

-- The preview set of an item: sheets of `tile_columns` × `tile_rows`
-- frames of `width` × `height`, one every `interval_ms`, in
-- `trickplay/<tag>/<n>.jpg`.
CREATE TABLE trickplay (
    item_id      BIGINT      PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    tag          VARCHAR(16) NOT NULL,
    interval_ms  BIGINT      NOT NULL,
    width        INTEGER     NOT NULL,
    height       INTEGER     NOT NULL,
    tile_columns INTEGER     NOT NULL,
    tile_rows    INTEGER     NOT NULL,
    thumbnails   INTEGER     NOT NULL,
    created_at   BIGINT      NOT NULL
);
