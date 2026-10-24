# Libraries: naming and metadata

Ferrocine reads the layouts Jellyfin, Kodi and Plex use. Libraries are
mounted read-only: nothing is written there.

## Movies

```
movies/
├── The Matrix (1999)/
│   ├── The Matrix (1999).mkv
│   ├── The Matrix (1999).en.srt        subtitle (language en)
│   ├── The Matrix (1999).fr.forced.srt forced French subtitle
│   ├── movie.nfo                       Kodi metadata (or "The Matrix (1999).nfo")
│   ├── poster.jpg                      also folder, cover, "<name>-poster"
│   └── fanart.jpg                      also backdrop, background, "<name>-fanart"
├── Alien (1979) {tmdb-348}/Alien.mkv   provider id in the name
└── Heat.1995.1080p.BluRay.x264.mkv     release names work too
```

- The title ends at the year when there is one, else at the first release
  token (`1080p`, `BluRay`, `x265`…).
- A directory holding a single movie names it when it has a year or a
  provider id (`{tmdb-…}`, `[imdbid-tt…]`, `{tvdb-…}`). `poster.jpg`,
  `fanart.jpg` and `movie.nfo` apply when the directory holds a single movie;
  otherwise use `<name>-poster.jpg` and `<name>.nfo`.
- **Extras:** files in `Extras`, `Featurettes`, `Behind The Scenes`,
  `Deleted Scenes`, `Interviews`, `Scenes`, `Shorts`, `Trailers` or `Other`
  folders of the movie's folder, and files named like `<movie>-trailer`
  (`-featurette`, `-behindthescenes`, `-deleted`, `-interview`, `-scene`,
  `-short`, `-extra`), are extras of the movie, shown under it.
- Ignored: hidden files, `Samples` directories and `-sample` files.

## `.ignore` files

As in Jellyfin, a `.ignore` file hides content from a library, e.g. a show
that sits in a movie directory:

- **Empty** (or blank): the directory holding it and everything below are
  skipped.
- **With patterns**: one gitignore pattern per line, matched without regard to
  case against paths relative to that directory: `Samples/` (directories
  only), `*.iso`, `SPs` (any file or directory of that name), `!keep.mkv` (an
  exception), `# comment`. Invalid patterns are skipped; a file with no valid
  pattern hides everything, like an empty one.
- **Nearest wins:** only the closest `.ignore` at or above a directory (above
  a file) applies, up to the library path; one in a subdirectory replaces its
  parents' rules instead of adding to them.

Items already scanned disappear at the next scan. Libraries stay read-only:
the files are written by you, never by Ferrocine. An unreadable `.ignore`
stops that scan from removing anything.

## Shows

```
shows/
└── Breaking Bad (2008)/
    ├── tvshow.nfo
    ├── poster.jpg, fanart.jpg
    ├── season01-poster.jpg              or poster.jpg inside "Season 01"
    ├── Season 01/
    │   ├── Breaking Bad - S01E01 - Pilot.mkv
    │   ├── Breaking Bad - S01E01 - Pilot.nfo
    │   └── Breaking Bad - S01E01 - Pilot-thumb.jpg
    ├── Season 02/
    │   └── 01 - Seven Thirty-Seven.mkv  number from the name, season from the directory
    └── Specials/
        └── S00E01 - Making of.mkv       season 0
```

- A series is a top-level directory. Episodes are numbered `S01E02`, `s1e2`,
  `1x02`, or by a leading number inside a `Season N` (also `S2`, `Saison 3`,
  `Staffel 4`) directory. Multi-episode files count as their first episode.
- Episode files at the library root are grouped by the name before `SxxEyy`.
- The episode title is what follows the number, or "Episode N".
- `episode_order: dvd` or `absolute` (with `metadata.tvdb`) numbers episodes
  as TheTVDB's DVD or absolute order instead of the broadcast order.

### Anime

With `anime: true`, release-group names are understood too: a series
directory holding `[SubsPlease] Sousou no Frieren - 01 (1080p).mkv` or
`Frieren Ep 12.mkv` gets episode 1 or 12 of season 1 (brackets, braces and
parentheses are ignored; a four-digit year is not an episode). AniList names
and describes the series, AniDB (when configured) numbers and names the
episodes.

