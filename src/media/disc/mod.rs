//! Discs: DVD folders (`VIDEO_TS`), DVD images (`.iso`) and Blu-ray folders
//! (`BDMV`).
//!
//! A DVD is read through its information files (see [`ifo`]): the disc's
//! titles, each a list of cells (runs of sectors in its title set's VOBs)
//! with their play times, chapters, audio and subtitle languages. The
//! longest title is the movie, the other titles of a minute or more its
//! extras, and in a show library its titles are episodes. An item's path
//! names the disc and the title, `…/VIDEO_TS#3` or `…/Disc.iso#3`, and
//! ffmpeg reads exactly the title's bytes: `subfile` ranges of the VOBs (or
//! of the image) joined by `concat:`. DVD timestamps restart along a title,
//! so the duration comes from the cells, and playback starts at the bytes of
//! the cell playing at the wanted time, never `-ss`: the input itself starts
//! there (`-skip_initial_bytes` does not pass through `subfile`). Cells under
//! two seconds at either end of a title (logos, warnings, whose nominal play
//! time may hide megabytes of stills) are left out.
//!
//! Without `VIDEO_TS.IFO`, the title set with the most data is the movie,
//! read whole (its IFO, if any, still gives the duration and the start
//! bytes). The feature of a Blu-ray is the largest file of `BDMV/STREAM`.

pub mod ifo;
pub mod iso;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static VOB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^VTS_(\d{2})_(\d)\.VOB$").unwrap());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// Part `part` (0: the menu) of DVD title set `set`.
    Dvd { set: u32, part: u32 },
    /// A DVD menu outside the title sets (`VIDEO_TS.VOB`).
    DvdMenu,
    /// A stream file of a Blu-ray.
    Bluray,
}

fn name_is(path: Option<&Path>, name: &str) -> bool {
    path.and_then(Path::file_name)
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
}

/// What a video file is on a disc, if it is part of one.
pub fn classify(path: &Path) -> Option<Part> {
    let name = path.file_name()?.to_string_lossy();
    if let Some(c) = VOB.captures(&name) {
        return Some(Part::Dvd {
            set: c[1].parse().ok()?,
            part: c[2].parse().ok()?,
        });
    }
    if name.eq_ignore_ascii_case("VIDEO_TS.VOB") {
        return Some(Part::DvdMenu);
    }
    let stream = path.parent();
    let is_m2ts = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m2ts"));
    (is_m2ts && name_is(stream, "STREAM") && name_is(stream.and_then(Path::parent), "BDMV")).then_some(Part::Bluray)
}

/// The folder of the movie a disc file belongs to: the one holding
/// `VIDEO_TS` or `BDMV` (or the VOBs themselves, for a DVD copied without its
/// `VIDEO_TS` folder).
pub fn movie_dir(path: &Path, part: Part) -> PathBuf {
    let dir = path.parent().unwrap_or(path);
    let up = match part {
        Part::Dvd { .. } | Part::DvdMenu if name_is(Some(dir), "VIDEO_TS") => dir.parent(),
        Part::Dvd { .. } | Part::DvdMenu => Some(dir),
        // STREAM, then BDMV.
        Part::Bluray => dir.parent().and_then(Path::parent),
    };
    up.unwrap_or(dir).to_path_buf()
}

/// A disc file met during a scan.
#[derive(Clone, Debug)]
pub struct DiscFile {
    pub path: PathBuf,
    pub part: Part,
    pub size: i64,
    pub mtime: i64,
}

/// The feature of a disc.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    /// Its first file.
    pub path: PathBuf,
    /// Of all its files.
    pub size: i64,
    pub mtime: i64,
    pub movie_dir: PathBuf,
}

