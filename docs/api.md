# HTTP API

Ferrocine serves one origin:

| Path | Purpose |
|---|---|
| `/` | The web UI (static files, SPA fallback to `index.html`) |
| `/auth/login`, `/auth/callback`, `/auth/logout` | OpenID Connect sign-in (see [DECISIONS.md](../DECISIONS.md), D10 and D11) |
| `/api/v1/…` | JSON API, authenticated by the session cookie |
| `/healthz`, `/readyz` | Probes (no authentication) |
| `/metrics` | Prometheus metrics (optional bearer token, `server.metrics_token`) |

Conventions:

- **Authentication.** Every `/api/v1` endpoint needs the `ferrocine_session` cookie set by
  `/auth/callback`. Without it: `401 {"error": "unauthenticated", "login": "/auth/login"}`.
- **CSRF.** Requests with an unsafe method (`POST`, `PUT`, `DELETE`) must be same-origin:
  `Sec-Fetch-Site: same-origin` (or `none`), or an `Origin` equal to `server.public_url`.
  Otherwise `403`.
- **Errors** are `{"error": "message"}` with a 4xx/5xx status.
- **Timestamps** are milliseconds since the Unix epoch; durations and positions are milliseconds
  (`*_ms`).
- **Ids.** Items have numeric ids (`i64`); libraries have the string id of the configuration.
- **Admin** endpoints answer `403` for non-admin users.

## Users

### `GET /api/v1/me`

```json
{ "id": 1, "username": "alice", "display_name": "Alice", "email": "alice@example.com", "admin": true,
  "groups": ["media", "media-admin"], "avatar": "/api/v1/me/avatar?v=3f1c0a9b2e7d4c11" }
```

- `groups`: the values of the provider's groups claim at sign-in (`auth.oidc.groups_claim`).
- `avatar`: the user's picture, or `null` when the provider sent none (the UI then shows initials).

### `GET /api/v1/me/avatar`

The user's picture: the OIDC `picture` claim, fetched at sign-in, cropped to a 192×192 square,
JPEG. `404` when there is none. Cached privately for a year (the URL in `/me` is versioned).

### `POST /auth/logout`

Form post (same-origin). Ends the session and redirects (303) to the provider's
`end_session_endpoint` when it has one, else to `/`.

### `GET /auth/login?return_to=/path`

Starts the sign-in; `return_to` must be a local path.

## Libraries

### `GET /api/v1/libraries`

Libraries the user may see (configuration `libraries[].access`).

```json
[
  { "id": "movies", "name": "Movies", "kind": "movies", "item_count": 412,
    "scan": { "running": false, "last_started_at": 1761200000000, "last_finished_at": 1761200042000,
              "last_error": null } }
]
```

`kind` is `movies` or `shows`. `scan` is present for admins only.

### `GET /api/v1/libraries/{id}/items`

Top-level items of a library: movies, or series. Query parameters (all optional):

| Parameter | Values |
|---|---|
| `sort` | `title` (default), `year`, `added`, `rating`, `released` |
| `order` | `asc` (default for title), `desc` (default for the others) |
| `genre` | one genre name |
| `played` | `true` / `false` (series: fully played or not) |
| `favorite` | `true` |
| `q` | case-insensitive substring of the title |
| `offset`, `limit` | paging; `limit` defaults to 100, at most 500 |

```json
{ "items": [ItemSummary, …], "total": 412 }
```

### `GET /api/v1/libraries/{id}/genres`

`["Action", "Drama", …]`, sorted.

### `POST /api/v1/libraries/{id}/scan` (admin)

Starts a scan; `202`. `?refresh_metadata=true` also refreshes metadata of unchanged items.

## Items

### ItemSummary

```json
{
  "id": 12,
  "library_id": "shows",
  "kind": "episode",
  "title": "Pilot",
  "year": 2008,
  "parent_id": 11,
  "series_id": 10,
  "series_title": "Breaking Bad",
  "season_number": 1,
  "episode_number": 1,
  "runtime_ms": 3480000,
  "rating": 8.2,
  "official_rating": "TV-14",
  "genres": ["Drama", "Crime"],
  "premiere_date": "2008-01-20",
  "added_at": 1761200000000,
  "images": {
    "poster": "/api/v1/items/12/images/poster?v=9f2c1a",
    "backdrop": null,
    "thumb": "/api/v1/items/12/images/thumb?v=77ab00",
    "logo": null
  },
  "user": { "position_ms": 120000, "played": false, "play_count": 0, "favorite": false,
            "last_played_at": 1761200000000 },
  "child_count": null,
  "unplayed_count": null
}
```

