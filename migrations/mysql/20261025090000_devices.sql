-- Devices signed in with Quick Connect (TV apps): a long-lived token each,
-- stored hashed, revocable from the user's profile.
CREATE TABLE devices (
    id           BIGINT       AUTO_INCREMENT PRIMARY KEY,
    user_id      BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         VARCHAR(255) NOT NULL,
    platform     VARCHAR(64)  NULL,
    -- SHA-256 of the token.
    token_hash   VARCHAR(64)  NOT NULL UNIQUE,
    created_at   BIGINT       NOT NULL,
    last_seen_at BIGINT       NOT NULL
) DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;
CREATE INDEX devices_user ON devices (user_id);

-- Pairings in progress: the device shows `code`, a signed-in user approves
-- it, and the device redeems its secret (hashed here) for a token.
CREATE TABLE device_pairings (
    secret_hash VARCHAR(64)  PRIMARY KEY,
    code        VARCHAR(16)  NOT NULL UNIQUE,
    device_name VARCHAR(255) NOT NULL,
    platform    VARCHAR(64)  NULL,
    created_at  BIGINT       NOT NULL,
    expires_at  BIGINT       NOT NULL,
    -- Set when approved.
    user_id     BIGINT       NULL REFERENCES users (id) ON DELETE CASCADE
) DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;