/// The feature of every disc among `files`.
pub fn features(files: Vec<DiscFile>) -> Vec<Feature> {
    // Per folder: DVD title sets (parts in order), Blu-ray stream files.
    let mut dvd: BTreeMap<PathBuf, BTreeMap<u32, Vec<DiscFile>>> = BTreeMap::new();
    let mut bluray: BTreeMap<PathBuf, Vec<DiscFile>> = BTreeMap::new();
    for f in files {
        let dir = f.path.parent().unwrap_or(&f.path).to_path_buf();
        match f.part {
            Part::Dvd { set, part } if part > 0 => dvd.entry(dir).or_default().entry(set).or_default().push(f),
            Part::Dvd { .. } | Part::DvdMenu => {}
            Part::Bluray => bluray.entry(dir).or_default().push(f),
        }
    }
    let mut out = Vec::new();
    for sets in dvd.into_values() {
        let total = |parts: &Vec<DiscFile>| parts.iter().map(|p| p.size).sum::<i64>();
        // The most data; the first title set on a tie.
        let Some(mut parts) = sets.into_values().rev().max_by_key(total) else {
            continue;
        };
        parts.sort_by_key(|p| match p.part {
            Part::Dvd { part, .. } => part,
            _ => 0,
        });
        out.push(Feature {
            path: parts[0].path.clone(),
            size: total(&parts),
            mtime: parts.iter().map(|p| p.mtime).max().unwrap_or_default(),
            movie_dir: movie_dir(&parts[0].path, parts[0].part),
        });
    }
    for files in bluray.into_values() {
        if let Some(f) = files.into_iter().max_by_key(|f| f.size) {
            out.push(Feature {
                movie_dir: movie_dir(&f.path, f.part),
                path: f.path,
                size: f.size,
                mtime: f.mtime,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The first part of a DVD title set read whole (a DVD without
/// `VIDEO_TS.IFO`).
pub fn is_title_set(path: &Path) -> bool {
    matches!(classify(path), Some(Part::Dvd { part: 1.., .. }))
}

/// Whether an item's path is a DVD title (or a whole title set): not a file
/// a client can play as is.
pub fn is_dvd(path: &Path) -> bool {
    locate(path).is_some() || is_title_set(path)
}

/// The parts of the DVD title set starting at `path`, in order.
fn dvd_parts(path: &Path) -> Vec<PathBuf> {
    let (Some(Part::Dvd { set, .. }), Some(dir)) = (classify(path), path.parent()) else {
        return vec![path.to_path_buf()];
    };
    let mut parts: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| match classify(&p) {
            Some(Part::Dvd { set: s, part }) if s == set && part > 0 => Some((part, p)),
            _ => None,
        })
        .collect();
    parts.sort();
    if parts.is_empty() {
        return vec![path.to_path_buf()];
    }
    parts.into_iter().map(|(_, p)| p).collect()
}

/// The parts of a title set read whole, joined.
fn title_set_input(path: &Path) -> String {
    let parts: Vec<String> = dvd_parts(path)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    // `|` separates the parts: a path holding one cannot be listed.
    if parts.len() < 2 || parts.iter().any(|p| p.contains('|')) {
        return path.to_string_lossy().into_owned();
    }
    format!("concat:{}", parts.join("|"))
}

// ------------------------------------------------------------ DVD titles

/// Where a DVD's files are.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A folder holding `VIDEO_TS.IFO` and the VOBs (usually `VIDEO_TS`).
    Folder(PathBuf),
    /// A disc image.
    Image(PathBuf),
}

impl Source {
    pub fn path(&self) -> &Path {
        match self {
            Source::Folder(p) | Source::Image(p) => p,
        }
    }

    /// The item path of title `number` (1-based).
    pub fn title_path(&self, number: u32) -> PathBuf {
        PathBuf::from(format!("{}#{number}", self.path().display()))
    }

    /// The folder of the movie: the one holding `VIDEO_TS`, or the image.
    pub fn movie_dir(&self) -> PathBuf {
        let p = self.path();
        let dir = match self {
            Source::Folder(_) if name_is(Some(p), "VIDEO_TS") => p.parent(),
            Source::Folder(_) => Some(p),
            Source::Image(_) => p.parent(),
        };
        dir.unwrap_or(p).to_path_buf()
    }
}

/// The disc and title an item path names (`…/VIDEO_TS#3`, `…/Film.iso#3`).
pub fn locate(path: &Path) -> Option<(Source, u32)> {
    let s = path.to_str()?;
    let (disc, number) = s.rsplit_once('#')?;
    let number: u32 = number.parse().ok().filter(|n| *n > 0)?;
    let disc = PathBuf::from(disc);
    let is_iso = disc.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"));
    if is_iso {
        Some((Source::Image(disc), number))
    } else if disc.is_dir() {
        Some((Source::Folder(disc), number))
    } else {
        None
    }
}

/// A byte range of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub path: PathBuf,
    pub offset: u64,
    pub len: u64,
}

/// A stream of a title, as the disc describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// The MPEG stream id, as ffprobe reports it (`0x80` for the first AC-3).
    pub id: u32,
    pub language: Option<String>,
    /// "Commentary", "Audio description", "Captions"…
    pub label: Option<String>,
    pub forced: bool,
}

/// A title of a DVD.
#[derive(Clone, Debug, PartialEq)]
pub struct Title {
    /// 1-based, as the disc numbers it.
    pub number: u32,
    pub duration_ms: i64,
    /// Start of each chapter.
    pub chapters: Vec<i64>,
    pub audio: Vec<Track>,
    pub subtitles: Vec<Track>,
    /// Subtitle colours, RGB.
    pub palette: [u32; 16],
    segments: Vec<Segment>,
    /// Per cell: start time, duration, first byte in the title, bytes.
    cells: Vec<(i64, i64, u64, u64)>,
    /// (title set, first sector) of each cell: which titles share cells.
    keys: Vec<(u8, u64)>,
}

impl Title {
    pub fn bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.len).sum()
    }

    /// The byte of the title where playback at `ms` starts: in the cell
    /// playing then, in proportion to its time, on a sector boundary.
    pub fn offset_at(&self, ms: i64) -> u64 {
        let Some(&(start, duration, first, bytes)) = self.cells.iter().find(|c| ms < c.0 + c.1).or(self.cells.last())
        else {
            return 0;
        };
        let into = (ms - start).clamp(0, duration.max(1)) as u128;
        let at = (u128::from(bytes) * into / duration.max(1) as u128) as u64;
        let at = at.min(bytes.saturating_sub(1));
        first + at / ifo::SECTOR * ifo::SECTOR
    }

    /// ffmpeg's input for the title's bytes.
    pub fn input(&self) -> String {
        self.input_from(0)
    }

    /// ffmpeg's input for the title's bytes from `skip` on: the ranges
    /// themselves start there (`-skip_initial_bytes` does not pass through
    /// `subfile`).
    pub fn input_from(&self, skip: u64) -> String {
        let mut segments = Vec::with_capacity(self.segments.len());
        let mut left = skip;
        for s in &self.segments {
            if left >= s.len {
                left -= s.len;
                continue;
            }
            segments.push(Segment {
                path: s.path.clone(),
                offset: s.offset + left,
                len: s.len - left,
            });
            left = 0;
        }
        let one = |s: &Segment| {
            format!(
                "subfile,,start,{},end,{},,:{}",
                s.offset,
                s.offset + s.len,
                s.path.display()
            )
        };
        match segments.as_slice() {
            [] => self.segments.last().map(one).unwrap_or_default(),
            [s] => one(s),
            many => format!("concat:{}", many.iter().map(one).collect::<Vec<_>>().join("|")),
        }
    }

    /// Whether every cell of this title is a cell of `other`.
    fn within(&self, other: &Title) -> bool {
        self.keys.iter().all(|k| other.keys.contains(k))
    }
}

