//! Libraries, items, artwork, user data, home and search.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::{AppState, error, internal, not_found};
use crate::media::probe::MediaInfo;
use crate::media::{ItemKind, Person};
use crate::store::{ImageKind, ItemRow, ListQuery, SortBy, SummaryRow, User, UserData, now_ms};

#[derive(Serialize, Default)]
pub struct Images {
    poster: Option<String>,
    backdrop: Option<String>,
    thumb: Option<String>,
    logo: Option<String>,
}

#[derive(Serialize)]
pub struct ItemSummary {
    id: i64,
    library_id: String,
    kind: ItemKind,
    title: String,
    year: Option<i32>,
    parent_id: Option<i64>,
    series_id: Option<i64>,
    series_title: Option<String>,
    season_number: Option<i32>,
    episode_number: Option<i32>,
    runtime_ms: Option<i64>,
    rating: Option<f64>,
    official_rating: Option<String>,
    genres: Vec<String>,
    premiere_date: Option<String>,
    added_at: i64,
    images: Images,
    user: UserData,
    child_count: Option<i64>,
    unplayed_count: Option<i64>,
    /// Of an extra: `trailer`, `featurette`, `behind_the_scenes`,
    /// `deleted_scene`, `interview`, `scene`, `short` or `extra`.
    extra_type: Option<String>,
}

#[derive(Serialize)]
pub struct ProviderIds {
    tmdb: Option<String>,
    imdb: Option<String>,
    tvdb: Option<String>,
    anidb: Option<String>,
    anilist: Option<String>,
}

#[derive(Serialize)]
pub struct MediaOut {
    file_name: String,
    size: Option<i64>,
    #[serde(flatten)]
    info: MediaInfo,
}

#[derive(Serialize)]
pub struct ItemDetail {
    #[serde(flatten)]
    summary: ItemSummary,
    overview: Option<String>,
    tagline: Option<String>,
    original_title: Option<String>,
    /// ISO 639-1.
    original_language: Option<String>,
    /// The provider's title in the library's language (differs from `title`
    /// when the original title is preferred).
    localized_title: Option<String>,
    studios: Vec<String>,
    people: Vec<Person>,
    provider_ids: ProviderIds,
    /// Ratings by source.
    ratings: Vec<crate::media::Rating>,
    media: Option<MediaOut>,
}

fn image_url(id: i64, kind: ImageKind, tag: &str) -> String {
    format!("/api/v1/items/{id}/images/{}?v={tag}", kind.as_str())
}