- `kind`: `movie`, `series`, `season`, `episode`, `extra`.
- `extra_type` (extras only): `trailer`, `featurette`, `behind_the_scenes`, `deleted_scene`,
  `interview`, `scene`, `short` or `extra` (DVD titles, `Extras` folders).
- `series_*`, `season_number`, `episode_number` only for seasons and episodes (`null` otherwise).
  `season_number` 0 is "Specials".
- `images.*` are URLs or `null`; the `v` parameter changes when the image does, so the images can
  be cached for long.
- `child_count`: seasons of a series, episodes of a season. `unplayed_count`: unplayed episodes of
  a series or season.

### `GET /api/v1/items/{id}`

An ItemSummary plus:

```json
{
  "overview": "…",
  "tagline": "…",
  "original_title": "…",
  "original_language": "en",
  "localized_title": "…",
  "studios": ["AMC"],
  "people": [{ "name": "Bryan Cranston", "role": "actor", "character": "Walter White" },
             { "name": "Vince Gilligan", "role": "director", "character": null }],
  "provider_ids": { "tmdb": "1396", "imdb": "tt0903747", "tvdb": "81189", "anidb": null, "anilist": null },
  "ratings": [{ "source": "tmdb", "value": 8.9, "max": 10.0 },
              { "source": "imdb", "value": 9.5, "max": 10.0 },
              { "source": "rotten_tomatoes", "value": 96.0, "max": 100.0 }],
  "media": {
    "file_name": "Breaking Bad S01E01.mkv",
    "size": 1234567890,
    "container": "mkv",
    "duration_ms": 3480000,
    "bitrate": 5000000,
    "video": { "codec": "hevc", "profile": "Main 10", "width": 1920, "height": 1080, "bit_depth": 10,
               "hdr": "hdr10", "frame_rate": 23.976 },
    "audio": [{ "index": 1, "codec": "eac3", "channels": 6, "language": "eng", "title": "Surround",
                "default": true }],
    "subtitles": [{ "index": 3, "codec": "subrip", "language": "eng", "title": null, "default": false,
                    "forced": false, "external": false, "text": true }]
  }
}
```