/// A DVD's files by name: (file, offset, length).
struct Files(HashMap<String, (PathBuf, u64, u64)>);

impl Files {
    fn open(source: &Source) -> std::io::Result<Option<Files>> {
        let mut map = HashMap::new();
        match source {
            Source::Folder(dir) => {
                for e in std::fs::read_dir(dir)?.flatten() {
                    let name = e.file_name().to_string_lossy().to_uppercase();
                    if let Ok(m) = e.metadata()
                        && m.is_file()
                    {
                        map.insert(name, (e.path(), 0, m.len()));
                    }
                }
            }
            Source::Image(path) => {
                let Some(files) = iso::video_ts(path)? else {
                    return Ok(None);
                };
                for f in files {
                    map.insert(f.name, (path.clone(), f.offset, f.len));
                }
            }
        }
        Ok(map.contains_key("VIDEO_TS.IFO").then_some(Files(map)))
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let (path, offset, len) = self.0.get(name)?;
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(*offset)).ok()?;
        let mut buf = vec![0u8; usize::try_from(*len).ok()?.min(16 << 20)];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// A title set's IFO, or its backup.
    fn title_set(&self, vts: u8) -> Option<ifo::TitleSet> {
        [format!("VTS_{vts:02}_0.IFO"), format!("VTS_{vts:02}_0.BUP")]
            .iter()
            .find_map(|n| self.read(n).as_deref().and_then(ifo::parse_vts))
    }

