//! Library scanner: walks the (read-only) library directories, probes new or
//! changed files, reads local metadata, fetches remote metadata and artwork,
//! and stores everything in the database.
//!
//! Scans are incremental: a file whose size and modification time did not
//! change is not probed again, and local metadata files are fingerprinted.
//! Items the scan did not see are deleted, unless the walk was incomplete
//! (an unreadable directory or a missing root, e.g. an unmounted share).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use anyhow::{Context, bail};
use futures_util::{StreamExt, stream};

use super::artwork::{self, find_image};
use super::disc::{self, DiscFile, Dvd, Source};
use super::dotignore::DotIgnore;
use super::naming::{self, ProviderIds};
use super::nfo::{self, Nfo};
use super::probe::{self, EXTERNAL_SUBTITLE_BASE, MediaInfo, SubtitleStream};
use super::{ItemKind, Meta};
use crate::config::{Config, LibraryConfig, LibraryKind};
use crate::metadata::{ProviderState, Providers, RemoteItem, RemoteSeason, SeriesIds};
use crate::store::{ImageInfo, ImageKind, ImageSource, ItemRow, ItemWrite, Store, now_ms, sha256_hex};

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// Re-read local metadata and fetch remote metadata for every item.
    pub refresh_metadata: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScanStats {
    pub files: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: u64,
    pub errors: usize,
    /// Subtitles downloaded automatically.
    pub subtitles: u32,
}

#[derive(Clone, Debug)]
struct VideoFile {
    path: PathBuf,
    root: PathBuf,
    /// Of every part, for a disc.
    size: i64,
    mtime: i64,
    /// A disc's title: the movie's folder (holding `VIDEO_TS`, `BDMV` or the
    /// image), which holds its artwork and `.nfo`, and the name to parse.
    disc: Option<(PathBuf, String)>,
    /// An extra: its kind and the folder of its movie.
    extra: Option<(&'static str, PathBuf)>,
}

/// An extra to index under its movie.
struct ExtraFile {
    parent: i64,
    path: PathBuf,
    kind: &'static str,
    title: String,
    size: i64,
    mtime: i64,
}

/// The DVDs of each season of a series, with their order.
type DiscsBySeason<'a> = BTreeMap<(String, i32), Vec<(Vec<(u64, String)>, &'a WalkDvd)>>;

/// A DVD (folder or image) met during a scan.
struct WalkDvd {
    dvd: Dvd,
    root: PathBuf,
    mtime: i64,
    /// The movie's folder and the name to parse.
    dir: PathBuf,
    name: String,
}

#[derive(Default)]
struct Walk {
    files: Vec<VideoFile>,
    dvds: Vec<WalkDvd>,
    /// File names of every directory walked.
    dirs: HashMap<PathBuf, Vec<String>>,
    /// An unreadable entry was met: do not delete what was not seen.
    incomplete: bool,
}

/// The state shared by the tasks of one scan.
struct Ctx<'a> {
    cfg: &'a Config,
    providers: Option<Providers>,
    lib: &'a LibraryConfig,
    generation: i64,
    opts: ScanOptions,
    existing: HashMap<String, ItemRow>,
    images: HashMap<(i64, ImageKind), ImageInfo>,
    walk: Walk,
    stats: Mutex<ScanStats>,
}

impl Ctx<'_> {
    fn count(&self, f: impl FnOnce(&mut ScanStats)) {
        if let Ok(mut s) = self.stats.lock() {
            f(&mut s);
        }
    }

    fn names(&self, dir: &Path) -> &[String] {
        self.walk.dirs.get(dir).map(Vec::as_slice).unwrap_or_default()
    }

    /// Whether an item's remote metadata should be fetched in this scan.
    fn remote_due(&self, existing: Option<&ItemRow>, now: i64) -> bool {
        if self.providers.is_none() {
            return false;
        }
        let Some(e) = existing else { return true };
        match e.remote_fetched_at {
            None => true,
            Some(_) if self.opts.refresh_metadata => true,
            Some(t) => !e.remote_matched && now - t > self.cfg.scanner.retry_unmatched.as_millis_i64(),
        }
    }
}

#[derive(Clone)]
pub struct Scanner {
    store: Store,
    http: reqwest::Client,
    running: Arc<Mutex<HashSet<String>>>,
    /// Provider state kept across scans (AniDB pacing and titles, tokens).
    providers: Arc<ProviderState>,
}

