//! Disc folders, each one movie: a DVD's `VIDEO_TS` and a Blu-ray's `BDMV`.
//!
//! The feature of a DVD is the title set holding the most data; its VOB parts
//! (`VTS_01_1.VOB`, `VTS_01_2.VOB`…) play as one stream through ffmpeg's
//! `concat:` protocol. Menus (`VIDEO_TS.VOB`, `VTS_NN_0.VOB`) and the smaller
//! title sets (extras, trailers) are left out. The feature of a Blu-ray is the
//! largest file of `BDMV/STREAM`. An item's path is the feature's first file.
//!
//! A DVD's timestamps restart inside its title set, so neither ffprobe's
//! duration nor ffmpeg's `-ss` can be trusted. The title set's IFO file gives
//! both: the longest program chain is the film, with its play time, and its
//! cells map times to sectors, where playback starts (`-skip_initial_bytes`).

use std::collections::BTreeMap;
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

/// Whether `path` is the first part of a DVD title set (an item's path):
/// the file alone is not the movie.
pub fn is_dvd(path: &Path) -> bool {
    matches!(classify(path), Some(Part::Dvd { part: 1.., .. }))
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

/// What to give ffmpeg and ffprobe as `-i` for an item's path: the whole
/// title set of a DVD, else the file.
pub fn ffmpeg_input(path: &Path) -> String {
    if !is_dvd(path) {
        return path.to_string_lossy().into_owned();
    }
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

/// One cell of a DVD program chain: a run of sectors and its play time.
#[derive(Clone, Debug, PartialEq)]
struct Cell {
    start_ms: i64,
    duration_ms: i64,
    /// Sectors of 2048 bytes from the start of the title set's first VOB.
    first: u64,
    last: u64,
}

/// The film of a DVD title set, from its IFO file.
#[derive(Clone, Debug, PartialEq)]
pub struct Title {
    pub duration_ms: i64,
    cells: Vec<Cell>,
}

const SECTOR: u64 = 2048;

fn u16_at(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn u32_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?)))
}

/// A play time: hours, minutes, seconds and frames in BCD, the frame rate in
/// the top bits of the last byte.
fn play_time_ms(b: &[u8]) -> Option<i64> {
    let bcd = |x: u8| i64::from(x >> 4) * 10 + i64::from(x & 0x0f);
    let fps = if b.get(3)? >> 6 == 1 { 25.0 } else { 30_000.0 / 1001.0 };
    let frames = bcd(b[3] & 0x3f) as f64;
    Some((bcd(b[0]) * 3600 + bcd(b[1]) * 60 + bcd(b[2])) * 1000 + (frames * 1000.0 / fps) as i64)
}

/// The longest program chain of a title set's IFO (`VTS_NN_0.IFO`).
pub fn parse_ifo(d: &[u8]) -> Option<Title> {
    if d.get(..12)? != b"DVDVIDEO-VTS" {
        return None;
    }
    let pgcit = usize::try_from(u32_at(d, 0xCC)? * SECTOR).ok()?;
    let mut best: Option<Title> = None;
    for i in 0..u16_at(d, pgcit)? {
        let pgc = pgcit + usize::try_from(u32_at(d, pgcit + 8 + 8 * i + 4)?).ok()?;
        let duration_ms = play_time_ms(d.get(pgc + 4..pgc + 8)?)?;
        if best.as_ref().is_some_and(|b| b.duration_ms >= duration_ms) {
            continue;
        }
        let cell_count = usize::from(*d.get(pgc + 3)?);
        let table = pgc + u16_at(d, pgc + 0xE8)?;
        let mut cells = Vec::with_capacity(cell_count);
        let mut start_ms = 0;
        for c in 0..cell_count {
            let e = d.get(table + 24 * c..table + 24 * c + 24)?;
            // Of a multi-angle block, only the first angle.
            let (block_mode, block_type) = (e[0] >> 6, (e[0] >> 4) & 3);
            if block_type == 1 && block_mode > 1 {
                continue;
            }
            let duration_ms = play_time_ms(&e[4..8])?;
            cells.push(Cell {
                start_ms,
                duration_ms,
                first: u32_at(e, 8)?,
                last: u32_at(e, 20)?,
            });
            start_ms += duration_ms;
        }
        if !cells.is_empty() {
            best = Some(Title { duration_ms, cells });
        }
    }
    best
}

