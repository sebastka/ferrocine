//! Artwork stored in the database (resized JPEG).

use std::collections::{HashMap, HashSet};

use sqlx::Row;

use super::{Store, placeholders};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    Poster,
    Backdrop,
    Thumb,
    /// A title logo with transparency (PNG).
    Logo,
}

impl ImageKind {
    pub const ALL: [ImageKind; 4] = [
        ImageKind::Poster,
        ImageKind::Backdrop,
        ImageKind::Thumb,
        ImageKind::Logo,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ImageKind::Poster => "poster",
            ImageKind::Backdrop => "backdrop",
            ImageKind::Thumb => "thumb",
            ImageKind::Logo => "logo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSource {
    /// A file next to the media.
    Local,
    /// Downloaded from a metadata provider.
    Remote,
    /// A frame extracted from the video.
    Extracted,
}

impl ImageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageSource::Local => "local",
            ImageSource::Remote => "remote",
            ImageSource::Extracted => "extracted",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "local" => ImageSource::Local,
            "remote" => ImageSource::Remote,
            _ => ImageSource::Extracted,
        }
    }

    /// Higher wins: local artwork over downloaded over extracted frames.
    pub fn priority(self) -> u8 {
        match self {
            ImageSource::Local => 3,
            ImageSource::Remote => 2,
            ImageSource::Extracted => 1,
        }
    }
}

/// What is stored for an image, without its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub source: ImageSource,
    pub source_key: String,
    pub tag: String,
}

impl Store {
    /// Stored images of the given items: `(item, kind) → tag`.
    pub async fn image_tags(&self, items: &[i64]) -> anyhow::Result<HashMap<(i64, ImageKind), String>> {
        let mut out = HashMap::new();
        for chunk in items.chunks(500) {
            let sql = format!(
                "SELECT item_id, kind, tag FROM images WHERE item_id IN ({})",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql));
            for id in chunk {
                q = q.bind(*id);
            }
            for r in q.fetch_all(self.pool()).await? {
                let kind: String = r.try_get(1)?;
                if let Some(k) = ImageKind::parse(&kind) {
                    out.insert((r.try_get(0)?, k), r.try_get(2)?);
                }
            }
        }
        Ok(out)
    }

    /// Image metadata of every item of a library (for the scanner).
    pub async fn library_images(&self, library: &str) -> anyhow::Result<HashMap<(i64, ImageKind), ImageInfo>> {
        let rows = sqlx::query(self.sql(
            "SELECT g.item_id, g.kind, g.source, g.source_key, g.tag FROM images g \
             JOIN items i ON i.id = g.item_id WHERE i.library_id = ?",
        ))
        .bind(library)
        .fetch_all(self.pool())
        .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let kind: String = r.try_get(1)?;
            let Some(k) = ImageKind::parse(&kind) else { continue };
            let source: String = r.try_get(2)?;
            out.insert(
                (r.try_get(0)?, k),
                ImageInfo {
                    source: ImageSource::parse(&source),
                    source_key: r.try_get(3)?,
                    tag: r.try_get(4)?,
                },
            );
        }
        Ok(out)
    }

    /// The tag of an item's image: its file in the metadata directory.
    pub async fn image_tag(&self, item: i64, kind: ImageKind) -> anyhow::Result<Option<String>> {
        let row = sqlx::query(self.sql("SELECT tag FROM images WHERE item_id = ? AND kind = ?"))
            .bind(item)
            .bind(kind.as_str())
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?)
    }

    /// Record an image stored in the metadata directory under `tag`.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_image(
        &self,
        item: i64,
        kind: ImageKind,
        source: ImageSource,
        source_key: &str,
        width: u32,
        height: u32,
        tag: &str,
    ) -> anyhow::Result<()> {
        let sql = format!(
            "INSERT INTO images (item_id, kind, source, source_key, width, height, tag, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?){}",
            self.upsert_tail(
                "item_id, kind",
                &["source", "source_key", "width", "height", "tag", "updated_at"]
            )
        );
        let key: String = source_key.chars().take(512).collect();
        sqlx::query(self.sql_owned(sql))
            .bind(item)
            .bind(kind.as_str())
            .bind(source.as_str())
            .bind(key)
            .bind(width as i32)
            .bind(height as i32)
            .bind(tag)
            .bind(super::now_ms())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Every image tag in use, to keep their files.
    pub async fn all_image_tags(&self) -> anyhow::Result<HashSet<String>> {
        let rows = sqlx::query(self.sql("SELECT DISTINCT tag FROM images"))
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }

    pub async fn delete_image(&self, item: i64, kind: ImageKind) -> anyhow::Result<()> {
        sqlx::query(self.sql("DELETE FROM images WHERE item_id = ? AND kind = ?"))
            .bind(item)
            .bind(kind.as_str())
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