    /// The VOBs of a title set's titles, in order: where each byte of the
    /// title set lies.
    fn title_set_vobs(&self, vts: u8) -> Vec<Segment> {
        (1..=9)
            .map_while(|part| self.0.get(&format!("VTS_{vts:02}_{part}.VOB")))
            .map(|(path, offset, len)| Segment {
                path: path.clone(),
                offset: *offset,
                len: *len,
            })
            .collect()
    }
}

/// Bytes `from..to` of a title set laid out over its VOBs.
fn slice(vobs: &[Segment], from: u64, to: u64, out: &mut Vec<Segment>) {
    let mut base = 0;
    for v in vobs {
        let (start, end) = (from.max(base), to.min(base + v.len));
        if start < end {
            let offset = v.offset + start - base;
            match out.last_mut() {
                Some(last) if last.path == v.path && last.offset + last.len == offset => last.len += end - start,
                _ => out.push(Segment {
                    path: v.path.clone(),
                    offset,
                    len: end - start,
                }),
            }
        }
        base += v.len;
    }
}

fn audio_label(extension: u8) -> Option<String> {
    match extension {
        2 => Some("Audio description".into()),
        3 | 4 => Some("Commentary".into()),
        _ => None,
    }
}

fn subtitle_label(extension: u8) -> Option<String> {
    match extension {
        5..=7 => Some("Captions".into()),
        9 => Some("Forced".into()),
        13..=15 => Some("Commentary".into()),
        _ => None,
    }
}

/// Title `number` of a title set: its chains' cells and chapters.
fn build_title(
    number: u32,
    vts: u8,
    set: &ifo::TitleSet,
    chapters: &[(usize, usize)],
    vobs: &[Segment],
) -> Option<Title> {
    // The program chains of the title, in order of their chapters.
    let mut chains: Vec<usize> = Vec::new();
    for &(pgc, _) in chapters {
        if chains.last() != Some(&pgc) && !chains.contains(&pgc) {
            chains.push(pgc);
        }
    }
    // Cells under two seconds at either end of the title: logos and
    // warnings, whose nominal play time may hide megabytes of stills.
    let all: Vec<(usize, usize)> = chains
        .iter()
        .filter_map(|&c| Some((c, set.chains.get(c.checked_sub(1)?)?.cells.len())))
        .flat_map(|(c, n)| (0..n).map(move |i| (c, i)))
        .collect();
    let short = |&&(c, i): &&(usize, usize)| set.chains[c - 1].cells[i].duration_ms < 2_000;
    let lead = all.iter().take_while(short).count();
    let tail = all.iter().rev().take_while(short).count();
    let dropped: std::collections::HashSet<(usize, usize)> = if lead + tail < all.len() {
        all[..lead].iter().chain(&all[all.len() - tail..]).copied().collect()
    } else {
        Default::default()
    };
    let mut segments = Vec::new();
    let mut cells = Vec::new();
    let mut keys = Vec::new();
    // Per chain: the index in `cells` of each of its cells (a dropped one:
    // of the next kept), for chapter starts.
    let mut cell_index: HashMap<usize, Vec<usize>> = HashMap::new();
    let (mut ms, mut bytes) = (0i64, 0u64);
    for &c in &chains {
        let chain = set.chains.get(c.checked_sub(1)?)?;
        for (i, cell) in chain.cells.iter().enumerate() {
            cell_index.entry(c).or_default().push(cells.len());
            if dropped.contains(&(c, i)) {
                continue;
            }
            let (from, to) = (cell.first_sector * ifo::SECTOR, (cell.last_sector + 1) * ifo::SECTOR);
            let before = segments.iter().map(|s: &Segment| s.len).sum::<u64>();
            slice(vobs, from, to, &mut segments);
            let len = segments.iter().map(|s| s.len).sum::<u64>() - before;
            cells.push((ms, cell.duration_ms, bytes, len));
            keys.push((vts, cell.first_sector));
            ms += cell.duration_ms;
            bytes += len;
        }
    }
    let first = set.chains.get(chains.first()?.checked_sub(1)?)?;
    if cells.is_empty() || bytes == 0 {
        return None;
    }
    let mut chapter_starts: Vec<i64> = chapters
        .iter()
        .filter_map(|&(pgc, program)| {
            let cell = set.chains.get(pgc - 1)?.program_cells.get(program.checked_sub(1)?)?;
            let index = *cell_index.get(&pgc)?.get(*cell)?;
            cells.get(index).map(|c| c.0)
        })
        .collect();
    // Chapters on dropped cells collapse onto the next one.
    chapter_starts.dedup();
    let audio = set
        .audio
        .iter()
        .enumerate()
        .filter_map(|(i, info)| {
            let physical = u32::from((*first.audio.get(i)?)?);
            let id = match info.coding {
                0 => 0x80 + physical,
                2 | 3 => 0x1C0 + physical,
                4 => 0xA0 + physical,
                6 => 0x88 + physical,
                _ => return None,
            };
            Some(Track {
                id,
                language: info.language.clone(),
                label: audio_label(info.extension),
                forced: false,
            })
        })
        .collect();
    let subtitles = set
        .subpictures
        .iter()
        .enumerate()
        .filter_map(|(i, info)| {
            Some(Track {
                id: 0x20 + u32::from((*first.subpictures.get(i)?)?),
                language: info.language.clone(),
                label: subtitle_label(info.extension),
                forced: info.extension == 9,
            })
        })
        .collect();
    Some(Title {
        number,
        duration_ms: ms,
        chapters: chapter_starts,
        audio,
        subtitles,
        palette: first.palette,
        segments,
        cells,
        keys,
    })
}