/// Attach images (with fallbacks to the season and series) and counts.
pub async fn summarize(state: &AppState, user: &User, rows: Vec<SummaryRow>) -> anyhow::Result<Vec<ItemSummary>> {
    let mut ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    ids.extend(rows.iter().filter_map(|r| r.parent_id));
    ids.extend(rows.iter().filter_map(|r| r.series_id));
    ids.sort_unstable();
    ids.dedup();
    let tags = state.store.image_tags(&ids).await?;
    let containers: Vec<i64> = rows
        .iter()
        .filter(|r| matches!(r.kind, ItemKind::Series | ItemKind::Season))
        .map(|r| r.id)
        .collect();
    let (children, unplayed_series, unplayed_seasons) = if containers.is_empty() {
        (HashMap::new(), HashMap::new(), HashMap::new())
    } else {
        let series: Vec<i64> = rows
            .iter()
            .filter(|r| r.kind == ItemKind::Series)
            .map(|r| r.id)
            .collect();
        let seasons: Vec<i64> = rows
            .iter()
            .filter(|r| r.kind == ItemKind::Season)
            .map(|r| r.id)
            .collect();
        (
            state.store.child_counts(&containers).await?,
            state.store.unplayed_counts(user.id, &series, true).await?,
            state.store.unplayed_counts(user.id, &seasons, false).await?,
        )
    };
    let find = |chain: &[Option<i64>], kind: ImageKind| {
        chain
            .iter()
            .flatten()
            .find_map(|id| tags.get(&(*id, kind)).map(|t| image_url(*id, kind, t)))
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            let chain = [Some(r.id), r.parent_id, r.series_id];
            let (child_count, unplayed_count) = match r.kind {
                ItemKind::Series => (
                    Some(children.get(&r.id).copied().unwrap_or(0)),
                    Some(unplayed_series.get(&r.id).copied().unwrap_or(0)),
                ),
                ItemKind::Season => (
                    Some(children.get(&r.id).copied().unwrap_or(0)),
                    Some(unplayed_seasons.get(&r.id).copied().unwrap_or(0)),
                ),
                _ => (None, None),
            };
            let mut user_data = r.user.clone();
            if let Some(n) = unplayed_count {
                // A series or season is played when no episode is left.
                user_data.played = n == 0 && child_count.unwrap_or(0) > 0;
            }
            ItemSummary {
                images: Images {
                    poster: find(&chain, ImageKind::Poster),
                    backdrop: find(&chain, ImageKind::Backdrop),
                    thumb: find(&chain[..1], ImageKind::Thumb),
                    logo: find(&chain, ImageKind::Logo),
                },
                id: r.id,
                library_id: r.library_id,
                kind: r.kind,
                title: r.title,
                year: r.year,
                parent_id: r.parent_id,
                series_id: r.series_id,
                series_title: r.series_title,
                season_number: r.season_number,
                episode_number: r.episode_number,
                runtime_ms: r.runtime_ms,
                rating: r.rating,
                official_rating: r.official_rating,
                genres: r.genres,
                premiere_date: r.premiere_date,
                added_at: r.added_at,
                user: user_data,
                child_count,
                unplayed_count,
                extra_type: r.extra_type,
            }
        })
        .collect())
}

/// An item the user may see.
pub async fn visible_item(state: &AppState, user: &User, id: i64) -> Result<ItemRow, Response> {
    match state.store.item_row(id).await {
        Ok(Some(row)) if state.can_see(user, &row.library_id) => Ok(row),
        Ok(_) => Err(not_found()),
        Err(e) => Err(internal(&e)),
    }
}

#[derive(Serialize)]
pub struct Me {
    #[serde(flatten)]
    user: User,
    /// Values of the groups claim at sign-in.
    groups: Vec<String>,
    /// URL of the user's picture, or `null` (the UI shows initials).
    avatar: Option<String>,
}

pub async fn me(Extension(user): Extension<User>) -> Json<Me> {
    Json(Me {
        groups: user.groups.clone(),
        avatar: user.avatar_tag.as_ref().map(|t| format!("/api/v1/me/avatar?v={t}")),
        user,
    })
}

pub async fn avatar(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
) -> Response {
    match state.store.avatar(user.id).await {
        Ok(Some(a)) => jpeg(a.tag, a.data, &headers),
        Ok(None) => not_found(),
        Err(e) => internal(&e),
    }
}

/// A JPEG with a long private cache (its URL is versioned) and an ETag.
fn jpeg(tag: String, data: Vec<u8>, headers: &HeaderMap) -> Response {
    let etag = format!("\"{tag}\"");
    let cache = HeaderValue::from_static("private, max-age=31536000, immutable");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::CACHE_CONTROL, cache)]).into_response();
    }
    // Logos are PNG (transparency), everything else JPEG.
    let png = data.starts_with(&[0x89, b'P', b'N', b'G']);
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(if png { "image/png" } else { "image/jpeg" }),
            ),
            (header::CACHE_CONTROL, cache),
            (
                header::ETAG,
                HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("")),
            ),
        ],
        data,
    )
        .into_response()
}

#[derive(Serialize)]
struct LibraryOut {
    id: String,
    name: String,
    kind: &'static str,
    item_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<ScanOut>,
}

#[derive(Serialize)]
pub struct ScanOut {
    pub running: bool,
    #[serde(flatten)]
    pub state: crate::store::ScanState,
}

