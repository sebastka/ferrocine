//! The store against PostgreSQL and MariaDB.

mod common;

use common::{database_urls, store, unique};
use ferrocine::config::{LibraryConfig, LibraryKind};
use ferrocine::media::{ItemKind, Meta};
use ferrocine::store::{
    Identity, ImageKind, ImageSource, ItemWrite, ListQuery, LoginRequest, NewSession, SortBy, Store, now_ms, sha256_hex,
};

fn library(id: &str, kind: LibraryKind) -> LibraryConfig {
    LibraryConfig {
        id: id.into(),
        name: id.into(),
        kind,
        paths: vec!["/media".into()],
        access: vec![],
        language: None,
        anime: false,
        episode_order: Default::default(),
        prefer_original_title: None,
    }
}

fn write(lib: &str, kind: ItemKind, key: &str, title: &str, year: Option<i32>) -> ItemWrite {
    ItemWrite {
        library_id: lib.into(),
        kind,
        parent_id: None,
        series_id: None,
        path_key: sha256_hex(format!("{lib}{key}").as_bytes()),
        path: Some(format!("/media/{key}")),
        meta: Meta {
            title: title.into(),
            year,
            genres: vec!["Drama".into()],
            runtime_ms: Some(3_600_000),
            ..Default::default()
        },
        season_number: None,
        episode_number: None,
        file_size: Some(1),
        file_mtime: Some(1),
        media: None,
        local_key: None,
        remote_fetched_at: None,
        remote_matched: false,
        added_at: now_ms(),
        seen_scan: 1,
        extra_type: None,
    }
}

async fn user(store: &Store, name: &str) -> i64 {
    store
        .upsert_user(
            &Identity {
                issuer: "https://idp.test".into(),
                subject: unique(name),
                username: name.into(),
                display_name: None,
                email: None,
                admin: false,
                groups: vec!["family".into()],
            },
            now_ms(),
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn items_lists_and_user_data() {
    for (name, url) in database_urls() {
        let store = store(&url).await;
        let lib = unique("movies");
        store
            .sync_libraries(&[library(&lib, LibraryKind::Movies)])
            .await
            .unwrap();
        let mut ids = Vec::new();
        for (key, title, year) in [
            ("a", "The Zebra", Some(2001)),
            ("b", "Apple 100%", Some(1999)),
            ("c", "Mango", None),
        ] {
            ids.push(
                store
                    .upsert_item(&write(&lib, ItemKind::Movie, key, title, year))
                    .await
                    .unwrap(),
            );
        }
        // Upserting again keeps the id.
        let again = store
            .upsert_item(&write(&lib, ItemKind::Movie, "a", "The Zebra (fixed)", Some(2001)))
            .await
            .unwrap();
        assert_eq!(again, ids[0], "{name}");
        let u = user(&store, "alice").await;
        let q = |sort, descending| ListQuery {
            library_id: lib.clone(),
            sort,
            descending,
            limit: 10,
            ..Default::default()
        };
        let (rows, total) = store.list_items(u, &q(SortBy::Title, false)).await.unwrap();
        assert_eq!(total, 3, "{name}");
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        // "The" is ignored when sorting.
        assert_eq!(titles, ["Apple 100%", "Mango", "The Zebra (fixed)"], "{name}");
        let (rows, _) = store.list_items(u, &q(SortBy::Year, true)).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.year).collect::<Vec<_>>(),
            [Some(2001), Some(1999), None],
            "{name}"
        );
        // LIKE metacharacters are literal.
        let mut lq = q(SortBy::Title, false);
        lq.title = Some("100%".into());
        assert_eq!(store.list_items(u, &lq).await.unwrap().1, 1, "{name}");
        lq.title = Some("%".into());
        assert_eq!(store.list_items(u, &lq).await.unwrap().1, 1, "{name}");
        let mut gq = q(SortBy::Title, false);
        gq.genre = Some("Drama".into());
        assert_eq!(store.list_items(u, &gq).await.unwrap().1, 3, "{name}");
        assert_eq!(store.genres(&lib).await.unwrap(), ["Drama"], "{name}");

        // Positions, played state and favorites.
        let now = now_ms();
        store.save_position(u, ids[1], 60_000, now).await.unwrap();
        let cw = store
            .continue_watching(u, std::slice::from_ref(&lib), 10)
            .await
            .unwrap();
        assert_eq!(cw.len(), 1, "{name}");
        assert_eq!(cw[0].user.position_ms, 60_000, "{name}");
        store.set_played(u, &[ids[1]], true, now).await.unwrap();
        store.set_played(u, &[ids[1]], true, now).await.unwrap();
        let s = store.summary(u, ids[1]).await.unwrap().unwrap();
        assert!(
            s.user.played && s.user.play_count == 2 && s.user.position_ms == 0,
            "{name}: {:?}",
            s.user
        );
        assert!(
            store
                .continue_watching(u, std::slice::from_ref(&lib), 10)
                .await
                .unwrap()
                .is_empty()
        );
        let mut pq = q(SortBy::Title, false);
        pq.played = Some(false);
        assert_eq!(store.list_items(u, &pq).await.unwrap().1, 2, "{name}");
        store.set_favorite(u, ids[2], true, now).await.unwrap();
        let mut fq = q(SortBy::Title, false);
        fq.favorite = true;
        let (rows, _) = store.list_items(u, &fq).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[2]], "{name}");
        // Another user sees nothing of it.
        let other = user(&store, "bob").await;
        assert!(
            !store.summary(other, ids[1]).await.unwrap().unwrap().user.played,
            "{name}"
        );

        // Images.
        let tag = store
            .put_image(ids[0], ImageKind::Poster, ImageSource::Local, "k", 2, 3, &[1, 2, 3])
            .await
            .unwrap();
        let img = store.image(ids[0], ImageKind::Poster).await.unwrap().unwrap();
        assert_eq!((img.tag, img.data), (tag.clone(), vec![1, 2, 3]), "{name}");
        assert_eq!(store.image_tags(&ids).await.unwrap().len(), 1, "{name}");

        // A scan that sees only one item deletes the others.
        store.touch_items(&[ids[0]], 2).await.unwrap();
        assert_eq!(store.delete_unseen(&lib, 2).await.unwrap(), 2, "{name}");
        assert_eq!(store.library_items(&lib).await.unwrap().len(), 1, "{name}");
    }
}