/// A DVD: its titles, in the disc's order.
#[derive(Clone, Debug)]
pub struct Dvd {
    pub source: Source,
    pub titles: Vec<Title>,
}

/// Extras are titles of a minute or more.
const EXTRA_MIN_MS: i64 = 60_000;
/// Episodes are titles of five minutes or more.
const EPISODE_MIN_MS: i64 = 300_000;

impl Dvd {
    /// Read a DVD's titles; `None` when `source` holds no DVD-Video.
    pub fn open(source: &Source) -> std::io::Result<Option<Dvd>> {
        let Some(files) = Files::open(source)? else {
            return Ok(None);
        };
        let Some(vmg) = files.read("VIDEO_TS.IFO").as_deref().and_then(ifo::parse_vmg) else {
            return Ok(None);
        };
        let mut sets: HashMap<u8, Option<(ifo::TitleSet, Vec<Segment>)>> = HashMap::new();
        let mut titles: Vec<Title> = Vec::new();
        for (i, t) in vmg.iter().enumerate() {
            let entry = sets
                .entry(t.vts)
                .or_insert_with(|| files.title_set(t.vts).map(|s| (s, files.title_set_vobs(t.vts))));
            let Some((set, vobs)) = entry else { continue };
            let Some(chapters) = set.titles.get(usize::from(t.vts_title).wrapping_sub(1)) else {
                continue;
            };
            if let Some(title) = build_title(i as u32 + 1, t.vts, set, chapters, vobs)
                // The same cells under another number add nothing.
                && !titles.iter().any(|o| o.keys == title.keys)
            {
                titles.push(title);
            }
        }
        Ok((!titles.is_empty()).then(|| Dvd {
            source: source.clone(),
            titles,
        }))
    }

    pub fn title(&self, number: u32) -> Option<&Title> {
        self.titles.iter().find(|t| t.number == number)
    }

    /// The movie: the longest title.
    pub fn feature(&self) -> Option<&Title> {
        self.titles
            .iter()
            .max_by_key(|t| (t.duration_ms, std::cmp::Reverse(t.number)))
    }

    /// The other titles worth watching: a minute or more, with sound (not a
    /// still gallery or a looping menu), and not a part of the movie (scene
    /// selections replay its cells).
    pub fn extras(&self) -> Vec<&Title> {
        let Some(feature) = self.feature() else {
            return Vec::new();
        };
        self.titles
            .iter()
            .filter(|t| {
                t.number != feature.number && t.duration_ms >= EXTRA_MIN_MS && !t.audio.is_empty() && !t.within(feature)
            })
            .collect()
    }

    /// The episodes of a TV disc, in order: titles of five minutes or more
    /// with sound, without "play all" titles (those replaying two others' cells).
    pub fn episodes(&self) -> Vec<&Title> {
        let long: Vec<&Title> = self
            .titles
            .iter()
            .filter(|t| t.duration_ms >= EPISODE_MIN_MS && !t.audio.is_empty())
            .collect();
        long.iter()
            .filter(|t| {
                long.iter()
                    .filter(|o| o.number != t.number && o.within(t) && o.keys.len() < t.keys.len())
                    .count()
                    < 2
            })
            .copied()
            .collect()
    }

