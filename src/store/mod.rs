//! Persistence on PostgreSQL or MariaDB through sqlx's `Any` driver.
//!
//! Queries are written once with `?` placeholders, which are rewritten to
//! `$n` for PostgreSQL. Only the few statements whose syntax differs (upserts,
//! advisory locks) are written per dialect. Timestamps are BIGINT
//! milliseconds and booleans SMALLINT, so no date or boolean type crosses the
//! driver boundary.

mod devices;
mod images;
mod items;
mod subtitles;
mod trickplay;
mod users;

use std::borrow::Cow;
use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use sqlx::any::{AnyConnectOptions, AnyPoolOptions};
use sqlx::pool::PoolConnection;
use sqlx::{Any, AnyPool, AssertSqlSafe, Row};

pub use devices::*;
pub use images::*;
pub use items::*;
pub use subtitles::*;
pub use trickplay::*;
pub use users::*;

use crate::config::{DatabaseConfig, LibraryConfig};

static PG_MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");
static MYSQL_MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/mysql");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
}

#[derive(Clone)]
pub struct Store {
    pool: AnyPool,
    dialect: Dialect,
}

/// Scan state of a library.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScanState {
    pub last_started_at: Option<i64>,
    pub last_finished_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Rewrite `?` placeholders as `$1`, `$2`… (queries contain no `?` literal).
fn to_postgres(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut n = 0;
    for c in sql.chars() {
        if c == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(c);
        }
    }
    out
}

/// SHA-256 as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub(crate) fn bool_i16(b: bool) -> i16 {
    i16::from(b)
}

/// `?, ?, ?` for an `IN (…)` list of `n` values.
pub(crate) fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn parse_url(url: &str) -> anyhow::Result<(Dialect, String)> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok((Dialect::Postgres, url.to_owned()))
    } else if let Some(rest) = url.strip_prefix("mariadb://") {
        Ok((Dialect::MySql, format!("mysql://{rest}")))
    } else if url.starts_with("mysql://") {
        Ok((Dialect::MySql, url.to_owned()))
    } else {
        bail!("unsupported database url: expected postgres://, mariadb:// or mysql://")
    }
}

impl Store {
    pub async fn connect(cfg: &DatabaseConfig) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let (dialect, url) = parse_url(&cfg.url)?;
        let options: AnyConnectOptions = url.parse().context("invalid database url")?;
        let pool = AnyPoolOptions::new()
            .max_connections(cfg.max_connections.max(4))
            .min_connections(0)
            .acquire_timeout(cfg.connect_timeout.0)
            .idle_timeout(Some(Duration::from_secs(300)))
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    if dialect == Dialect::MySql {
                        // Strict mode: an over-long value is an error, not silently truncated.
                        sqlx::query(AssertSqlSafe(
                            "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION'",
                        ))
                        .execute(conn)
                        .await?;
                    }
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .context("cannot connect to the database")?;
        Ok(Self { pool, dialect })
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub(crate) fn sql(&self, s: &'static str) -> AssertSqlSafe<Cow<'static, str>> {
        AssertSqlSafe(match self.dialect {
            Dialect::MySql => Cow::Borrowed(s),
            Dialect::Postgres => Cow::Owned(to_postgres(s)),
        })
    }

    pub(crate) fn sql_owned(&self, s: String) -> AssertSqlSafe<Cow<'static, str>> {
        AssertSqlSafe(match self.dialect {
            Dialect::MySql => Cow::Owned(s),
            Dialect::Postgres => Cow::Owned(to_postgres(&s)),
        })
    }

    /// `ON CONFLICT (…) DO UPDATE SET a = EXCLUDED.a` or `ON DUPLICATE KEY UPDATE a = VALUES(a)`.
    pub(crate) fn upsert_tail(&self, key: &str, columns: &[&str]) -> String {
        match self.dialect {
            Dialect::Postgres => {
                let sets: Vec<String> = columns.iter().map(|c| format!("{c} = EXCLUDED.{c}")).collect();
                format!(" ON CONFLICT ({key}) DO UPDATE SET {}", sets.join(", "))
            }
            Dialect::MySql => {
                let sets: Vec<String> = columns.iter().map(|c| format!("{c} = VALUES({c})")).collect();
                format!(" ON DUPLICATE KEY UPDATE {}", sets.join(", "))
            }
        }
    }

    pub(crate) fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        let migrator = match self.dialect {
            Dialect::Postgres => &PG_MIGRATIONS,
            Dialect::MySql => &MYSQL_MIGRATIONS,
        };
        // sqlx's own migration lock did not stop two processes migrating
        // MariaDB at once (tested: duplicate entry, then "partially
        // applied"), so instances starting together queue on ours.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let lock = loop {
            if let Some(l) = self.try_lock("migrate").await? {
                break l;
            }
            if std::time::Instant::now() > deadline {
                bail!("another instance has been migrating the database for two minutes");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let result = migrator.run(&self.pool).await.context("database migration failed");
        lock.release().await;
        result
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query(self.sql("SELECT 1")).execute(&self.pool).await?;
        Ok(())
    }

    // ----------------------------------------------------------- libraries