impl Title {
    /// The byte where playback at `ms` starts: in the cell playing then, in
    /// proportion to its time.
    pub fn offset_at(&self, ms: i64) -> u64 {
        let cell = self
            .cells
            .iter()
            .find(|c| ms < c.start_ms + c.duration_ms)
            .unwrap_or_else(|| self.cells.last().expect("a title has cells"));
        let into = (ms - cell.start_ms).clamp(0, cell.duration_ms.max(1)) as u64;
        let sectors = cell.last.saturating_sub(cell.first) + 1;
        let sector = cell.first + sectors * into / cell.duration_ms.max(1) as u64;
        sector.min(cell.last) * SECTOR
    }
}

/// The film of the DVD title set starting at `path`, from its IFO file (or
/// the backup copy, `.BUP`).
pub fn title(path: &Path) -> Option<Title> {
    let Some(Part::Dvd { set, .. }) = classify(path) else {
        return None;
    };
    let dir = path.parent()?;
    let wanted = [format!("VTS_{set:02}_0.IFO"), format!("VTS_{set:02}_0.BUP")];
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| wanted.iter().any(|w| n.to_string_lossy().eq_ignore_ascii_case(w)))
        })
        .collect();
    // The IFO before its backup.
    found.sort_by_key(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bup")));
    found
        .iter()
        .find_map(|p| std::fs::read(p).ok().as_deref().and_then(parse_ifo))
}

/// ffmpeg's input arguments for an item's path, starting at `start_ms`: up
/// to and including `-i`. A DVD with an IFO starts at the cell's bytes.
pub fn input_args(path: &Path, start_ms: i64) -> Vec<String> {
    let mut a = Vec::new();
    match is_dvd(path).then(|| title(path)).flatten() {
        Some(t) => {
            a.push("-skip_initial_bytes".into());
            a.push(t.offset_at(start_ms).to_string());
        }
        None if start_ms > 0 => {
            a.push("-ss".into());
            a.push(format!("{:.3}", start_ms as f64 / 1000.0));
        }
        None => {}
    }
    a.push("-i".into());
    a.push(ffmpeg_input(path));
    a
}

#[cfg(test)]
mod tests {
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

    /// An IFO of one program chain with `cells`: (seconds, first sector, last
    /// sector, cell category byte), plus a shorter chain before it.
    pub(crate) fn ifo(cells: &[(u8, u32, u32, u8)]) -> Vec<u8> {
        let mut d = vec![0u8; 4096];
        d[..12].copy_from_slice(b"DVDVIDEO-VTS");
        d[0xCC..0xD0].copy_from_slice(&1u32.to_be_bytes());
        let pgcit = 2048;
        d[pgcit..pgcit + 2].copy_from_slice(&2u16.to_be_bytes());
        let bcd = |n: u8| ((n / 10) << 4) | (n % 10);
        let mut pgc_at = 16usize;
        for (i, chain) in [vec![(5u8, 0u32, 9u32, 0u8)], cells.to_vec()].iter().enumerate() {
            let e = pgcit + 8 + 8 * i;
            d[e + 4..e + 8].copy_from_slice(&(pgc_at as u32).to_be_bytes());
            let pgc = pgcit + pgc_at;
            d[pgc + 3] = chain.len() as u8;
            let total: u32 = chain
                .iter()
                .filter(|c| !((c.3 >> 4) & 3 == 1 && c.3 >> 6 > 1))
                .map(|c| u32::from(c.0))
                .sum();
            d[pgc + 4..pgc + 8].copy_from_slice(&[0, bcd((total / 60) as u8), bcd((total % 60) as u8), 0xC0]);
            d[pgc + 0xE8..pgc + 0xEA].copy_from_slice(&0x100u16.to_be_bytes());
            for (c, (secs, first, last, category)) in chain.iter().enumerate() {
                let e = pgc + 0x100 + 24 * c;
                d[e] = *category;
                d[e + 4..e + 8].copy_from_slice(&[0, bcd(secs / 60), bcd(secs % 60), 0xC0]);
                d[e + 8..e + 12].copy_from_slice(&first.to_be_bytes());
                d[e + 20..e + 24].copy_from_slice(&last.to_be_bytes());
            }
            pgc_at += 0x100 + 24 * chain.len() + 16;
        }
        d
    }

