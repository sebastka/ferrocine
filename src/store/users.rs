//! Users (created at their first sign-in), sessions and sign-ins in progress.

use sqlx::Row;

use super::{Store, bool_i16, sha256_hex};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub admin: bool,
    /// Values of the groups claim at the last sign-in.
    #[serde(skip)]
    pub groups: Vec<String>,
    /// Version of the stored avatar, if any.
    #[serde(skip)]
    pub avatar_tag: Option<String>,
}

/// The stored avatar of a user.
pub struct Avatar {
    pub tag: String,
    pub data: Vec<u8>,
}

/// Identity claims of a sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub admin: bool,
    pub groups: Vec<String>,
}

pub struct NewSession<'a> {
    pub id_hash: &'a str,
    pub user_id: i64,
    pub expires_at: i64,
    pub id_token: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub user: User,
    pub expires_at: i64,
    pub last_seen_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginRequest {
    pub verifier: String,
    pub nonce: String,
    pub return_to: String,
    pub created_at: i64,
}

pub(super) fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Store {
    /// Create or update the user of a sign-in; returns the user.
    pub async fn upsert_user(&self, id: &Identity, now: i64) -> anyhow::Result<User> {
        let key = sha256_hex(format!("{}\n{}", id.issuer, id.subject).as_bytes());
        let sql = format!(
            "INSERT INTO users (identity_key, issuer, subject, username, display_name, email, admin, claim_groups, \
             created_at, last_login_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?){}",
            self.upsert_tail(
                "identity_key",
                &[
                    "username",
                    "display_name",
                    "email",
                    "admin",
                    "claim_groups",
                    "last_login_at"
                ]
            )
        );
        sqlx::query(self.sql_owned(sql))
            .bind(&key)
            .bind(clip(&id.issuer, 512))
            .bind(clip(&id.subject, 255))
            .bind(clip(&id.username, 255))
            .bind(id.display_name.as_deref().map(|s| clip(s, 255)))
            .bind(id.email.as_deref().map(|s| clip(s, 255)))
            .bind(bool_i16(id.admin))
            .bind(serde_json::to_string(&id.groups)?)
            .bind(now)
            .bind(now)
            .execute(self.pool())
            .await?;
        let row = sqlx::query(self.sql("SELECT id, avatar_tag FROM users WHERE identity_key = ?"))
            .bind(&key)
            .fetch_one(self.pool())
            .await?;
        Ok(User {
            id: row.try_get(0)?,
            username: id.username.clone(),
            display_name: id.display_name.clone(),
            email: id.email.clone(),
            admin: id.admin,
            groups: id.groups.clone(),
            avatar_tag: row.try_get(1)?,
        })
    }

    /// Where the stored avatar of a user came from.
    pub async fn avatar_source(&self, user_id: i64) -> anyhow::Result<Option<String>> {
        let row = sqlx::query(self.sql("SELECT avatar_source FROM users WHERE id = ?"))
            .bind(user_id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?.flatten())
    }

    /// Store (or, with `None`, remove) a user's avatar; returns its tag.
    pub async fn set_avatar(&self, user_id: i64, avatar: Option<(&[u8], &str)>) -> anyhow::Result<Option<String>> {
        let (data, tag, source) = match avatar {
            Some((data, source)) => (
                Some(data.to_vec()),
                Some(sha256_hex(data)[..16].to_owned()),
                Some(clip(source, 1024)),
            ),
            None => (None, None, None),
        };
        sqlx::query(self.sql("UPDATE users SET avatar = ?, avatar_tag = ?, avatar_source = ? WHERE id = ?"))
            .bind(data)
            .bind(tag.clone())
            .bind(source)
            .bind(user_id)
            .execute(self.pool())
            .await?;
        Ok(tag)
    }

    pub async fn avatar(&self, user_id: i64) -> anyhow::Result<Option<Avatar>> {
        let row = sqlx::query(self.sql("SELECT avatar_tag, avatar FROM users WHERE id = ? AND avatar IS NOT NULL"))
            .bind(user_id)
            .fetch_optional(self.pool())
            .await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(Avatar {
            tag: r.try_get(0)?,
            data: r.try_get(1)?,
        }))
    }

    pub async fn create_session(&self, s: &NewSession<'_>, now: i64) -> anyhow::Result<()> {
        sqlx::query(self.sql(
            "INSERT INTO sessions (id_hash, user_id, created_at, expires_at, last_seen_at, id_token, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(s.id_hash)
        .bind(s.user_id)
        .bind(now)
        .bind(s.expires_at)
        .bind(now)
        .bind(s.id_token.map(str::to_owned))
        .bind(s.user_agent.map(|u| clip(u, 512)))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn session(&self, id_hash: &str) -> anyhow::Result<Option<SessionRow>> {
        let row = sqlx::query(self.sql(
            "SELECT u.id, u.username, u.display_name, u.email, u.admin, u.claim_groups, s.expires_at, s.last_seen_at, \
             u.avatar_tag FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.id_hash = ?",
        ))
        .bind(id_hash)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let groups: Option<String> = r.try_get(5)?;
        Ok(Some(SessionRow {
            user: User {
                id: r.try_get(0)?,
                username: r.try_get(1)?,
                display_name: r.try_get(2)?,
                email: r.try_get(3)?,
                admin: r.try_get::<i16, _>(4)? != 0,
                groups: groups.and_then(|g| serde_json::from_str(&g).ok()).unwrap_or_default(),
                avatar_tag: r.try_get(8)?,
            },
            expires_at: r.try_get(6)?,
            last_seen_at: r.try_get(7)?,
        }))
    }

    pub async fn touch_session(&self, id_hash: &str, now: i64) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE sessions SET last_seen_at = ? WHERE id_hash = ?"))
            .bind(now)
            .bind(id_hash)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Delete a session; returns its ID token (for the provider logout).
    pub async fn delete_session(&self, id_hash: &str) -> anyhow::Result<Option<String>> {
        let token = sqlx::query(self.sql("SELECT id_token FROM sessions WHERE id_hash = ?"))
            .bind(id_hash)
            .fetch_optional(self.pool())
            .await?
            .map(|r| r.try_get::<Option<String>, _>(0))
            .transpose()?
            .flatten();
        sqlx::query(self.sql("DELETE FROM sessions WHERE id_hash = ?"))
            .bind(id_hash)
            .execute(self.pool())
            .await?;
        Ok(token)
    }

    /// Remove expired and idle sessions and stale sign-ins.
    pub async fn purge_sessions(&self, now: i64, idle_ms: i64, login_ttl_ms: i64) -> anyhow::Result<u64> {
        let a = sqlx::query(self.sql("DELETE FROM sessions WHERE expires_at < ? OR last_seen_at < ?"))
            .bind(now)
            .bind(now - idle_ms)
            .execute(self.pool())
            .await?
            .rows_affected();
        sqlx::query(self.sql("DELETE FROM login_requests WHERE created_at < ?"))
            .bind(now - login_ttl_ms)
            .execute(self.pool())
            .await?;
        Ok(a)
    }

    pub async fn create_login(&self, state_hash: &str, l: &LoginRequest) -> anyhow::Result<()> {
        sqlx::query(self.sql(
            "INSERT INTO login_requests (state_hash, verifier, nonce, return_to, created_at) VALUES (?, ?, ?, ?, ?)",
        ))
        .bind(state_hash)
        .bind(&l.verifier)
        .bind(&l.nonce)
        .bind(clip(&l.return_to, 1024))
        .bind(l.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Fetch and delete a sign-in in progress (single use).
    pub async fn take_login(&self, state_hash: &str) -> anyhow::Result<Option<LoginRequest>> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query(
            self.sql("SELECT verifier, nonce, return_to, created_at FROM login_requests WHERE state_hash = ?"),
        )
        .bind(state_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let deleted = sqlx::query(self.sql("DELETE FROM login_requests WHERE state_hash = ?"))
            .bind(state_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        // Two concurrent callbacks: only the one that deleted the row wins.
        let (Some(r), 1) = (row, deleted) else { return Ok(None) };
        Ok(Some(LoginRequest {
            verifier: r.try_get(0)?,
            nonce: r.try_get(1)?,
            return_to: r.try_get(2)?,
            created_at: r.try_get(3)?,
        }))
    }
}
