//! The whole server: sign-in through a mock OpenID Connect provider, scans of
//! generated media, the API, direct play, remux and transcode over HLS, and
//! subtitles. Needs a database and ffmpeg.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{self, RsaKeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common::{database_urls, have_ffmpeg, media, unique};

/// Throwaway key, generated for the tests only.
/// An instance marks the libraries missing from its configuration as
/// removed: tests with different libraries in one database run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const RSA_PKCS1: &[u8] = include_bytes!("../src/auth/test-rsa-2048.pkcs1.der");

fn enc(v: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(v)
}

/// Authorization codes: code → (nonce, PKCE challenge, user).
type Codes = Arc<Mutex<HashMap<String, (String, String, String)>>>;

#[derive(Clone, Default)]
struct Idp {
    issuer: String,
    codes: Codes,
    /// The user the next authorization signs in.
    next_user: Arc<Mutex<String>>,
}

fn sign(claims: &Value) -> String {
    let pair = RsaKeyPair::from_der(RSA_PKCS1).unwrap();
    let header = json!({"alg": "RS256", "kid": "k1", "typ": "JWT"});
    let signed = format!(
        "{}.{}",
        enc(header.to_string().as_bytes()),
        enc(claims.to_string().as_bytes())
    );
    let mut sig = vec![0; pair.public().modulus_len()];
    pair.sign(
        &signature::RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{signed}.{}", enc(&sig))
}

async fn discovery(State(idp): State<Idp>) -> Json<Value> {
    let i = &idp.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "userinfo_endpoint": format!("{i}/userinfo"),
        "end_session_endpoint": format!("{i}/logout"),
        "jwks_uri": format!("{i}/jwks"),
    }))
}

async fn jwks() -> Json<Value> {
    let pair = RsaKeyPair::from_der(RSA_PKCS1).unwrap();
    let parts: signature::RsaPublicKeyComponents<Vec<u8>> = pair.public().into();
    Json(
        json!({"keys": [{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": enc(&parts.n), "e": enc(&parts.e)}]}),
    )
}

async fn authorize(State(idp): State<Idp>, Query(q): Query<HashMap<String, String>>) -> Response {
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].split(' ').any(|s| s == "openid"));
    let code = unique("code");
    let user = idp.next_user.lock().unwrap().clone();
    idp.codes
        .lock()
        .unwrap()
        .insert(code.clone(), (q["nonce"].clone(), q["code_challenge"].clone(), user));
    Redirect::to(&format!("{}?code={code}&state={}", q["redirect_uri"], q["state"])).into_response()
}

async fn token(State(idp): State<Idp>, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> Response {
    // client_secret_basic.
    let basic = headers["authorization"].to_str().unwrap();
    assert_eq!(
        basic,
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("ferrocine:s3cret")
        )
    );
    let Some((nonce, challenge, user)) = idp.codes.lock().unwrap().remove(&f["code"]) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
    };
    if enc(&Sha256::digest(f["code_verifier"].as_bytes())) != challenge {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "error_description": "PKCE"})),
        )
            .into_response();
    }
    let now = jiff::Timestamp::now().as_second();
    let id_token = sign(&json!({
        "iss": idp.issuer, "aud": "ferrocine", "sub": format!("sub-{user}"), "exp": now + 300, "iat": now,
        "nonce": nonce, "preferred_username": user, "name": user.to_uppercase(),
        // Only the admin has a picture.
        "picture": (user == "admin").then(|| format!("{}/picture.png", idp.issuer)),
    }));
    Json(json!({"access_token": format!("at-{user}"), "token_type": "Bearer", "id_token": id_token})).into_response()
}

async fn userinfo(headers: HeaderMap) -> Json<Value> {
    // Groups come from userinfo only, as with many providers.
    let user = headers["authorization"]
        .to_str()
        .unwrap()
        .trim_start_matches("Bearer at-")
        .to_owned();
    let groups: Vec<&str> = match user.as_str() {
        "admin" => vec!["media", "media-admin"],
        "kid" => vec!["media"],
        _ => vec![],
    };
    Json(json!({"sub": format!("sub-{user}"), "groups": groups}))
}

