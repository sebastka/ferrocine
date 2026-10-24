# Design decisions

Decisions taken while building Ferrocine without asking, with their rationale.
Each can be revisited; the "Revisit when" notes say what would change the
answer. Entries marked ⚠️ are the ones most worth reviewing together.

## Scope and architecture

### D1. Two projects: a Rust core and a TypeScript UI ⚠️
- **Core** (this repository): one Rust process with the JSON API, the OpenID
  Connect sign-in, the library scanner, metadata, artwork and transcoding.
- **UI** ([`ferrocine-ui`](https://github.com/sebastka/ferrocine-ui), a separate
  repository, as asked): a React + Vite single-page app, shipped as an nginx image.
- **Why TypeScript for the UI, not Rust/WebAssembly.** The player is mostly
  glue around browser media APIs: Media Source Extensions, `hls.js`,
  `canPlayType`, text tracks, fullscreen. A Rust UI (Leptos, Dioxus, Yew) would
  still call all of them through JavaScript bindings, with slower iteration
  and a larger bundle.
- **The contract** is [`docs/api.md`](docs/api.md); the UI keeps a copy and
  types written from it.
- Desktop and phone clients are out of scope for now (D10).

### D2. One origin: the UI proxies the core
- **Choice.** The UI image serves the app and reverse-proxies `/api/`, `/auth/`,
  `/healthz` and `/readyz` to the core. The browser sees one origin.
- **Why.** The session is an `HttpOnly`, `SameSite=Lax` cookie (D11). With one
  origin there is no CORS, no third-party cookie, and the strict Content
  Security Policy (`default-src 'self'`) holds. `<video>` and `<img>` elements
  send the cookie by themselves; no token is ever handed to JavaScript.
- **Alternative kept open.** The core serves a built UI itself when
  `server.web_dir` is set, with the same security headers. That suits
  single-container setups and development.

### D3. Configuration as code, state and textual metadata in SQL, files in a directory ⚠️
- **YAML** holds everything that is configuration: server, database,
  sign-in, transcoding, metadata providers, **libraries** and who may see them.
  There is no UI or API to change any of it.
- **The loader.** The path is a file or a directory of `*.yaml` files merged in
  name order (hidden files, such as Kubernetes' `..data`, skipped); a section
  may appear in one file only, `libraries` lists are concatenated. `${VAR}`,
  `${VAR:-default}`, `${file:/path}` (Docker and Kubernetes secrets) and `$$`
  are substituted in parsed values, never in raw text, so a secret cannot
  change the document's structure. YAML 1.2 booleans only (`no` stays a
  string). Unknown keys are errors with their path, and validation lists every
  error at once.
- **The database** holds state and textual metadata: items, their metadata
  and technical description, users (created at their first sign-in) and
  their avatars, sessions, watch progress, favorites, scan state. Artwork,
  seek bar previews and downloaded subtitles are files of the metadata
  directory (D5), which rows name by tag.
- **Libraries** are mirrored into a table so that items can reference them. A
  library removed from the configuration is flagged, and its items and their
  watch history are kept until it comes back. Purging them is a manual
  `DELETE` for now.
- **Reload** with `SIGHUP` (`docker kill -s HUP ferrocine`) or
  `POST /api/v1/reload` (administrators). An invalid configuration is refused
  (422, with the errors) and the running one kept. What applies at once:
  libraries (new ones and ones whose paths changed are scanned), access lists,
  sign-in rules and provider (a new provider is discovered; existing sessions
  stay valid, with the groups they signed in with), metadata, scanner and
  session settings, `server.metrics_token`. What waits for a restart, keeping
  its running values meanwhile: the rest of `server` (listener, public URL),
  `database` (the pool) and `transcoding` (running ffmpeg processes, the
  detected acceleration). The answer and the log say which sections are which.
- **No file watching.** A Kubernetes ConfigMap update needs a reload (or a
  reloader sidecar).
- **The running configuration is visible** to administrators
  (`GET /api/v1/config`, the UI's Administration page): every setting with its
  value, default, description and, for discrete settings (`hw_accel`,
  `x264_preset`, a library's `kind`, the database engine), the alternatives,
  each described. It is built field by field from an **allow-list**, so a
  secret field added later is not returned by accident: secrets (client
  secret, metrics token, TMDB key) are reported as set or not, and the
  database URL loses its password and any credential-looking query
  parameter. A test serializes a configuration with every setting and fails
  when one has no description; another feeds secrets everywhere and checks
  none comes out. The descriptions live in the server, next to the schema,
  so the UI needs no copy of them.

### D4. PostgreSQL and MariaDB only, through sqlx's `Any` driver
As asked; no SQLite.

- **One SQL text for both engines.** Queries are written once with `?`
  placeholders, rewritten to `$n` for PostgreSQL. Only upserts
  (`ON CONFLICT` / `ON DUPLICATE KEY UPDATE`) and advisory locks
  (`pg_try_advisory_lock` / `GET_LOCK`) are written per dialect.
- **Portable column types.** Timestamps are BIGINT milliseconds since the
  epoch and booleans SMALLINT: `Any` supports few types, and the two engines
  disagree on dates and booleans.
- **Two migration trees,** `migrations/postgres` and `migrations/mysql`, kept
  in sync by hand.

- **No floating point columns.** The `Any` driver sent a 64-bit float that
  PostgreSQL refused ("incorrect binary data format"). Ratings are stored as
  hundredths in an INTEGER (`rating_x100`).
- **No `CHAR(n)`.** `Any` cannot decode PostgreSQL's `bpchar`; fixed-length
  hashes are `VARCHAR(64)`.
- **Hashed path keys.** Items are unique by `path_key`, the SHA-256 of library,
  kind and path: full paths exceed MariaDB's index length limit.
- **JSON in TEXT.** Genres, studios, people and the technical media
  description are JSON text; they are read with the item, never queried
  inside, except a `LIKE` for the genre filter.
- **Our own migration lock.** sqlx's migration lock did not stop two
  processes from migrating MariaDB at once in the tests (duplicate entry, then
  "migration partially applied"). Migrations now run under a named advisory
  lock, so instances starting together queue.
- **MariaDB is the tested target;** `mysql://` URLs work but MySQL itself is
  not tested.

### D5. Read-only libraries; binary metadata in a directory, the rest in SQL ⚠️
- **Nothing is ever written next to the media.** Libraries can be mounted
  read-only, and the image's user could not write there anyway. `.nfo` files,
  posters and subtitles found next to the media are read (D8).
- **The metadata directory** (`metadata.directory`, a volume) holds artwork,
  seek bar previews and downloaded subtitles. Artwork first went to the
  database, as the one thing to back up; seek bar previews (2–6 MB per film)
  made that too heavy, so binary metadata moved out. What grids, sorting and
  search query (titles, overviews, people, ratings, search text) stays in the
  database, as does all state; user avatars count as state.
- **Content-addressed files.** An image or subtitle is named by the first 16
  hex digits of its SHA-256 (the tag the rows and URLs carry), under a
  two-digit fan-out, so a file is stored once however many items use it; a
  preview set is a directory named by a tag of the file it shows and the
  settings. Files are written to a temporary name and renamed, so readers
  never see half a file. Files no row names are removed hourly once an hour
  old, a grace period for instances sharing the directory.
- **Sharing.** Instances sharing a database must share the directory (a
  ReadWriteMany volume, NFS…). Changing `metadata.directory` needs a restart
  and moves nothing: the next scans fetch artwork again.
- **Backups.** The database is the state; the directory can be rebuilt by
  scanning (artwork is fetched or extracted again, previews made again),
  except downloaded subtitles, which would cost the provider's quota: back
  both up, or accept losing those.
- **Migration.** Artwork stored in the database by earlier versions is
  dropped by a migration, which also makes every item due a provider
  lookup: the next scan downloads remote artwork again, reads local files,
  extracts frames (textual metadata is kept, providers only fill gaps);
  downloaded subtitles move to files at the first start.
- **Sizes.** Images are resized (posters and thumbnails 600 px wide, backdrops
  1200 px) and re-encoded as JPEG (quality 85), about 30–150 KB each, some
  300 KB per film; previews add 2–3 MB per film at 240 px.
- **Ephemeral files** besides: HLS segments of running playbacks
  (`transcoding.temp_dir`, D14), wiped at start.

### D6. Items: movies, and series → seasons → episodes; identity by path
- **Kinds.** Movie libraries hold movies. Show libraries hold series, their
  seasons (with season 0 for specials) and episodes. Music, books and photos
  are not supported.
- **Identity.** An item is keyed by library, kind and file path (directory for
  a series). Its numeric id survives rescans, metadata refreshes and file
  changes in place (new size or date: re-probed, same id).
- **Cost.** Moving or renaming a file creates a new item and loses its watch
  progress.

*Revisit when* that becomes annoying: a content fingerprint (size plus hashes
of a few blocks) could carry user data over to the moved file.

## Libraries and metadata

### D7. Naming conventions of Jellyfin, Kodi and Plex
The scanner reads the layouts people already have:

- **Movies:** `Title (Year)/Title (Year).mkv`, release names such as
  `Title.Year.1080p.BluRay.x264.mkv` (the title ends at the year, else at the
  first release-name token), and provider hints `{tmdb-603}`,
  `[imdbid-tt0133093]`. The directory name is used when it holds only that
  movie and carries a year or an id.
- **Shows:** a series per top-level directory (`Show (Year)`), episodes as
  `S01E02`, `s1e2`, `1x02`, or `02 - Title` inside a `Season N` directory;
  `Specials` is season 0; multi-episode files count as their first episode.
  Episodes at the library root are grouped by the name before `SxxEyy`.
- **Skipped:** hidden files, `Samples` directories and `-sample` files, NAS
  metadata directories (`@eaDir`, `#recycle`), and what `.ignore` files hide, read as
  Jellyfin reads them (current releases): the nearest one decides; empty, it
  hides its directory; otherwise gitignore patterns, case-insensitive, via
  ripgrep's `ignore` crate. Patterns are relative to the `.ignore`'s
  directory (Jellyfin matches the absolute path, which only differs for
  patterns naming a directory above it), and the search stops at the
  library path.
- **DVDs** (folders and images) are read through their information files,
  parsed here (a few hundred lines, no `libdvdread`: Jellyfin's ffmpeg has
  neither it nor the `dvdvideo` demuxer). A DVD's timestamps restart along a
  title: on a real disc ffprobe saw 49 minutes of a 113-minute film, and
  `-ss` found nothing at some times, with the concat protocol and with
  Jellyfin's concat demuxer alike. The disc's title table names the titles;
  each is a list of cells (sector runs with play times), read as exactly
  those bytes: `subfile` ranges of the VOBs, or of the image, joined by
  `concat:`. Duration, chapters and track languages come from the IFO;
  playback starts at the bytes of the cell playing at the wanted time, by
  starting the input there (`-skip_initial_bytes` does not pass through
  `subfile`); seeking anywhere took about 0.1 s on two real discs. Streams
  of a DVD are mapped by MPEG stream id, since their order depends on where
  reading starts. Cells under two seconds at either end of a title are
  left out: on a real disc they were warnings whose nominal half second
  hid 50 MB of stills each, inside the film's byte range.
- **A DVD's titles:** the longest is the movie; titles of a minute or more,
  with sound, not replaying the movie's cells (scene selections) are its
  extras; in a show library, titles of five minutes or more are episodes,
  without "play all" titles (those replaying two others), numbered on in
  disc order within the season. An item's path is `<disc>#<title>`.
  Images are read through their ISO 9660 side (DVD-Video discs are UDF
  bridge discs); Blu-ray images, UDF only, are not. Without `VIDEO_TS.IFO`,
  the title set with the most data is the movie. The Blu-ray feature is the
  largest `BDMV/STREAM` file.
- **Extras** are items of their own (kind `extra`, under their movie):
  DVD titles, files of Jellyfin's extras folders and `-trailer`-style files.
  They are listed by the movie's children and play like anything else.
- **Sidecars:** `Movie.en.srt`, `Movie.fre.forced.srt` (language, forced flag,
  label); `poster`, `folder`, `cover`, `fanart`, `backdrop`, `<name>-poster`,
  `<name>-thumb`, `seasonNN-poster`.

Parsing is unit-tested on real-world names; the end-to-end test scans
generated libraries.

### D8. Metadata precedence: `.nfo`, then providers in a fixed order, then the file name
- **Local `.nfo` files** (Kodi format: `movie.nfo`, `<name>.nfo`,
  `tvshow.nfo`) win: they are what the owner curated.
- **Providers** (each optional) fill what is missing, the first one to know
  a field winning; a title from a file name is replaced, one from an `.nfo`
  is kept. The order is fixed per library rather than configurable, so the
  configuration stays a list of keys:
  - movies: AniList (anime libraries), TMDB;
  - series: AniList then AniDB (anime libraries), TheTVDB (when
    `episode_order` is `dvd` or `absolute`), TMDB, TheTVDB;
  - seasons and episodes: AniDB (anime), TheTVDB in the library's order,
    TMDB, TheTVDB in aired order;
  - then, once the ids are known, OMDb adds IMDb, Rotten Tomatoes and
    Metacritic ratings and the certification, and fanart.tv adds logos and
    textless backdrops (preferred) and posters (after the others).
- **Original titles** are shown instead of the localized ones for works
  whose original language is in `metadata.prefer_original_title` (`"*"`:
  all), overridable per library. The provider's localized title, the
  original title and its language (TMDB, TheTVDB, AniList's country, AniDB)
  are all stored, so a change applies on reload to every item without
  fetching metadata again; `.nfo` titles are never switched.
- **Ratings** are kept per source (`tmdb`, `imdb`, `rotten_tomatoes`,
  `metacritic`, `anilist`, `anidb`) with their scale; the single `rating`
  out of 10 is the first provider's.
- **Lookups** use ids first (TMDB, IMDb, TVDB, AniDB, AniList, from the
  name or the `.nfo`), then a search by title and year. TMDB's movie search
  keeps films with any release in the year asked for, but dates each by its
  main release, which moves (Hamilton, premiered 2020, is dated by its 2025
  re-release): its results are trusted, not filtered again on that date.
  Candidates rank by exact title (or original title, folded as in search),
  then a date within a year, then TMDB's order; for movies, the first three
  are checked against the file's duration (TMDB's runtime within 30 % or
  20 minutes, so extended cuts still fit) and the first that fits wins, the
  first candidate otherwise. Series search filters by first air date itself,
  so its year check stays. AniDB, which forbids
  search requests, is matched against its daily title dump (cached for a
  day) by exact normalized title. Unmatched items are tried again after
  `scanner.retry_unmatched` (7 days); an item whose providers all failed is
  tried at the next scan.
- **Rate limits:** AniDB bans clients sending more than one request every
  2 seconds, so requests are paced to 2.5 s and anime answers cached for a
  day; TheTVDB tokens are cached for a week; at most 8 lookups run at once.
- **Artwork:** local files, then the providers' (in the order above), then a
  frame extracted with ffmpeg at 10 % of the video (for movies and episodes
  without any image). Logos are stored as PNG with transparency.
- **The TMDB key** may be a v4 read access token (sent as a bearer token) or
  a v3 key. Image downloads carry no credential.
- **Search** matches titles (shown, original, localized) and cast and crew
  names. They are kept per item as one folded text (lower case, accents
  removed with Unicode decomposition, punctuation as spaces), filled when an
  item is written and, for older items, at startup; every word of a query
  must appear (`LIKE '%word%'`, the same on both databases). A scan for a
  substring across a few thousand items is fast enough without a full-text
  index, which PostgreSQL and MariaDB would each do differently. Search
  results say why they matched when it was not the shown title.
- **Refresh.** `POST /items/{id}/refresh` (admin) clears the item's (and its
  children's) metadata state; the next scan rebuilds them from scratch. A
  library scan with `refresh_metadata=true` does the same for every item.

### D9. Incremental scans that never delete by mistake
- **Incremental.** A file whose size and modification time did not change is
  not probed again. `.nfo` and sidecar subtitle files are fingerprinted, so
  editing one updates the item. Artwork is reprocessed only when its source
  changes.
- **Safe deletion.** Items not seen by a scan are deleted only when the walk
  was complete. An unreadable directory, or a missing library root (an
  unmounted share), keeps everything, with a warning.
- **When.** At start, every `scanner.interval` (6 h), and on demand
  (`POST /libraries/{id}/scan`, admin). Requests queue and merge; one library
  is scanned at a time.
- **Several instances.** A scan takes a per-library advisory lock in the
  database, so instances sharing it do not scan the same library together.
- **No file watching.** inotify does not work on network shares, which is
  where large libraries live, and periodic incremental scans are cheap.

*Revisit when* new files must appear within seconds: a watcher on local
filesystems could trigger the existing scan.

## Sign-in and access

### D10. OpenID Connect, with Ferrocine as the relying party ⚠️
- **Flow.** Authorization code with PKCE (S256), `state` and `nonce`, done by
  the Rust core (a backend for the frontend): the browser never holds a token.
  `/auth/login` stores the sign-in in the database and binds it to the
  browser with a short-lived cookie (login CSRF); `/auth/callback` checks the
  cookie, uses the sign-in once, exchanges the code (`client_secret_basic`,
  or `client_secret_post` when the provider only supports that, or a public
  client) and verifies the ID token.
- **ID token checks,** with `ring` (already the TLS crypto provider) and the
  provider's JWKS: asymmetric algorithms only (never `none` or HMAC), the key's
  algorithm matching,
  issuer, audience (and `azp` with several audiences), expiry, issue time,
  nonce. Keys are refreshed hourly and on an unknown key id (at most every
  30 s). `openidconnect` and `jsonwebtoken` were avoided: both pull the `rsa`
  crate, which has an open timing advisory that cargo-deny flags.
- **Userinfo** completes the ID token's claims (it wins on conflicts): many
  providers put groups there only.
- **Who may sign in:** `auth.oidc.users`, values of `groups_claim` (empty =
  anyone the provider accepts); `admins` grants administration. Groups are
  read at sign-in and stored; a change at the provider applies at the next
  sign-in.
- **The user's picture** (the `picture` claim) is fetched at sign-in when it
  changed, cropped to a 192×192 square and stored as JPEG in `users`, then
  served by `/api/v1/me/avatar`: the UI's Content Security Policy allows no
  other origin, and the provider is not contacted on every page view. As
  users may be able to set that URL at the provider, only https is fetched
  (http on localhost), with a 5 s timeout, a 2 MB cap and three redirects at
  most, and only a decoded and re-encoded image is kept. A failure never
  blocks the sign-in; without a picture the UI shows initials.
- **Desktop and phone clients** will need bearer tokens. The verifier already
  exists; accepting access tokens on `/api/v1` is a small addition once a
  client exists.

### D11. Sessions in the database
- **Choice.** The cookie holds 32 random bytes; the `sessions` table holds
  their SHA-256, the user and the ID token (for the provider's logout). A
  database leak does not hand out sessions.
- **Why not in memory:** a restart would sign everyone out, and instances
  could not share sessions. **Why not an encrypted cookie:** the ID token kept
  for the provider's logout makes it large, and sessions could not be revoked.
- **Lifetime.** `session_idle` (14 days unused) and `session_lifetime` (90
  days). Lookups are cached for a minute per process, which spares the
  database on segment requests; a sign-out on another instance takes up to a
  minute to apply there.
- **Cookies** are `HttpOnly`, `SameSite=Lax`, and `Secure` with the
  `__Host-` prefix when `public_url` is https.
- **CSRF.** Unsafe requests must be same-origin: `Sec-Fetch-Site:
  same-origin`, or an `Origin` equal to `public_url`. Sign-out is a POST.
- **Sign-out** deletes the session, then sends the browser to the provider's
  `end_session_endpoint` with `id_token_hint`.

### D12. Per-library access lists
- **Choice.** `libraries[].access` lists the groups that may see a library;
  empty means everyone; administrators see every library. Items of a hidden
  library answer 404 (not 403), so their existence is not revealed.
- **Why.** "The kids see Kids, grown-ups see everything" is the common need,
  and it stays configuration as code.
- **Not done:** per-user parental ratings (certification limits).

## Playback

### D13. The client says what it plays; the server picks the cheapest method
- **Request.** `POST /items/{id}/playback` carries the client's containers,
  video and audio codecs (probed in the browser), an optional audio track and
  height or bitrate limits.
- **Direct play:** container, video and audio playable, within the limits,
  and the first audio track wanted (browsers play the first one). The file is
  served with range requests.
- **Remux:** the video is playable but the container is not (MKV), or another
  audio track is wanted. The video is copied into fMP4 HLS; the audio is
  copied when the client plays it, else converted to AAC stereo.
- **Transcode:** anything else, to H.264 High 8-bit, scaled to the limits.
  10-bit H.264 (High 10) always transcodes: no browser decodes it.
- **The answer** lists the reasons, which the player shows.
- **HDR is tone-mapped when transcoded** (D16); a remux keeps it as it is,
  for clients that play HEVC.

### D14. HLS sessions that start where the user is ⚠️
- **Choice.** A session runs ffmpeg from the requested position (`-ss`) and
  writes an fMP4 HLS **EVENT** playlist that grows as segments are produced.
  Stream time 0 is item time `start_ms`. Seeking inside what is produced
  stays in the stream; seeking elsewhere starts a new session there. The
  player draws the whole item's timeline and offsets the times.
- **Why.** It works the same for remuxes and transcodes. A complete VOD
  playlist built in advance (Jellyfin's approach) needs segments cut exactly
  where the playlist says; for a remux (video copied) that means indexing the
  keyframes of every file first, which is slow on network shares.
- **Cost.** A seek outside the produced range takes a second or two (a new
  ffmpeg). With video copied, the stream starts at the keyframe before the
  position, so it can start a few seconds early.
- **Bounded resources.**
  - ffmpeg is paused (SIGSTOP) when it is 3 minutes ahead of the last segment
    requested, and resumed when the player catches up, so a paused film does
    not fill the disk. Signals are sent with `rustix`, which keeps the crate
    free of `unsafe`.
  - Segments more than 10 minutes behind the player are deleted; a seek back
    there gets a 404, and the UI starts a new session.
  - Sessions unread for 60 s are stopped; at most `max_sessions` (8) run;
    the scratch directory is wiped at start.
  - Only the user who created a session can read it.
- **One replica.** Sessions live in the process; several replicas would need
  sticky routing by session id.

*Revisit when* seeking latency matters (keyframe-indexed VOD playlists for
transcodes only), or when the core must scale out.

### D15. Hardware acceleration: one image, proven at start, with fallbacks per session
- **Supported:** VA-API (Intel, AMD), Quick Sync (Intel), NVENC (NVIDIA), all
  in the one image (D18). `hw_accel: auto` tries them in that order; each must
  encode a few test frames on the configured device, since an encoder listed
  by ffmpeg may lack a driver or a GPU.
- **Three pipelines per transcode, tried in order until the first segment
  appears:** full hardware (decode, scale, encode on the GPU); software
  decode and scale with hardware encode (for codecs the GPU cannot decode);
  software (libx264).
- **Tested on zeus** (AMD Radeon RX 6900 XT, Jellyfin's ffmpeg 8.1.3, in the container, as
  user 65532, read-only, no capability): 4K HEVC 10-bit HDR10 to 1080p H.264
  runs fully on the GPU at 7.4× real time (decoding is the limit); MPEG-2
  episodes too; an MPEG-4 part 2 file, which the GPU cannot decode, fell back
  to the hybrid pipeline within 0.1 s. Output checked with ffprobe: H.264
  High, 1920×1080, yuv420p, AAC stereo.
- **Implemented, not run on real hardware:** Quick Sync and NVENC (no such GPU
  here). Their command lines are unit-tested, and the image's ffmpeg accepts
  them: without the hardware they fail at device initialisation (no CUDA
  library, not an Intel GPU), and the session falls back to software.
- **ffmpeg runs as a subprocess,** not linked: the binary stays static, a
  crashing decoder cannot take the server down, and accelerators come with
  the image's ffmpeg build. Probing uses `ffprobe` the same way.

### D16. HDR tone mapping: on by default, on the CPU at the output size
- **Choice.** HDR sources (HDR10, HLG, Dolby Vision) that are transcoded are
  tone-mapped to SDR BT.709 (`transcoding.tone_mapping`, on by default).
  Without it, the H.264 output carries PQ or HLG values as if they were SDR
  and looks washed out, so the opt-out is for the odd GPU or CPU where it
  fails or is too slow, not a preference.
- **How.** Jellyfin's `tonemapx` (SIMD, BT.2390 curve, applies Dolby Vision
  metadata) on frames already scaled to the output size. With VA-API, the GPU
  decodes and scales to 10-bit (`scale_vaapi=…:format=p010`), the 1080p frames
  are downloaded, tone-mapped and uploaded for the GPU encoder. With a stock
  ffmpeg (a custom `transcoding.ffmpeg`), the upstream chain `zscale` +
  `tonemap` (Hable curve: upstream has no BT.2390) is used; with neither,
  HDR is transcoded without tone mapping and the start-up log says so.
- **Measured on zeus** (4K HEVC 10-bit HDR10 → 1080p, AMD RX 6900 XT): about
  5× real time with VA-API, about 7× fully in software on 32 cores; the output
  is tagged BT.709. The test media are synthetic, so the picture quality on
  real HDR films has not been judged yet.
- **Why not on the GPU.** libplacebo through Vulkan is the GPU route for AMD
  (VA-API frames mapped to Vulkan). On this GPU and Mesa it failed to import
  10-bit frames (`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`) or
  lost the device, twice resetting the GPU: not something to ship. OpenCL
  needs a runtime the image does not carry. `tonemap_vaapi` (Intel),
  `vpp_qsv` and `tonemap_cuda` exist in the image's ffmpeg but could not be
  tested here; Quick Sync and NVENC sessions that tone-map therefore start at
  the hybrid pipeline (CPU decoding and tone mapping, GPU encoding).

*Revisit when* Intel or NVIDIA hardware is at hand: their GPU tone mappers
would spare the CPU on 4K sources.

### D16b. Seek bar previews ("trickplay")
- **What.** A frame every 10 s (`metadata.trickplay.interval`), 240 px wide,
  tiled 10 × 10 into JPEG sheets: hovering the seek bar shows the frame of
  that time, with the chapter. Item details describe the set (frame size,
  grid, count, sheet URLs, which carry the set's tag and are cached as
  immutable).
- **How.** A background pass after scans (and hourly) makes the previews
  items lack, one item at a time, so it never competes with itself.
  ffmpeg decodes keyframes only (`-skip_frame nokey`): a frame is the last
  keyframe before its time, and a 4K film takes seconds to a few minutes.
  Square pixels first (anamorphic DVDs), HDR tone-mapped like transcodes.
  DVD titles are read through their byte ranges (D7). A set is named by the
  file's identity and the settings: an unchanged file is never read twice; a
  failure is not retried before a restart.
- **Where.** In the metadata directory (D5): 2–3 MB per film, too much for
  the database.

### D17. Subtitles as WebVTT, converted on demand
- **Text subtitles** (SubRip, ASS/SSA, mov_text, WebVTT; embedded or
  sidecar) are converted by ffmpeg once, cached in memory (32 tracks), and
  shifted in Rust for streams that start later, instead of having ffmpeg read
  the file again.
- **Bitmap subtitles** (PGS, VobSub, DVD) are burned into the picture when
  the client asks (`burn_subtitle`): an `overlay` before scaling, on
  software frames (such sessions start at the hybrid pipeline: software
  decoding, GPU encoding), with the disc's palette for DVD subtitles.
  Subtitles a DVD declares but ffprobe did not meet yet are listed from the
  IFO, and a burned-in DVD session reads further ahead (200 MB, 10 minutes)
  so that the stream exists when ffmpeg binds it.
- **Chapters** come from ffprobe (`-show_chapters`), or from a DVD's IFO.
- **Downloaded subtitles** come from OpenSubtitles.com, the largest source
  with a documented API. They are stored in the database as WebVTT
  (libraries stay read-only, D5) and appear as streams with indices from
  1 000 000. Users search and download from the player; administrators
  remove them. After each scan, movies and episodes without a text subtitle
  in a `subtitles.languages` language (and whose first audio track is not in
  it) get the best match: by file hash first, then human translations, the
  hearing-impaired preference, trusted uploaders, downloads. Each search is
  recorded, so a miss is retried only after `scanner.retry_unmatched`; at
  most `max_downloads_per_scan` downloads run per scan, and a quota error
  stops downloads until the announced reset (or an hour).
- **An account is required:** the API only downloads for signed-in users,
  so `username` and `password` are mandatory with the API key.

## Build, image and operations

### D18. Image: built on `dhi.io/rust`, running on hardened Debian trixie with Jellyfin's ffmpeg ⚠️
- **Build stage:** `dhi.io/rust:1-alpine3.24-dev`, as asked. The binary is
  linked fully statically (`-C target-feature=+crt-static`, with an explicit
  `--target` so proc-macros stay dynamic): it needs nothing from the runtime
  image, whatever its C library.
- **Runtime stage:** `dhi.io/debian-base:trixie` (non-root user 65532, CA
  bundle, no package manager) plus **`jellyfin-ffmpeg8`**, the ffmpeg build
  Jellyfin maintains for exactly this use. It bundles, under
  `/usr/lib/jellyfin-ffmpeg`, current GPU drivers: Intel's media driver and
  Quick Sync runtime, Mesa's VA-API driver for AMD, libva, Vulkan, and NVENC,
  which loads the NVIDIA driver that the NVIDIA container toolkit mounts. One
  image per architecture (amd64, arm64) serves AMD, Intel and NVIDIA.
  `ffmpeg` and `ffprobe` are linked into `/usr/local/bin`.
- **Versions** (October 2026): ffmpeg 8.1.3, Intel media driver 26.1.6,
  libva 2.24, Mesa's VA-API driver 26.0.8. Debian trixie had ffmpeg 7.1.5 and
  Intel's driver 25.2.3; sid has newer ones, but sid's packages need sid's
  glibc, which would replace the hardened one and turn the image into a
  partial sid upgrade, and Docker Hardened Images publish no newer Debian base.
- **Why not build ffmpeg ourselves:** ffmpeg, Intel's driver and its memory
  library, libva, libvpl and the Quick Sync runtime for two architectures, on
  every release, roughly 30–60 minutes of CI each, with their security fixes
  to track ourselves. Static ffmpeg builds (BtbN) bring no drivers; Intel's
  package repository targets Ubuntu only.
- **Also gained:** the tone-mapping filters (`tonemapx`, used for HDR, D16;
  `tonemap_vaapi`, `tonemap_opencl`, `tonemap_cuda`, libplacebo), AMF, and a
  smaller image: **487 MB**, against 851 MB with
  Debian's ffmpeg (which pulls X11, SDL, the whole Mesa GL stack) and 615 MB
  with Alpine's (which has no NVENC: NVIDIA's libraries need glibc).
- **Trust:** Jellyfin's repository is signed; its key is checked into
  `docker/jellyfin.asc` (fingerprint
  `4918 AABC 486C A052 358D 778D 4902 3CD0 1DE2 1A7B`), so a build never
  fetches a key.
- **Pinned and updated by Renovate.** The Dockerfile pins
  `JELLYFIN_FFMPEG_VERSION`; a Renovate custom manager (`deb` datasource on
  Jellyfin's repository) proposes updates, reviewed by hand rather than
  auto-merged. Its other dependencies (a few Debian libraries) follow trixie's
  security updates at each build.
- **No package manager left behind.** apt and dpkg are borrowed from
  `dhi.io/debian-base:trixie-dev` through a bind mount for the install step
  (`docker/install-packages.sh`); the package database is updated, so
  scanners and SBOMs see what is installed. Debian's package scripts assume
  `perl-base`, `debconf` and `init-system-helpers`, which the hardened base
  lacks, so those three are installed too.
- **Runtime:** user 65532, read-only root filesystem, no capability. VA-API
  and Quick Sync need `/dev/dri` and the host's render group (`group_add`);
  NVENC needs the NVIDIA container toolkit (`--gpus all`); the image sets
  `NVIDIA_DRIVER_CAPABILITIES=compute,video,utility` so that the toolkit mounts
  CUDA (decoding, scaling) as well as NVENC. `XDG_CACHE_HOME=/tmp/.cache`
  gives Mesa's shader cache a writable place.
- **Rust 1.96**, the toolchain of the DHI build image, is the MSRV and the CI
  toolchain, so local, CI and image builds agree. The image has no clippy,
  rustfmt or rustdoc, so CI lints with rustup, and doctests are disabled.

### D19. CI, release, Renovate, license
- **CI** (`ci.yaml`): rustfmt, clippy `-D warnings`, tests against the latest
  PostgreSQL and MariaDB service containers (ffmpeg installed for the
  end-to-end test), the example configurations validated, yamllint,
  cargo-deny (advisories, bans, licenses, sources), and a Docker build with a
  smoke test that also checks ffmpeg's encoders.
- **Release** (`release.yaml`): multi-arch images (amd64 and arm64 on native
  runners, merged by digest) pushed to `ghcr.io/sebastka/ferrocine` with SBOM
  and provenance. Pushes to `master` publish `edge` and `sha-<commit>`.
  Releases are dated, as in the other images of this account: running the
  workflow by hand on `master` publishes `YYYY-MM-DD` (UTC; `.1`, `.2`… for
  further releases the same day) and `latest`, a Git tag and a GitHub release
  with generated notes. The deployment examples pin a release, and Renovate
  moves them to each new one. Dates rather than semver: the server and the UI ship
  together and continuously, and a date says how current an image is. The
  binary reports the tag (`--version`, `/api/v1/system`).
- **Signing:** every published image (edge included) is signed with cosign,
  keyless, by the release workflow's GitHub identity, and gets a build
  provenance attestation from `actions/attest`, pushed to the registry; both
  are verified in the workflow before a release is created. Signing the
  index digest covers each platform's image and its SBOM and provenance.
- **Pinning:** Actions by commit SHA, images by multi-arch index digest.
- **Renovate** rather than Dependabot, which reads neither the `# syntax=`
  directive, nor service containers, nor images mounted in `RUN`, nor the
  pinned `jellyfin-ffmpeg8` version (D18). It runs
  nightly; everything but majors (and Cargo `0.x` minors, which are breaking)
  merges itself once CI passes; releases wait 3 days except security fixes.
- **Credentials:** pulling Docker Hardened Images needs `DOCKERHUB_USERNAME`
  and `DOCKERHUB_TOKEN`; pull requests from forks skip the Docker job.
- **Dependencies** are added with `default-features = false`; `ring` is the
  only crypto provider (aws-lc-rs and OpenSSL are banned in `deny.toml`).
- **License:** MIT.

### D20. Tests
- **Unit tests** for name parsing, `.nfo`, ffprobe output, playback
  decisions and ffmpeg command lines, WebVTT shifting, JWT checks, PKCE,
  cookies, origins and configuration.
- **Store tests** on both databases: lists, filters, sorting, `LIKE`
  escaping, user data, series counts, cascades, sessions, single-use
  sign-ins, advisory locks.
- **End-to-end test** on both databases: a mock OpenID Connect provider and a
  mock TMDB, a library generated with ffmpeg; sign-in (refused without a
  group, replayed callbacks refused), scans, `.nfo` and TMDB precedence,
  artwork (local, downloaded, extracted), access lists, direct play with
  ranges, remux and transcode over HLS (software), subtitles, progress, next
  up, search, rescans that remove a file, metadata refresh, configuration
  reloads (an invalid one refused, access lists applied, transcoding changes
  reported as needing a restart), HDR10 tone-mapped to BT.709 (checked with
  ffprobe on the produced segment), cross-site writes refused, sign-out.
- **Container stack** (`dev/compose.yaml`: mock-oauth2-server, PostgreSQL, the
  core and the UI image) driven through the UI's proxy, with VA-API (D15),
  and walked through in headless Chromium: sign-in at the mock provider, home,
  item pages, and playback in the real player of a VP9/Opus WebM (direct), an
  H.264/AC-3 Matroska (remux, audio to AAC) and a 4K HEVC 10-bit HDR10
  Matroska (VA-API transcode), each playing past 3 s without a console error.
- **Databases tested:** PostgreSQL 18, MariaDB 11.8 and 13.0.

## Not done yet

- **Clients:** desktop, phone and TV apps; bearer access tokens for them
  (D10); Chromecast/DLNA.
- **Content:** music, books, photos, live TV; collections, multiple versions
  or parts (`cd1`/`cd2`) of a movie, people pages and their pictures.
- **Playback:** intro and credits detection, SyncPlay.
- **Operations:** configuration file watching, purging removed libraries,
  sharing playback sessions across replicas.
- **Users:** parental ratings per user.
- **Metadata:** Addic7ed/Podnapisi subtitles; MusicBrainz; per-library
  provider order; images from AniDB for episodes.
