//! Devices signed in with Quick Connect, and pairings in progress.

use sqlx::Row;

use super::users::clip;
use super::{Store, User};

/// A device signed in to a user's account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub platform: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
}

/// The device of a token, and its user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    pub user: User,
    pub device_id: i64,
    pub last_seen_at: i64,
}

pub struct NewPairing<'a> {
    pub secret_hash: &'a str,
    pub code: &'a str,
    pub device_name: &'a str,
    pub platform: Option<&'a str>,
    pub expires_at: i64,
}

/// A pairing awaiting approval, as shown to the approving user.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Pairing {
    pub device_name: String,
    pub platform: Option<String>,
    pub expires_at: i64,
}

/// Where a pairing stands, for the device polling it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Redeemed {
    /// Unknown, expired or already redeemed.
    Gone,
    Pending,
    /// Approved: the device now exists, with the token whose hash was given.
    Device {
        device: Device,
        user_id: i64,
    },
}

impl Store {
    /// Pairings not yet expired (a bound on unauthenticated requests).
    pub async fn open_pairings(&self, now: i64) -> anyhow::Result<i64> {
        let row = sqlx::query(self.sql("SELECT COUNT(*) FROM device_pairings WHERE expires_at > ?"))
            .bind(now)
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get(0)?)
    }

    /// Store a pairing; `false` when its code is taken (draw another).
    pub async fn create_pairing(&self, p: &NewPairing<'_>, now: i64) -> anyhow::Result<bool> {
        let taken = sqlx::query(self.sql("SELECT 1 FROM device_pairings WHERE code = ?"))
            .bind(p.code)
            .fetch_optional(self.pool())
            .await?
            .is_some();
        if taken {
            return Ok(false);
        }
        sqlx::query(self.sql(
            "INSERT INTO device_pairings (secret_hash, code, device_name, platform, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        ))
        .bind(p.secret_hash)
        .bind(p.code)
        .bind(clip(p.device_name, 255))
        .bind(p.platform.map(|s| clip(s, 64)))
        .bind(now)
        .bind(p.expires_at)
        .execute(self.pool())
        .await?;
        Ok(true)
    }

    /// The pairing showing `code`, while it awaits approval.
    pub async fn pairing(&self, code: &str, now: i64) -> anyhow::Result<Option<Pairing>> {
        let row = sqlx::query(self.sql(
            "SELECT device_name, platform, expires_at FROM device_pairings \
             WHERE code = ? AND expires_at > ? AND user_id IS NULL",
        ))
        .bind(code)
        .bind(now)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(Pairing {
            device_name: r.try_get(0)?,
            platform: r.try_get(1)?,
            expires_at: r.try_get(2)?,
        }))
    }

    /// Approve the pairing showing `code` for `user_id`; `false` when there
    /// is none awaiting approval.
    pub async fn approve_pairing(&self, code: &str, user_id: i64, now: i64) -> anyhow::Result<bool> {
        let n = sqlx::query(
            self.sql("UPDATE device_pairings SET user_id = ? WHERE code = ? AND expires_at > ? AND user_id IS NULL"),
        )
        .bind(user_id)
        .bind(code)
        .bind(now)
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// The device side of a pairing: once approved, create the device with
    /// `token_hash` and end the pairing (single use).
    pub async fn redeem_pairing(&self, secret_hash: &str, token_hash: &str, now: i64) -> anyhow::Result<Redeemed> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query(
            self.sql("SELECT device_name, platform, expires_at, user_id FROM device_pairings WHERE secret_hash = ?"),
        )
        .bind(secret_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(r) = row else { return Ok(Redeemed::Gone) };
        let name: String = r.try_get(0)?;
        let platform: Option<String> = r.try_get(1)?;
        let expires_at: i64 = r.try_get(2)?;
        let user_id: Option<i64> = r.try_get(3)?;
        if expires_at <= now {
            return Ok(Redeemed::Gone);
        }
        let Some(user_id) = user_id else {
            return Ok(Redeemed::Pending);
        };
        let deleted = sqlx::query(self.sql("DELETE FROM device_pairings WHERE secret_hash = ?"))
            .bind(secret_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // Two concurrent polls: only the one that deleted the row wins.
        if deleted != 1 {
            return Ok(Redeemed::Gone);
        }
        sqlx::query(self.sql(
            "INSERT INTO devices (user_id, name, platform, token_hash, created_at, last_seen_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        ))
        .bind(user_id)
        .bind(&name)
        .bind(platform.clone())
        .bind(token_hash)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let id: i64 = sqlx::query(self.sql("SELECT id FROM devices WHERE token_hash = ?"))
            .bind(token_hash)
            .fetch_one(&mut *tx)
            .await?
            .try_get(0)?;
        tx.commit().await?;
        Ok(Redeemed::Device {
            device: Device {
                id,
                name,
                platform,
                created_at: now,
                last_seen_at: now,
            },
            user_id,
        })
    }

    /// The device holding the token of `token_hash`.
    pub async fn device_by_token(&self, token_hash: &str) -> anyhow::Result<Option<DeviceRow>> {
        let row = sqlx::query(self.sql(
            "SELECT u.id, u.username, u.display_name, u.email, u.admin, u.claim_groups, d.id, d.last_seen_at, \
             u.avatar_tag FROM devices d JOIN users u ON u.id = d.user_id WHERE d.token_hash = ?",
        ))
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let groups: Option<String> = r.try_get(5)?;
        Ok(Some(DeviceRow {
            user: User {
                id: r.try_get(0)?,
                username: r.try_get(1)?,
                display_name: r.try_get(2)?,
                email: r.try_get(3)?,
                admin: r.try_get::<i16, _>(4)? != 0,
                groups: groups.and_then(|g| serde_json::from_str(&g).ok()).unwrap_or_default(),
                avatar_tag: r.try_get(8)?,
            },
            device_id: r.try_get(6)?,
            last_seen_at: r.try_get(7)?,
        }))
    }

    pub async fn touch_device(&self, id: i64, now: i64) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE devices SET last_seen_at = ? WHERE id = ?"))
            .bind(now)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// A user's devices, most recently used first.
    pub async fn devices(&self, user_id: i64) -> anyhow::Result<Vec<Device>> {
        let rows = sqlx::query(self.sql(
            "SELECT id, name, platform, created_at, last_seen_at FROM devices WHERE user_id = ? \
             ORDER BY last_seen_at DESC, id DESC",
        ))
        .bind(user_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Device {
                    id: r.try_get(0)?,
                    name: r.try_get(1)?,
                    platform: r.try_get(2)?,
                    created_at: r.try_get(3)?,
                    last_seen_at: r.try_get(4)?,
                })
            })
            .collect()
    }

    /// Sign a user's device out; returns its token hash (to drop from
    /// caches), `None` when the user has no such device.
    pub async fn delete_device(&self, user_id: i64, id: i64) -> anyhow::Result<Option<String>> {
        let hash: Option<String> = sqlx::query(self.sql("SELECT token_hash FROM devices WHERE id = ? AND user_id = ?"))
            .bind(id)
            .bind(user_id)
            .fetch_optional(self.pool())
            .await?
            .map(|r| r.try_get(0))
            .transpose()?;
        if hash.is_some() {
            sqlx::query(self.sql("DELETE FROM devices WHERE id = ? AND user_id = ?"))
                .bind(id)
                .bind(user_id)
                .execute(self.pool())
                .await?;
        }
        Ok(hash)
    }

    /// Remove devices unused for `idle_ms` and expired pairings; returns
    /// the devices removed.
    pub async fn purge_devices(&self, now: i64, idle_ms: i64) -> anyhow::Result<u64> {
        let n = sqlx::query(self.sql("DELETE FROM devices WHERE last_seen_at < ?"))
            .bind(now - idle_ms)
            .execute(self.pool())
            .await?
            .rows_affected();
        sqlx::query(self.sql("DELETE FROM device_pairings WHERE expires_at < ?"))
            .bind(now)
            .execute(self.pool())
            .await?;
        Ok(n)
    }
}