    #[test]
    fn reads_the_film_from_the_ifo() {
        // Two cells, then a second angle of the second (skipped), then a third.
        let d = ifo(&[
            (10, 100, 199, 0),
            (20, 200, 399, 0x50),
            (20, 400, 599, 0x90),
            (30, 600, 899, 0),
        ]);
        let t = parse_ifo(&d).unwrap();
        assert_eq!(t.duration_ms, 60_000);
        assert_eq!(t.cells.len(), 3);
        assert_eq!(t.offset_at(0), 100 * SECTOR);
        assert_eq!(t.offset_at(5_000), 150 * SECTOR);
        assert_eq!(t.offset_at(10_000), 200 * SECTOR);
        assert_eq!(t.offset_at(45_000), 750 * SECTOR);
        assert_eq!(t.offset_at(999_000), 899 * SECTOR);
        assert!(parse_ifo(b"DVDVIDEO-VMG").is_none());
        assert!(parse_ifo(&d[..2100]).is_none());
        assert_eq!(play_time_ms(&[0x01, 0x53, 0x07, 0xC0 | 0x15]), Some(6_787_000 + 500));
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
            movie_dir(Path::new("/m/A/VTS_01_1.VOB"), Part::Dvd { set: 1, part: 1 }),
            Path::new("/m/A")
        );
        assert_eq!(
            movie_dir(Path::new("/m/B/BDMV/STREAM/00001.m2ts"), Part::Bluray),
            Path::new("/m/B")
        );
    }

    #[test]
    fn the_feature_is_the_biggest_title_set() {
        // The layout of a real DVD9: the film in VTS_01, an extra in VTS_03.
        let d = "/m/A Date with Judy (1948) NTSC DVD9/VIDEO_TS";
        let files = vec![
            f(&format!("{d}/VIDEO_TS.VOB"), 10),
            f(&format!("{d}/VTS_01_0.VOB"), 2_994_176),
            f(&format!("{d}/VTS_01_2.VOB"), 1_073_739_776),
            f(&format!("{d}/VTS_01_1.VOB"), 1_073_739_776),
            f(&format!("{d}/VTS_01_3.VOB"), 764_956_672),
            f(&format!("{d}/VTS_02_1.VOB"), 20_480),
            f(&format!("{d}/VTS_03_1.VOB"), 761_888_768),
            f("/m/B/BDMV/STREAM/00000.m2ts", 5),
            f("/m/B/BDMV/STREAM/00800.m2ts", 9_000),
        ];
        let got = features(files);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, Path::new(&format!("{d}/VTS_01_1.VOB")));
        assert_eq!(got[0].size, 1_073_739_776 * 2 + 764_956_672);
        assert_eq!(got[0].mtime, 1_073_739_776);
        assert_eq!(got[0].movie_dir, Path::new("/m/A Date with Judy (1948) NTSC DVD9"));
        assert_eq!(got[1].path, Path::new("/m/B/BDMV/STREAM/00800.m2ts"));
        assert_eq!(got[1].movie_dir, Path::new("/m/B"));
    }

    #[test]
    fn dvd_parts_play_as_one_input() {
        let dir = std::env::temp_dir().join(format!("fc-disc-{}", std::process::id()));
        let vts = dir.join("VIDEO_TS");
        std::fs::create_dir_all(&vts).unwrap();
        for n in [
            "VTS_01_0.VOB",
            "VTS_01_2.VOB",
            "VTS_01_1.VOB",
            "VTS_02_1.VOB",
            "VTS_01_10.VOB",
        ] {
            std::fs::write(vts.join(n), b"x").unwrap();
        }
        let first = vts.join("VTS_01_1.VOB");
        assert!(is_dvd(&first) && !is_dvd(&vts.join("VTS_01_0.VOB")));
        assert_eq!(
            ffmpeg_input(&first),
            format!("concat:{}|{}", first.display(), vts.join("VTS_01_2.VOB").display())
        );
        // A title set in one part is the file itself.
        assert_eq!(
            ffmpeg_input(&vts.join("VTS_02_1.VOB")),
            vts.join("VTS_02_1.VOB").to_string_lossy()
        );
        assert_eq!(ffmpeg_input(Path::new("/m/x.mkv")), "/m/x.mkv");
        // Without an IFO, a start time is a timestamp; with one, a byte.
        assert_eq!(input_args(&first, 2_000)[..2], ["-ss", "2.000"]);
        std::fs::write(vts.join("vts_01_0.ifo"), ifo(&[(8, 0, 0, 0)])).unwrap();
        assert_eq!(input_args(&first, 2_000)[..2], ["-skip_initial_bytes", "0"]);
        assert_eq!(title(&first).unwrap().duration_ms, 8_000);
        assert_eq!(input_args(Path::new("/m/x.mkv"), 0), ["-i", "/m/x.mkv"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