    /// Every byte of the disc's files: the last modification wins.
    pub fn mtime(&self) -> i64 {
        let paths: Vec<&Path> = self
            .titles
            .iter()
            .flat_map(|t| t.segments.iter().map(|s| s.path.as_path()))
            .collect();
        paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| mtime_ms(&m))
            .max()
            .unwrap_or_default()
    }
}

fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// The title an item path names.
pub fn title(path: &Path) -> Option<Title> {
    match locate(path) {
        Some((source, number)) => Dvd::open(&source).ok()??.title(number).cloned(),
        None if is_title_set(path) => title_set_title(path),
        None => None,
    }
}

/// A title set read whole (no `VIDEO_TS.IFO`): its longest program chain,
/// over all its bytes, when its IFO is there.
fn title_set_title(path: &Path) -> Option<Title> {
    let Some(Part::Dvd { set, .. }) = classify(path) else {
        return None;
    };
    let dir = path.parent()?;
    let files = Files(
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let len = e.metadata().ok()?.len();
                Some((e.file_name().to_string_lossy().to_uppercase(), (e.path(), 0, len)))
            })
            .collect(),
    );
    let vts = u8::try_from(set).ok()?;
    let ts = files.title_set(vts)?;
    let longest = (1..=ts.chains.len()).max_by_key(|&c| ts.chains[c - 1].duration_ms)?;
    let chapters: Vec<(usize, usize)> = (1..=ts.chains[longest - 1].program_cells.len().max(1))
        .map(|p| (longest, p))
        .collect();
    build_title(1, vts, &ts, &chapters, &files.title_set_vobs(vts))
}

/// ffmpeg's input arguments for an item's path, starting at `start_ms`: up
/// to and including `-i`. A DVD title starts at the bytes of that time.
pub fn input_args(path: &Path, start_ms: i64) -> Vec<String> {
    let mut a = Vec::new();
    if is_dvd(path)
        && let Some(t) = title(path)
    {
        a.push("-i".into());
        a.push(t.input_from(t.offset_at(start_ms)));
        return a;
    }
    if start_ms > 0 {
        a.push("-ss".into());
        a.push(format!("{:.3}", start_ms as f64 / 1000.0));
    }
    a.push("-i".into());
    a.push(if is_title_set(path) {
        title_set_input(path)
    } else {
        path.to_string_lossy().into_owned()
    });
    a
}

/// ffmpeg's input (`-i`) for an item's path, from its start.
pub fn ffmpeg_input(path: &Path) -> String {
    if locate(path).is_some()
        && let Some(t) = title(path)
    {
        return t.input();
    }
    if is_title_set(path) {
        return title_set_input(path);
    }
    path.to_string_lossy().into_owned()
}

