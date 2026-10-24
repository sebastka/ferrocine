//! Items (movies, series, seasons, episodes), as written by the scanner and
//! read by the API.

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::{Dialect, Store, bool_i16, placeholders};
use crate::media::probe::MediaInfo;
use crate::media::{ItemKind, Meta, Person};

const IN_CHUNK: usize = 500;

/// A query argument, so that dynamic queries can collect their binds.
#[derive(Clone, Debug)]
pub enum Arg {
    I(i64),
    S(String),
}

macro_rules! bind_args {
    ($q:expr, $args:expr) => {{
        let mut q = $q;
        for a in $args {
            q = match a {
                Arg::I(v) => q.bind(*v),
                Arg::S(s) => q.bind(s.clone()),
            };
        }
        q
    }};
}

/// Every column of `items`, in [`ItemRow::from_row`] order.
const ITEM_COLUMNS: &str = "id, library_id, kind, parent_id, series_id, path_key, path, title, sort_title, \
     original_title, year, premiere_date, season_number, episode_number, overview, tagline, rating_x100, \
     official_rating, genres, studios, people, tmdb_id, imdb_id, tvdb_id, runtime_ms, file_size, file_mtime, \
     media, local_key, remote_fetched_at, remote_matched, added_at, content_added_at, ratings, anidb_id, anilist_id, \
     original_language, provider_title, extra_type";

#[derive(Clone, Debug, PartialEq)]
pub struct ItemRow {
    pub id: i64,
    pub library_id: String,
    pub kind: ItemKind,
    pub parent_id: Option<i64>,
    pub series_id: Option<i64>,
    pub path_key: String,
    pub path: Option<String>,
    pub meta: Meta,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub file_size: Option<i64>,
    pub file_mtime: Option<i64>,
    pub media: Option<MediaInfo>,
    pub local_key: Option<String>,
    pub remote_fetched_at: Option<i64>,
    pub remote_matched: bool,
    pub added_at: i64,
    pub content_added_at: i64,
    /// Of an extra: `trailer`, `featurette`, `extra`…
    pub extra_type: Option<String>,
}