## Supported files

- **Video:** mkv, mp4, m4v, avi, mov, wmv, ts, m2ts, mts, webm, mpg, mpeg,
  flv, ogv, 3gp, vob, divx.
- **Subtitles:** embedded text tracks (SubRip, ASS/SSA, mov_text, WebVTT),
  and srt, vtt, ass, ssa files next to the video. Bitmap subtitles (PGS,
  VobSub, DVD) are burned into the picture when chosen.
- **Images:** jpg, jpeg, png, webp.

## DVD folders and images, Blu-ray folders

A DVD is read through its information files (`VIDEO_TS.IFO`,
`VTS_NN_0.IFO`), as a folder or as an image (`.iso`); keep them with the
VOBs. Its longest title is the movie, named after the folder holding it, or
after the image when the image sits at the library's root or among other
videos (put `poster.jpg`, `movie.nfo`… next to it):

```
movies/
├── A Date with Judy (1948) NTSC DVD9/
│   └── VIDEO_TS/                    title 1: the film (113 min, 28 chapters)
│                                    titles 5, 8–10: its extras
├── Rambo (2008)/RAMBO.ISO           the same, from an image
└── Some Film (2010)/
    └── BDMV/STREAM/00800.m2ts       Blu-ray: the largest stream file
```

- **The movie:** the longest title, played exactly: duration, chapters,
  audio and subtitle languages come from the disc, and seeking jumps to the
  right place (DVD timestamps restart along a film, so they cannot be
  trusted). Logos and warnings of under two seconds at either end of a
  title are left out. DVDs always play through a transcode (no browser
  plays MPEG-2).
- **Extras:** the other titles of a minute or more, with sound, that are not
  part of the film (scene selections replay its cells); they appear under
  the movie as "Extra 1", "Extra 2"….
- **Shows:** in a show library, each title of five minutes or more is an
  episode, except "play all" titles; episodes are numbered on in disc order
  within the season:

  ```
  shows/Firefly/Season 1/Disc 1/VIDEO_TS
  shows/Firefly/Season 1/Disc 2/VIDEO_TS
  shows/Firefly/Firefly S01D3.iso
  ```

  The season comes from a `Season N` folder or from the disc's name
  (`S01D3`, `Season 1 Disc 3`). Disc numbers sort naturally (2 before 10).
- **Without `VIDEO_TS.IFO`:** the title set with the most data is the movie,
  read whole.
- **Blu-ray:** the largest file of `BDMV/STREAM` is the movie. Blu-ray
  images (UDF only) are not read.

## Where metadata comes from

1. **The `.nfo` file** wins: title, original and sort titles, year, dates,
   plot, tagline, rating, certification, genres, studios, people, provider ids.
2. **Providers** fill the rest, including artwork, season and episode
   details, in this order: AniList and AniDB (anime libraries), TheTVDB
   (first when `episode_order` is not `aired`), TMDB, TheTVDB; then OMDb
   (ratings) and fanart.tv (logos, backdrops). They are searched by provider
   id first (from the name or the `.nfo`, e.g. `{tmdb-603}`, `[imdbid-tt0133093]`,
   `{tvdb-81189}`), then by title, accepting only a result released within a
   year of the year in the name. A title from the file name is replaced, by
   the original title when the work's original language is in
   `prefer_original_title` (globally under `metadata`, or per library).
3. **The file name** gives the title and year when nothing else does; ffprobe
   gives the runtime.

Artwork: local files (`poster`, `fanart`, `clearlogo`/`logo`,
`<name>-clearlogo`…), then the providers', then a frame of the video (movies
and episodes with no image at all). Everything is resized and stored in the
database.

Subtitles downloaded from OpenSubtitles.com (on demand from the player, or
automatically for `subtitles.languages`) are stored in the database too.

## Scans

Libraries are scanned at start, every `scanner.interval` (6 h), and when an
administrator asks (`POST /api/v1/libraries/{id}/scan`, or the UI's admin
page). Unchanged files are not probed again. If a library directory is
missing or unreadable (an unmounted share), the scan keeps every item.

A wrong match is fixed by renaming with a provider id (`{tmdb-…}`) or by an
`.nfo`, then "Refresh metadata" on the item (administrators).
