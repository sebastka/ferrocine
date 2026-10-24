//! Seek bar previews: which preview set (in the metadata directory) an item has.

use std::collections::HashSet;

use sqlx::Row;

use super::{Store, now_ms};

/// A preview set: sheets of `columns` × `rows` frames of `width` × `height`,
/// one frame every `interval_ms`, `thumbnails` in all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trickplay {
    pub tag: String,
    pub interval_ms: i64,
    pub width: i32,
    pub height: i32,
    pub columns: i32,
    pub rows: i32,
    pub thumbnails: i32,
}

impl Store {
    pub async fn trickplay(&self, item_id: i64) -> anyhow::Result<Option<Trickplay>> {
        let row = sqlx::query(self.sql(
            "SELECT tag, interval_ms, width, height, tile_columns, tile_rows, thumbnails FROM trickplay \
             WHERE item_id = ?",
        ))
        .bind(item_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            Some(r) => Some(Trickplay {
                tag: r.try_get(0)?,
                interval_ms: r.try_get(1)?,
                width: r.try_get(2)?,
                height: r.try_get(3)?,
                columns: r.try_get(4)?,
                rows: r.try_get(5)?,
                thumbnails: r.try_get(6)?,
            }),
            None => None,
        })
    }

    pub async fn put_trickplay(&self, item_id: i64, t: &Trickplay) -> anyhow::Result<()> {
        let sql = format!(
            "INSERT INTO trickplay (item_id, tag, interval_ms, width, height, tile_columns, tile_rows, thumbnails, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?){}",
            self.upsert_tail(
                "item_id",
                &[
                    "tag",
                    "interval_ms",
                    "width",
                    "height",
                    "tile_columns",
                    "tile_rows",
                    "thumbnails",
                    "created_at"
                ]
            )
        );
        sqlx::query(self.sql_owned(sql))
            .bind(item_id)
            .bind(&t.tag)
            .bind(t.interval_ms)
            .bind(t.width)
            .bind(t.height)
            .bind(t.columns)
            .bind(t.rows)
            .bind(t.thumbnails)
            .bind(now_ms())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Every preview set in use.
    pub async fn all_trickplay_tags(&self) -> anyhow::Result<HashSet<String>> {
        let rows = sqlx::query(self.sql("SELECT tag FROM trickplay"))
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }
}
