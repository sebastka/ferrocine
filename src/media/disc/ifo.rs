//! DVD-Video information files: `VIDEO_TS.IFO` (the disc's titles) and
//! `VTS_NN_0.IFO` (a title set's program chains, cells, chapters, streams).
//!
//! Only what playback needs is read. Offsets follow the DVD-Video
//! specification as documented by the dvdread project; every read is bounds
//! checked, so a damaged file yields `None`, never a panic.

/// Sectors are 2048 bytes; cell addresses count sectors from the start of a
/// title set's first VOB (`VTS_NN_1.VOB`).
pub const SECTOR: u64 = 2048;

fn u16_at(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn u32_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?)))
}

/// A play time: hours, minutes, seconds and frames in BCD, the frame rate in
/// the top bits of the last byte (25 or 29.97 frames per second).
pub fn play_time_ms(b: &[u8]) -> Option<i64> {
    let bcd = |x: u8| i64::from(x >> 4) * 10 + i64::from(x & 0x0f);
    let fps = if b.get(3)? >> 6 == 1 { 25.0 } else { 30_000.0 / 1001.0 };
    let frames = bcd(b[3] & 0x3f) as f64;
    Some((bcd(b[0]) * 3600 + bcd(b[1]) * 60 + bcd(b[2])) * 1000 + (frames * 1000.0 / fps) as i64)
}

/// A title of the disc, from `VIDEO_TS.IFO`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmgTitle {
    /// Its title set (`VTS_NN`).
    pub vts: u8,
    /// Its number within the title set.
    pub vts_title: u8,
}

