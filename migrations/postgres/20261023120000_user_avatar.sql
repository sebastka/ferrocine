-- The user's picture (the OIDC `picture` claim), fetched at sign-in, cropped
-- to a square and stored as JPEG; `avatar_source` is the URL it came from, so
-- that an unchanged picture is not fetched again.
ALTER TABLE users
    ADD COLUMN avatar        BYTEA         NULL,
    ADD COLUMN avatar_tag    VARCHAR(16)   NULL,
    ADD COLUMN avatar_source VARCHAR(1024) NULL;
