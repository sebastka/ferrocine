//! Subtitles downloaded from providers, and the record of automatic searches.

use sqlx::Row;

use super::{Store, bool_i16};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSubtitle {
    pub id: i64,
    pub item_id: i64,
    pub language: String,
    pub source: String,
    pub source_id: String,
    pub release_name: Option<String>,
    pub hearing_impaired: bool,
    pub forced: bool,
    pub hash_match: bool,
    pub created_by: String,
    pub created_at: i64,
}

pub struct NewSubtitle<'a> {
    pub item_id: i64,
    pub language: &'a str,
    pub source: &'a str,
    pub source_id: &'a str,
    pub release_name: Option<&'a str>,
    pub hearing_impaired: bool,
    pub forced: bool,
    pub hash_match: bool,
    /// Tag of the WebVTT file in the metadata directory.
    pub content_tag: &'a str,
    pub created_by: &'a str,
}

const COLUMNS: &str = "id, item_id, language, source, source_id, release_name, hearing_impaired, forced, hash_match, created_by, created_at";

fn from_row(r: &sqlx::any::AnyRow) -> Result<StoredSubtitle, sqlx::Error> {
    Ok(StoredSubtitle {
        id: r.try_get(0)?,
        item_id: r.try_get(1)?,
        language: r.try_get(2)?,
        source: r.try_get(3)?,
        source_id: r.try_get(4)?,
        release_name: r.try_get(5)?,
        hearing_impaired: r.try_get::<i16, _>(6)? != 0,
        forced: r.try_get::<i16, _>(7)? != 0,
        hash_match: r.try_get::<i16, _>(8)? != 0,
        created_by: r.try_get(9)?,
        created_at: r.try_get(10)?,
    })
}

impl Store {
    pub async fn subtitles_of(&self, item_id: i64) -> anyhow::Result<Vec<StoredSubtitle>> {
        let rows = sqlx::query(self.sql_owned(format!(
            "SELECT {COLUMNS} FROM subtitles WHERE item_id = ? ORDER BY language, id"
        )))
        .bind(item_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(from_row).collect::<Result<_, _>>().map_err(Into::into)
    }

    /// The tag of a stored subtitle's WebVTT file.
    pub async fn subtitle_tag(&self, item_id: i64, id: i64) -> anyhow::Result<Option<String>> {
        let row = sqlx::query(self.sql("SELECT content_tag FROM subtitles WHERE id = ? AND item_id = ?"))
            .bind(id)
            .bind(item_id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.try_get::<Option<String>, _>(0)).transpose()?.flatten())
    }

    /// Subtitles whose WebVTT is still in the database: (id, text).
    pub async fn subtitles_without_files(&self) -> anyhow::Result<Vec<(i64, String)>> {
        let rows = sqlx::query(
            self.sql("SELECT id, content FROM subtitles WHERE content_tag IS NULL AND content IS NOT NULL"),
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?))).collect()
    }

    /// The WebVTT of a subtitle is now in the metadata directory.
    pub async fn set_subtitle_file(&self, id: i64, tag: &str) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE subtitles SET content_tag = ?, content = NULL WHERE id = ?"))
            .bind(tag)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Every subtitle file tag in use.
    pub async fn all_subtitle_tags(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let rows = sqlx::query(self.sql("SELECT DISTINCT content_tag FROM subtitles WHERE content_tag IS NOT NULL"))
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }

    /// Store a subtitle; an existing one with the same source id is kept.
    /// Returns the stored row.
    pub async fn add_subtitle(&self, s: &NewSubtitle<'_>, now: i64) -> anyhow::Result<StoredSubtitle> {
        let existing = sqlx::query(self.sql_owned(format!(
            "SELECT {COLUMNS} FROM subtitles WHERE item_id = ? AND source = ? AND source_id = ?"
        )))
        .bind(s.item_id)
        .bind(s.source)
        .bind(s.source_id)
        .fetch_optional(self.pool())
        .await?;
        if let Some(r) = existing {
            return Ok(from_row(&r)?);
        }
        let release: Option<String> = s.release_name.map(|r| r.chars().take(512).collect());
        sqlx::query(self.sql(
            "INSERT INTO subtitles (item_id, language, source, source_id, release_name, hearing_impaired, forced, \
             hash_match, content_tag, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(s.item_id)
        .bind(s.language)
        .bind(s.source)
        .bind(s.source_id)
        .bind(release)
        .bind(bool_i16(s.hearing_impaired))
        .bind(bool_i16(s.forced))
        .bind(bool_i16(s.hash_match))
        .bind(s.content_tag)
        .bind(s.created_by.chars().take(255).collect::<String>())
        .bind(now)
        .execute(self.pool())
        .await?;
        let row = sqlx::query(self.sql_owned(format!(
            "SELECT {COLUMNS} FROM subtitles WHERE item_id = ? AND source = ? AND source_id = ?"
        )))
        .bind(s.item_id)
        .bind(s.source)
        .bind(s.source_id)
        .fetch_one(self.pool())
        .await?;
        Ok(from_row(&row)?)
    }

    pub async fn delete_subtitle(&self, item_id: i64, id: i64) -> anyhow::Result<bool> {
        let r = sqlx::query(self.sql("DELETE FROM subtitles WHERE id = ? AND item_id = ?"))
            .bind(id)
            .bind(item_id)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// When an automatic search for `language` last ran for an item, and
    /// whether it found something.
    pub async fn subtitle_search(&self, item_id: i64, language: &str) -> anyhow::Result<Option<(i64, bool)>> {
        let row = sqlx::query(
            self.sql("SELECT searched_at, found FROM subtitle_searches WHERE item_id = ? AND language = ?"),
        )
        .bind(item_id)
        .bind(language)
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            Some(r) => Some((r.try_get(0)?, r.try_get::<i16, _>(1)? != 0)),
            None => None,
        })
    }

    pub async fn record_subtitle_search(
        &self,
        item_id: i64,
        language: &str,
        found: bool,
        now: i64,
    ) -> anyhow::Result<()> {
        let sql = format!(
            "INSERT INTO subtitle_searches (item_id, language, searched_at, found) VALUES (?, ?, ?, ?){}",
            self.upsert_tail("item_id, language", &["searched_at", "found"])
        );
        sqlx::query(self.sql_owned(sql))
            .bind(item_id)
            .bind(language)
            .bind(now)
            .bind(bool_i16(found))
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