/// The colours of a DVD title's subtitles, as ffmpeg's `palette` option
/// wants them (`rrggbb,…`, 16 of them).
pub fn subtitle_palette(path: &Path) -> Option<String> {
    let t = title(path)?;
    Some(
        t.palette
            .iter()
            .map(|c| format!("{c:06x}"))
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// A name for the item's file: the movie folder (or image) of a disc.
pub fn display_name(path: &Path) -> Option<String> {
    if let Some((source, _)) = locate(path) {
        let p = match &source {
            Source::Image(p) => p.clone(),
            Source::Folder(_) => source.movie_dir(),
        };
        return p.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    let part = classify(path)?;
    movie_dir(path, part)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::ifo::build::{Chain, vmg, vts};
    use super::*;

    fn f(path: &str, size: i64) -> DiscFile {
        let path = PathBuf::from(path);
        DiscFile {
            part: classify(&path).unwrap(),
            path,
            size,
            mtime: size,
        }
    }

    #[test]
    fn classifies_disc_files() {
        let p = |s: &str| classify(Path::new(s));
        assert_eq!(p("/m/A/VIDEO_TS/VTS_01_1.VOB"), Some(Part::Dvd { set: 1, part: 1 }));
        assert_eq!(p("/m/A/VIDEO_TS/vts_03_0.vob"), Some(Part::Dvd { set: 3, part: 0 }));
        assert_eq!(p("/m/A/VIDEO_TS/VIDEO_TS.VOB"), Some(Part::DvdMenu));
        assert_eq!(p("/m/B/BDMV/STREAM/00001.m2ts"), Some(Part::Bluray));
        assert_eq!(p("/m/B/Movie.m2ts"), None);
        assert_eq!(p("/m/C/Movie.vob"), None);
        assert_eq!(
            movie_dir(Path::new("/m/A/VIDEO_TS/VTS_01_1.VOB"), Part::Dvd { set: 1, part: 1 }),
            Path::new("/m/A")
        );
        assert_eq!(
            movie_dir(Path::new("/m/B/BDMV/STREAM/00001.m2ts"), Part::Bluray),
            Path::new("/m/B")
        );
    }

    #[test]
    fn the_feature_is_the_biggest_title_set() {
        let d = "/m/A Date with Judy (1948) NTSC DVD9/VIDEO_TS";
        let files = vec![
            f(&format!("{d}/VIDEO_TS.VOB"), 10),
            f(&format!("{d}/VTS_01_0.VOB"), 2_994_176),
            f(&format!("{d}/VTS_01_2.VOB"), 1_073_739_776),
            f(&format!("{d}/VTS_01_1.VOB"), 1_073_739_776),
            f(&format!("{d}/VTS_03_1.VOB"), 761_888_768),
            f("/m/B/BDMV/STREAM/00000.m2ts", 5),
            f("/m/B/BDMV/STREAM/00800.m2ts", 9_000),
        ];
        let got = features(files);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, Path::new(&format!("{d}/VTS_01_1.VOB")));
        assert_eq!(got[0].size, 1_073_739_776 * 2);
        assert_eq!(got[0].movie_dir, Path::new("/m/A Date with Judy (1948) NTSC DVD9"));
        assert_eq!(got[1].path, Path::new("/m/B/BDMV/STREAM/00800.m2ts"));
    }

    /// A DVD: title set 1 holds the film (two VOBs, chapters at cells 1 and
    /// 3), a "scene selection" replaying the film's 2nd cell and a 10 s
    /// logo; title set 2 an extra of 2 minutes.
    fn dvd_files() -> Vec<(&'static str, Vec<u8>)> {
        let set1 = vts(
            &[
                Chain {
                    cells: vec![(600, 0, 9), (600, 10, 19), (600, 20, 29)]
                        .into_iter()
                        .map(|(s, a, b)| (s, a, b, 0))
                        .collect(),
                    chapters: vec![1, 3],
                },
                Chain {
                    cells: vec![(600, 10, 19, 0)],
                    chapters: vec![1],
                },
                Chain {
                    cells: vec![(10, 30, 30, 0)],
                    chapters: vec![1],
                },
            ],
            &["en", "fr"],
            &["en"],
        );
        let set2 = vts(
            &[Chain {
                cells: vec![(120, 0, 4, 0)],
                chapters: vec![1],
            }],
            &["en"],
            &[],
        );
        vec![
            ("VIDEO_TS.IFO", vmg(&[(1, 1), (1, 2), (1, 3), (2, 1), (1, 1)])),
            ("VTS_01_0.IFO", set1),
            // 31 sectors over two VOBs: the second cell straddles them.
            ("VTS_01_1.VOB", vec![1u8; 15 * 2048]),
            ("VTS_01_2.VOB", vec![2u8; 16 * 2048]),
            ("VTS_02_0.IFO", set2),
            ("VTS_02_1.VOB", vec![3u8; 5 * 2048]),
        ]
    }

    fn check(dvd: &Dvd, film_input: &str) {
        // Title 5 repeats title 1: left out.
        assert_eq!(dvd.titles.iter().map(|t| t.number).collect::<Vec<_>>(), [1, 2, 3, 4]);
        let film = dvd.feature().unwrap();
        assert_eq!((film.number, film.duration_ms, film.bytes()), (1, 1_800_000, 30 * 2048));
        assert_eq!(film.chapters, [0, 1_200_000]);
        let langs: Vec<_> = film.audio.iter().map(|a| (a.id, a.language.clone())).collect();
        assert_eq!(langs, [(0x80, Some("en".into())), (0x81, Some("fr".into()))]);
        assert_eq!(film.subtitles[0].id, 0x20);
        assert_eq!(film.palette[1], 0xEBEBEB);
        assert_eq!(film.input(), film_input);
        assert_eq!(film.offset_at(0), 0);
        assert_eq!(film.offset_at(900_000), 15 * 2048);
        assert_eq!(film.offset_at(99_000_000), 29 * 2048);
        // The scene selection replays the film; the logo is too short.
        assert_eq!(dvd.extras().iter().map(|t| t.number).collect::<Vec<_>>(), [4]);
        // Episodes: the film and the scene selection (10 min); no play-all here.
        assert_eq!(dvd.episodes().iter().map(|t| t.number).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn reads_a_dvd_folder() {
        let dir = std::env::temp_dir().join(format!("fc-dvd-{}", std::process::id()));
        let vts_dir = dir.join("Film (1999)").join("VIDEO_TS");
        std::fs::create_dir_all(&vts_dir).unwrap();
        for (name, data) in dvd_files() {
            std::fs::write(vts_dir.join(name.to_lowercase()), data).unwrap();
        }
        let source = Source::Folder(vts_dir.clone());
        let dvd = Dvd::open(&source).unwrap().unwrap();
        // Cell 2 straddles the VOBs: two ranges, the first ending the first VOB.
        let v = |n: &str| vts_dir.join(n).display().to_string();
        check(
            &dvd,
            &format!(
                "concat:subfile,,start,0,end,30720,,:{}|subfile,,start,0,end,30720,,:{}",
                v("vts_01_1.vob"),
                v("vts_01_2.vob")
            ),
        );
        assert_eq!(source.movie_dir(), dir.join("Film (1999)"));
        let item = source.title_path(4);
        assert_eq!(locate(&item), Some((source.clone(), 4)));
        assert!(is_dvd(&item));
        assert_eq!(title(&item).unwrap().duration_ms, 120_000);
        assert_eq!(display_name(&item).as_deref(), Some("Film (1999)"));
        let args = input_args(&source.title_path(1), 900_000);
        // At 15 min: the start of the second VOB, the first one done.
        assert_eq!(
            args,
            [
                "-i".to_owned(),
                format!(
                    "subfile,,start,0,end,30720,,:{}",
                    vts_dir.join("vts_01_2.vob").display()
                )
            ]
        );
        assert!(locate(Path::new("/nowhere/VIDEO_TS#1")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_a_dvd_image() {
        let path = std::env::temp_dir().join(format!("fc-dvd-{}.iso", std::process::id()));
        let files = dvd_files();
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
        std::fs::write(&path, iso::build::image(&refs)).unwrap();
        let source = Source::Image(path.clone());
        let dvd = Dvd::open(&source).unwrap().unwrap();
        // In the image the two VOBs are back to back: one range.
        let extents = iso::video_ts(&path).unwrap().unwrap();
        let start = extents.iter().find(|e| e.name == "VTS_01_1.VOB").unwrap().offset;
        check(
            &dvd,
            &format!("subfile,,start,{start},end,{},,:{}", start + 30 * 2048, path.display()),
        );
        assert_eq!(locate(&source.title_path(1)), Some((source, 1)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_title_set_without_disc_index() {
        let dir = std::env::temp_dir().join(format!("fc-disc-{}", std::process::id()));
        let vts_dir = dir.join("VIDEO_TS");
        std::fs::create_dir_all(&vts_dir).unwrap();
        for n in ["VTS_01_0.VOB", "VTS_01_2.VOB", "VTS_01_1.VOB", "VTS_02_1.VOB"] {
            std::fs::write(vts_dir.join(n), b"x").unwrap();
        }
        let first = vts_dir.join("VTS_01_1.VOB");
        assert!(is_dvd(&first) && !is_dvd(&vts_dir.join("VTS_01_0.VOB")));
        let whole = format!("concat:{}|{}", first.display(), vts_dir.join("VTS_01_2.VOB").display());
        // Without any IFO, a start time is a timestamp.
        assert_eq!(input_args(&first, 2_000), ["-ss", "2.000", "-i", whole.as_str()]);
        // With the title set's IFO, a byte, and the duration.
        let set = vts(
            &[Chain {
                cells: vec![(8, 0, 0, 0)],
                chapters: vec![1],
            }],
            &[],
            &[],
        );
        std::fs::write(vts_dir.join("vts_01_0.ifo"), set).unwrap();
        let from_ifo = input_args(&first, 2_000);
        assert_eq!(from_ifo[0], "-i");
        assert!(
            from_ifo[1].starts_with("concat:subfile,,start,0,end,1,,:"),
            "{from_ifo:?}"
        );
        assert_eq!(title(&first).unwrap().duration_ms, 8_000);
        assert_eq!(input_args(Path::new("/m/x.mkv"), 0), ["-i", "/m/x.mkv"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