pub async fn libraries(State(state): State<Arc<AppState>>, Extension(user): Extension<User>) -> Response {
    let counts = match state.store.item_counts().await {
        Ok(c) => c,
        Err(e) => return internal(&e),
    };
    let scans: HashMap<String, crate::store::ScanState> = if user.admin {
        match state.store.scan_states().await {
            Ok(s) => s.into_iter().collect(),
            Err(e) => return internal(&e),
        }
    } else {
        HashMap::new()
    };
    let out: Vec<LibraryOut> = state
        .visible(&user)
        .into_iter()
        .map(|l| LibraryOut {
            id: l.id.clone(),
            name: l.name.clone(),
            kind: l.kind.as_str(),
            item_count: counts.get(&l.id).copied().unwrap_or(0),
            scan: user.admin.then(|| ScanOut {
                running: state.scanner.is_running(&l.id),
                state: scans.get(&l.id).cloned().unwrap_or_default(),
            }),
        })
        .collect();
    Json(out).into_response()
}

#[derive(Deserialize)]
pub struct ListParams {
    sort: Option<String>,
    order: Option<String>,
    genre: Option<String>,
    played: Option<bool>,
    favorite: Option<bool>,
    q: Option<String>,
    offset: Option<i64>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct Page {
    items: Vec<ItemSummary>,
    total: i64,
}

pub async fn library_items(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<String>,
    Query(p): Query<ListParams>,
) -> Response {
    if !state.can_see(&user, &id) {
        return not_found();
    }
    let sort = match p.sort.as_deref() {
        None | Some("title") => SortBy::Title,
        Some("year") => SortBy::Year,
        Some("added") => SortBy::Added,
        Some("rating") => SortBy::Rating,
        Some("released") => SortBy::Released,
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "sort: expected title, year, added, rating or released",
            );
        }
    };
    let descending = match p.order.as_deref() {
        Some("asc") => false,
        Some("desc") => true,
        None => sort != SortBy::Title,
        Some(_) => return error(StatusCode::BAD_REQUEST, "order: expected asc or desc"),
    };
    let q = ListQuery {
        library_id: id,
        sort,
        descending,
        genre: p.genre.filter(|g| !g.is_empty()),
        played: p.played,
        favorite: p.favorite.unwrap_or(false),
        title: p.q.map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()),
        offset: p.offset.unwrap_or(0).max(0),
        limit: p.limit.unwrap_or(100).clamp(1, 500),
    };
    let (rows, total) = match state.store.list_items(user.id, &q).await {
        Ok(x) => x,
        Err(e) => return internal(&e),
    };
    match summarize(&state, &user, rows).await {
        Ok(items) => Json(Page { items, total }).into_response(),
        Err(e) => internal(&e),
    }
}

pub async fn genres(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<String>,
) -> Response {
    if !state.can_see(&user, &id) {
        return not_found();
    }
    match state.store.genres(&id).await {
        Ok(g) => Json(g).into_response(),
        Err(e) => internal(&e),
    }
}

pub async fn item(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let summary = match state.store.summary(user.id, id).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return internal(&e),
    };
    let summary = match summarize(&state, &user, vec![summary]).await {
        Ok(mut v) => v.remove(0),
        Err(e) => return internal(&e),
    };
    let stored = match state.store.subtitles_of(id).await {
        Ok(s) => s,
        Err(e) => return internal(&e),
    };
    let media = row.media.map(|mut info| {
        // Server paths stay on the server.
        for s in &mut info.subtitles {
            s.path = None;
        }
        info.subtitles
            .extend(stored.iter().map(crate::api::subtitles::stream_of));
        MediaOut {
            // A disc's file is one of many: the movie's folder (or image) names it.
            file_name: row
                .path
                .as_deref()
                .map(std::path::Path::new)
                .and_then(|p| {
                    crate::media::disc::display_name(p)
                        .or_else(|| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                })
                .unwrap_or_default(),
            size: row.file_size,
            info,
        }
    });
    let m = row.meta;
    Json(ItemDetail {
        summary,
        overview: m.overview,
        tagline: m.tagline,
        original_title: m.original_title,
        original_language: m.original_language,
        localized_title: m.provider_title,
        studios: m.studios,
        people: m.people,
        provider_ids: ProviderIds {
            tmdb: m.tmdb_id,
            imdb: m.imdb_id,
            tvdb: m.tvdb_id,
            anidb: m.anidb_id,
            anilist: m.anilist_id,
        },
        ratings: m.ratings,
        media,
    })
    .into_response()
}