// ------------------------------------------------------------ mock TMDB

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(800, 1200, image::Rgb([200, 40, 40]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

async fn tmdb(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // A v4 token is sent as a bearer token to the API, never to the image CDN.
    let auth = headers.get("authorization").map(|v| v.to_str().unwrap().to_owned());
    if path.starts_with("t/p/") {
        assert_eq!(auth, None);
    } else {
        assert_eq!(auth.as_deref(), Some("Bearer eyJ.test.token"));
    }
    let body = match path.as_str() {
        "find/tt0133093" => json!({"movie_results": [{"id": 603}], "tv_results": []}),
        p if p.starts_with("find/") => json!({"movie_results": [], "tv_results": []}),
        "search/movie" if q["query"] == "Alien" => {
            assert_eq!(q["year"], "1979");
            json!({"results": [{"id": 9999, "release_date": "2010-01-01"}, {"id": 348, "release_date": "1979-05-25"}]})
        }
        "search/movie" => json!({"results": []}),
        "movie/603" => json!({
            "id": 603, "title": "The Matrix (TMDB)", "overview": "Remote overview", "tagline": "Welcome to the Real World.",
            "release_date": "1999-03-30", "runtime": 136, "vote_average": 8.2, "vote_count": 25000,
            "genres": [{"name": "Remote genre"}], "imdb_id": "tt0133093",
            "poster_path": "/matrix-poster.jpg", "backdrop_path": "/matrix-backdrop.jpg",
            "credits": {"cast": [{"name": "Keanu Reeves", "character": "Neo"}], "crew": []},
        }),
        "movie/348" => json!({
            "id": 348, "title": "Alien", "overview": "In space no one can hear you scream.", "release_date": "1979-05-25",
            "vote_average": 8.1, "vote_count": 10, "genres": [{"name": "Horror"}, {"name": "Science Fiction"}],
            "poster_path": "/alien-poster.jpg", "backdrop_path": "/alien-backdrop.jpg", "imdb_id": "tt0078748",
            "release_dates": {"results": [{"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}]},
        }),
        "search/tv" if q["query"] == "Breaking Test" => {
            json!({"results": [{"id": 1396, "first_air_date": "2008-01-20"}]})
        }
        "search/tv" => json!({"results": []}),
        "tv/1396" => json!({
            "id": 1396, "name": "Breaking Bad", "overview": "Remote series overview", "first_air_date": "2008-01-20",
            "genres": [{"name": "Crime"}], "poster_path": "/bb-poster.jpg", "backdrop_path": "/bb-backdrop.jpg",
            "external_ids": {"imdb_id": "tt0903747", "tvdb_id": 81189},
        }),
        "tv/1396/season/1" => json!({
            "id": 3572, "name": "Season 1", "overview": "The first season.", "air_date": "2008-01-20",
            "episodes": [
                {"episode_number": 1, "name": "Pilot (TMDB)", "overview": "Walter's first day.", "still_path": "/s1e1.jpg"},
                {"episode_number": 2, "name": "Cat's in the Bag...", "overview": "Second.", "air_date": "2008-01-27"}
            ]
        }),
        p if p.starts_with("tv/1396/season/") => return StatusCode::NOT_FOUND.into_response(),
        p if p.starts_with("t/p/") => {
            return ([(axum::http::header::CONTENT_TYPE, "image/png")], png()).into_response();
        }
        other => panic!("unexpected TMDB request {other}"),
    };
    Json(body).into_response()
}

// ------------------------------------------- mock TheTVDB, OMDb, fanart.tv

async fn tvdb_login(Json(body): Json<Value>) -> Response {
    if body["apikey"] != "tvdb-key" {
        return (StatusCode::UNAUTHORIZED, Json(json!({"status": "failure"}))).into_response();
    }
    Json(json!({"status": "success", "data": {"token": "tvdb-token"}})).into_response()
}

async fn tvdb(
    State(idp): State<Idp>,
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tvdb-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let img = |n: &str| format!("{}/img/{n}", idp.issuer);
    let data = match path.as_str() {
        "search" if q["query"] == "Firefly" => json!([{"tvdb_id": "78874", "name": "Firefly", "year": "2002"}]),
        "search" => json!([]),
        p if p.starts_with("search/remoteid/") => json!([]),
        "series/78874/extended" => json!({
            "id": 78874, "name": "Firefly", "overview": "Captain Malcolm Reynolds and his crew take any job.",
            "firstAired": "2002-09-20", "genres": [{"name": "Science Fiction"}], "originalNetwork": {"name": "FOX"},
            "remoteIds": [{"id": "tt0303461", "sourceName": "IMDB"}],
            "artworks": [
                {"type": 2, "image": img("firefly-poster.png"), "language": "eng", "score": 10},
                {"type": 23, "image": img("firefly-logo.png"), "language": "eng", "score": 5}
            ]
        }),
        // DVD order: "Serenity" first (broadcast order aired "The Train Job" first).
        "series/78874/episodes/dvd/eng" if q.get("page").is_none_or(|p| p == "0") => json!({"episodes": [
            {"id": 1, "seasonNumber": 1, "number": 1, "name": "Serenity", "aired": "2002-12-20", "runtime": 86},
            {"id": 2, "seasonNumber": 1, "number": 2, "name": "The Train Job", "aired": "2002-09-20"}
        ]}),
        "series/78874/episodes/dvd/eng" => json!({"episodes": []}),
        other => panic!("unexpected TheTVDB request {other}"),
    };
    Json(json!({"status": "success", "data": data})).into_response()
}

async fn omdb(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(q["apikey"], "omdb-key");
    Json(match q["i"].as_str() {
        "tt0078748" => json!({"Response": "True", "Rated": "R", "imdbID": "tt0078748", "imdbRating": "8.5",
            "Ratings": [{"Source": "Internet Movie Database", "Value": "8.5/10"}, {"Source": "Rotten Tomatoes", "Value": "93%"}]}),
        "tt0303461" => json!({"Response": "True", "Rated": "TV-14", "imdbRating": "9.0"}),
        _ => json!({"Response": "False", "Error": "Incorrect IMDb ID."}),
    })
}

async fn fanart(
    State(idp): State<Idp>,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(q["api_key"], "fanart-key");
    let img = |n: &str| format!("{}/img/{n}", idp.issuer);
    match path.as_str() {
        "movies/348" => Json(json!({
            "hdmovielogo": [{"url": img("alien-logo.png"), "lang": "en", "likes": "3"}],
            "moviebackground": [{"url": img("alien-fanart.png"), "lang": "00", "likes": "1"}]
        }))
        .into_response(),
        "tv/78874" => {
            Json(json!({"hdtvlogo": [{"url": img("firefly-hd-logo.png"), "lang": "en", "likes": "9"}]})).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ----------------------------------------------------- mock AniList, AniDB

async fn anilist(State(idp): State<Idp>, Json(body): Json<Value>) -> Json<Value> {
    let media = json!({
        "id": 154587, "idMal": 52991,
        "title": {"romaji": "Sousou no Frieren", "english": "Frieren: Beyond Journey's End", "native": "葬送のフリーレン"},
        "description": "An elf mage outlives her party.<br>(Source: Crunchyroll)",
        "startDate": {"year": 2023, "month": 9, "day": 29}, "episodes": 28, "duration": 24,
        "genres": ["Adventure", "Fantasy"], "averageScore": 91, "countryOfOrigin": "JP",
        "coverImage": {"extraLarge": format!("{}/img/frieren-cover.png", idp.issuer)}, "bannerImage": null,
        "studios": {"nodes": [{"name": "Madhouse"}]}, "staff": {"edges": []}
    });
    let search = body["variables"]["search"].as_str().unwrap_or_default();
    let found = if search == "Sousou no Frieren" {
        vec![media]
    } else {
        vec![]
    };
    Json(json!({"data": {"Page": {"media": found}}}))
}

async fn anidb_titles() -> Response {
    use std::io::Write;
    let xml = r#"<?xml version="1.0"?><animetitles>
<anime aid="17617"><title type="main" xml:lang="x-jat">Sousou no Frieren</title>
<title type="official" xml:lang="en">Frieren: Beyond Journey&apos;s End</title></anime>
<anime aid="1"><title type="main" xml:lang="x-jat">Seikai no Monshou</title></anime></animetitles>"#;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(xml.as_bytes()).unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "application/gzip")],
        gz.finish().unwrap(),
    )
        .into_response()
}

async fn anidb_api(Query(q): Query<HashMap<String, String>>) -> Response {
    assert_eq!(
        (q["request"].as_str(), q["client"].as_str(), q["clientver"].as_str()),
        ("anime", "ferrocine", "1")
    );
    assert_eq!(q["aid"], "17617");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?><anime id="17617" restricted="false"><type>TV Series</type>
<startdate>2023-09-29</startdate><titles><title xml:lang="x-jat" type="main">Sousou no Frieren</title>
<title xml:lang="en" type="official">Frieren: Beyond Journey&apos;s End</title></titles>
<ratings><permanent count="5000">8.95</permanent></ratings>
<episodes><episode id="1"><epno type="1">1</epno><length>25</length><airdate>2023-09-29</airdate>
<title xml:lang="en">The Journey&apos;s End</title><summary>The party returns.</summary></episode></episodes></anime>"#;
    xml.into_response()
}

// --------------------------------------------------- mock OpenSubtitles.com

async fn os_search(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    assert_eq!(headers["api-key"], "os-key");
    assert!(headers["user-agent"].to_str().unwrap().starts_with("Ferrocine v"));
    let langs = q.get("languages").cloned().unwrap_or_default();
    let entry = |lang: &str, id: i64, hash: bool| {
        json!({"attributes": {"language": lang, "download_count": 100, "moviehash_match": hash, "release": format!("Release.{id}"),
            "files": [{"file_id": id, "file_name": format!("{id}.srt")}]}})
    };
    let data = match (
        q.get("imdb_id").map(String::as_str),
        q.get("parent_imdb_id").map(String::as_str),
    ) {
        (Some("78748"), _) if langs == "fr" => vec![entry("fr", 111, false)],
        (_, Some("303461")) if langs == "en" => vec![entry("en", 222, true), entry("en", 333, false)],
        _ => vec![],
    };
    Json(json!({"total_count": data.len(), "data": data})).into_response()
}

async fn os_login(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(headers["api-key"], "os-key");
    if body != json!({"username": "someone", "password": "os-password"}) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"token": "os-token", "status": 200})).into_response()
}

async fn os_download(State(idp): State<Idp>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(headers["api-key"], "os-key");
    // Downloads need a signed-in user.
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer os-token") {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "missing token"}))).into_response();
    }
    assert_eq!(body["sub_format"], "srt");
    match body["file_id"].as_i64() {
        Some(333) => (
            StatusCode::NOT_ACCEPTABLE,
            Json(json!({"message": "quota", "remaining": 0, "reset_time_utc": "2099-01-01T00:00:00Z"})),
        )
            .into_response(),
        Some(id) => Json(json!({"link": format!("{}/os/file/{id}", idp.issuer), "remaining": 9})).into_response(),
        None => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn os_file(axum::extract::Path(id): axum::extract::Path<i64>) -> String {
    format!("1\r\n00:00:01,000 --> 00:00:03,000\r\nSubtitle {id}\r\n")
}

async fn start_idp() -> Idp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let idp = Idp {
        issuer: format!("http://127.0.0.1:{}", addr.port()),
        ..Default::default()
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/userinfo", get(userinfo))
        .route("/tmdb/{*path}", get(tmdb))
        .route(
            "/img/{name}",
            get(|| async { ([(axum::http::header::CONTENT_TYPE, "image/png")], png()) }),
        )
        .route("/tvdb/login", post(tvdb_login))
        .route("/tvdb/{*path}", get(tvdb))
        .route("/omdb/", get(omdb))
        .route("/fanart/{*path}", get(fanart))
        .route("/anilist", post(anilist))
        .route("/anidb/titles", get(anidb_titles))
        .route("/anidb/api", get(anidb_api))
        .route("/os/subtitles", get(os_search))
        .route("/os/login", post(os_login))
        .route("/os/download", post(os_download))
        .route("/os/file/{id}", get(os_file))
        .route(
            "/picture.png",
            get(|| async { ([(axum::http::header::CONTENT_TYPE, "image/png")], png()) }),
        )
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A browser: keeps cookies, does not follow redirects, sends Fetch Metadata.
struct Browser {
    http: reqwest::Client,
    base: String,
    cookies: Mutex<HashMap<String, String>>,
}

impl Browser {
    fn new(base: &str) -> Self {
        Browser {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base: base.to_owned(),
            cookies: Mutex::new(HashMap::new()),
        }
    }

    fn cookie_header(&self) -> String {
        let c = self.cookies.lock().unwrap();
        c.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    fn keep(&self, resp: &reqwest::Response) {
        for v in resp.headers().get_all("set-cookie") {
            let s = v.to_str().unwrap();
            let (kv, attrs) = s.split_once(';').unwrap_or((s, ""));
            let (k, val) = kv.split_once('=').unwrap();
            let mut c = self.cookies.lock().unwrap();
            if attrs.contains("Max-Age=0") {
                c.remove(k);
            } else {
                c.insert(k.to_owned(), val.to_owned());
            }
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> reqwest::Response {
        let resp = req
            .header("cookie", self.cookie_header())
            .header("sec-fetch-site", "same-origin")
            .send()
            .await
            .unwrap();
        self.keep(&resp);
        resp
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base)
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.send(self.http.get(self.url(path))).await
    }

    async fn json(&self, path: &str) -> Value {
        let resp = self.get(path).await;
        assert_eq!(resp.status(), 200, "GET {path}");
        resp.json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.send(self.http.post(self.url(path)).json(&body)).await
    }

    /// The whole sign-in: our login, the provider, our callback.
    async fn sign_in(&self, idp: &Idp, user: &str) -> reqwest::Response {
        *idp.next_user.lock().unwrap() = user.to_owned();
        let login = self.get("/auth/login?return_to=/items/1").await;
        assert_eq!(login.status(), 303);
        let to_idp = login.headers()["location"].to_str().unwrap().to_owned();
        assert!(to_idp.starts_with(&idp.issuer));
        let back = self.http.get(&to_idp).send().await.unwrap();
        let callback = back.headers()["location"].to_str().unwrap().to_owned();
        self.get(&callback).await
    }
}

fn config(db: &str, idp: &Idp, port: u16, movies: &str, shows: &str, temp: &str, lib_suffix: &str) -> String {
    format!(
        r#"
server:
  listen: 127.0.0.1:{port}
  public_url: http://127.0.0.1:{port}
database:
  url: {db}
auth:
  oidc:
    issuer: {issuer}
    client_id: ferrocine
    client_secret: s3cret
    users: [media]
    admins: [media-admin]
transcoding:
  hw_accel: none
  temp_dir: {temp}
  max_sessions: 2
  segment_seconds: 2
  x264_preset: ultrafast
metadata:
  tmdb:
    api_key: eyJ.test.token
    api_url: {issuer}/tmdb
    image_url: {issuer}/tmdb/t/p
scanner:
  interval: 0
  probe_concurrency: 2
libraries:
  - id: movies-{lib_suffix}
    name: Movies
    kind: movies
    paths: [{movies}]
  - id: shows-{lib_suffix}
    name: Shows
    kind: shows
    paths: [{shows}]
    access: [grown-ups]
"#,
        issuer = idp.issuer
    )
}

async fn wait_for_scans(b: &Browser) {
    let mut libs = Value::Null;
    for _ in 0..240 {
        libs = b.json("/api/v1/libraries").await;
        let done = libs
            .as_array()
            .unwrap()
            .iter()
            .all(|l| !l["scan"]["running"].as_bool().unwrap() && l["scan"]["last_finished_at"].is_i64());
        if done {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("scans did not finish: {libs}");
}

fn find<'a>(items: &'a Value, title: &str) -> &'a Value {
    items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"] == title)
        .unwrap_or_else(|| panic!("{title} not in {items}"))
}

const CAPS: &str =
    r#"{"containers": ["mp4", "webm"], "video_codecs": ["h264", "vp9"], "audio_codecs": ["aac", "mp3", "opus"]}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_in_scan_and_play() {
    if !have_ffmpeg() {
        return;
    }
    let _serial = SERIAL.lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,ferrocine=info"))
        .with_test_writer()
        .try_init();
    let m = media();
    for (name, url) in database_urls() {
        let idp = start_idp().await;
        let port = free_port();
        let temp = m.root.join(format!("tmp-{name}"));
        let text = config(
            &url,
            &idp,
            port,
            &m.movies.to_string_lossy(),
            &m.shows.to_string_lossy(),
            &temp.to_string_lossy(),
            &unique(name),
        );
        let config_path = m.root.join(format!("config-{name}.yaml"));
        std::fs::write(&config_path, &text).unwrap();
        let cfg = ferrocine::config::load(&config_path).unwrap();
        let movies_id = cfg.libraries[0].id.clone();
        let shows_id = cfg.libraries[1].id.clone();
        tokio::spawn(ferrocine::app::run(config_path.clone(), cfg));
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if reqwest::get(format!("{base}/healthz")).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Unauthenticated, then refused (no group), then signed in.
        let anon = Browser::new(&base);
        let r = anon.get("/api/v1/me").await;
        assert_eq!(r.status(), 401, "{name}");
        let stranger = Browser::new(&base);
        assert_eq!(stranger.sign_in(&idp, "stranger").await.status(), 403, "{name}");
        assert_eq!(stranger.get("/api/v1/me").await.status(), 401, "{name}");
        let admin = Browser::new(&base);
        let r = admin.sign_in(&idp, "admin").await;
        assert_eq!(r.status(), 303, "{name}");
        assert_eq!(r.headers()["location"], "/items/1", "{name}");
        let me = admin.json("/api/v1/me").await;
        assert_eq!(
            (me["username"].as_str(), me["admin"].as_bool()),
            (Some("admin"), Some(true)),
            "{name}"
        );
        assert_eq!(me["display_name"], "ADMIN", "{name}");
        assert_eq!(me["groups"], json!(["media", "media-admin"]), "{name}");
        // The picture was fetched at sign-in, cropped to a square and stored.
        let avatar_url = me["avatar"].as_str().expect("an avatar URL").to_owned();
        assert!(avatar_url.starts_with("/api/v1/me/avatar?v="), "{avatar_url}");
        let avatar = admin.get(&avatar_url).await;
        assert_eq!(avatar.headers()["content-type"], "image/jpeg");
        let avatar = image::load_from_memory(&avatar.bytes().await.unwrap()).unwrap();
        assert_eq!((avatar.width(), avatar.height()), (192, 192));
        // A replayed callback is refused.
        let replay = Browser::new(&base);
        *idp.next_user.lock().unwrap() = "admin".into();
        let r = replay.get("/auth/callback?code=x&state=y").await;
        assert_eq!(r.status(), 400, "{name}");

        wait_for_scans(&admin).await;

        // Movies.
        let page = admin
            .json(&format!("/api/v1/libraries/{movies_id}/items?sort=title"))
            .await;
        assert_eq!(page["total"], 4, "{name}: {page}");
        let items = &page["items"];
        let matrix = find(items, "The Matrix");
        let alien = find(items, "Alien");
        let old = find(items, "Old Film");
        assert_eq!(matrix["year"], 1999, "{name}");
        assert_eq!(alien["year"], 1979, "{name}");
        assert!(
            matrix["images"]["poster"]
                .as_str()
                .unwrap()
                .contains("/images/poster?v="),
            "{name}"
        );
        // TMDB: searched by title and year, artwork downloaded.
        assert_eq!(alien["official_rating"], "R", "{name}");
        assert_eq!(alien["genres"], json!(["Horror", "Science Fiction"]), "{name}");
        assert!(
            alien["images"]["poster"].is_string() && alien["images"]["backdrop"].is_string(),
            "{name}: {alien}"
        );
        let detail = admin.json(&format!("/api/v1/items/{}", alien["id"])).await;
        assert_eq!(detail["overview"], "In space no one can hear you scream.", "{name}");
        assert_eq!(detail["provider_ids"]["tmdb"], "348", "{name}");
        // No poster: a frame was extracted.
        assert!(old["images"]["thumb"].is_string(), "{name}: {old}");
        let detail = admin.json(&format!("/api/v1/items/{}", matrix["id"])).await;
        // The .nfo wins (title, plot, genres); TMDB, found through the IMDb
        // id of the .nfo, fills the rest. The local poster stays.
        assert_eq!(detail["title"], "The Matrix", "{name}");
        assert_eq!(detail["overview"], "A hacker learns the truth.", "{name}");
        assert_eq!(detail["genres"], json!(["Action", "Science Fiction"]), "{name}");
        assert_eq!(detail["tagline"], "Welcome to the Real World.", "{name}");
        assert_eq!(detail["provider_ids"]["tmdb"], "603", "{name}");
        assert_eq!(detail["people"][0]["character"], "Neo", "{name}");
        assert!(detail["images"]["backdrop"].is_string(), "{name}");
        assert_eq!(detail["provider_ids"]["imdb"], "tt0133093", "{name}");
        assert_eq!(detail["media"]["container"], "mp4", "{name}");
        assert_eq!(detail["media"]["file_name"], "The Matrix (1999).mp4", "{name}");
        let subs = detail["media"]["subtitles"].as_array().unwrap();
        assert_eq!(subs.len(), 1, "{name}");
        assert_eq!(
            (subs[0]["external"].as_bool(), subs[0]["language"].as_str()),
            (Some(true), Some("en"))
        );
        assert!(subs[0].get("path").is_none(), "server paths must not leak");
        let genres = admin.json(&format!("/api/v1/libraries/{movies_id}/genres")).await;
        assert_eq!(genres, json!(["Action", "Horror", "Science Fiction"]), "{name}");

        // Artwork.
        let poster = admin.get(matrix["images"]["poster"].as_str().unwrap()).await;
        assert_eq!(poster.status(), 200);
        assert_eq!(poster.headers()["content-type"], "image/jpeg");
        let etag = poster.headers()["etag"].to_str().unwrap().to_owned();
        let bytes = poster.bytes().await.unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        let again = admin
            .send(
                admin
                    .http
                    .get(admin.url(matrix["images"]["poster"].as_str().unwrap()))
                    .header("if-none-match", &etag),
            )
            .await;
        assert_eq!(again.status(), 304);

        // Direct play, with ranges.
        let id = matrix["id"].as_i64().unwrap();
        let caps: Value = serde_json::from_str(CAPS).unwrap();
        let p: Value = admin
            .post(&format!("/api/v1/items/{id}/playback"), json!({"capabilities": caps}))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(p["method"], "direct", "{name}: {p}");
        let r = admin
            .send(
                admin
                    .http
                    .get(admin.url(p["url"].as_str().unwrap()))
                    .header("range", "bytes=0-99"),
            )
            .await;
        assert_eq!(r.status(), 206);
        assert_eq!(r.bytes().await.unwrap().len(), 100);
        let vtt = admin
            .get(&format!(
                "/api/v1/items/{id}/subtitles/{}?start_ms=5000",
                subs[0]["index"]
            ))
            .await
            .text()
            .await
            .unwrap();
        assert!(vtt.starts_with("WEBVTT"), "{vtt}");
        assert!(
            !vtt.contains("Wake up") && vtt.contains("00:00:03.000 --> 00:00:05.000"),
            "{vtt}"
        );

        // Remux (Matroska) with the embedded subtitle.
        let aid = alien["id"].as_i64().unwrap();
        let p: Value = admin
            .post(
                &format!("/api/v1/items/{aid}/playback"),
                json!({"capabilities": caps, "start_ms": 4000}),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(p["method"], "remux", "{name}: {p}");
        assert_eq!(p["start_ms"], 4000);
        play_hls(&admin, &p).await;
        let detail = admin.json(&format!("/api/v1/items/{aid}")).await;
        let embedded = &detail["media"]["subtitles"][0];
        assert_eq!(embedded["external"], false);
        let vtt = admin
            .get(&format!("/api/v1/items/{aid}/subtitles/{}", embedded["index"]))
            .await
            .text()
            .await
            .unwrap();
        assert!(vtt.contains("no one can hear you scream"), "{vtt}");
        // Another user cannot read the session.
        let kid = Browser::new(&base);
        assert_eq!(kid.sign_in(&idp, "kid").await.status(), 303);
        let kid_me = kid.json("/api/v1/me").await;
        assert!(kid_me["avatar"].is_null(), "{name}: {kid_me}");
        assert_eq!(kid.get("/api/v1/me/avatar").await.status(), 404);
        let stolen = kid.get(p["url"].as_str().unwrap()).await;
        assert_eq!(stolen.status(), 404);
        let sid = p["session_id"].as_str().unwrap();
        assert_eq!(
            admin
                .send(admin.http.delete(admin.url(&format!("/api/v1/hls/{sid}"))))
                .await
                .status(),
            204
        );
        assert_eq!(admin.get(p["url"].as_str().unwrap()).await.status(), 404);

        // HDR10: transcoded with tone mapping to SDR (BT.709), in software.
        let nebula = find(items, "Nebula");
        let nid = nebula["id"].as_i64().unwrap();
        let detail = admin.json(&format!("/api/v1/items/{nid}")).await;
        assert_eq!(detail["media"]["video"]["hdr"], "hdr10", "{name}");
        let p: Value = admin
            .post(&format!("/api/v1/items/{nid}/playback"), json!({"capabilities": caps}))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            (p["method"].as_str(), p["tone_mapping"].as_bool()),
            (Some("transcode"), Some(true)),
            "{name}: {p}"
        );
        let segment = play_hls(&admin, &p).await;
        assert_eq!(
            probe_transfer(&segment, &m.root.join(format!("nebula-{name}.mp4"))),
            "bt709",
            "{name}"
        );
        let sid = p["session_id"].as_str().unwrap();
        admin
            .send(admin.http.delete(admin.url(&format!("/api/v1/hls/{sid}"))))
            .await;

        // Transcode (MPEG-4 part 2), software since tests have no GPU.
        let oid = old["id"].as_i64().unwrap();
        let p: Value = admin
            .post(
                &format!("/api/v1/items/{oid}/playback"),
                json!({"capabilities": caps, "max_height": 240}),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(p["method"], "transcode", "{name}: {p}");
        assert!(
            p["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains("mpeg4"))
        );
        play_hls(&admin, &p).await;
        // The running configuration, described, without its secrets.
        let described = admin.json("/api/v1/config").await;
        let described_text = described.to_string();
        assert!(
            !described_text.contains("s3cret") && !described_text.contains("eyJ.test.token"),
            "{name}: secrets leaked"
        );
        let sections: Vec<&str> = described["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            sections,
            [
                "server",
                "database",
                "auth",
                "transcoding",
                "metadata",
                "scanner",
                "subtitles",
                "libraries"
            ]
        );
        let transcoding = &described["sections"][3]["fields"];
        let hw = transcoding
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "hw_accel")
            .unwrap();
        assert_eq!(
            (hw["value"].as_str(), hw["choices"].as_array().unwrap().len()),
            (Some("none"), 5),
            "{hw}"
        );
        assert_eq!(described["sections"][7]["items"].as_array().unwrap().len(), 2);
        assert_eq!(kid.get("/api/v1/config").await.status(), 403);
        let system = admin.json("/api/v1/system").await;
        assert_eq!(system["transcodes"].as_array().unwrap().len(), 1, "{system}");
        assert_eq!(kid.get("/api/v1/system").await.status(), 403);
        // Cross-site writes are refused.
        let r = admin
            .http
            .post(admin.url(&format!("/api/v1/items/{oid}/played")))
            .header("cookie", admin.cookie_header())
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);

        // Progress, continue watching, played.
        let r = admin
            .post(&format!("/api/v1/items/{oid}/progress"), json!({"position_ms": 6000}))
            .await;
        assert_eq!(r.status(), 204);
        let home = admin.json("/api/v1/home").await;
        assert_eq!(home["continue_watching"][0]["id"], oid, "{name}: {home}");
        let r = admin
            .post(&format!("/api/v1/items/{oid}/progress"), json!({"position_ms": 11_900}))
            .await;
        assert_eq!(r.status(), 204);
        let s = admin.json(&format!("/api/v1/items/{oid}")).await;
        assert_eq!(s["user"]["played"], true, "{name}");

        // Shows: structure, names, specials, access control.
        let page = admin.json(&format!("/api/v1/libraries/{shows_id}/items")).await;
        assert_eq!(page["total"], 1, "{name}: {page}");
        let series = &page["items"][0];
        assert_eq!(series["title"], "Breaking Test", "{name}");
        assert_eq!(
            (series["child_count"].as_i64(), series["unplayed_count"].as_i64()),
            (Some(3), Some(4))
        );
        let seasons = admin.json(&format!("/api/v1/items/{}/children", series["id"])).await;
        let names: Vec<&str> = seasons
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["title"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Specials", "Season 1", "Season 2"], "{name}");
        let s1 = &seasons[1];
        let episodes = admin.json(&format!("/api/v1/items/{}/children", s1["id"])).await;
        let titles: Vec<&str> = episodes
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap())
            .collect();
        // TMDB names replace names from files, not from .nfo files.
        assert_eq!(titles, ["Pilot (TMDB)", "Cat's in the Bag..."], "{name}");
        assert!(episodes[0]["images"]["thumb"].is_string(), "{name}");
        assert_eq!(s1["title"], "Season 1", "{name}");
        let season = admin.json(&format!("/api/v1/items/{}", s1["id"])).await;
        assert_eq!(season["overview"], "The first season.", "{name}");
        let show = admin.json(&format!("/api/v1/items/{}", series["id"])).await;
        assert_eq!(show["overview"], "A chemistry teacher.", "{name}");
        assert_eq!(show["provider_ids"]["tvdb"], "81189", "{name}");
        assert_eq!(episodes[0]["series_title"], "Breaking Test");
        // Episodes fall back to the series poster.
        assert!(
            episodes[0]["images"]["poster"]
                .as_str()
                .unwrap()
                .contains(&format!("/items/{}/", series["id"])),
            "{name}"
        );
        // Next up after watching the pilot.
        let pilot = episodes[0]["id"].as_i64().unwrap();
        assert_eq!(
            admin
                .post(&format!("/api/v1/items/{pilot}/played"), json!({}))
                .await
                .status(),
            204
        );
        let home = admin.json("/api/v1/home").await;
        assert_eq!(home["next_up"][0]["id"], episodes[1]["id"], "{name}: {home}");
        let found = admin.json("/api/v1/search?q=pilot%20(tmdb").await;
        assert_eq!(found[0]["id"], pilot, "{name}");
        // The kid is not in `grown-ups`.
        let libs = kid.json("/api/v1/libraries").await;
        assert_eq!(libs.as_array().unwrap().len(), 1, "{name}: {libs}");
        assert!(libs[0].get("scan").is_none());
        assert_eq!(kid.get(&format!("/api/v1/items/{pilot}")).await.status(), 404);
        assert_eq!(kid.json("/api/v1/search?q=pilot").await, json!([]));

        // Reload: an invalid file is refused and the running configuration kept;
        // a valid one opening the shows to everyone applies at once.
        std::fs::write(&config_path, text.replace("access: [grown-ups]", "access: grown-ups")).unwrap();
        let r = admin.post("/api/v1/reload", json!({})).await;
        assert_eq!(r.status(), 422, "{name}");
        assert_eq!(
            kid.json("/api/v1/libraries").await.as_array().unwrap().len(),
            1,
            "{name}"
        );
        assert_eq!(kid.post("/api/v1/reload", json!({})).await.status(), 403, "{name}");
        std::fs::write(
            &config_path,
            text.replace("    access: [grown-ups]\n", "")
                .replace("max_sessions: 2", "max_sessions: 3"),
        )
        .unwrap();
        let report: Value = admin.post("/api/v1/reload", json!({})).await.json().await.unwrap();
        assert_eq!(report["applied"], json!(["libraries"]), "{name}: {report}");
        assert_eq!(report["restart_required"], json!(["transcoding"]), "{name}: {report}");
        assert_eq!(
            kid.json("/api/v1/libraries").await.as_array().unwrap().len(),
            2,
            "{name}"
        );
        assert_eq!(kid.get(&format!("/api/v1/items/{pilot}")).await.status(), 200, "{name}");
        // Old Film found nothing remotely: it keeps its extracted frame.
        assert_eq!(old["genres"], json!([]), "{name}");

        // A rescan finds nothing new; a removed file disappears.
        let removed = m.shows.join("Breaking Test (2008)/Season 02/s02e01.mp4");
        let saved = m.root.join(format!("saved-{name}.mp4"));
        std::fs::rename(&removed, &saved).unwrap();
        let r = admin
            .post(&format!("/api/v1/libraries/{shows_id}/scan"), json!({}))
            .await;
        assert_eq!(r.status(), 202);
        tokio::time::sleep(Duration::from_millis(300)).await;
        wait_for_scans(&admin).await;
        let seasons = admin.json(&format!("/api/v1/items/{}/children", series["id"])).await;
        assert_eq!(seasons.as_array().unwrap().len(), 2, "{name}: {seasons}");
        std::fs::rename(&saved, &removed).unwrap();

        // A metadata refresh rebuilds an item from its name, .nfo and provider.
        let r = admin
            .post(&format!("/api/v1/items/{}/refresh", alien["id"]), json!({}))
            .await;
        assert_eq!(r.status(), 202);
        assert_eq!(
            kid.post(&format!("/api/v1/items/{}/refresh", old["id"]), json!({}))
                .await
                .status(),
            403
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        wait_for_scans(&admin).await;
        let detail = admin.json(&format!("/api/v1/items/{}", alien["id"])).await;
        assert_eq!(
            (detail["title"].as_str(), detail["provider_ids"]["tmdb"].as_str()),
            (Some("Alien"), Some("348"))
        );
        assert_eq!(detail["id"], alien["id"], "{name}: the id survives");

        // Sign-out ends the session and goes to the provider.
        let r = admin.send(admin.http.post(admin.url("/auth/logout"))).await;
        assert_eq!(r.status(), 303);
        assert!(
            r.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with(&format!("{}/logout?", idp.issuer))
        );
        assert_eq!(admin.get("/api/v1/me").await.status(), 401);
        let _ = SocketAddr::from(([127, 0, 0, 1], port));
    }
    let _ = std::fs::remove_dir_all(&m.root);
}

/// The colour transfer of the video in an init segment followed by a media segment.
fn probe_transfer(fmp4: &[u8], path: &std::path::Path) -> String {
    std::fs::write(path, fmp4).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_transfer",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_end_matches(',')
        .to_owned()
}

/// Read an HLS playlist, its init segment and its first media segment;
/// returns them concatenated (a playable fMP4 fragment).
async fn play_hls(b: &Browser, p: &Value) -> Vec<u8> {
    let url = p["url"].as_str().unwrap();
    let playlist = b.get(url).await;
    assert_eq!(playlist.status(), 200);
    assert_eq!(playlist.headers()["cache-control"], "no-store");
    let text = playlist.text().await.unwrap();
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"), "{text}");
    assert!(text.contains("init.mp4"), "{text}");
    let base = url.trim_end_matches("index.m3u8");
    let init = b.get(&format!("{base}init.mp4")).await;
    assert_eq!(init.status(), 200);
    let mut out = init.bytes().await.unwrap().to_vec();
    let seg = text.lines().find(|l| l.ends_with(".m4s")).unwrap();
    let seg = b.get(&format!("{base}{seg}")).await;
    assert_eq!(seg.status(), 200);
    let seg = seg.bytes().await.unwrap();
    assert!(seg.len() > 1000);
    out.extend_from_slice(&seg);
    assert_eq!(b.get(&format!("{base}..%2Fescape")).await.status(), 404);
    out
}

/// TheTVDB in DVD order, OMDb, fanart.tv, AniList, AniDB and OpenSubtitles
/// (automatic and on demand), against mocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn providers_and_subtitles() {
    if !have_ffmpeg() {
        return;
    }
    let _serial = SERIAL.lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,ferrocine=info"))
        .with_test_writer()
        .try_init();
    let root = std::env::temp_dir().join(unique("fc-providers"));
    let h264_aac = [
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-shortest",
    ];
    common::video(&root.join("movies/Alien (1979)/Alien (1979).mkv"), 2, &h264_aac);
    common::video(
        &root.join("shows/Firefly (2002)/Season 01/Firefly - S01E01.mkv"),
        2,
        &h264_aac,
    );
    common::video(
        &root.join("anime/Sousou no Frieren/[SubsPlease] Sousou no Frieren - 01 (1080p).mkv"),
        2,
        &h264_aac,
    );
    for (name, url) in database_urls() {
        let idp = start_idp().await;
        let port = free_port();
        let suffix = unique(name);
        let text = format!(
            r#"
server:
  listen: 127.0.0.1:{port}
  public_url: http://127.0.0.1:{port}
database:
  url: {url}
auth:
  oidc:
    issuer: {issuer}
    client_id: ferrocine
    client_secret: s3cret
    users: [media]
    admins: [media-admin]
transcoding:
  hw_accel: none
  temp_dir: {temp}
metadata:
  tmdb:
    api_key: eyJ.test.token
    api_url: {issuer}/tmdb
    image_url: {issuer}/tmdb/t/p
  tvdb:
    api_key: tvdb-key
    api_url: {issuer}/tvdb
  omdb:
    api_key: omdb-key
    api_url: {issuer}/omdb
  fanart:
    api_key: fanart-key
    api_url: {issuer}/fanart
  anilist:
    api_url: {issuer}/anilist
  anidb:
    client: ferrocine
    client_version: 1
    api_url: {issuer}/anidb/api
    titles_url: {issuer}/anidb/titles
    image_url: {issuer}/img
subtitles:
  opensubtitles:
    api_key: os-key
    username: someone
    password: os-password
    api_url: {issuer}/os
  languages: [fr]
scanner:
  interval: 0
libraries:
  - id: movies-{suffix}
    name: Movies
    kind: movies
    paths: [{root}/movies]
  - id: shows-{suffix}
    name: Shows
    kind: shows
    paths: [{root}/shows]
    episode_order: dvd
  - id: anime-{suffix}
    name: Anime
    kind: shows
    paths: [{root}/anime]
    anime: true
    prefer_original_title: [ja]
"#,
            issuer = idp.issuer,
            temp = root.join(format!("tmp-{name}")).display(),
            root = root.display(),
        );
        let config_path = root.join(format!("config-{name}.yaml"));
        std::fs::write(&config_path, &text).unwrap();
        let cfg = ferrocine::config::load(&config_path).unwrap();
        let ids: Vec<String> = cfg.libraries.iter().map(|l| l.id.clone()).collect();
        tokio::spawn(ferrocine::app::run(config_path.clone(), cfg));
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if reqwest::get(format!("{base}/healthz")).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let admin = Browser::new(&base);
        assert_eq!(admin.sign_in(&idp, "admin").await.status(), 303, "{name}");
        wait_for_scans(&admin).await;

        // A movie: TMDB, OMDb ratings, a fanart.tv logo, an automatic subtitle.
        let page = admin.json(&format!("/api/v1/libraries/{}/items", ids[0])).await;
        let alien = admin
            .json(&format!("/api/v1/items/{}", find(&page["items"], "Alien")["id"]))
            .await;
        let ratings: Vec<(&str, f64)> = alien["ratings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["source"].as_str().unwrap(), r["value"].as_f64().unwrap()))
            .collect();
        for expected in [("tmdb", 8.1), ("imdb", 8.5), ("rotten_tomatoes", 93.0)] {
            assert!(ratings.contains(&expected), "{name}: {ratings:?}");
        }
        assert_eq!(alien["official_rating"], "R", "{name}");
        assert_eq!(alien["provider_ids"]["imdb"], "tt0078748", "{name}");
        let logo = admin.get(alien["images"]["logo"].as_str().expect("a logo")).await;
        assert_eq!(logo.headers()["content-type"], "image/png", "{name}");
        let subs = alien["media"]["subtitles"].as_array().unwrap();
        let fr = subs
            .iter()
            .find(|s| s["language"] == "fr")
            .unwrap_or_else(|| panic!("{name}: {subs:?}"));
        let index = fr["index"].as_i64().unwrap();
        assert!(index >= 1_000_000, "{name}: {fr}");
        let vtt = admin
            .get(&format!("/api/v1/items/{}/subtitles/{index}", alien["id"]))
            .await
            .text()
            .await
            .unwrap();
        assert!(
            vtt.starts_with("WEBVTT") && vtt.contains("00:00:01.000 --> 00:00:03.000") && vtt.contains("Subtitle 111"),
            "{name}: {vtt}"
        );

        // A series in DVD order from TheTVDB, with OMDb and fanart.tv.
        let page = admin.json(&format!("/api/v1/libraries/{}/items", ids[1])).await;
        let firefly = admin
            .json(&format!("/api/v1/items/{}", find(&page["items"], "Firefly")["id"]))
            .await;
        assert_eq!(
            (
                firefly["provider_ids"]["tvdb"].as_str(),
                firefly["provider_ids"]["imdb"].as_str()
            ),
            (Some("78874"), Some("tt0303461")),
            "{name}"
        );
        assert_eq!(firefly["official_rating"], "TV-14", "{name}");
        assert!(
            firefly["images"]["logo"].is_string() && firefly["images"]["poster"].is_string(),
            "{name}"
        );
        let seasons = admin.json(&format!("/api/v1/items/{}/children", firefly["id"])).await;
        let episodes = admin
            .json(&format!("/api/v1/items/{}/children", seasons[0]["id"]))
            .await;
        assert_eq!(episodes[0]["title"], "Serenity", "{name}: {episodes}");
        let episode = episodes[0]["id"].as_i64().unwrap();

        // On demand: search, download, quota, removal.
        let r = admin
            .get(&format!("/api/v1/items/{episode}/subtitles/search?language=en"))
            .await;
        assert_eq!(r.status(), 200, "{name}");
        let found: Value = r.json().await.unwrap();
        let results = found["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "{name}: {found}");
        assert_eq!(
            (results[0]["file_id"].as_i64(), results[0]["hash_match"].as_bool()),
            (Some(222), Some(true))
        );
        let r = admin
            .post(
                &format!("/api/v1/items/{episode}/subtitles"),
                json!({"file_id": 222, "language": "en", "release": "Release.222", "hash_match": true}),
            )
            .await;
        assert_eq!(r.status(), 201, "{name}");
        let stream: Value = r.json().await.unwrap();
        let index = stream["index"].as_i64().unwrap();
        assert!(index >= 1_000_000 && stream["language"] == "en", "{name}: {stream}");
        let detail = admin.json(&format!("/api/v1/items/{episode}")).await;
        assert!(
            detail["media"]["subtitles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["index"] == index),
            "{name}"
        );
        let vtt = admin
            .get(&format!("/api/v1/items/{episode}/subtitles/{index}"))
            .await
            .text()
            .await
            .unwrap();
        assert!(vtt.contains("Subtitle 222"), "{name}: {vtt}");
        let r = admin
            .post(
                &format!("/api/v1/items/{episode}/subtitles"),
                json!({"file_id": 333, "language": "en"}),
            )
            .await;
        assert_eq!(r.status(), 429, "{name}");
        let kid = Browser::new(&base);
        assert_eq!(kid.sign_in(&idp, "kid").await.status(), 303, "{name}");
        let path = format!("/api/v1/items/{episode}/subtitles/{index}");
        assert_eq!(kid.send(kid.http.delete(kid.url(&path))).await.status(), 403, "{name}");
        assert_eq!(
            admin.send(admin.http.delete(admin.url(&path))).await.status(),
            204,
            "{name}"
        );
        assert_eq!(admin.get(&path).await.status(), 404, "{name}");

        // Anime: AniList for the series, AniDB for the episodes.
        let page = admin.json(&format!("/api/v1/libraries/{}/items", ids[2])).await;
        let frieren = admin
            .json(&format!(
                "/api/v1/items/{}",
                find(&page["items"], "葬送のフリーレン")["id"]
            ))
            .await;
        // The library prefers Japanese titles.
        assert_eq!(
            (
                frieren["original_language"].as_str(),
                frieren["localized_title"].as_str()
            ),
            (Some("ja"), Some("Frieren: Beyond Journey's End")),
            "{name}"
        );
        assert_eq!(
            (
                frieren["provider_ids"]["anilist"].as_str(),
                frieren["provider_ids"]["anidb"].as_str()
            ),
            (Some("154587"), Some("17617")),
            "{name}: {frieren}"
        );
        assert_eq!(
            frieren["overview"], "An elf mage outlives her party.\n(Source: Crunchyroll)",
            "{name}"
        );
        let seasons = admin.json(&format!("/api/v1/items/{}/children", frieren["id"])).await;
        let episodes = admin
            .json(&format!("/api/v1/items/{}/children", seasons[0]["id"]))
            .await;
        assert_eq!(episodes[0]["title"], "The Journey's End", "{name}: {episodes}");

        // Without the preference, after a reload: the English title, at once.
        std::fs::write(
            &config_path,
            text.replace("    prefer_original_title: [ja]\n", "    prefer_original_title: []\n"),
        )
        .unwrap();
        assert_eq!(admin.post("/api/v1/reload", json!({})).await.status(), 200, "{name}");
        let again = admin.json(&format!("/api/v1/items/{}", frieren["id"])).await;
        assert_eq!(again["title"], "Frieren: Beyond Journey's End", "{name}");
        assert_eq!(again["original_title"], "葬送のフリーレン", "{name}");

        // The configuration page describes the new sections without secrets.
        let config = admin.get("/api/v1/config").await.text().await.unwrap();
        assert!(
            config.contains("opensubtitles") && config.contains("episode_order"),
            "{name}"
        );
        for secret in ["os-key", "os-password", "tvdb-key", "omdb-key", "fanart-key"] {
            assert!(!config.contains(secret), "{name}: {secret} leaked");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}