/// The titles of the disc (`VIDEO_TS.IFO`, table TT_SRPT).
pub fn parse_vmg(d: &[u8]) -> Option<Vec<VmgTitle>> {
    if d.get(..12)? != b"DVDVIDEO-VMG" {
        return None;
    }
    let table = usize::try_from(u32_at(d, 0xC4)? * SECTOR).ok()?;
    let count = u16_at(d, table)?;
    (0..count)
        .map(|i| {
            let e = d.get(table + 8 + 12 * i..table + 8 + 12 * i + 12)?;
            Some(VmgTitle {
                vts: e[6],
                vts_title: e[7],
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub duration_ms: i64,
    pub first_sector: u64,
    pub last_sector: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramChain {
    pub duration_ms: i64,
    /// Without the other angles of multi-angle blocks.
    pub cells: Vec<Cell>,
    /// Per program (chapter): the index of its first cell in `cells`.
    pub program_cells: Vec<usize>,
    /// Logical audio stream → physical stream number, when present.
    pub audio: [Option<u8>; 8],
    /// Logical subpicture stream → physical stream number (for the title
    /// set's aspect ratio), when present.
    pub subpictures: [Option<u8>; 32],
    /// 16 colours of the subpictures, as RGB.
    pub palette: [u32; 16],
}

/// What the title set says of a stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInfo {
    /// ISO 639-1, when the disc says.
    pub language: Option<String>,
    /// 0 none; audio: 1 normal, 2 for the visually impaired, 3 and 4
    /// director's comments; subpictures: 1–3 normal, 5–7 captions, 9 forced,
    /// 13–15 director's comments.
    pub extension: u8,
    /// Audio only: 0 AC-3, 2 and 3 MPEG, 4 LPCM, 6 DTS.
    pub coding: u8,
}

/// A title set's IFO (`VTS_NN_0.IFO`).
#[derive(Clone, Debug, PartialEq)]
pub struct TitleSet {
    pub audio: Vec<StreamInfo>,
    pub subpictures: Vec<StreamInfo>,
    /// Per title of the set: its chapters, as (program chain, program), 1-based.
    pub titles: Vec<Vec<(usize, usize)>>,
    pub chains: Vec<ProgramChain>,
}

fn language(code: &[u8], specified: bool) -> Option<String> {
    let s = std::str::from_utf8(code).ok()?;
    (specified && s.bytes().all(|b| b.is_ascii_lowercase())).then(|| crate::lang::iso1(s).unwrap_or(s).to_owned())
}

/// YCrCb (as stored) to RGB.
fn rgb(ycrcb: u64) -> u32 {
    let (y, cr, cb) = (
        ((ycrcb >> 16) & 0xff) as f64,
        ((ycrcb >> 8) & 0xff) as f64,
        (ycrcb & 0xff) as f64,
    );
    let c = |v: f64| v.round().clamp(0.0, 255.0) as u32;
    let r = c(y + 1.402 * (cr - 128.0));
    let g = c(y - 0.344_136 * (cb - 128.0) - 0.714_136 * (cr - 128.0));
    let b = c(y + 1.772 * (cb - 128.0));
    (r << 16) | (g << 8) | b
}

fn parse_chain(d: &[u8], at: usize, widescreen: bool) -> Option<ProgramChain> {
    let programs = usize::from(*d.get(at + 2)?);
    let cell_count = usize::from(*d.get(at + 3)?);
    let duration_ms = play_time_ms(d.get(at + 4..at + 8)?)?;
    let mut audio = [None; 8];
    for (i, slot) in audio.iter_mut().enumerate() {
        let c = u16_at(d, at + 0x0C + 2 * i)?;
        *slot = (c & 0x8000 != 0).then_some(((c >> 8) & 0x07) as u8);
    }
    let mut subpictures = [None; 32];
    for (i, slot) in subpictures.iter_mut().enumerate() {
        let c = u32_at(d, at + 0x1C + 4 * i)?;
        let physical = if widescreen { (c >> 16) & 0x1f } else { (c >> 24) & 0x1f };
        *slot = (c & 0x8000_0000 != 0).then_some(physical as u8);
    }
    let mut palette = [0; 16];
    for (i, slot) in palette.iter_mut().enumerate() {
        *slot = rgb(u32_at(d, at + 0xA4 + 4 * i)?);
    }
    let mut cells = Vec::with_capacity(cell_count);
    // Original cell number (1-based) → index in `cells`.
    let mut kept = Vec::with_capacity(cell_count);
    if cell_count > 0 {
        let table = at + u16_at(d, at + 0xE8)?;
        for c in 0..cell_count {
            let e = d.get(table + 24 * c..table + 24 * c + 24)?;
            let (block_mode, block_type) = (e[0] >> 6, (e[0] >> 4) & 3);
            // Of a multi-angle block, the first angle only.
            if block_type == 1 && block_mode > 1 {
                kept.push(cells.len().saturating_sub(1));
                continue;
            }
            kept.push(cells.len());
            cells.push(Cell {
                duration_ms: play_time_ms(&e[4..8])?,
                first_sector: u32_at(e, 8)?,
                last_sector: u32_at(e, 20)?,
            });
        }
    }
    let mut program_cells = Vec::with_capacity(programs);
    if programs > 0 {
        let map = at + u16_at(d, at + 0xE6)?;
        for p in 0..programs {
            let entry = usize::from(*d.get(map + p)?);
            program_cells.push(*kept.get(entry.checked_sub(1)?)?);
        }
    }
    Some(ProgramChain {
        duration_ms,
        cells,
        program_cells,
        audio,
        subpictures,
        palette,
    })
}

pub fn parse_vts(d: &[u8]) -> Option<TitleSet> {
    if d.get(..12)? != b"DVDVIDEO-VTS" {
        return None;
    }
    // Video attributes: aspect ratio 3 is 16:9.
    let widescreen = (d.get(0x200)? >> 2) & 3 == 3;
    let audio = (0..u16_at(d, 0x202)?.min(8))
        .map(|i| {
            let e = d.get(0x204 + 8 * i..0x204 + 8 * i + 8)?;
            Some(StreamInfo {
                language: language(&e[2..4], (e[0] >> 2) & 3 == 1),
                extension: e[5],
                coding: e[0] >> 5,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let subpictures = (0..u16_at(d, 0x254)?.min(32))
        .map(|i| {
            let e = d.get(0x256 + 6 * i..0x256 + 6 * i + 6)?;
            Some(StreamInfo {
                language: language(&e[2..4], e[0] & 3 == 1),
                extension: e[5],
                coding: 0,
            })
        })
        .collect::<Option<Vec<_>>>()?;

    // Titles and their chapters (VTS_PTT_SRPT).
    let ptt = usize::try_from(u32_at(d, 0xC8)? * SECTOR).ok()?;
    let count = u16_at(d, ptt)?;
    let end = ptt + usize::try_from(u32_at(d, ptt + 4)?).ok()? + 1;
    let mut titles = Vec::with_capacity(count);
    for t in 0..count {
        let start = ptt + usize::try_from(u32_at(d, ptt + 8 + 4 * t)?).ok()?;
        let stop = if t + 1 < count {
            ptt + usize::try_from(u32_at(d, ptt + 8 + 4 * (t + 1))?).ok()?
        } else {
            end
        };
        let chapters = (start..stop.max(start))
            .step_by(4)
            .take_while(|o| o + 4 <= stop)
            .map(|o| Some((u16_at(d, o)?, u16_at(d, o + 2)?)))
            .collect::<Option<Vec<_>>>()?;
        titles.push(chapters);
    }

    // Program chains (VTS_PGCIT).
    let pgcit = usize::try_from(u32_at(d, 0xCC)? * SECTOR).ok()?;
    let chains = (0..u16_at(d, pgcit)?)
        .map(|i| {
            let at = pgcit + usize::try_from(u32_at(d, pgcit + 8 + 8 * i + 4)?).ok()?;
            parse_chain(d, at, widescreen)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(TitleSet {
        audio,
        subpictures,
        titles,
        chains,
    })
}

#[doc(hidden)]
pub mod build {
    //! IFO files for tests: a disc's titles, and title sets of program chains.

    fn bcd(n: u8) -> u8 {
        ((n / 10) << 4) | (n % 10)
    }

    /// A 29.97 fps play time.
    fn time(secs: u32) -> [u8; 4] {
        [
            bcd((secs / 3600) as u8),
            bcd((secs / 60 % 60) as u8),
            bcd((secs % 60) as u8),
            0xC0,
        ]
    }

    /// `VIDEO_TS.IFO` listing (title set, title within it) for each title.
    pub fn vmg(titles: &[(u8, u8)]) -> Vec<u8> {
        let mut d = vec![0u8; 4096];
        d[..12].copy_from_slice(b"DVDVIDEO-VMG");
        d[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes());
        d[2048..2050].copy_from_slice(&(titles.len() as u16).to_be_bytes());
        for (i, (vts, ttn)) in titles.iter().enumerate() {
            let e = 2048 + 8 + 12 * i;
            d[e + 6] = *vts;
            d[e + 7] = *ttn;
        }
        d
    }

    pub struct Chain {
        /// (seconds, first sector, last sector, cell category byte).
        pub cells: Vec<(u32, u32, u32, u8)>,
        /// First cell (1-based) of each chapter.
        pub chapters: Vec<u8>,
    }

    /// `VTS_NN_0.IFO`: one title per chain; audio and subpicture languages.
    pub fn vts(chains: &[Chain], audio: &[&str], subs: &[&str]) -> Vec<u8> {
        let mut d = vec![0u8; 8192];
        d[..12].copy_from_slice(b"DVDVIDEO-VTS");
        d[0x202..0x204].copy_from_slice(&(audio.len() as u16).to_be_bytes());
        for (i, l) in audio.iter().enumerate() {
            let e = 0x204 + 8 * i;
            d[e] = 1 << 2;
            d[e + 2..e + 4].copy_from_slice(l.as_bytes());
            d[e + 5] = 1;
        }
        d[0x254..0x256].copy_from_slice(&(subs.len() as u16).to_be_bytes());
        for (i, l) in subs.iter().enumerate() {
            let e = 0x256 + 6 * i;
            d[e] = 1;
            d[e + 2..e + 4].copy_from_slice(l.as_bytes());
            d[e + 5] = 1;
        }
        // Titles: sector 1; program chains: sector 2.
        d[0xC8..0xCC].copy_from_slice(&1u32.to_be_bytes());
        d[0xCC..0xD0].copy_from_slice(&2u32.to_be_bytes());
        let ptt = 2048;
        d[ptt..ptt + 2].copy_from_slice(&(chains.len() as u16).to_be_bytes());
        let mut at = 8 + 4 * chains.len();
        for (t, chain) in chains.iter().enumerate() {
            d[ptt + 8 + 4 * t..ptt + 12 + 4 * t].copy_from_slice(&(at as u32).to_be_bytes());
            for p in 0..chain.chapters.len() {
                d[ptt + at..ptt + at + 2].copy_from_slice(&((t + 1) as u16).to_be_bytes());
                d[ptt + at + 2..ptt + at + 4].copy_from_slice(&((p + 1) as u16).to_be_bytes());
                at += 4;
            }
        }
        d[ptt + 4..ptt + 8].copy_from_slice(&((at - 1) as u32).to_be_bytes());
        let pgcit = 4096;
        d[pgcit..pgcit + 2].copy_from_slice(&(chains.len() as u16).to_be_bytes());
        let mut at = 8 + 8 * chains.len();
        for (i, chain) in chains.iter().enumerate() {
            d[pgcit + 8 + 8 * i + 4..pgcit + 8 + 8 * i + 8].copy_from_slice(&(at as u32).to_be_bytes());
            let pgc = pgcit + at;
            d[pgc + 2] = chain.chapters.len() as u8;
            d[pgc + 3] = chain.cells.len() as u8;
            let total: u32 = chain
                .cells
                .iter()
                .filter(|c| !((c.3 >> 4) & 3 == 1 && c.3 >> 6 > 1))
                .map(|c| c.0)
                .sum();
            d[pgc + 4..pgc + 8].copy_from_slice(&time(total));
            for a in 0..audio.len() {
                d[pgc + 0x0C + 2 * a..pgc + 0x0E + 2 * a]
                    .copy_from_slice(&(0x8000u16 | ((a as u16) << 8)).to_be_bytes());
            }
            for s in 0..subs.len() {
                d[pgc + 0x1C + 4 * s..pgc + 0x20 + 4 * s]
                    .copy_from_slice(&(0x8000_0000u32 | ((s as u32) << 24)).to_be_bytes());
            }
            // Palette entry 1: white (Y 235, Cr 128, Cb 128).
            d[pgc + 0xA4 + 4..pgc + 0xA4 + 8].copy_from_slice(&0x00EB_8080u32.to_be_bytes());
            d[pgc + 0xE6..pgc + 0xE8].copy_from_slice(&0xF0u16.to_be_bytes());
            for (p, first) in chain.chapters.iter().enumerate() {
                d[pgc + 0xF0 + p] = *first;
            }
            d[pgc + 0xE8..pgc + 0xEA].copy_from_slice(&0x100u16.to_be_bytes());
            for (c, (secs, first, last, category)) in chain.cells.iter().enumerate() {
                let e = pgc + 0x100 + 24 * c;
                d[e] = *category;
                d[e + 4..e + 8].copy_from_slice(&time(*secs));
                d[e + 8..e + 12].copy_from_slice(&first.to_be_bytes());
                d[e + 20..e + 24].copy_from_slice(&last.to_be_bytes());
            }
            at += 0x100 + 24 * chain.cells.len() + 8;
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::build::{Chain, vmg, vts};
    use super::*;

    #[test]
    fn reads_titles_chains_and_streams() {
        let titles = parse_vmg(&vmg(&[(1, 1), (1, 2), (2, 1)])).unwrap();
        assert_eq!(titles[2], VmgTitle { vts: 2, vts_title: 1 });
        let d = vts(
            &[
                Chain {
                    // Two cells, a second angle of the second (left out), a third.
                    cells: vec![
                        (10, 100, 199, 0),
                        (20, 200, 399, 0x50),
                        (20, 400, 599, 0x90),
                        (30, 600, 899, 0),
                    ],
                    chapters: vec![1, 2, 4],
                },
                Chain {
                    cells: vec![(5, 900, 949, 0)],
                    chapters: vec![1],
                },
            ],
            &["en", "fr"],
            &["fr"],
        );
        let set = parse_vts(&d).unwrap();
        assert_eq!(set.titles, [vec![(1, 1), (1, 2), (1, 3)], vec![(2, 1)]]);
        let film = &set.chains[0];
        assert_eq!(film.duration_ms, 60_000);
        assert_eq!(film.cells.len(), 3);
        // Chapter 3 starts at the 4th cell, the 3rd kept.
        assert_eq!(film.program_cells, [0, 1, 2]);
        assert_eq!(film.audio[..3], [Some(0), Some(1), None]);
        assert_eq!(film.subpictures[0], Some(0));
        assert_eq!(film.palette[1], 0xEBEBEB);
        assert_eq!(set.audio[1].language.as_deref(), Some("fr"));
        assert_eq!(set.subpictures[0].language.as_deref(), Some("fr"));
        assert!(parse_vts(&d[..3000]).is_none());
        assert!(parse_vmg(b"DVDVIDEO-VTS").is_none());
        assert_eq!(play_time_ms(&[0x01, 0x53, 0x07, 0xC0 | 0x15]), Some(6_787_500));
    }
}