#[tokio::test]
async fn series_structure_and_counts() {
    for (name, url) in database_urls() {
        let store = store(&url).await;
        let lib = unique("shows");
        store
            .sync_libraries(&[library(&lib, LibraryKind::Shows)])
            .await
            .unwrap();
        let series = store
            .upsert_item(&write(&lib, ItemKind::Series, "s", "Show", Some(2008)))
            .await
            .unwrap();
        let mut season = write(&lib, ItemKind::Season, "s1", "Season 1", None);
        season.parent_id = Some(series);
        season.series_id = Some(series);
        season.season_number = Some(1);
        let season = store.upsert_item(&season).await.unwrap();
        let mut eps = Vec::new();
        for n in 1..=3 {
            let mut e = write(&lib, ItemKind::Episode, &format!("e{n}"), &format!("Ep {n}"), None);
            e.parent_id = Some(season);
            e.series_id = Some(series);
            e.season_number = Some(1);
            e.episode_number = Some(n);
            eps.push(store.upsert_item(&e).await.unwrap());
        }
        let u = user(&store, "carol").await;
        store.set_played(u, &eps[..1], true, now_ms()).await.unwrap();
        assert_eq!(
            store.child_counts(&[series, season]).await.unwrap()[&season],
            3,
            "{name}"
        );
        assert_eq!(
            store.unplayed_counts(u, &[series], true).await.unwrap()[&series],
            2,
            "{name}"
        );
        assert_eq!(
            store.unplayed_counts(u, &[season], false).await.unwrap()[&season],
            2,
            "{name}"
        );
        let ordered = store.children(u, season).await.unwrap();
        assert_eq!(
            ordered.iter().map(|e| e.episode_number).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
        assert_eq!(ordered[0].series_title.as_deref(), Some("Show"), "{name}");
        let recent = store
            .recently_watched_series(u, std::slice::from_ref(&lib), 0, 10)
            .await
            .unwrap();
        assert_eq!(recent, [series], "{name}");
        // Deleting the series cascades to its seasons and episodes.
        store.touch_items(&[], 5).await.unwrap();
        assert_eq!(store.delete_unseen(&lib, 5).await.unwrap(), 5, "{name}");
    }
}

#[tokio::test]
async fn sessions_logins_and_locks() {
    for (name, url) in database_urls() {
        let store = store(&url).await;
        let u = user(&store, "dave").await;
        let hash = sha256_hex(unique("session").as_bytes());
        let now = now_ms();
        store
            .create_session(
                &NewSession {
                    id_hash: &hash,
                    user_id: u,
                    expires_at: now + 60_000,
                    id_token: Some("token"),
                    user_agent: Some("test"),
                },
                now,
            )
            .await
            .unwrap();
        let s = store.session(&hash).await.unwrap().unwrap();
        assert_eq!(
            (s.user.id, s.user.groups.clone()),
            (u, vec!["family".to_owned()]),
            "{name}"
        );
        assert_eq!(
            store.delete_session(&hash).await.unwrap().as_deref(),
            Some("token"),
            "{name}"
        );
        assert!(store.session(&hash).await.unwrap().is_none(), "{name}");

        // Sign-ins are single use.
        let state = sha256_hex(unique("state").as_bytes());
        let login = LoginRequest {
            verifier: "v".into(),
            nonce: "n".into(),
            return_to: "/x".into(),
            created_at: now,
        };
        store.create_login(&state, &login).await.unwrap();
        assert_eq!(store.take_login(&state).await.unwrap(), Some(login), "{name}");
        assert_eq!(store.take_login(&state).await.unwrap(), None, "{name}");

        // Advisory locks exclude each other until released.
        let lock_name = unique("lock");
        let a = store.try_lock(&lock_name).await.unwrap().expect("first lock");
        assert!(store.try_lock(&lock_name).await.unwrap().is_none(), "{name}");
        a.release().await;
        let b = store.try_lock(&lock_name).await.unwrap();
        assert!(b.is_some(), "{name}");
    }
}
