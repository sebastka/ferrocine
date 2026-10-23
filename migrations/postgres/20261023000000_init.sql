-- Timestamps are milliseconds since the Unix epoch (UTC), stored as BIGINT and
-- booleans as SMALLINT, so that the same queries work on PostgreSQL and MariaDB.
-- Lists (genres, people, …) and the technical media description are JSON text.

-- Libraries are configuration (YAML); this table mirrors them so that items
-- can reference them, and holds their scan state.
CREATE TABLE libraries (
    id               VARCHAR(64)  PRIMARY KEY,
    name             VARCHAR(255) NOT NULL,
    kind             VARCHAR(16)  NOT NULL,
    created_at       BIGINT       NOT NULL,
    updated_at       BIGINT       NOT NULL,
    -- Set when the library disappears from the configuration.
    removed_at       BIGINT       NULL,
    scan_started_at  BIGINT       NULL,
    scan_finished_at BIGINT       NULL,
    scan_error       TEXT         NULL
);

-- Movies, series, seasons and episodes. kind: 1 movie, 2 series, 3 season, 4 episode.
CREATE TABLE items (
    id                  BIGSERIAL     PRIMARY KEY,
    library_id          VARCHAR(64)   NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    kind                SMALLINT      NOT NULL,
    parent_id           BIGINT        NULL REFERENCES items (id) ON DELETE CASCADE,
    series_id           BIGINT        NULL,
    -- SHA-256 of what identifies the item on disk (file, directory, season).
    path_key            VARCHAR(64)      NOT NULL UNIQUE,
    path                TEXT          NULL,
    title               VARCHAR(512)  NOT NULL,
    sort_title          VARCHAR(512)  NOT NULL,
    original_title      VARCHAR(512)  NULL,
    year                INTEGER       NULL,
    premiere_date       VARCHAR(10)   NULL,
    season_number       INTEGER       NULL,
    episode_number      INTEGER       NULL,
    overview            TEXT          NULL,
    tagline             VARCHAR(1024) NULL,
    -- Community rating × 100 (sqlx's Any driver mis-binds floats on PostgreSQL).
    rating_x100         INTEGER       NULL,
    official_rating     VARCHAR(32)   NULL,
    genres              TEXT          NULL,
    studios             TEXT          NULL,
    people              TEXT          NULL,
    tmdb_id             VARCHAR(32)   NULL,
    imdb_id             VARCHAR(32)   NULL,
    tvdb_id             VARCHAR(32)   NULL,
    runtime_ms          BIGINT        NULL,
    file_size           BIGINT        NULL,
    file_mtime          BIGINT        NULL,
    media               TEXT          NULL,
    -- Fingerprint of the local metadata files (.nfo) last read.
    local_key           VARCHAR(64)   NULL,
    -- Last remote lookup, and whether it found a match.
    remote_fetched_at   BIGINT        NULL,
    remote_matched      SMALLINT      NOT NULL DEFAULT 0,
    added_at            BIGINT        NOT NULL,
    -- Series and seasons: when an episode was last added.
    content_added_at    BIGINT        NOT NULL,
    updated_at          BIGINT        NOT NULL,
    -- Scan generation that last saw the item; older ones are deleted.
    seen_scan           BIGINT        NOT NULL
);
CREATE INDEX items_library_sort ON items (library_id, kind, sort_title);
CREATE INDEX items_library_added ON items (library_id, kind, content_added_at);
CREATE INDEX items_parent ON items (parent_id);
CREATE INDEX items_series ON items (series_id);

-- Artwork, resized and re-encoded as JPEG. kind: poster, backdrop, thumb.
CREATE TABLE images (
    item_id    BIGINT       NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    kind       VARCHAR(16)  NOT NULL,
    -- local, remote or extracted; source_key identifies the original so that
    -- an unchanged source is not processed again.
    source     VARCHAR(16)  NOT NULL,
    source_key VARCHAR(512) NOT NULL,
    width      INTEGER      NOT NULL,
    height     INTEGER      NOT NULL,
    tag        VARCHAR(16)     NOT NULL,
    data       BYTEA        NOT NULL,
    updated_at BIGINT       NOT NULL,
    PRIMARY KEY (item_id, kind)
);

CREATE TABLE users (
    id            BIGSERIAL    PRIMARY KEY,
    -- SHA-256 of issuer and subject.
    identity_key  VARCHAR(64)     NOT NULL UNIQUE,
    issuer        VARCHAR(512) NOT NULL,
    subject       VARCHAR(255) NOT NULL,
    username      VARCHAR(255) NOT NULL,
    display_name  VARCHAR(255) NULL,
    email         VARCHAR(255) NULL,
    admin         SMALLINT     NOT NULL DEFAULT 0,
    -- Values of the groups claim at the last sign-in (JSON list).
    claim_groups  TEXT         NULL,
    created_at    BIGINT       NOT NULL,
    last_login_at BIGINT       NOT NULL
);

-- Browser sessions; the cookie holds a random value, the table its SHA-256.
CREATE TABLE sessions (
    id_hash      VARCHAR(64)     PRIMARY KEY,
    user_id      BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   BIGINT       NOT NULL,
    expires_at   BIGINT       NOT NULL,
    last_seen_at BIGINT       NOT NULL,
    -- For RP-initiated logout (id_token_hint).
    id_token     TEXT         NULL,
    user_agent   VARCHAR(512) NULL
);
CREATE INDEX sessions_user ON sessions (user_id);
CREATE INDEX sessions_expires ON sessions (expires_at);

-- Sign-ins in progress (authorization code flow with PKCE).
CREATE TABLE login_requests (
    state_hash VARCHAR(64)      PRIMARY KEY,
    verifier   VARCHAR(128)  NOT NULL,
    nonce      VARCHAR(128)  NOT NULL,
    return_to  VARCHAR(1024) NOT NULL,
    created_at BIGINT        NOT NULL
);

CREATE TABLE user_data (
    user_id        BIGINT   NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    item_id        BIGINT   NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    position_ms    BIGINT   NOT NULL DEFAULT 0,
    played         SMALLINT NOT NULL DEFAULT 0,
    play_count     INTEGER  NOT NULL DEFAULT 0,
    favorite       SMALLINT NOT NULL DEFAULT 0,
    last_played_at BIGINT   NULL,
    updated_at     BIGINT   NOT NULL,
    PRIMARY KEY (user_id, item_id)
);
CREATE INDEX user_data_recent ON user_data (user_id, last_played_at);
CREATE INDEX user_data_item ON user_data (item_id);