`media` is `null` for series and seasons. `video` may be `null` (audio-only file). `hdr` is
`null`, `hdr10`, `hlg` or `dolby_vision`. Subtitles with `text: false` (PGS, VobSub, DVD) are
bitmaps: they are shown by burning them into the picture (`burn_subtitle` in a playback request).
External subtitle files get indices from 1000, DVD subtitles the disc declares but that were not
met while probing from 2000, downloaded subtitles from 1 000 000 (`source: "opensubtitles"`,
`null` for the others). `chapters` is `[{ "start_ms": 0, "title": "…" | null }, …]`. Streams may
carry `stream_id` (the container's id, e.g. `0x80` on a DVD). `file_name` of a DVD is its folder
or image.

`ratings` sources: `tmdb`, `imdb`, `rotten_tomatoes`, `metacritic`, `anilist`, `anidb`.

### `GET /api/v1/items/{id}/children`

Seasons of a series, episodes of a season, or extras of a movie, in order: `[ItemSummary, …]`.

### `GET /api/v1/items/{id}/images/{kind}`

`kind`: `poster`, `backdrop`, `thumb`, `logo`. JPEG (logos: PNG with transparency); `404` when absent. Cached privately for a year
(URLs are versioned).

### `POST /api/v1/items/{id}/refresh` (admin)

Re-reads local metadata and fetches remote metadata again for this item (and its children for a
series or season). `202`.

## User data

| Endpoint | Effect |
|---|---|
| `POST /api/v1/items/{id}/progress` `{"position_ms": 61000}` | Saves the position. Positions within the last 10 % (or under 5 % of the runtime) mark the item played / reset. `204` |
| `POST /api/v1/items/{id}/played` | Marks played (a series or season: every episode). `204` |
| `DELETE /api/v1/items/{id}/played` | Marks unplayed. `204` |
| `POST /api/v1/items/{id}/favorite` / `DELETE …` | Favorite on / off. `204` |

### `GET /api/v1/home`

```json
{
  "continue_watching": [ItemSummary, …],
  "next_up": [ItemSummary, …],
  "latest": [{ "library": { "id": "movies", "name": "Movies", "kind": "movies" }, "items": [ItemSummary, …] }]
}
```

- `continue_watching`: movies and episodes with a saved position, not played, most recent first
  (at most 20).
- `next_up`: for each series watched in the last 90 days, the next unplayed episode after the
  last played one (at most 20).
- `latest`: per library, the 20 most recently added movies or series (series ordered by their most
  recent episode).

### `GET /api/v1/search?q=…&limit=…`

Movies, series and episodes whose title contains `q` (at least 2 characters): `[ItemSummary, …]`.

## Playback

### `POST /api/v1/items/{id}/playback`

The client says what it can play; the server picks the cheapest method.

```json
{
  "start_ms": 0,
  "audio_index": null,
  "max_height": null,
  "max_bitrate": null,
  "force_transcode": false,
  "burn_subtitle": null,
  "capabilities": {
    "containers": ["mp4", "webm"],
    "video_codecs": ["h264", "hevc", "vp9", "av1"],
    "audio_codecs": ["aac", "mp3", "opus", "flac"]
  }
}
```

- `audio_index`: a `media.audio[].index`; default: the default track.
- `max_height`, `max_bitrate` (bits/s): limits; exceeding them forces a transcode.
- `burn_subtitle`: a `media.subtitles[].index` with `text: false`, drawn into the picture (forces a
  transcode; the answer's `burned_subtitle` says so). `400` for a text subtitle (those come as
  WebVTT).

Answer:

```json
{
  "method": "transcode",
  "url": "/api/v1/hls/4f1c…/index.m3u8",
  "session_id": "4f1c…",
  "start_ms": 600000,
  "duration_ms": 3480000,
  "hw_accel": "vaapi",
  "tone_mapping": false,
  "burned_subtitle": null,
  "reasons": ["video codec mpeg4 is not supported by the client"]
}
```

- `method`:
  - `direct`: `url` is `/api/v1/items/{id}/stream`, the original file with HTTP range support.
    `start_ms` is `0`; the client seeks itself.
  - `remux`: HLS (fMP4); the video is copied, the audio copied or converted to AAC.
  - `transcode`: HLS (fMP4); the video is re-encoded to H.264 (hardware when available).
- For HLS, **stream time 0 is item time `start_ms`**: the stream starts where asked. The playlist
  is an EVENT playlist that grows as segments are produced (ffmpeg runs faster than real time);
  `#EXT-X-ENDLIST` marks the end. To seek outside the produced range, the client requests a new
  playback with the new `start_ms` (and deletes the old session).
- `hw_accel`: `vaapi`, `qsv`, `nvenc` or `null` (software, or no video encoding).
- `tone_mapping`: `true` when an HDR source (HDR10, HLG, Dolby Vision) is transcoded to SDR with
  tone mapping (`transcoding.tone_mapping`, on by default). A remux keeps HDR as it is.

### `GET /api/v1/items/{id}/stream`

The original file (`Range` supported). For direct play.

### `GET /api/v1/hls/{session}/{file}`

`index.m3u8`, `init.mp4`, `seg00000.m4s`… Only the user who created the session may read it.
The playlist request waits (up to 30 s) for ffmpeg to write it.

### `DELETE /api/v1/hls/{session}`

Stops the transcode and removes its files. `204`. Sessions not read for 60 s are stopped anyway.

ffmpeg pauses when it is 3 minutes ahead of the last segment requested, and segments more than
10 minutes behind it are deleted: a request for one answers `404`, and the client should start a
new playback at that position (as for a seek outside the produced range).

### `GET /api/v1/items/{id}/subtitles/{index}[.vtt]?start_ms=0`

A text subtitle stream (embedded, external or downloaded), converted to WebVTT. With `start_ms`,
cues are shifted so that time 0 is `start_ms` (matches an HLS stream started there).

### `GET /api/v1/items/{id}/subtitles/search?language=fr,en`

Searches OpenSubtitles.com for a movie or episode (by file hash and provider ids). `language`:
ISO 639-1 codes; default the configured `subtitles.languages`, else `en`. Hash matches first.
`503` when no subtitle provider is configured, `502` when it does not answer.

```json
{ "results": [{ "file_id": 1954081, "language": "fr", "release": "Sintel.2010.1080p",
                "file_name": "sintel.fr.srt", "hearing_impaired": false, "foreign_parts_only": false,
                "ai_translated": false, "machine_translated": false, "from_trusted": true,
                "downloads": 1130, "rating": null, "hash_match": true }] }
```

### `POST /api/v1/items/{id}/subtitles`

Downloads a search result and stores it: send back `file_id`, `language` and optionally `release`,
`hearing_impaired`, `foreign_parts_only`, `hash_match`. `201` with the new subtitle stream (as in
`media.subtitles`); the same file twice gives the stored one. `429` when the download quota is used up.

### `DELETE /api/v1/items/{id}/subtitles/{index}` (admin)

Removes a downloaded subtitle (`index` ≥ 1 000 000). `204`.

## Administration

### `GET /api/v1/system` (admin)

```json
{
  "version": "0.1.0",
  "ffmpeg": { "version": "8.1.2", "hw_accel": "vaapi", "device": "/dev/dri/renderD128",
              "encoders": ["h264_vaapi", "hevc_vaapi", "libx264"] },
  "metadata": { "tmdb": true },
  "transcodes": [{ "session_id": "4f1c…", "user": "alice", "item_id": 12, "title": "Pilot",
                   "method": "transcode", "hw_accel": "vaapi", "started_at": 1761200000000 }],
  "libraries": [{ "id": "movies", "scan": { … } }]
}
```

### `GET /api/v1/config` (admin)

The running configuration, described: every setting with its value, its default, a description and,
for settings with a fixed set of values, the alternatives. Secrets are never returned: they are
reported as `set` or not, and the database URL loses its password.

```json
{
  "sections": [
    {
      "key": "transcoding",
      "title": "Transcoding",
      "description": "ffmpeg, hardware acceleration and the limits of HLS playback sessions.",
      "applies": "restart",
      "fields": [
        { "key": "hw_accel", "kind": "choice", "value": "auto", "default": "auto",
          "description": "Hardware acceleration for transcodes. …",
          "choices": [{ "value": "auto", "description": "Try VA-API, then Quick Sync, then NVENC; …" },
                      { "value": "none", "description": "Software only (libx264)." }] },
        { "key": "max_sessions", "kind": "integer", "value": 8, "default": 8, "description": "…" }
      ]
    },
    {
      "key": "auth", "title": "Sign-in", "description": "…", "applies": "reload",
      "fields": [
        { "key": "oidc.client_secret", "kind": "secret", "value": null, "default": null, "set": true,
          "description": "…" }
      ]
    },
    {
      "key": "libraries", "title": "Libraries", "description": "…", "applies": "reload", "fields": [],
      "items": [{ "title": "Movies", "fields": [{ "key": "id", "kind": "string", "value": "movies", … }] }]
    }
  ]
}
```

- Sections, in order: `server`, `database`, `auth`, `transcoding`, `metadata`, `scanner`, `libraries`.
- `applies`: when a change to the section takes effect, `reload` or `restart` (a reload reports
  restart-only changes and keeps the running values meanwhile). `server.metrics_token` applies at
  a reload although its section says `restart`.
- `kind`: `string`, `integer`, `boolean`, `duration` (e.g. `"6h"`), `path`, `url`, `address`,
  `list`, `choice` (see `choices`), `secret` (see `set`).
- `key`: the path within the section; nested settings are dotted (`oidc.issuer`, `tmdb.language`).
- `value`: the running value, `null` when unset (or for secrets). `default`: the value when the
  setting is absent, `null` when it has none (required, or simply off).
- `libraries` lists one `items` entry per library.

### `POST /api/v1/reload` (admin)

Reloads the configuration files, like `SIGHUP`. An invalid configuration is refused with `422`
and the error text; the running configuration is kept.

```json
{ "applied": ["libraries", "auth"], "restart_required": ["transcoding"], "scans_queued": ["kids"] }
```

`applied`: sections whose changes now apply (`auth`, `metadata`, `scanner`, `libraries`,
`server.metrics_token`). `restart_required`: sections that changed but keep their running values
until a restart (`server`, `database`, `transcoding`). `scans_queued`: libraries that are new or
whose paths changed.

## Probes and metrics

- `GET /healthz`: `200 ok` while the process runs.
- `GET /readyz`: `200` when the database answers, `503` otherwise.
- `GET /metrics`: OpenMetrics text: active HLS sessions by method and acceleration, playbacks
  started by method, scans by library and result, movies or series per library.