    /// Mirror the configured libraries and flag the others as removed. The
    /// items of a removed library stay until it comes back or is purged.
    pub async fn sync_libraries(&self, libraries: &[LibraryConfig]) -> anyhow::Result<()> {
        let now = now_ms();
        let upsert = format!(
            "INSERT INTO libraries (id, name, kind, created_at, updated_at, removed_at) VALUES (?, ?, ?, ?, ?, NULL){}",
            self.upsert_tail("id", &["name", "kind", "updated_at", "removed_at"])
        );
        let mut tx = self.pool.begin().await?;
        for l in libraries {
            sqlx::query(self.sql_owned(upsert.clone()))
                .bind(&l.id)
                .bind(&l.name)
                .bind(l.kind.as_str())
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("cannot register library {}", l.id))?;
        }
        let active: Vec<String> = sqlx::query(self.sql("SELECT id FROM libraries WHERE removed_at IS NULL"))
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|r| r.try_get::<String, _>(0))
            .collect::<Result<_, _>>()?;
        for id in active {
            if !libraries.iter().any(|l| l.id == id) {
                sqlx::query(self.sql("UPDATE libraries SET removed_at = ? WHERE id = ?"))
                    .bind(now)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                tracing::info!(library = %id, "library removed from the configuration; its items are kept");
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn scan_started(&self, library: &str, at: i64) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE libraries SET scan_started_at = ? WHERE id = ?"))
            .bind(at)
            .bind(library)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn scan_finished(&self, library: &str, at: i64, error: Option<&str>) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE libraries SET scan_finished_at = ?, scan_error = ? WHERE id = ?"))
            .bind(at)
            .bind(error.map(str::to_owned))
            .bind(library)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn scan_states(&self) -> anyhow::Result<Vec<(String, ScanState)>> {
        let rows =
            sqlx::query(self.sql(
                "SELECT id, scan_started_at, scan_finished_at, scan_error FROM libraries WHERE removed_at IS NULL",
            ))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get(0)?,
                    ScanState {
                        last_started_at: r.try_get(1)?,
                        last_finished_at: r.try_get(2)?,
                        last_error: r.try_get(3)?,
                    },
                ))
            })
            .collect()
    }

    // --------------------------------------------------------------- locks

    /// Try to take a named, session-scoped advisory lock, so that two
    /// instances sharing the database do not scan the same library at once.
    /// The lock lives as long as the returned guard's connection.
    pub async fn try_lock(&self, name: &str) -> anyhow::Result<Option<NamedLock>> {
        let mut conn = self.pool.acquire().await?;
        let got = match self.dialect {
            Dialect::Postgres => {
                let key = lock_key(name);
                let row = sqlx::query(AssertSqlSafe("SELECT CAST(pg_try_advisory_lock($1) AS INTEGER)"))
                    .bind(key)
                    .fetch_one(&mut *conn)
                    .await?;
                row.try_get::<i32, _>(0)? == 1
            }
            Dialect::MySql => {
                let row = sqlx::query(AssertSqlSafe("SELECT CAST(COALESCE(GET_LOCK(?, 0), 0) AS SIGNED)"))
                    .bind(lock_name(name))
                    .fetch_one(&mut *conn)
                    .await?;
                row.try_get::<i64, _>(0)? == 1
            }
        };
        Ok(got.then(|| NamedLock {
            conn: Some(conn),
            name: name.to_owned(),
            dialect: self.dialect,
        }))
    }
}

/// PostgreSQL advisory locks take a 64-bit key.
fn lock_key(name: &str) -> i64 {
    let digest = Sha256::digest(format!("ferrocine:{name}").as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(b)
}

/// MariaDB lock names are limited to 64 characters.
fn lock_name(name: &str) -> String {
    let full = format!("ferrocine:{name}");
    if full.len() <= 64 {
        full
    } else {
        format!("ferrocine:{}", &sha256_hex(name.as_bytes())[..48])
    }
}

/// An advisory lock held on its own connection.
pub struct NamedLock {
    conn: Option<PoolConnection<Any>>,
    name: String,
    dialect: Dialect,
}

impl NamedLock {
    pub async fn release(mut self) {
        let Some(mut conn) = self.conn.take() else { return };
        let result = match self.dialect {
            Dialect::Postgres => sqlx::query(AssertSqlSafe("SELECT pg_advisory_unlock($1)"))
                .bind(lock_key(&self.name))
                .execute(&mut *conn)
                .await
                .map(|_| ()),
            Dialect::MySql => sqlx::query(AssertSqlSafe("SELECT RELEASE_LOCK(?)"))
                .bind(lock_name(&self.name))
                .execute(&mut *conn)
                .await
                .map(|_| ()),
        };
        if let Err(e) = result {
            // Closing the connection releases the lock anyway.
            tracing::warn!(error = %e, lock = %self.name, "cannot release lock; closing its connection");
            let _ = conn.detach();
        }
    }
}

impl Drop for NamedLock {
    fn drop(&mut self) {
        // Not released explicitly (panic, cancelled task): close the
        // connection rather than return it to the pool still holding the lock.
        if let Some(conn) = self.conn.take() {
            drop(conn.detach());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_numbered_for_postgres() {
        assert_eq!(to_postgres("a = ? AND b IN (?, ?)"), "a = $1 AND b IN ($2, $3)");
    }

    #[test]
    fn urls_select_the_dialect() {
        assert_eq!(parse_url("postgres://x/y").unwrap().0, Dialect::Postgres);
        let (d, url) = parse_url("mariadb://u:p@h/db").unwrap();
        assert_eq!((d, url.as_str()), (Dialect::MySql, "mysql://u:p@h/db"));
        assert!(parse_url("sqlite://x.db").is_err());
    }

    #[test]
    fn lock_names_fit_mariadb() {
        assert!(lock_name(&"x".repeat(200)).len() <= 64);
        assert_eq!(lock_name("scan:movies"), "ferrocine:scan:movies");
    }
}