fn json_list<T: serde::de::DeserializeOwned>(s: Option<String>) -> Vec<T> {
    s.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn from_x100(v: Option<i32>) -> Option<f64> {
    v.map(|r| f64::from(r) / 100.0)
}

fn to_json<T: serde::Serialize>(v: &[T]) -> Option<String> {
    (!v.is_empty()).then(|| serde_json::to_string(v).unwrap_or_default())
}

impl ItemRow {
    fn from_row(r: &AnyRow) -> Result<Self, sqlx::Error> {
        Ok(ItemRow {
            id: r.try_get(0)?,
            library_id: r.try_get(1)?,
            kind: ItemKind::from_code(r.try_get(2)?),
            parent_id: r.try_get(3)?,
            series_id: r.try_get(4)?,
            path_key: r.try_get(5)?,
            path: r.try_get(6)?,
            meta: Meta {
                title: r.try_get(7)?,
                // A sort title derived from the title is derived again on write.
                sort_title: {
                    let title: String = r.try_get(7)?;
                    r.try_get::<Option<String>, _>(8)?
                        .filter(|s| *s != crate::media::naming::sort_title(&title))
                },
                original_title: r.try_get(9)?,
                original_language: r.try_get(36)?,
                provider_title: r.try_get(37)?,
                year: r.try_get(10)?,
                premiere_date: r.try_get(11)?,
                overview: r.try_get(14)?,
                tagline: r.try_get(15)?,
                rating: from_x100(r.try_get(16)?),
                official_rating: r.try_get(17)?,
                genres: json_list(r.try_get(18)?),
                studios: json_list(r.try_get(19)?),
                people: json_list::<Person>(r.try_get(20)?),
                tmdb_id: r.try_get(21)?,
                imdb_id: r.try_get(22)?,
                tvdb_id: r.try_get(23)?,
                anidb_id: r.try_get(34)?,
                anilist_id: r.try_get(35)?,
                runtime_ms: r.try_get(24)?,
                ratings: json_list(r.try_get(33)?),
            },
            season_number: r.try_get(12)?,
            episode_number: r.try_get(13)?,
            file_size: r.try_get(25)?,
            file_mtime: r.try_get(26)?,
            media: r
                .try_get::<Option<String>, _>(27)?
                .and_then(|s| serde_json::from_str(&s).ok()),
            local_key: r.try_get(28)?,
            remote_fetched_at: r.try_get(29)?,
            remote_matched: r.try_get::<i16, _>(30)? != 0,
            added_at: r.try_get(31)?,
            content_added_at: r.try_get(32)?,
            extra_type: r.try_get(38)?,
        })
    }
}

/// What the scanner writes for one item.
#[derive(Clone, Debug)]
pub struct ItemWrite {
    pub library_id: String,
    pub kind: ItemKind,
    pub parent_id: Option<i64>,
    pub series_id: Option<i64>,
    pub path_key: String,
    pub path: Option<String>,
    pub meta: Meta,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub file_size: Option<i64>,
    pub file_mtime: Option<i64>,
    pub media: Option<MediaInfo>,
    pub local_key: Option<String>,
    pub remote_fetched_at: Option<i64>,
    pub remote_matched: bool,
    /// Used when the item is new; an existing item keeps its date.
    pub added_at: i64,
    pub seen_scan: i64,
    /// Of an extra: `trailer`, `featurette`, `extra`…
    pub extra_type: Option<String>,
}

/// One row of a list, before images and counts are attached.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryRow {
    pub id: i64,
    pub library_id: String,
    pub kind: ItemKind,
    pub title: String,
    pub year: Option<i32>,
    pub parent_id: Option<i64>,
    pub series_id: Option<i64>,
    pub series_title: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub runtime_ms: Option<i64>,
    pub rating: Option<f64>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub premiere_date: Option<String>,
    pub added_at: i64,
    pub user: UserData,
    pub extra_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct UserData {
    pub position_ms: i64,
    pub played: bool,
    pub play_count: i32,
    pub favorite: bool,
    pub last_played_at: Option<i64>,
}

const SUMMARY_SELECT: &str = "SELECT i.id, i.library_id, i.kind, i.title, i.year, i.parent_id, i.series_id, s.title, \
     i.season_number, i.episode_number, i.runtime_ms, i.rating_x100, i.official_rating, i.genres, i.premiere_date, \
     i.added_at, ud.position_ms, ud.played, ud.play_count, ud.favorite, ud.last_played_at, i.extra_type \
     FROM items i LEFT JOIN items s ON s.id = i.series_id \
     LEFT JOIN user_data ud ON ud.item_id = i.id AND ud.user_id = ?";

impl SummaryRow {
    fn from_row(r: &AnyRow) -> Result<Self, sqlx::Error> {
        Ok(SummaryRow {
            id: r.try_get(0)?,
            library_id: r.try_get(1)?,
            kind: ItemKind::from_code(r.try_get(2)?),
            title: r.try_get(3)?,
            year: r.try_get(4)?,
            parent_id: r.try_get(5)?,
            series_id: r.try_get(6)?,
            series_title: r.try_get(7)?,
            season_number: r.try_get(8)?,
            episode_number: r.try_get(9)?,
            runtime_ms: r.try_get(10)?,
            rating: from_x100(r.try_get(11)?),
            official_rating: r.try_get(12)?,
            genres: json_list(r.try_get(13)?),
            premiere_date: r.try_get(14)?,
            added_at: r.try_get(15)?,
            user: UserData {
                position_ms: r.try_get::<Option<i64>, _>(16)?.unwrap_or(0),
                played: r.try_get::<Option<i16>, _>(17)?.unwrap_or(0) != 0,
                play_count: r.try_get::<Option<i32>, _>(18)?.unwrap_or(0),
                favorite: r.try_get::<Option<i16>, _>(19)?.unwrap_or(0) != 0,
                last_played_at: r.try_get(20)?,
            },
            extra_type: r.try_get(21)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Title,
    Year,
    Added,
    Rating,
    Released,
}

/// Filters of a library listing.
#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    pub library_id: String,
    pub sort: SortBy,
    pub descending: bool,
    pub genre: Option<String>,
    pub played: Option<bool>,
    pub favorite: bool,
    pub title: Option<String>,
    pub offset: i64,
    pub limit: i64,
}

/// Every word of a query in the item's search text (titles, cast, crew).
fn search_clauses(query: &str, clauses: &mut Vec<String>, args: &mut Vec<Arg>) {
    let terms = crate::search::terms(query);
    if terms.is_empty() {
        // Nothing searchable (punctuation only): nothing found.
        clauses.push("1 = 0".into());
    }
    for t in terms {
        clauses.push("i.search_text LIKE ?".into());
        args.push(Arg::S(format!("%{}%", like_escape(&t))));
    }
}

/// Escape `%`, `_` and `\` for a LIKE pattern (backslash is the default
/// escape character of both engines).
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Store {
    // ----------------------------------------------------------- scanner

    /// Every item of a library.
    pub async fn library_items(&self, library: &str) -> anyhow::Result<Vec<ItemRow>> {
        let rows = sqlx::query(self.sql_owned(format!("SELECT {ITEM_COLUMNS} FROM items WHERE library_id = ?")))
            .bind(library)
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(ItemRow::from_row)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    pub async fn item_row(&self, id: i64) -> anyhow::Result<Option<ItemRow>> {
        let row = sqlx::query(self.sql_owned(format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?")))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(ItemRow::from_row).transpose()?)
    }

    /// Insert or update an item by `path_key`; returns its id.
    pub async fn upsert_item(&self, w: &ItemWrite) -> anyhow::Result<i64> {
        let updated = [
            "kind",
            "parent_id",
            "series_id",
            "path",
            "title",
            "sort_title",
            "original_title",
            "year",
            "premiere_date",
            "season_number",
            "episode_number",
            "overview",
            "tagline",
            "rating_x100",
            "official_rating",
            "genres",
            "studios",
            "people",
            "tmdb_id",
            "imdb_id",
            "tvdb_id",
            "runtime_ms",
            "file_size",
            "file_mtime",
            "media",
            "local_key",
            "remote_fetched_at",
            "remote_matched",
            "updated_at",
            "seen_scan",
            "ratings",
            "anidb_id",
            "anilist_id",
            "original_language",
            "provider_title",
            "extra_type",
            "search_text",
        ];
        let sql = format!(
            "INSERT INTO items (library_id, kind, parent_id, series_id, path_key, path, title, sort_title, \
             original_title, year, premiere_date, season_number, episode_number, overview, tagline, rating_x100, \
             official_rating, genres, studios, people, tmdb_id, imdb_id, tvdb_id, runtime_ms, file_size, \
             file_mtime, media, local_key, remote_fetched_at, remote_matched, added_at, content_added_at, \
             updated_at, seen_scan, ratings, anidb_id, anilist_id, original_language, provider_title, extra_type, search_text) \
             VALUES ({}){}",
            placeholders(41),
            self.upsert_tail("path_key", &updated)
        );
        let m = &w.meta;
        let sort = m
            .sort_title
            .clone()
            .unwrap_or_else(|| crate::media::naming::sort_title(&m.title));
        let now = super::now_ms();
        sqlx::query(self.sql_owned(sql))
            .bind(&w.library_id)
            .bind(w.kind.code())
            .bind(w.parent_id)
            .bind(w.series_id)
            .bind(&w.path_key)
            .bind(w.path.clone())
            .bind(truncate(&m.title, 512))
            .bind(truncate(&sort, 512))
            .bind(m.original_title.as_deref().map(|t| truncate(t, 512)))
            .bind(m.year)
            .bind(m.premiere_date.as_deref().map(|d| truncate(d, 10)))
            .bind(w.season_number)
            .bind(w.episode_number)
            .bind(m.overview.clone())
            .bind(m.tagline.as_deref().map(|t| truncate(t, 1024)))
            .bind(m.rating.filter(|r| r.is_finite()).map(|r| (r * 100.0).round() as i32))
            .bind(m.official_rating.as_deref().map(|t| truncate(t, 32)))
            .bind(to_json(&m.genres))
            .bind(to_json(&m.studios))
            .bind(to_json(&m.people))
            .bind(m.tmdb_id.as_deref().map(|t| truncate(t, 32)))
            .bind(m.imdb_id.as_deref().map(|t| truncate(t, 32)))
            .bind(m.tvdb_id.as_deref().map(|t| truncate(t, 32)))
            .bind(m.runtime_ms)
            .bind(w.file_size)
            .bind(w.file_mtime)
            .bind(w.media.as_ref().map(|x| serde_json::to_string(x).unwrap_or_default()))
            .bind(w.local_key.clone())
            .bind(w.remote_fetched_at)
            .bind(bool_i16(w.remote_matched))
            .bind(w.added_at)
            .bind(w.added_at)
            .bind(now)
            .bind(w.seen_scan)
            .bind(to_json(&m.ratings))
            .bind(m.anidb_id.as_deref().map(|t| truncate(t, 32)))
            .bind(m.anilist_id.as_deref().map(|t| truncate(t, 32)))
            .bind(m.original_language.as_deref().map(|t| truncate(t, 8)))
            .bind(m.provider_title.as_deref().map(|t| truncate(t, 512)))
            .bind(w.extra_type.as_deref().map(|t| truncate(t, 32)))
            .bind(crate::search::index_text(m))
            .execute(self.pool())
            .await
            .with_context(|| format!("cannot store item {:?}", w.meta.title))?;
        let row = sqlx::query(self.sql("SELECT id FROM items WHERE path_key = ?"))
            .bind(&w.path_key)
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get(0)?)
    }

    /// Apply `languages` (see [`Meta::choose_title`]) to the items of a
    /// library whose title came from a provider. Returns how many changed.
    pub async fn retitle(&self, library: &str, languages: &[String]) -> anyhow::Result<u64> {
        let rows = sqlx::query(self.sql(
            "SELECT id, title, sort_title, original_title, original_language, provider_title FROM items \
             WHERE library_id = ? AND provider_title IS NOT NULL",
        ))
        .bind(library)
        .fetch_all(self.pool())
        .await?;
        let mut changed = 0;
        for r in &rows {
            let old: String = r.try_get(1)?;
            let sort: Option<String> = r.try_get(2)?;
            let mut m = Meta {
                title: old.clone(),
                original_title: r.try_get(3)?,
                original_language: r.try_get(4)?,
                provider_title: r.try_get(5)?,
                ..Default::default()
            };
            if !m.choose_title(languages) {
                continue;
            }
            // Keep a sort title of its own (from an .nfo); derive one again otherwise.
            let sort = match sort {
                Some(s) if s != crate::media::naming::sort_title(&old) => s,
                _ => crate::media::naming::sort_title(&m.title),
            };
            sqlx::query(self.sql("UPDATE items SET title = ?, sort_title = ?, updated_at = ? WHERE id = ?"))
                .bind(truncate(&m.title, 512))
                .bind(truncate(&sort, 512))
                .bind(super::now_ms())
                .bind(r.try_get::<i64, _>(0)?)
                .execute(self.pool())
                .await?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Titles and people of items, for search results: (original title,
    /// localized title, people) by id.
    pub async fn search_details(
        &self,
        ids: &[i64],
    ) -> anyhow::Result<HashMap<i64, (Option<String>, Option<String>, Vec<Person>)>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT id, original_title, provider_title, people FROM items WHERE id IN ({})",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql));
            for id in chunk {
                q = q.bind(id);
            }
            for r in q.fetch_all(self.pool()).await? {
                out.insert(
                    r.try_get(0)?,
                    (r.try_get(1)?, r.try_get(2)?, json_list::<Person>(r.try_get(3)?)),
                );
            }
        }
        Ok(out)
    }

    /// Fill the search text of items written before it existed; returns how
    /// many were filled.
    pub async fn backfill_search_text(&self) -> anyhow::Result<u64> {
        let mut total = 0;
        loop {
            let rows = sqlx::query(self.sql(
                "SELECT id, title, original_title, provider_title, people FROM items WHERE search_text IS NULL LIMIT 500",
            ))
            .fetch_all(self.pool())
            .await?;
            if rows.is_empty() {
                return Ok(total);
            }
            for r in &rows {
                let meta = Meta {
                    title: r.try_get(1)?,
                    original_title: r.try_get(2)?,
                    provider_title: r.try_get(3)?,
                    people: json_list(r.try_get(4)?),
                    ..Default::default()
                };
                sqlx::query(self.sql("UPDATE items SET search_text = ? WHERE id = ?"))
                    .bind(crate::search::index_text(&meta))
                    .bind(r.try_get::<i64, _>(0)?)
                    .execute(self.pool())
                    .await?;
            }
            total += rows.len() as u64;
        }
    }

    /// Mark unchanged items as seen by scan `generation`.
    pub async fn touch_items(&self, ids: &[i64], generation: i64) -> anyhow::Result<()> {
        for chunk in ids.chunks(IN_CHUNK) {
            let sql = format!(
                "UPDATE items SET seen_scan = ? WHERE id IN ({})",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql)).bind(generation);
            for id in chunk {
                q = q.bind(*id);
            }
            q.execute(self.pool()).await?;
        }
        Ok(())
    }

    pub async fn set_content_added(&self, id: i64, at: i64) -> anyhow::Result<()> {
        sqlx::query(self.sql("UPDATE items SET content_added_at = ? WHERE id = ?"))
            .bind(at)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Delete the items of a library that scan `generation` did not see:
    /// extras and episodes first, then seasons, series and movies.
    pub async fn delete_unseen(&self, library: &str, generation: i64) -> anyhow::Result<u64> {
        let mut total = 0;
        for kind in [
            ItemKind::Extra,
            ItemKind::Episode,
            ItemKind::Season,
            ItemKind::Series,
            ItemKind::Movie,
        ] {
            let r = sqlx::query(self.sql("DELETE FROM items WHERE library_id = ? AND kind = ? AND seen_scan < ?"))
                .bind(library)
                .bind(kind.code())
                .bind(generation)
                .execute(self.pool())
                .await?;
            total += r.rows_affected();
        }
        Ok(total)
    }

    // --------------------------------------------------------------- API

    async fn summaries(
        &self,
        user_id: i64,
        where_sql: &str,
        order_sql: &str,
        args: &[Arg],
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<SummaryRow>> {
        let sql = format!("{SUMMARY_SELECT} WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit} OFFSET {offset}");
        let q = sqlx::query(self.sql_owned(sql)).bind(user_id);
        let q = bind_args!(q, args);
        let rows = q.fetch_all(self.pool()).await?;
        rows.iter()
            .map(SummaryRow::from_row)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    fn libraries_clause(libraries: &[String], args: &mut Vec<Arg>) -> String {
        if libraries.is_empty() {
            return "1 = 0".into();
        }
        args.extend(libraries.iter().cloned().map(Arg::S));
        format!("i.library_id IN ({})", placeholders(libraries.len()))
    }

    /// Movies or series of a library, with the total before paging.
    pub async fn list_items(&self, user_id: i64, q: &ListQuery) -> anyhow::Result<(Vec<SummaryRow>, i64)> {
        let mut clauses = vec!["i.library_id = ?".to_owned(), "i.kind IN (1, 2)".to_owned()];
        // Binds of the WHERE clause, after the user id of the join.
        let mut args = vec![Arg::S(q.library_id.clone())];
        if let Some(g) = &q.genre {
            clauses.push("i.genres LIKE ?".into());
            args.push(Arg::S(format!("%{}%", like_escape(&serde_json::to_string(g)?))));
        }
        if let Some(t) = &q.title {
            search_clauses(t, &mut clauses, &mut args);
        }
        if q.favorite {
            clauses.push("ud.favorite = 1".into());
        }
        if let Some(played) = q.played {
            // A series is played when none of its episodes is unplayed.
            let unplayed_episode = "EXISTS (SELECT 1 FROM items e LEFT JOIN user_data eu ON eu.item_id = e.id \
                 AND eu.user_id = ? WHERE e.series_id = i.id AND e.kind = 4 AND COALESCE(eu.played, 0) = 0)";
            let movie_played = "COALESCE(ud.played, 0) = 1";
            clauses.push(if played {
                format!("((i.kind = 1 AND {movie_played}) OR (i.kind = 2 AND NOT {unplayed_episode}))")
            } else {
                format!("((i.kind = 1 AND NOT ({movie_played})) OR (i.kind = 2 AND {unplayed_episode}))")
            });
            args.push(Arg::I(user_id));
        }
        let where_sql = clauses.join(" AND ");
        let dir = if q.descending { "DESC" } else { "ASC" };
        let order = match q.sort {
            SortBy::Title => format!("i.sort_title {dir}, i.id"),
            SortBy::Year => format!("(i.year IS NULL), i.year {dir}, i.sort_title, i.id"),
            SortBy::Added => format!("i.content_added_at {dir}, i.id {dir}"),
            SortBy::Rating => format!("(i.rating_x100 IS NULL), i.rating_x100 {dir}, i.sort_title, i.id"),
            SortBy::Released => format!("(i.premiere_date IS NULL), i.premiere_date {dir}, i.sort_title, i.id"),
        };
        let items = self
            .summaries(user_id, &where_sql, &order, &args, q.limit, q.offset)
            .await?;
        let count_sql = format!(
            "SELECT COUNT(*) FROM items i LEFT JOIN user_data ud ON ud.item_id = i.id AND ud.user_id = ? WHERE {where_sql}"
        );
        let cq = sqlx::query(self.sql_owned(count_sql)).bind(user_id);
        let cq = bind_args!(cq, &args);
        let total: i64 = cq.fetch_one(self.pool()).await?.try_get(0)?;
        Ok((items, total))
    }

    pub async fn summary(&self, user_id: i64, id: i64) -> anyhow::Result<Option<SummaryRow>> {
        let rows = self.summaries(user_id, "i.id = ?", "i.id", &[Arg::I(id)], 1, 0).await?;
        Ok(rows.into_iter().next())
    }

    /// Seasons of a series or episodes of a season, in order.
    pub async fn children(&self, user_id: i64, parent: i64) -> anyhow::Result<Vec<SummaryRow>> {
        self.summaries(
            user_id,
            "i.parent_id = ?",
            "(i.season_number IS NULL), i.season_number, (i.episode_number IS NULL), i.episode_number, i.sort_title",
            &[Arg::I(parent)],
            10_000,
            0,
        )
        .await
    }

    /// Every episode of a series, in order.
    pub async fn series_episodes(&self, user_id: i64, series: i64) -> anyhow::Result<Vec<SummaryRow>> {
        self.summaries(
            user_id,
            "i.series_id = ? AND i.kind = 4",
            "(i.season_number IS NULL), i.season_number, (i.episode_number IS NULL), i.episode_number, i.sort_title",
            &[Arg::I(series)],
            100_000,
            0,
        )
        .await
    }

    pub async fn search(
        &self,
        user_id: i64,
        libraries: &[String],
        text: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SummaryRow>> {
        let mut args = Vec::new();
        let mut clauses = vec![
            Self::libraries_clause(libraries, &mut args),
            "i.kind IN (1, 2, 4)".to_owned(),
        ];
        search_clauses(text, &mut clauses, &mut args);
        self.summaries(
            user_id,
            &clauses.join(" AND "),
            "i.kind, i.sort_title, i.id",
            &args,
            limit,
            0,
        )
        .await
    }

    pub async fn continue_watching(
        &self,
        user_id: i64,
        libraries: &[String],
        limit: i64,
    ) -> anyhow::Result<Vec<SummaryRow>> {
        let mut args = Vec::new();
        let libs = Self::libraries_clause(libraries, &mut args);
        self.summaries(
            user_id,
            &format!("{libs} AND i.kind IN (1, 4) AND ud.position_ms > 0 AND ud.played = 0"),
            "ud.last_played_at DESC, i.id",
            &args,
            limit,
            0,
        )
        .await
    }

    pub async fn latest(&self, user_id: i64, library: &str, limit: i64) -> anyhow::Result<Vec<SummaryRow>> {
        self.summaries(
            user_id,
            "i.library_id = ? AND i.kind IN (1, 2)",
            "i.content_added_at DESC, i.id DESC",
            &[Arg::S(library.to_owned())],
            limit,
            0,
        )
        .await
    }

    /// Series with an episode played since `since`, most recent first.
    pub async fn recently_watched_series(
        &self,
        user_id: i64,
        libraries: &[String],
        since: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<i64>> {
        let mut args = Vec::new();
        let libs = Self::libraries_clause(libraries, &mut args);
        let sql = format!(
            "SELECT i.series_id, MAX(ud.last_played_at) AS t FROM user_data ud JOIN items i ON i.id = ud.item_id \
             WHERE ud.user_id = ? AND i.kind = 4 AND ud.played = 1 AND ud.last_played_at > ? AND {libs} \
             GROUP BY i.series_id ORDER BY t DESC LIMIT {limit}"
        );
        let q = sqlx::query(self.sql_owned(sql)).bind(user_id).bind(since);
        let q = bind_args!(q, &args);
        let rows = q.fetch_all(self.pool()).await?;
        rows.iter()
            .map(|r| r.try_get::<Option<i64>, _>(0))
            .filter_map(Result::transpose)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Distinct genres of the movies and series of a library.
    pub async fn genres(&self, library: &str) -> anyhow::Result<Vec<String>> {
        let rows =
            sqlx::query(self.sql(
                "SELECT DISTINCT genres FROM items WHERE library_id = ? AND kind IN (1, 2) AND genres IS NOT NULL",
            ))
            .bind(library)
            .fetch_all(self.pool())
            .await?;
        let mut set: HashSet<String> = HashSet::new();
        for r in rows {
            set.extend(json_list::<String>(r.try_get(0)?));
        }
        let mut out: Vec<String> = set.into_iter().collect();
        out.sort_by_key(|g| g.to_lowercase());
        Ok(out)
    }

    pub async fn item_counts(&self) -> anyhow::Result<HashMap<String, i64>> {
        let rows =
            sqlx::query(self.sql("SELECT library_id, COUNT(*) FROM items WHERE kind IN (1, 2) GROUP BY library_id"))
                .fetch_all(self.pool())
                .await?;
        rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?))).collect()
    }

    /// Children per parent (seasons of series, episodes of seasons).
    pub async fn child_counts(&self, parents: &[i64]) -> anyhow::Result<HashMap<i64, i64>> {
        let mut out = HashMap::new();
        for chunk in parents.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT parent_id, COUNT(*) FROM items WHERE parent_id IN ({}) GROUP BY parent_id",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql));
            for id in chunk {
                q = q.bind(*id);
            }
            for r in q.fetch_all(self.pool()).await? {
                out.insert(r.try_get(0)?, r.try_get(1)?);
            }
        }
        Ok(out)
    }

    /// Unplayed episodes per series (`by_series`) or per season.
    pub async fn unplayed_counts(
        &self,
        user_id: i64,
        ids: &[i64],
        by_series: bool,
    ) -> anyhow::Result<HashMap<i64, i64>> {
        let column = if by_series { "e.series_id" } else { "e.parent_id" };
        let mut out = HashMap::new();
        for chunk in ids.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT {column}, COUNT(*) FROM items e LEFT JOIN user_data u ON u.item_id = e.id AND u.user_id = ? \
                 WHERE e.kind = 4 AND {column} IN ({}) AND COALESCE(u.played, 0) = 0 GROUP BY {column}",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql)).bind(user_id);
            for id in chunk {
                q = q.bind(*id);
            }
            for r in q.fetch_all(self.pool()).await? {
                out.insert(r.try_get(0)?, r.try_get(1)?);
            }
        }
        Ok(out)
    }

    /// Episodes under a series or season (for marking them played).
    pub async fn episode_ids_under(&self, id: i64) -> anyhow::Result<Vec<i64>> {
        let rows = sqlx::query(self.sql("SELECT id FROM items WHERE kind = 4 AND (series_id = ? OR parent_id = ?)"))
            .bind(id)
            .bind(id)
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|r| r.try_get(0))
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Direct children rows (for metadata refresh of a series).
    pub async fn child_rows(&self, parent: i64) -> anyhow::Result<Vec<ItemRow>> {
        let rows = sqlx::query(self.sql_owned(format!("SELECT {ITEM_COLUMNS} FROM items WHERE parent_id = ?")))
            .bind(parent)
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(ItemRow::from_row)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Clear the remote lookup state so that the next scan fetches again.
    pub async fn reset_remote(&self, ids: &[i64]) -> anyhow::Result<()> {
        for chunk in ids.chunks(IN_CHUNK) {
            let sql = format!(
                "UPDATE items SET remote_fetched_at = NULL, remote_matched = 0, local_key = NULL WHERE id IN ({})",
                placeholders(chunk.len())
            );
            let mut q = sqlx::query(self.sql_owned(sql));
            for id in chunk {
                q = q.bind(*id);
            }
            q.execute(self.pool()).await?;
        }
        Ok(())
    }

    // ----------------------------------------------------------- user data

    pub async fn save_position(&self, user_id: i64, item_id: i64, position_ms: i64, now: i64) -> anyhow::Result<()> {
        let sql = format!(
            "INSERT INTO user_data (user_id, item_id, position_ms, played, play_count, favorite, last_played_at, updated_at) \
             VALUES (?, ?, ?, 0, 0, 0, ?, ?){}",
            self.upsert_tail("user_id, item_id", &["position_ms", "last_played_at", "updated_at"])
        );
        sqlx::query(self.sql_owned(sql))
            .bind(user_id)
            .bind(item_id)
            .bind(position_ms)
            .bind(now)
            .bind(now)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Mark items played (position reset, play count incremented) or unplayed.
    pub async fn set_played(&self, user_id: i64, items: &[i64], played: bool, now: i64) -> anyhow::Result<()> {
        let sql = match (played, self.dialect()) {
            (true, Dialect::Postgres) => {
                "INSERT INTO user_data (user_id, item_id, position_ms, played, play_count, favorite, last_played_at, updated_at) \
                 VALUES (?, ?, 0, 1, 1, 0, ?, ?) ON CONFLICT (user_id, item_id) DO UPDATE SET position_ms = 0, played = 1, \
                 play_count = user_data.play_count + 1, last_played_at = EXCLUDED.last_played_at, updated_at = EXCLUDED.updated_at"
            }
            (true, Dialect::MySql) => {
                "INSERT INTO user_data (user_id, item_id, position_ms, played, play_count, favorite, last_played_at, updated_at) \
                 VALUES (?, ?, 0, 1, 1, 0, ?, ?) ON DUPLICATE KEY UPDATE position_ms = 0, played = 1, \
                 play_count = play_count + 1, last_played_at = VALUES(last_played_at), updated_at = VALUES(updated_at)"
            }
            (false, _) => {
                "UPDATE user_data SET played = 0, position_ms = 0, updated_at = ? WHERE user_id = ? AND item_id = ?"
            }
        };
        let mut tx = self.pool().begin().await?;
        for item in items {
            let q = sqlx::query(self.sql(sql));
            let q = if played {
                q.bind(user_id).bind(*item).bind(now).bind(now)
            } else {
                q.bind(now).bind(user_id).bind(*item)
            };
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_favorite(&self, user_id: i64, item_id: i64, favorite: bool, now: i64) -> anyhow::Result<()> {
        let sql = format!(
            "INSERT INTO user_data (user_id, item_id, position_ms, played, play_count, favorite, last_played_at, updated_at) \
             VALUES (?, ?, 0, 0, 0, ?, NULL, ?){}",
            self.upsert_tail("user_id, item_id", &["favorite", "updated_at"])
        );
        sqlx::query(self.sql_owned(sql))
            .bind(user_id)
            .bind(item_id)
            .bind(bool_i16(favorite))
            .bind(now)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns_are_escaped() {
        assert_eq!(like_escape("100%_a\\b"), "100\\%\\_a\\\\b");
    }

    #[test]
    fn truncation_respects_characters() {
        assert_eq!(truncate("héllo", 2), "hé");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