fn path_key(lib: &str, kind: ItemKind, identity: &str) -> String {
    sha256_hex(format!("{lib}\0{}\0{identity}", kind.code()).as_bytes())
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// `path|size|mtime` of a file, if it exists.
fn fingerprint(path: &Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    Some(format!("{}|{}|{}", path.display(), m.len(), mtime_ms(&m)))
}

fn read_nfo(path: Option<&Path>) -> Option<Nfo> {
    let text = std::fs::read(path?).ok()?;
    Some(nfo::parse(&String::from_utf8_lossy(&text)))
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn parent(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

fn walk(roots: &[PathBuf]) -> anyhow::Result<Walk> {
    let mut out = Walk::default();
    for root in roots {
        let meta =
            std::fs::metadata(root).with_context(|| format!("library path {} is not readable", root.display()))?;
        if !meta.is_dir() {
            bail!("library path {} is not a directory", root.display());
        }
        let mut dot_ignore = DotIgnore::new(root);
        let mut discs = Vec::new();
        let mut dvd_folders: BTreeSet<PathBuf> = BTreeSet::new();
        let mut images: Vec<PathBuf> = Vec::new();
        let mut ignored = 0usize;
        let entries = walkdir::WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                let is_dir = e.file_type().is_dir();
                // Extras folders are walked: their files are extras.
                if e.depth() > 0 && is_dir && naming::is_skipped_dir(&e.file_name().to_string_lossy()) {
                    return false;
                }
                if dot_ignore.is_ignored(e.path(), is_dir) {
                    ignored += 1;
                    return false;
                }
                true
            });
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read library entry; nothing will be removed by this scan");
                    out.incomplete = true;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let dir = parent(path);
            if name.eq_ignore_ascii_case("VIDEO_TS.IFO") {
                dvd_folders.insert(dir.clone());
            } else if naming::has_extension(path, &["iso"]) {
                images.push(path.to_path_buf());
            } else if naming::has_extension(path, &naming::VIDEO_EXTENSIONS) && !naming::is_sample(&stem(path)) {
                let Ok(meta) = entry.metadata() else {
                    out.incomplete = true;
                    continue;
                };
                let (size, mtime) = (meta.len() as i64, mtime_ms(&meta));
                // Disc files are gathered: one feature per disc, below.
                if let Some(part) = disc::classify(path) {
                    discs.push(DiscFile {
                        path: path.to_path_buf(),
                        part,
                        size,
                        mtime,
                    });
                } else {
                    out.files.push(VideoFile {
                        path: path.to_path_buf(),
                        root: root.clone(),
                        size,
                        mtime,
                        disc: None,
                        extra: extra_of(root, path),
                    });
                }
            }
            out.dirs.entry(dir).or_default().push(name);
        }
        // DVDs with their information files: read by title.
        let mut sources: Vec<Source> = dvd_folders.into_iter().map(Source::Folder).collect();
        sources.extend(images.into_iter().map(Source::Image));
        for source in sources {
            match Dvd::open(&source) {
                Ok(Some(dvd)) => {
                    if let Source::Folder(dir) = &source {
                        discs.retain(|d| d.path.parent() != Some(dir.as_path()));
                    }
                    let (dir, name) = disc_name(root, &source, &out.dirs);
                    out.dvds.push(WalkDvd {
                        mtime: dvd.mtime(),
                        dvd,
                        root: root.clone(),
                        dir,
                        name,
                    });
                }
                Ok(None) => tracing::debug!(path = %source.path().display(), "not a DVD-Video disc; skipped"),
                Err(e) => {
                    tracing::warn!(path = %source.path().display(), error = %e, "cannot read disc; nothing will be removed by this scan");
                    out.incomplete = true;
                }
            }
        }
        // Without them (or for Blu-ray): the biggest title set or stream file.
        for f in disc::features(std::mem::take(&mut discs)) {
            let name = f
                .movie_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.files.push(VideoFile {
                path: f.path,
                root: root.clone(),
                size: f.size,
                mtime: f.mtime,
                disc: Some((f.movie_dir, name)),
                extra: None,
            });
        }
        if dot_ignore.unreadable {
            out.incomplete = true;
        }
        if ignored > 0 {
            tracing::info!(path = %root.display(), entries = ignored, "skipped by .ignore files");
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The kind of extra a file is and its movie's folder: in an extras folder
/// (`Alien (1979)/Trailers/…`), or named like one next to the movie.
fn extra_of(root: &Path, path: &Path) -> Option<(&'static str, PathBuf)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut dir = root.to_path_buf();
    for c in rel.parent()?.components() {
        let name = c.as_os_str().to_string_lossy();
        if let Some(kind) = naming::extra_dir(&name) {
            return Some((kind, dir));
        }
        dir.push(c);
    }
    naming::extra_file(&stem(path)).map(|kind| (kind, parent(path)))
}

/// The movie folder of a DVD and the name to parse: the folder holding it,
/// or the image's own name when it sits at the library's root or among
/// other videos.
fn disc_name(root: &Path, source: &Source, dirs: &HashMap<PathBuf, Vec<String>>) -> (PathBuf, String) {
    let dir = source.movie_dir();
    let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    if let Source::Image(image) = source {
        let crowded = dirs.get(&dir).is_some_and(|names| {
            names
                .iter()
                .filter(|n| {
                    let p = Path::new(n);
                    naming::has_extension(p, &["iso"]) || naming::has_extension(p, &naming::VIDEO_EXTENSIONS)
                })
                .count()
                > 1
        });
        if dir == root || crowded {
            return (dir, stem(image));
        }
    }
    (dir, dir_name.unwrap_or_default())
}

/// Order of disc names: `Disc 2` before `Disc 10`.
fn natural_key(s: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut digits = String::new();
    let mut text = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_digit() {
            if !text.is_empty() {
                out.push((0, std::mem::take(&mut text)));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
                digits.clear();
            }
            text.push(c);
        }
    }
    if !digits.is_empty() {
        out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
    }
    if !text.is_empty() {
        out.push((0, text));
    }
    out
}

/// External subtitle files next to a video.
fn sidecar_subtitles(dir: &Path, names: &[String], video_stem: &str) -> Vec<SubtitleStream> {
    let mut found: Vec<&String> = names
        .iter()
        .filter(|n| naming::has_extension(Path::new(n), &naming::SUBTITLE_EXTENSIONS))
        .filter(|n| n.starts_with(video_stem))
        .collect();
    found.sort();
    found
        .into_iter()
        .filter_map(|n| {
            let (language, forced, title) = naming::subtitle_tags(video_stem, n)?;
            let ext = Path::new(n).extension()?.to_str()?.to_lowercase();
            Some((n, language, forced, title, ext))
        })
        .enumerate()
        .map(|(i, (n, language, forced, title, ext))| SubtitleStream {
            index: EXTERNAL_SUBTITLE_BASE + i as i32,
            stream_id: None,
            codec: match ext.as_str() {
                "srt" => "subrip".into(),
                "vtt" => "webvtt".into(),
                other => other.to_owned(),
            },
            language,
            title,
            default: false,
            forced,
            external: true,
            text: true,
            path: Some(dir.join(n).to_string_lossy().into_owned()),
            source: None,
        })
        .collect()
}

/// Embedded streams from `media`, plus the current sidecar files.
fn with_sidecars(mut media: MediaInfo, sidecars: Vec<SubtitleStream>) -> MediaInfo {
    media.subtitles.retain(|s| !s.external);
    media.subtitles.extend(sidecars);
    media
}

/// Local images wanted for an item: kind → candidate base names.
type LocalArt = Vec<(ImageKind, Option<PathBuf>)>;

impl Scanner {
    pub fn new(store: Store, http: reqwest::Client, providers: Arc<ProviderState>) -> Self {
        Self {
            store,
            http,
            running: Arc::new(Mutex::new(HashSet::new())),
            providers,
        }
    }

    pub fn is_running(&self, library: &str) -> bool {
        self.running.lock().is_ok_and(|r| r.contains(library))
    }

    /// Scan one library with the configuration `cfg`, unless this or another
    /// instance is already scanning it.
    pub async fn scan(
        &self,
        cfg: &Config,
        lib: &LibraryConfig,
        opts: ScanOptions,
    ) -> anyhow::Result<Option<ScanStats>> {
        {
            let Ok(mut running) = self.running.lock() else {
                return Ok(None);
            };
            if !running.insert(lib.id.clone()) {
                return Ok(None);
            }
        }
        let result = self.scan_locked(cfg, lib, opts).await;
        if let Ok(mut running) = self.running.lock() {
            running.remove(&lib.id);
        }
        result
    }

    async fn scan_locked(
        &self,
        cfg: &Config,
        lib: &LibraryConfig,
        opts: ScanOptions,
    ) -> anyhow::Result<Option<ScanStats>> {
        let Some(lock) = self.store.try_lock(&format!("scan:{}", lib.id)).await? else {
            tracing::info!(library = %lib.id, "another instance is scanning this library");
            return Ok(None);
        };
        let started = now_ms();
        self.store.scan_started(&lib.id, started).await?;
        // Unchanged items follow a changed `prefer_original_title` too.
        let retitled = self
            .store
            .retitle(&lib.id, lib.original_title_languages(&cfg.metadata))
            .await?;
        if retitled > 0 {
            tracing::info!(library = %lib.id, items = retitled, "titles switched for prefer_original_title");
        }
        tracing::info!(library = %lib.id, refresh = opts.refresh_metadata, "scan started");
        let mut result = self.scan_inner(cfg, lib, opts, started).await;
        if result.is_ok() {
            match crate::subtitles::automatic(&self.store, cfg, lib, &self.http, &self.providers).await {
                Ok(n) => {
                    if let Ok(stats) = result.as_mut() {
                        stats.subtitles = n;
                    }
                }
                Err(e) => tracing::warn!(library = %lib.id, error = %format!("{e:#}"), "automatic subtitles failed"),
            }
        }
        let finished = now_ms();
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        if let Err(e) = self.store.scan_finished(&lib.id, finished, error.as_deref()).await {
            tracing::warn!(error = %e, "cannot record the end of the scan");
        }
        lock.release().await;
        match &result {
            Ok(s) => tracing::info!(
                library = %lib.id,
                files = s.files,
                added = s.added,
                updated = s.updated,
                unchanged = s.unchanged,
                removed = s.removed,
                errors = s.errors,
                subtitles = s.subtitles,
                seconds = (finished - started) / 1000,
                "scan finished"
            ),
            Err(e) => tracing::error!(library = %lib.id, error = %format!("{e:#}"), "scan failed"),
        }
        result.map(Some)
    }

    async fn scan_inner(
        &self,
        cfg: &Config,
        lib: &LibraryConfig,
        opts: ScanOptions,
        generation: i64,
    ) -> anyhow::Result<ScanStats> {
        let roots = lib.paths.clone();
        let walked = tokio::task::spawn_blocking(move || walk(&roots)).await??;
        let existing = self
            .store
            .library_items(&lib.id)
            .await?
            .into_iter()
            .map(|r| (r.path_key.clone(), r))
            .collect();
        let images = self.store.library_images(&lib.id).await?;
        let ctx = Ctx {
            cfg,
            providers: Providers::new(cfg, &self.http, &self.providers),
            lib,
            generation,
            opts,
            existing,
            images,
            stats: Mutex::new(ScanStats {
                files: walked.files.len(),
                ..Default::default()
            }),
            walk: walked,
        };
        match lib.kind {
            LibraryKind::Movies => self.scan_movies(&ctx).await,
            LibraryKind::Shows => self.scan_shows(&ctx).await,
        }
        let mut stats = ctx.stats.lock().map(|s| s.clone()).unwrap_or_default();
        if ctx.walk.incomplete {
            tracing::warn!(library = %lib.id, "incomplete walk: missing items are kept");
        } else {
            stats.removed = self.store.delete_unseen(&lib.id, generation).await?;
        }
        Ok(stats)
    }

    // -------------------------------------------------------------- movies

    async fn scan_movies(&self, ctx: &Ctx<'_>) {
        let concurrency = ctx.cfg.scanner.probe_concurrency;
        // The movies: files, and the longest title of each DVD.
        let mut movies: Vec<VideoFile> = ctx.walk.files.iter().filter(|f| f.extra.is_none()).cloned().collect();
        for d in &ctx.walk.dvds {
            if let Some(t) = d.dvd.feature() {
                movies.push(VideoFile {
                    path: d.dvd.source.title_path(t.number),
                    root: d.root.clone(),
                    size: t.bytes() as i64,
                    mtime: d.mtime,
                    disc: Some((d.dir.clone(), d.name.clone())),
                    extra: None,
                });
            }
        }
        // Futures built in a loop rather than by a closure: rustc cannot
        // prove `Send` for closures returning borrowing futures.
        let mut tasks = Vec::with_capacity(movies.len());
        for f in &movies {
            tasks.push(async move {
                match self.movie(ctx, f).await {
                    Ok(id) => Some((f.path.clone(), id)),
                    Err(e) => {
                        ctx.count(|s| s.errors += 1);
                        tracing::warn!(file = %f.path.display(), error = %format!("{e:#}"), "cannot index movie");
                        None
                    }
                }
            });
        }
        let ids: HashMap<PathBuf, i64> = stream::iter(tasks)
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        // Each movie's folder (a disc's: the one holding `VIDEO_TS`, `BDMV`
        // or the image, not its stream file's) and name, for its extras.
        let folders: Vec<(PathBuf, String, i64)> = movies
            .iter()
            .filter_map(|f| {
                let id = *ids.get(&f.path)?;
                Some(match &f.disc {
                    Some((dir, name)) => (dir.clone(), name.to_lowercase(), id),
                    None => (parent(&f.path), stem(&f.path).to_lowercase(), id),
                })
            })
            .collect();

        // Their extras: other titles of the DVD, files of extras folders.
        let mut extras: Vec<ExtraFile> = Vec::new();
        for d in &ctx.walk.dvds {
            let Some(parent) = d
                .dvd
                .feature()
                .and_then(|t| ids.get(&d.dvd.source.title_path(t.number)))
            else {
                continue;
            };
            for (n, t) in d.dvd.extras().into_iter().enumerate() {
                let path = d.dvd.source.title_path(t.number);
                extras.push(ExtraFile {
                    parent: *parent,
                    path,
                    kind: "extra",
                    title: format!("Extra {}", n + 1),
                    size: t.bytes() as i64,
                    mtime: d.mtime,
                });
            }
        }
        for f in ctx.walk.files.iter() {
            let Some((kind, movie_dir)) = &f.extra else { continue };
            // The movie of that folder: the only one, or the one the extra is named after.
            let candidates: Vec<&(PathBuf, String, i64)> =
                folders.iter().filter(|(dir, _, _)| dir == movie_dir).collect();
            let file_stem = stem(&f.path).to_lowercase();
            let parent = match candidates.as_slice() {
                [(_, _, id)] => Some(*id),
                many => many
                    .iter()
                    .find(|(_, name, _)| file_stem.starts_with(name.as_str()))
                    .map(|(_, _, id)| *id),
            };
            let Some(parent) = parent else {
                tracing::info!(file = %f.path.display(), "extra without a movie in its folder; skipped");
                continue;
            };
            let title = naming::extra_title(&stem(&f.path), kind);
            extras.push(ExtraFile {
                parent,
                path: f.path.clone(),
                kind,
                title,
                size: f.size,
                mtime: f.mtime,
            });
        }
        let mut tasks = Vec::with_capacity(extras.len());
        for x in &extras {
            tasks.push(async move {
                if let Err(e) = self.extra(ctx, x).await {
                    ctx.count(|s| s.errors += 1);
                    tracing::warn!(file = %x.path.display(), error = %format!("{e:#}"), "cannot index extra");
                }
            });
        }
        stream::iter(tasks)
            .buffer_unordered(concurrency)
            .collect::<Vec<()>>()
            .await;
    }

    /// Index a movie; returns its id.
    async fn movie(&self, ctx: &Ctx<'_>, f: &VideoFile) -> anyhow::Result<i64> {
        let path_str = f.path.to_string_lossy().into_owned();
        let key = path_key(&ctx.lib.id, ItemKind::Movie, &path_str);
        let existing = ctx.existing.get(&key);
        // A disc is named, described and illustrated by its movie's folder.
        let dir = f.disc.as_ref().map_or_else(|| parent(&f.path), |d| d.0.clone());
        let names = ctx.names(&dir);
        let file_stem = match &f.disc {
            Some((_, name)) => name.clone(),
            None => stem(&f.path),
        };
        let alone = f.disc.is_some()
            || names
                .iter()
                .filter(|n| {
                    naming::has_extension(Path::new(n), &naming::VIDEO_EXTENSIONS)
                        && !naming::is_extra_file(&stem(Path::new(n)))
                })
                .count()
                == 1;
        let meta_cfg = &ctx.cfg.metadata;
        let nfo_path = meta_cfg
            .nfo
            .then(|| {
                let own = dir.join(format!("{file_stem}.nfo"));
                if own.exists() {
                    Some(own)
                } else {
                    alone.then(|| dir.join("movie.nfo")).filter(|p| p.exists())
                }
            })
            .flatten();
        let sidecars = sidecar_subtitles(&dir, names, &file_stem);
        let local_key = local_key(nfo_path.as_deref(), &sidecars);

        let now = now_ms();
        let file_changed = existing.is_none_or(|e| e.file_size != Some(f.size) || e.file_mtime != Some(f.mtime));
        let local_changed = existing.is_none_or(|e| e.local_key.as_deref() != Some(local_key.as_str()));
        let remote_due = ctx.remote_due(existing, now);
        let local_art: LocalArt = if meta_cfg.local_artwork {
            let mut poster = vec![format!("{file_stem}-poster")];
            let mut backdrop = vec![format!("{file_stem}-fanart"), format!("{file_stem}-backdrop")];
            let mut logo = vec![format!("{file_stem}-clearlogo"), format!("{file_stem}-logo")];
            if alone {
                poster.extend(["poster", "folder", "cover", "movie"].map(String::from));
                backdrop.extend(["fanart", "backdrop", "background"].map(String::from));
                logo.extend(["clearlogo", "logo"].map(String::from));
            }
            vec![
                (ImageKind::Poster, find_image(&dir, names, &poster)),
                (ImageKind::Backdrop, find_image(&dir, names, &backdrop)),
                (ImageKind::Logo, find_image(&dir, names, &logo)),
            ]
        } else {
            Vec::new()
        };

        if let Some(e) = existing
            && !file_changed
            && !local_changed
            && !remote_due
            && !ctx.opts.refresh_metadata
        {
            self.store.touch_items(&[e.id], ctx.generation).await?;
            ctx.count(|s| s.unchanged += 1);
            let media = e.media.clone();
            self.artwork(ctx, e.id, ItemKind::Movie, &local_art, None, &f.path, media.as_ref())
                .await?;
            return Ok(e.id);
        }

        let media = match existing.and_then(|e| e.media.clone()) {
            Some(m) if !file_changed => m,
            _ => probe::probe(&ctx.cfg.transcoding.ffprobe, &f.path).await?,
        };
        let media = with_sidecars(media, sidecars);

        // Name: the directory when it is "Title (Year)" and holds only this
        // movie, else the file name.
        let from_file = naming::parse_title_year(&file_stem);
        let from_dir = naming::parse_title_year(&dir.file_name().map(|d| d.to_string_lossy()).unwrap_or_default());
        // A disc's name is its folder's (or its image's): already `file_stem`.
        let named = if f.disc.is_none()
            && (alone && from_dir.year.is_some() && from_file.year.is_none() || alone && !from_dir.ids.is_empty())
        {
            from_dir
        } else {
            from_file
        };
        // A cleared local key (metadata refresh) rebuilds the metadata too.
        let reset = ctx.opts.refresh_metadata || existing.is_none_or(|e| e.local_key.is_none());
        let mut meta = match existing {
            Some(e) if !reset => e.meta.clone(),
            _ => name_meta(&named.title, named.year, &named.ids),
        };
        let nfo = read_nfo(nfo_path.as_deref());
        if let Some(n) = &nfo
            && (reset || local_changed)
        {
            meta.overlay_nfo(n);
        }
        let mut remote = None;
        let (mut fetched_at, mut matched) = existing.map_or((None, false), |e| (e.remote_fetched_at, e.remote_matched));
        if remote_due && let Some(providers) = &ctx.providers {
            // The file's duration tells films of the same name apart.
            let mut hint = meta.clone();
            hint.runtime_ms = media.duration_ms.or(hint.runtime_ms);
            match providers.movie(&hint, ctx.lib).await {
                Ok(r) => {
                    fetched_at = Some(now);
                    matched = r.is_some();
                    if let Some(r) = &r {
                        let keep_title = nfo.as_ref().is_some_and(|n| n.title.is_some());
                        meta.fill_from(&r.meta, keep_title);
                    } else {
                        tracing::debug!(title = %meta.title, year = ?meta.year, "no remote metadata");
                    }
                    remote = r;
                }
                Err(e) => tracing::warn!(title = %meta.title, error = %format!("{e:#}"), "metadata lookup failed"),
            }
        }
        if meta.runtime_ms.is_none() {
            meta.runtime_ms = media.duration_ms;
        }
        meta.choose_title(ctx.lib.original_title_languages(&ctx.cfg.metadata));
        let id = self
            .store
            .upsert_item(&ItemWrite {
                library_id: ctx.lib.id.clone(),
                kind: ItemKind::Movie,
                parent_id: None,
                series_id: None,
                path_key: key,
                path: Some(path_str),
                meta,
                season_number: None,
                episode_number: None,
                file_size: Some(f.size),
                file_mtime: Some(f.mtime),
                media: Some(media.clone()),
                local_key: Some(local_key),
                remote_fetched_at: fetched_at,
                remote_matched: matched,
                added_at: existing.map_or(now, |e| e.added_at),
                seen_scan: ctx.generation,
                extra_type: None,
            })
            .await?;
        ctx.count(|s| {
            if existing.is_some() {
                s.updated += 1
            } else {
                s.added += 1
            }
        });
        self.artwork(
            ctx,
            id,
            ItemKind::Movie,
            &local_art,
            remote.as_ref(),
            &f.path,
            Some(&media),
        )
        .await?;
        Ok(id)
    }

    /// Index an extra of a movie.
    async fn extra(&self, ctx: &Ctx<'_>, x: &ExtraFile) -> anyhow::Result<()> {
        let (parent, path, kind, title, size, mtime) =
            (x.parent, x.path.as_path(), x.kind, x.title.clone(), x.size, x.mtime);
        let path_str = path.to_string_lossy().into_owned();
        let key = path_key(&ctx.lib.id, ItemKind::Extra, &path_str);
        let existing = ctx.existing.get(&key);
        let unchanged = existing.is_some_and(|e| {
            e.file_size == Some(size)
                && e.file_mtime == Some(mtime)
                && e.parent_id == Some(parent)
                && e.meta.title == title
                && e.extra_type.as_deref() == Some(kind)
        });
        if let Some(e) = existing
            && unchanged
            && !ctx.opts.refresh_metadata
        {
            self.store.touch_items(&[e.id], ctx.generation).await?;
            ctx.count(|s| s.unchanged += 1);
            return self
                .artwork(ctx, e.id, ItemKind::Extra, &[], None, path, e.media.as_ref())
                .await;
        }
        let media = match existing.and_then(|e| e.media.clone()) {
            Some(m) if existing.is_some_and(|e| e.file_size == Some(size) && e.file_mtime == Some(mtime)) => m,
            _ => probe::probe(&ctx.cfg.transcoding.ffprobe, path).await?,
        };
        let meta = Meta {
            title,
            runtime_ms: media.duration_ms,
            ..Default::default()
        };
        let id = self
            .store
            .upsert_item(&ItemWrite {
                library_id: ctx.lib.id.clone(),
                kind: ItemKind::Extra,
                parent_id: Some(parent),
                series_id: None,
                path_key: key,
                path: Some(path_str),
                meta,
                season_number: None,
                episode_number: None,
                file_size: Some(size),
                file_mtime: Some(mtime),
                media: Some(media.clone()),
                local_key: Some(String::new()),
                remote_fetched_at: None,
                remote_matched: false,
                added_at: existing.map_or_else(now_ms, |e| e.added_at),
                seen_scan: ctx.generation,
                extra_type: Some(kind.to_owned()),
            })
            .await?;
        ctx.count(|s| {
            if existing.is_some() {
                s.updated += 1
            } else {
                s.added += 1
            }
        });
        self.artwork(ctx, id, ItemKind::Extra, &[], None, path, Some(&media))
            .await
    }

    // --------------------------------------------------------------- shows

    async fn scan_shows(&self, ctx: &Ctx<'_>) {
        // Group episodes by series (top-level directory, or name prefix for
        // files at the root).
        let mut series: BTreeMap<String, SeriesGroup> = BTreeMap::new();
        for f in &ctx.walk.files {
            // Extras folders belong to movies.
            if f.extra.is_some() {
                continue;
            }
            // A Blu-ray, or a DVD without its information files: one stream,
            // several episodes that nothing tells apart.
            if f.disc.is_some() {
                tracing::info!(path = %f.path.display(), "disc without DVD information files in a show library; skipped");
                continue;
            }
            let Ok(rel) = f.path.strip_prefix(&f.root) else {
                continue;
            };
            let dirs: Vec<String> = rel
                .parent()
                .map(|p| {
                    p.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let file_stem = stem(&f.path);
            let parsed = naming::parse_episode(&file_stem).or_else(|| {
                ctx.lib
                    .anime
                    .then(|| naming::parse_absolute_episode(&file_stem))
                    .flatten()
            });
            let (identity, name, dir) = match dirs.first() {
                Some(top) => {
                    let dir = f.root.join(top);
                    (dir.to_string_lossy().into_owned(), top.clone(), Some(dir))
                }
                None => match parsed.as_ref().and_then(|p| p.prefix.clone()) {
                    Some(prefix) => (format!("{}#{}", f.root.display(), prefix.to_lowercase()), prefix, None),
                    None => {
                        tracing::warn!(file = %f.path.display(), "episode outside a series directory without a series name; skipped");
                        continue;
                    }
                },
            };
            let season_from_dir = dirs.iter().skip(1).rev().find_map(|d| naming::parse_season_dir(d));
            let episode = parsed.or_else(|| season_from_dir.and(naming::parse_numbered_episode(&file_stem)));
            let (season, number, title) = match episode {
                Some(e) => (e.season.or(season_from_dir).unwrap_or(1), e.episode, e.title),
                None => {
                    // Last resort: the episode's own .nfo.
                    let nfo = read_nfo(Some(&parent(&f.path).join(format!("{file_stem}.nfo"))));
                    match nfo.and_then(|n| Some((n.season.or(season_from_dir).unwrap_or(1), n.episode?, n.title))) {
                        Some(x) => x,
                        None => {
                            tracing::warn!(file = %f.path.display(), "no episode number in the file name; skipped");
                            continue;
                        }
                    }
                }
            };
            series
                .entry(identity.clone())
                .or_insert_with(|| SeriesGroup {
                    identity,
                    name,
                    dir,
                    episodes: Vec::new(),
                })
                .episodes
                .push(EpisodeFile {
                    file: f.clone(),
                    season,
                    episode: number,
                    title,
                });
        }
        // DVDs: their episode titles, numbered on in disc order within a
        // season (`Show/Season 1/Disc 1/VIDEO_TS`, `Show/Show S01D2.iso`).
        let mut discs: DiscsBySeason<'_> = BTreeMap::new();
        for d in &ctx.walk.dvds {
            let Ok(rel) = d.dvd.source.path().strip_prefix(&d.root) else {
                continue;
            };
            let dirs: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let Some(top) = dirs.first().filter(|_| dirs.len() > 1) else {
                tracing::warn!(path = %d.dvd.source.path().display(), "disc outside a series directory; skipped");
                continue;
            };
            let season = dirs[1..]
                .iter()
                .rev()
                .find_map(|n| naming::parse_season_dir(n).or_else(|| naming::disc_season(n)))
                .unwrap_or(1);
            let series_dir = d.root.join(top);
            series
                .entry(series_dir.to_string_lossy().into_owned())
                .or_insert_with(|| SeriesGroup {
                    identity: series_dir.to_string_lossy().into_owned(),
                    name: top.clone(),
                    dir: Some(series_dir.clone()),
                    episodes: Vec::new(),
                });
            discs
                .entry((series_dir.to_string_lossy().into_owned(), season))
                .or_default()
                .push((natural_key(&rel.to_string_lossy()), d));
        }
        for ((identity, season), mut list) in discs {
            list.sort_by(|a, b| a.0.cmp(&b.0));
            // After the season's episode files, if any.
            let group = series.get_mut(&identity).expect("added above");
            let mut number = group
                .episodes
                .iter()
                .filter(|e| e.season == season)
                .map(|e| e.episode)
                .max()
                .unwrap_or(0);
            for (_, d) in list {
                for t in d.dvd.episodes() {
                    number += 1;
                    group.episodes.push(EpisodeFile {
                        file: VideoFile {
                            path: d.dvd.source.title_path(t.number),
                            root: d.root.clone(),
                            size: t.bytes() as i64,
                            mtime: d.mtime,
                            disc: Some((d.dir.clone(), d.name.clone())),
                            extra: None,
                        },
                        season,
                        episode: number,
                        title: None,
                    });
                }
            }
        }
        for group in series.values() {
            if let Err(e) = self.series(ctx, group).await {
                ctx.count(|s| s.errors += 1);
                tracing::warn!(series = %group.name, error = %format!("{e:#}"), "cannot index series");
            }
        }
    }

    async fn series(&self, ctx: &Ctx<'_>, g: &SeriesGroup) -> anyhow::Result<()> {
        let now = now_ms();
        let key = path_key(&ctx.lib.id, ItemKind::Series, &g.identity);
        let existing = ctx.existing.get(&key);
        let meta_cfg = &ctx.cfg.metadata;
        let dir_names: Vec<String> = g.dir.as_ref().map(|d| ctx.names(d).to_vec()).unwrap_or_default();
        let nfo_path = g
            .dir
            .as_ref()
            .filter(|_| meta_cfg.nfo)
            .map(|d| d.join("tvshow.nfo"))
            .filter(|p| p.exists());
        let local_key = local_key(nfo_path.as_deref(), &[]);
        let local_changed = existing.is_none_or(|e| e.local_key.as_deref() != Some(local_key.as_str()));
        let remote_due = ctx.remote_due(existing, now);
        // A cleared local key (metadata refresh) rebuilds the metadata too.
        let reset = ctx.opts.refresh_metadata || existing.is_none_or(|e| e.local_key.is_none());

        let named = naming::parse_title_year(&g.name);
        let mut meta = match existing {
            Some(e) if !reset => e.meta.clone(),
            _ => name_meta(&named.title, named.year, &named.ids),
        };
        let nfo = read_nfo(nfo_path.as_deref());
        if let Some(n) = &nfo
            && (reset || local_changed)
        {
            meta.overlay_nfo(n);
        }
        let mut remote = None;
        let (mut fetched_at, mut matched) = existing.map_or((None, false), |e| (e.remote_fetched_at, e.remote_matched));
        if remote_due && let Some(providers) = &ctx.providers {
            match providers.series(&meta, ctx.lib).await {
                Ok(r) => {
                    fetched_at = Some(now);
                    matched = r.is_some();
                    if let Some(r) = &r {
                        meta.fill_from(&r.meta, nfo.as_ref().is_some_and(|n| n.title.is_some()));
                    }
                    remote = r;
                }
                Err(e) => tracing::warn!(series = %meta.title, error = %format!("{e:#}"), "metadata lookup failed"),
            }
        }
        meta.choose_title(ctx.lib.original_title_languages(&ctx.cfg.metadata));
        let series_ids = Some(SeriesIds::of(&meta)).filter(|ids| matched && !ids.is_empty());
        let series_title = meta.title.clone();
        let series_id = if existing.is_some() && !reset && !local_changed && remote.is_none() && !remote_due {
            let id = existing.map(|e| e.id).unwrap_or_default();
            self.store.touch_items(&[id], ctx.generation).await?;
            id
        } else {
            self.store
                .upsert_item(&ItemWrite {
                    library_id: ctx.lib.id.clone(),
                    kind: ItemKind::Series,
                    parent_id: None,
                    series_id: None,
                    path_key: key,
                    path: g.dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
                    meta,
                    season_number: None,
                    episode_number: None,
                    file_size: None,
                    file_mtime: None,
                    media: None,
                    local_key: Some(local_key),
                    remote_fetched_at: fetched_at,
                    remote_matched: matched,
                    added_at: existing.map_or(now, |e| e.added_at),
                    seen_scan: ctx.generation,
                    extra_type: None,
                })
                .await?
        };
        if let Some(dir) = &g.dir {
            let art: LocalArt = if meta_cfg.local_artwork {
                vec![
                    (
                        ImageKind::Poster,
                        find_image(dir, &dir_names, &["poster", "folder", "cover"].map(String::from)),
                    ),
                    (
                        ImageKind::Backdrop,
                        find_image(dir, &dir_names, &["fanart", "backdrop", "background"].map(String::from)),
                    ),
                    (
                        ImageKind::Logo,
                        find_image(dir, &dir_names, &["clearlogo", "logo"].map(String::from)),
                    ),
                ]
            } else {
                Vec::new()
            };
            self.artwork(
                ctx,
                series_id,
                ItemKind::Series,
                &art,
                remote.as_ref(),
                Path::new(""),
                None,
            )
            .await?;
        } else if let Some(r) = &remote {
            self.artwork(ctx, series_id, ItemKind::Series, &[], Some(r), Path::new(""), None)
                .await?;
        }

        // Seasons, then their episodes.
        let seasons: BTreeSet<i32> = g.episodes.iter().map(|e| e.season).collect();
        let mut series_added = existing.map_or(now, |e| e.added_at);
        let mut latest_episode = 0i64;
        for season in seasons {
            let parts = SeasonParts {
                series_id,
                series_identity: &g.identity,
                series_dir: g.dir.as_deref(),
                series_dir_names: &dir_names,
                series_ids: series_ids.as_ref(),
                number: season,
            };
            match self.season(ctx, &parts, g).await {
                Ok(latest) => latest_episode = latest_episode.max(latest),
                Err(e) => {
                    ctx.count(|s| s.errors += 1);
                    tracing::warn!(series = %series_title, season, error = %format!("{e:#}"), "cannot index season");
                }
            }
        }
        if latest_episode > 0 {
            series_added = series_added.max(latest_episode);
        }
        if existing.is_none_or(|e| e.content_added_at != series_added) {
            self.store.set_content_added(series_id, series_added).await?;
        }
        Ok(())
    }

    /// Index a season and its episodes; returns when an episode was last added.
    async fn season(&self, ctx: &Ctx<'_>, p: &SeasonParts<'_>, g: &SeriesGroup) -> anyhow::Result<i64> {
        let now = now_ms();
        let identity = format!("{}\0season\0{}", p.series_identity, p.number);
        let key = path_key(&ctx.lib.id, ItemKind::Season, &identity);
        let existing = ctx.existing.get(&key);
        let episodes: Vec<&EpisodeFile> = g.episodes.iter().filter(|e| e.season == p.number).collect();
        let reset = ctx.opts.refresh_metadata || existing.is_none();
        let episodes_due = episodes.iter().any(|e| {
            let k = path_key(&ctx.lib.id, ItemKind::Episode, &e.file.path.to_string_lossy());
            let ex = ctx.existing.get(&k);
            ctx.remote_due(ex, now)
                || ex.is_none_or(|x| x.file_size != Some(e.file.size) || x.file_mtime != Some(e.file.mtime))
        });
        let season_due = ctx.remote_due(existing, now);
        // Remote season data serves the season and its episodes.
        let mut remote: Option<RemoteSeason> = None;
        let mut fetched = false;
        if let (Some(providers), Some(ids)) = (&ctx.providers, p.series_ids)
            && (season_due || episodes_due)
        {
            fetched = true;
            remote = providers.season(ids, p.number, ctx.lib).await;
        }
        let default_title = if p.number == 0 {
            "Specials".to_owned()
        } else {
            format!("Season {}", p.number)
        };
        let mut meta = match existing {
            Some(e) if !reset => e.meta.clone(),
            _ => Meta {
                title: default_title,
                ..Default::default()
            },
        };
        let (mut fetched_at, mut matched) = existing.map_or((None, false), |e| (e.remote_fetched_at, e.remote_matched));
        if fetched && season_due {
            fetched_at = Some(now);
            matched = remote.is_some();
            if let Some(r) = &remote {
                // Keep "Season N": provider names vary ("Season 1", "Book One").
                let title = meta.title.clone();
                meta.fill_from(&r.meta, true);
                meta.title = title;
            }
        }
        let season_dir = episodes
            .first()
            .map(|e| parent(&e.file.path))
            .filter(|d| Some(d.as_path()) != p.series_dir);
        let season_id = self
            .store
            .upsert_item(&ItemWrite {
                library_id: ctx.lib.id.clone(),
                kind: ItemKind::Season,
                parent_id: Some(p.series_id),
                series_id: Some(p.series_id),
                path_key: key,
                path: season_dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
                meta,
                season_number: Some(p.number),
                episode_number: None,
                file_size: None,
                file_mtime: None,
                media: None,
                local_key: None,
                remote_fetched_at: fetched_at,
                remote_matched: matched,
                added_at: existing.map_or(now, |e| e.added_at),
                seen_scan: ctx.generation,
                extra_type: None,
            })
            .await?;
        if ctx.cfg.metadata.local_artwork || remote.is_some() {
            let mut art: LocalArt = Vec::new();
            if ctx.cfg.metadata.local_artwork {
                let mut candidate = None;
                if let Some(dir) = p.series_dir {
                    let bases = if p.number == 0 {
                        vec!["season-specials-poster".to_owned(), "season00-poster".to_owned()]
                    } else {
                        vec![
                            format!("season{:02}-poster", p.number),
                            format!("season{}-poster", p.number),
                        ]
                    };
                    candidate = find_image(dir, p.series_dir_names, &bases);
                }
                if candidate.is_none()
                    && let Some(d) = &season_dir
                {
                    candidate = find_image(d, ctx.names(d), &["poster", "folder", "cover"].map(String::from));
                }
                art.push((ImageKind::Poster, candidate));
            }
            let season_remote = remote.as_ref().filter(|_| fetched).map(|r| RemoteItem {
                images: r.images.clone(),
                ..Default::default()
            });
            self.artwork(
                ctx,
                season_id,
                ItemKind::Season,
                &art,
                season_remote.as_ref(),
                Path::new(""),
                None,
            )
            .await?;
        }

        let concurrency = ctx.cfg.scanner.probe_concurrency;
        let remote_ref = remote.as_ref().filter(|_| fetched);
        let mut tasks = Vec::with_capacity(episodes.len());
        for e in episodes {
            tasks.push(async move {
                match self.episode(ctx, e, p, season_id, remote_ref).await {
                    Ok(added) => added,
                    Err(err) => {
                        ctx.count(|s| s.errors += 1);
                        tracing::warn!(file = %e.file.path.display(), error = %format!("{err:#}"), "cannot index episode");
                        0
                    }
                }
            });
        }
        let added: Vec<i64> = stream::iter(tasks).buffer_unordered(concurrency).collect().await;
        let latest = added.into_iter().max().unwrap_or(0);
        if latest > 0 && existing.is_none_or(|e| e.content_added_at != latest) {
            self.store.set_content_added(season_id, latest).await?;
        }
        Ok(latest)
    }

    /// Index an episode; returns when it was added.
    async fn episode(
        &self,
        ctx: &Ctx<'_>,
        e: &EpisodeFile,
        p: &SeasonParts<'_>,
        season_id: i64,
        remote_season: Option<&RemoteSeason>,
    ) -> anyhow::Result<i64> {
        let now = now_ms();
        let f = &e.file;
        let path_str = f.path.to_string_lossy().into_owned();
        let key = path_key(&ctx.lib.id, ItemKind::Episode, &path_str);
        let existing = ctx.existing.get(&key);
        let dir = parent(&f.path);
        let names = ctx.names(&dir);
        let file_stem = stem(&f.path);
        let nfo_path = ctx
            .cfg
            .metadata
            .nfo
            .then(|| dir.join(format!("{file_stem}.nfo")))
            .filter(|p| p.exists());
        let sidecars = sidecar_subtitles(&dir, names, &file_stem);
        let local_key = local_key(nfo_path.as_deref(), &sidecars);
        let file_changed = existing.is_none_or(|x| x.file_size != Some(f.size) || x.file_mtime != Some(f.mtime));
        let local_changed = existing.is_none_or(|x| x.local_key.as_deref() != Some(local_key.as_str()));
        let remote_due = p.series_ids.is_some() && remote_season.is_some() && ctx.remote_due(existing, now);
        let moved = existing.is_some_and(|x| x.parent_id != Some(season_id) || x.episode_number != Some(e.episode));
        let local_art: LocalArt = if ctx.cfg.metadata.local_artwork {
            vec![(
                ImageKind::Thumb,
                find_image(&dir, names, &[format!("{file_stem}-thumb"), file_stem.clone()]),
            )]
        } else {
            Vec::new()
        };
        if let Some(x) = existing
            && !file_changed
            && !local_changed
            && !remote_due
            && !moved
            && !ctx.opts.refresh_metadata
        {
            self.store.touch_items(&[x.id], ctx.generation).await?;
            ctx.count(|s| s.unchanged += 1);
            let media = x.media.clone();
            self.artwork(ctx, x.id, ItemKind::Episode, &local_art, None, &f.path, media.as_ref())
                .await?;
            return Ok(x.added_at);
        }
        let media = match existing.and_then(|x| x.media.clone()) {
            Some(m) if !file_changed => m,
            _ => probe::probe(&ctx.cfg.transcoding.ffprobe, &f.path).await?,
        };
        let media = with_sidecars(media, sidecars);
        // A cleared local key (metadata refresh) rebuilds the metadata too.
        let reset = ctx.opts.refresh_metadata || existing.is_none_or(|e| e.local_key.is_none());
        let mut meta = match existing {
            Some(x) if !reset => x.meta.clone(),
            _ => Meta {
                title: e.title.clone().unwrap_or_else(|| format!("Episode {}", e.episode)),
                ..Default::default()
            },
        };
        let nfo = read_nfo(nfo_path.as_deref());
        if let Some(n) = &nfo
            && (reset || local_changed)
        {
            meta.overlay_nfo(n);
        }
        let (mut fetched_at, mut matched) = existing.map_or((None, false), |x| (x.remote_fetched_at, x.remote_matched));
        let mut remote = None;
        if remote_due && let Some(rs) = remote_season {
            fetched_at = Some(now);
            remote = rs.episodes.get(&e.episode).cloned();
            matched = remote.is_some();
            if let Some(r) = &remote {
                // A title from the file name is replaced, one from an .nfo kept.
                let keep_title = nfo.as_ref().is_some_and(|n| n.title.is_some());
                meta.fill_from(&r.meta, keep_title);
            }
        }
        if meta.runtime_ms.is_none() {
            meta.runtime_ms = media.duration_ms;
        }
        let added_at = existing.map_or(now, |x| x.added_at);
        let id = self
            .store
            .upsert_item(&ItemWrite {
                library_id: ctx.lib.id.clone(),
                kind: ItemKind::Episode,
                parent_id: Some(season_id),
                series_id: Some(p.series_id),
                path_key: key,
                path: Some(path_str),
                meta,
                season_number: Some(e.season),
                episode_number: Some(e.episode),
                file_size: Some(f.size),
                file_mtime: Some(f.mtime),
                media: Some(media.clone()),
                local_key: Some(local_key),
                remote_fetched_at: fetched_at,
                remote_matched: matched,
                added_at,
                seen_scan: ctx.generation,
                extra_type: None,
            })
            .await?;
        ctx.count(|s| {
            if existing.is_some() {
                s.updated += 1
            } else {
                s.added += 1
            }
        });
        self.artwork(
            ctx,
            id,
            ItemKind::Episode,
            &local_art,
            remote.as_ref(),
            &f.path,
            Some(&media),
        )
        .await?;
        Ok(added_at)
    }

    // ------------------------------------------------------------- artwork

    /// Bring an item's images up to date: local files win over downloaded
    /// artwork, which wins over frames extracted from the video.
    #[allow(clippy::too_many_arguments)]
    async fn artwork(
        &self,
        ctx: &Ctx<'_>,
        id: i64,
        kind: ItemKind,
        local: &[(ImageKind, Option<PathBuf>)],
        remote: Option<&RemoteItem>,
        file: &Path,
        media: Option<&MediaInfo>,
    ) -> anyhow::Result<()> {
        let cfg = &ctx.cfg.metadata;
        for image in ImageKind::ALL {
            let current = ctx.images.get(&(id, image));
            let width = artwork::max_width(image, cfg.poster_width);
            let local_file = local.iter().find(|(k, _)| *k == image).and_then(|(_, p)| p.as_ref());
            if let Some(path) = local_file {
                let Some(fp) = fingerprint(path) else { continue };
                if current.is_some_and(|c| c.source == ImageSource::Local && c.source_key == fp) {
                    continue;
                }
                match artwork::load_file(path, image, width, cfg.jpeg_quality).await {
                    Ok(p) => {
                        self.store
                            .put_image(id, image, ImageSource::Local, &fp, p.width, p.height, &p.data)
                            .await?;
                    }
                    Err(e) => tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "cannot read artwork"),
                }
                continue;
            }
            // A local image whose file is gone.
            let mut current = current;
            if current.is_some_and(|c| c.source == ImageSource::Local) && local.iter().any(|(k, _)| *k == image) {
                self.store.delete_image(id, image).await?;
                current = None;
            }
            // Downloaded artwork, in the providers' order of preference.
            let candidates: Vec<&str> = remote
                .map(|r| {
                    r.images
                        .iter()
                        .filter(|i| i.kind == image)
                        .map(|i| i.url.as_str())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(first) = candidates.first() {
                if current.is_some_and(|c| c.source_key == *first || c.source == ImageSource::Local) {
                    continue;
                }
                let http = ctx.providers.as_ref().map(|p| p.http.clone()).unwrap_or_default();
                for url in &candidates {
                    let bytes = match crate::metadata::download(&http, url).await {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(image = %url, error = %format!("{e:#}"), "cannot download artwork");
                            continue;
                        }
                    };
                    let quality = cfg.jpeg_quality;
                    match tokio::task::spawn_blocking(move || artwork::process_for(image, &bytes, width, quality))
                        .await?
                    {
                        Ok(p) => {
                            self.store
                                .put_image(id, image, ImageSource::Remote, url, p.width, p.height, &p.data)
                                .await?;
                            break;
                        }
                        Err(e) => tracing::warn!(image = %url, error = %format!("{e:#}"), "cannot decode artwork"),
                    }
                }
                continue;
            }
            // Last resort: a frame of the video, for episodes and movies.
            let wants_frame =
                image == ImageKind::Thumb && kind.playable() && cfg.extract_thumbnails && current.is_none();
            if let (true, Some(m)) = (wants_frame, media)
                && m.video.is_some()
            {
                let at = m.duration_ms.map_or(30_000, |d| (d / 10).min(300_000));
                let source_key = format!("frame:{at}");
                match artwork::extract_frame(&ctx.cfg.transcoding.ffmpeg, file, at, width).await {
                    Ok(bytes) => {
                        let quality = cfg.jpeg_quality;
                        if let Ok(p) =
                            tokio::task::spawn_blocking(move || artwork::process(&bytes, width, quality)).await?
                        {
                            self.store
                                .put_image(
                                    id,
                                    image,
                                    ImageSource::Extracted,
                                    &source_key,
                                    p.width,
                                    p.height,
                                    &p.data,
                                )
                                .await?;
                        }
                    }
                    Err(e) => tracing::debug!(file = %file.display(), error = %format!("{e:#}"), "no thumbnail"),
                }
            }
        }
        Ok(())
    }
}

struct SeriesGroup {
    identity: String,
    name: String,
    dir: Option<PathBuf>,
    episodes: Vec<EpisodeFile>,
}

struct EpisodeFile {
    file: VideoFile,
    season: i32,
    episode: i32,
    title: Option<String>,
}

struct SeasonParts<'a> {
    series_id: i64,
    series_identity: &'a str,
    series_dir: Option<&'a Path>,
    series_dir_names: &'a [String],
    series_ids: Option<&'a SeriesIds>,
    number: i32,
}

fn name_meta(title: &str, year: Option<i32>, ids: &ProviderIds) -> Meta {
    Meta {
        title: title.to_owned(),
        year,
        tmdb_id: ids.tmdb.clone(),
        imdb_id: ids.imdb.clone(),
        tvdb_id: ids.tvdb.clone(),
        ..Default::default()
    }
}

/// Fingerprint of the local metadata of an item: its `.nfo` and sidecar files.
fn local_key(nfo: Option<&Path>, sidecars: &[SubtitleStream]) -> String {
    let mut parts = vec![nfo.and_then(fingerprint).unwrap_or_default()];
    parts.extend(
        sidecars
            .iter()
            .filter_map(|s| s.path.as_deref().map(Path::new).and_then(fingerprint)),
    );
    sha256_hex(parts.join("\n").as_bytes())[..32].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_finds_extras_and_skips_hidden_samples_and_dot_ignored_files() {
        let root = std::env::temp_dir().join(format!("fc-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for p in [
            "Alien (1979)/Alien (1979).mkv",
            "Alien (1979)/Alien (1979).en.srt",
            "Alien (1979)/poster.jpg",
            "Alien (1979)/Extras/Making of.mkv",
            "Alien (1979)/Alien (1979)-trailer.mp4",
            "Alien (1979)/Samples/sample.mkv",
            "Alien (1979)/alien-sample.mkv",
            ".hidden/x.mkv",
            "notes.txt",
            // A show in a movie library, hidden by an empty .ignore.
            "Some Show/.ignore",
            "Some Show/Season 01/Some Show - S01E01.mkv",
        ] {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            let content: &[u8] = if p.ends_with(".ignore") { b"" } else { b"x" };
            std::fs::write(&f, content).unwrap();
        }
        let w = walk(std::slice::from_ref(&root)).unwrap();
        let dir = root.join("Alien (1979)");
        let found: Vec<(String, Option<&str>)> = w
            .files
            .iter()
            .map(|f| {
                assert!(f.extra.as_ref().is_none_or(|(_, movie)| *movie == dir));
                (
                    f.path.file_name().unwrap().to_string_lossy().into_owned(),
                    f.extra.as_ref().map(|e| e.0),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("Alien (1979)-trailer.mp4".into(), Some("trailer")),
                ("Alien (1979).mkv".into(), None),
                ("Making of.mkv".into(), Some("extra")),
            ]
        );
        assert!(!w.incomplete);
        let names = &w.dirs[&dir];
        let subs = sidecar_subtitles(&dir, names, "Alien (1979)");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].language.as_deref(), Some("en"));
        assert_eq!(subs[0].index, EXTERNAL_SUBTITLE_BASE);
        assert!(walk(&[root.join("missing")]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_keys_differ_by_kind_and_library() {
        let a = path_key("tv", ItemKind::Series, "/x");
        assert_ne!(a, path_key("tv", ItemKind::Season, "/x"));
        assert_ne!(a, path_key("tv2", ItemKind::Series, "/x"));
        assert_eq!(a.len(), 64);
    }
}
