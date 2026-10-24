//! The `VIDEO_TS` directory of a DVD image (`.iso`), from its ISO 9660 file
//! system. DVD-Video discs are UDF bridge discs: their ISO 9660 side lists
//! the same files, each stored in one piece, which is all playback needs
//! (ffmpeg reads them with the `subfile` protocol). Blu-ray images carry
//! UDF only and are not read.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SECTOR: u64 = 2048;

/// A file in the image: its bytes `offset..offset + len`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extent {
    pub name: String,
    pub offset: u64,
    pub len: u64,
}

fn read_at(f: &mut std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// The directory records of a directory: (name, extent, size, is directory).
fn records(d: &[u8]) -> Vec<(String, u64, u64, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < d.len() {
        let len = usize::from(d[at]);
        if len == 0 {
            // Records never cross a sector: on to the next one.
            at = (at / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        }
        let Some(r) = d.get(at..at + len) else { break };
        if r.len() < 34 {
            break;
        }
        let extent = u64::from(u32::from_le_bytes(r[2..6].try_into().unwrap()));
        let size = u64::from(u32::from_le_bytes(r[10..14].try_into().unwrap()));
        let is_dir = r[25] & 0x02 != 0;
        let name_len = usize::from(r[32]);
        if let Some(raw) = r.get(33..33 + name_len)
            && !(name_len == 1 && raw[0] <= 1)
        {
            // `VTS_01_1.VOB;1` → `VTS_01_1.VOB`.
            let name = String::from_utf8_lossy(raw);
            let name = name.split(';').next().unwrap_or_default().trim_end_matches('.');
            out.push((name.to_uppercase(), extent, size, is_dir));
        }
        at += len;
    }
    out
}

/// The files of the image's `VIDEO_TS` directory; `None` when it has none
/// (not a DVD-Video image, or no ISO 9660 file system).
pub fn video_ts(path: &Path) -> std::io::Result<Option<Vec<Extent>>> {
    let mut f = std::fs::File::open(path)?;
    // Volume descriptors from sector 16; the primary one is type 1.
    let mut root = None;
    for sector in 16..32 {
        let Ok(vd) = read_at(&mut f, sector * SECTOR, SECTOR as usize) else {
            return Ok(None);
        };
        if &vd[1..6] != b"CD001" {
            return Ok(None);
        }
        match vd[0] {
            1 => {
                let r = &vd[156..190];
                root = Some((
                    u64::from(u32::from_le_bytes(r[2..6].try_into().unwrap())),
                    u64::from(u32::from_le_bytes(r[10..14].try_into().unwrap())),
                ));
                break;
            }
            255 => break,
            _ => {}
        }
    }
    let Some((extent, size)) = root else {
        return Ok(None);
    };
    let dir = read_at(&mut f, extent * SECTOR, size.min(1 << 20) as usize)?;
    let Some((_, extent, size, _)) = records(&dir).into_iter().find(|r| r.3 && r.0 == "VIDEO_TS") else {
        return Ok(None);
    };
    let dir = read_at(&mut f, extent * SECTOR, size.min(1 << 20) as usize)?;
    let files: Vec<Extent> = records(&dir)
        .into_iter()
        .filter(|r| !r.3)
        .map(|(name, extent, len, _)| Extent {
            name,
            offset: extent * SECTOR,
            len,
        })
        .collect();
    Ok(files.iter().any(|e| e.name == "VIDEO_TS.IFO").then_some(files))
}

#[doc(hidden)]
pub mod build {
    //! ISO 9660 images for tests: a `VIDEO_TS` directory of the given files.

    const SECTOR: usize = 2048;

    fn record(name: &[u8], extent: u32, size: u32, dir: bool) -> Vec<u8> {
        let len = 33 + name.len() + (1 - name.len() % 2);
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&extent.to_le_bytes());
        r[6..10].copy_from_slice(&extent.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[25] = if dir { 2 } else { 0 };
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r
    }

    pub fn image(files: &[(&str, &[u8])]) -> Vec<u8> {
        // 16 system sectors, PVD, terminator, root (18), VIDEO_TS (19), files.
        let mut next = 20usize;
        let mut placed = Vec::new();
        for (name, data) in files {
            placed.push((name, next, data));
            next += data.len().div_ceil(SECTOR).max(1);
        }
        let mut d = vec![0u8; next * SECTOR];
        let pvd = 16 * SECTOR;
        d[pvd] = 1;
        d[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        d[pvd + 6] = 1;
        d[pvd + 156..pvd + 190].copy_from_slice(&record(&[0], 18, SECTOR as u32, true));
        d[17 * SECTOR] = 255;
        d[17 * SECTOR + 1..17 * SECTOR + 6].copy_from_slice(b"CD001");
        let mut root = record(&[0], 18, SECTOR as u32, true);
        root.extend(record(&[1], 18, SECTOR as u32, true));
        root.extend(record(b"VIDEO_TS", 19, SECTOR as u32, true));
        d[18 * SECTOR..18 * SECTOR + root.len()].copy_from_slice(&root);
        let mut dir = record(&[0], 19, SECTOR as u32, true);
        dir.extend(record(&[1], 18, SECTOR as u32, true));
        for (name, at, data) in &placed {
            dir.extend(record(
                format!("{name};1").as_bytes(),
                *at as u32,
                data.len() as u32,
                false,
            ));
            d[at * SECTOR..at * SECTOR + data.len()].copy_from_slice(data);
        }
        d[19 * SECTOR..19 * SECTOR + dir.len()].copy_from_slice(&dir);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_video_ts() {
        let path = std::env::temp_dir().join(format!("fc-iso-{}.iso", std::process::id()));
        let vob = vec![7u8; 5000];
        std::fs::write(
            &path,
            build::image(&[("VIDEO_TS.IFO", b"DVDVIDEO-VMG"), ("VTS_01_1.VOB", &vob)]),
        )
        .unwrap();
        let files = video_ts(&path).unwrap().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].name, "VTS_01_1.VOB");
        assert_eq!((files[1].offset, files[1].len), (21 * SECTOR, 5000));
        std::fs::write(&path, vec![0u8; 40 * 2048]).unwrap();
        assert_eq!(video_ts(&path).unwrap(), None);
        std::fs::remove_file(path).unwrap();
    }
}