pub async fn children(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(resp) = visible_item(&state, &user, id).await {
        return resp;
    }
    let rows = match state.store.children(user.id, id).await {
        Ok(r) => r,
        Err(e) => return internal(&e),
    };
    match summarize(&state, &user, rows).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => internal(&e),
    }
}

pub async fn image(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path((id, kind)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(kind) = ImageKind::parse(&kind) else {
        return not_found();
    };
    if let Err(resp) = visible_item(&state, &user, id).await {
        return resp;
    }
    let img = match state.store.image(id, kind).await {
        Ok(Some(i)) => i,
        Ok(None) => return not_found(),
        Err(e) => return internal(&e),
    };
    jpeg(img.tag, img.data, &headers)
}

#[derive(Deserialize)]
pub struct Progress {
    position_ms: i64,
}

pub async fn progress(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
    Json(p): Json<Progress>,
) -> Response {
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if !row.kind.playable() {
        return error(StatusCode::BAD_REQUEST, "only movies and episodes have a position");
    }
    let runtime = row
        .media
        .as_ref()
        .and_then(|m| m.duration_ms)
        .or(row.meta.runtime_ms)
        .unwrap_or(0);
    let position = p.position_ms.max(0);
    let now = now_ms();
    let result = if runtime > 0 && position * 10 >= runtime * 9 {
        state.store.set_played(user.id, &[id], true, now).await
    } else if runtime > 0 && position * 20 < runtime {
        // Barely started: no resume point.
        state.store.save_position(user.id, id, 0, now).await
    } else {
        state.store.save_position(user.id, id, position, now).await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(&e),
    }
}

async fn set_played(state: &AppState, user: &User, id: i64, played: bool) -> Response {
    let row = match visible_item(state, user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let ids = if row.kind.playable() {
        Ok(vec![id])
    } else {
        state.store.episode_ids_under(id).await
    };
    let result = match ids {
        Ok(ids) => state.store.set_played(user.id, &ids, played, now_ms()).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(&e),
    }
}

pub async fn played(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    set_played(&state, &user, id, true).await
}

pub async fn unplayed(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    set_played(&state, &user, id, false).await
}

async fn set_favorite(state: &AppState, user: &User, id: i64, favorite: bool) -> Response {
    if let Err(resp) = visible_item(state, user, id).await {
        return resp;
    }
    match state.store.set_favorite(user.id, id, favorite, now_ms()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(&e),
    }
}

pub async fn favorite(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    set_favorite(&state, &user, id, true).await
}

pub async fn unfavorite(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    set_favorite(&state, &user, id, false).await
}

#[derive(Serialize)]
struct LibraryRef {
    id: String,
    name: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct Latest {
    library: LibraryRef,
    items: Vec<ItemSummary>,
}

#[derive(Serialize)]
struct Home {
    continue_watching: Vec<ItemSummary>,
    next_up: Vec<ItemSummary>,
    latest: Vec<Latest>,
}

/// The next episode to watch in a series: the first unplayed one after the
/// last played one, specials aside.
pub fn next_episode(episodes: &[SummaryRow]) -> Option<&SummaryRow> {
    let regular: Vec<&SummaryRow> = episodes.iter().filter(|e| e.season_number != Some(0)).collect();
    let last = regular.iter().rposition(|e| e.user.played)?;
    regular[last + 1..].iter().find(|e| !e.user.played).copied()
}

const NEXT_UP_WINDOW_MS: i64 = 90 * 86_400_000;

pub async fn home(State(state): State<Arc<AppState>>, Extension(user): Extension<User>) -> Response {
    match build_home(&state, &user).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => internal(&e),
    }
}

async fn build_home(state: &AppState, user: &User) -> anyhow::Result<Home> {
    let libs: Vec<String> = state.visible(user).iter().map(|l| l.id.clone()).collect();
    let cw = state.store.continue_watching(user.id, &libs, 20).await?;
    let series = state
        .store
        .recently_watched_series(user.id, &libs, now_ms() - NEXT_UP_WINDOW_MS, 20)
        .await?;
    let mut next = Vec::new();
    for s in series {
        let episodes = state.store.series_episodes(user.id, s).await?;
        if let Some(e) = next_episode(&episodes) {
            next.push(e.clone());
        }
    }
    let mut latest = Vec::new();
    for l in state.visible(user) {
        let rows = state.store.latest(user.id, &l.id, 20).await?;
        if rows.is_empty() {
            continue;
        }
        latest.push(Latest {
            library: LibraryRef {
                id: l.id.clone(),
                name: l.name.clone(),
                kind: l.kind.as_str(),
            },
            items: summarize(state, user, rows).await?,
        });
    }
    Ok(Home {
        continue_watching: summarize(state, user, cw).await?,
        next_up: summarize(state, user, next).await?,
        latest,
    })
}

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    limit: Option<i64>,
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Query(p): Query<SearchParams>,
) -> Response {
    let q = p.q.trim();
    if q.chars().count() < 2 {
        return error(StatusCode::BAD_REQUEST, "q: at least 2 characters");
    }
    let libs: Vec<String> = state.visible(&user).iter().map(|l| l.id.clone()).collect();
    let rows = match state
        .store
        .search(user.id, &libs, q, p.limit.unwrap_or(50).clamp(1, 200))
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(&e),
    };
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let (items, details) = match tokio::try_join!(summarize(&state, &user, rows), state.store.search_details(&ids)) {
        Ok(x) => x,
        Err(e) => return internal(&e),
    };
    let terms = crate::search::terms(q);
    let hits: Vec<SearchHit> = items
        .into_iter()
        .map(|item| {
            let matched = details.get(&item.id).and_then(|(original, localized, people)| {
                crate::search::why(
                    &item.title,
                    &[original.as_deref(), localized.as_deref()],
                    people,
                    &terms,
                )
            });
            SearchHit { item, matched }
        })
        .collect();
    Json(hits).into_response()
}

/// A search result, and why it matched when not by its title.
#[derive(Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    item: ItemSummary,
    matched: Option<crate::search::Matched>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(season: i32, n: i32, played: bool) -> SummaryRow {
        SummaryRow {
            id: i64::from(season * 100 + n),
            library_id: "tv".into(),
            kind: ItemKind::Episode,
            title: format!("{season}x{n}"),
            year: None,
            parent_id: None,
            series_id: Some(1),
            series_title: None,
            season_number: Some(season),
            episode_number: Some(n),
            runtime_ms: None,
            rating: None,
            official_rating: None,
            genres: vec![],
            premiere_date: None,
            added_at: 0,
            user: UserData {
                played,
                ..Default::default()
            },
            extra_type: None,
        }
    }

    #[test]
    fn next_up_follows_the_last_played_episode() {
        let eps = [
            ep(0, 1, false),
            ep(1, 1, true),
            ep(1, 2, false),
            ep(1, 3, true),
            ep(2, 1, false),
        ];
        assert_eq!(next_episode(&eps).unwrap().id, 201);
        assert!(next_episode(&[ep(1, 1, false)]).is_none());
        assert!(next_episode(&[ep(1, 1, true)]).is_none());
    }
}
