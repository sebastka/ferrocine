//! The metadata directory: artwork, downloaded subtitles and seek bar
//! previews, as files. State and textual metadata stay in the database, which
//! names these files by tag.
//!
//! Images and subtitles are content-addressed: a file's name is the first 16
//! hex digits of its SHA-256 (the tag the database and URLs carry), under a
//! two-digit fan-out (`images/3f/3f2a…`), so identical files are stored once.
//! A preview set is a directory named by its own tag (`trickplay/<tag>/1.jpg`…).
//! Files are written to a temporary name and renamed, so a reader never sees
//! half a file, and files no row refers to are removed by [`Assets::collect`]
//! once they are an hour old (another instance may be writing their row).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};

/// Unreferenced files younger than this are kept.
const GRACE: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Subtitle,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Image => "images",
            Kind::Subtitle => "subtitles",
        }
    }
}

/// A tag: 16 lowercase hex digits (never a path).
pub fn valid_tag(tag: &str) -> bool {
    tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn tag_of(data: &[u8]) -> String {
    crate::store::sha256_hex(data)[..16].to_owned()
}

#[derive(Debug)]
pub struct Assets {
    root: PathBuf,
}

/// What [`Assets::collect`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub files: u64,
    pub bytes: u64,
}

impl Assets {
    /// Open (and create) the directory; fail when it is not writable.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        for dir in ["images", "subtitles", "trickplay", "tmp"] {
            std::fs::create_dir_all(root.join(dir))
                .with_context(|| format!("cannot create {}", root.join(dir).display()))?;
        }
        let probe = root.join("tmp").join(format!(".probe-{}", std::process::id()));
        std::fs::write(&probe, b"ok")
            .with_context(|| format!("metadata directory {} is not writable", root.display()))?;
        let _ = std::fs::remove_file(probe);
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, kind: Kind, tag: &str) -> Option<PathBuf> {
        valid_tag(tag).then(|| self.root.join(kind.dir()).join(&tag[..2]).join(tag))
    }

    /// A fresh path in the directory's own temporary folder (same file
    /// system as the destination, so renaming is atomic).
    fn temp(&self, what: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.root.join("tmp").join(format!("{what}-{}-{n}", std::process::id()))
    }

    /// Store `data`; returns its tag.
    pub async fn put(&self, kind: Kind, data: &[u8]) -> anyhow::Result<String> {
        let tag = tag_of(data);
        let path = self.path(kind, &tag).expect("a computed tag is valid");
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(tag);
        }
        let tmp = self.temp(kind.dir());
        tokio::fs::write(&tmp, data)
            .await
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("cannot store {}", path.display()))?;
        Ok(tag)
    }

    pub async fn read(&self, kind: Kind, tag: &str) -> anyhow::Result<Vec<u8>> {
        let Some(path) = self.path(kind, tag) else {
            bail!("invalid tag {tag:?}");
        };
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))
    }

    /// The directory of a preview set.
    pub fn trickplay_dir(&self, tag: &str) -> Option<PathBuf> {
        valid_tag(tag).then(|| self.root.join("trickplay").join(tag))
    }

    /// One sheet of a preview set (1-based).
    pub fn trickplay_sheet(&self, tag: &str, sheet: u32) -> Option<PathBuf> {
        Some(self.trickplay_dir(tag)?.join(format!("{sheet}.jpg")))
    }

    /// A temporary directory to make a preview set in, then [`Assets::adopt_trickplay`].
    pub async fn trickplay_workdir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.temp("trickplay");
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Move a finished preview set in place under `tag`.
    pub async fn adopt_trickplay(&self, workdir: &Path, tag: &str) -> anyhow::Result<()> {
        let Some(dest) = self.trickplay_dir(tag) else {
            bail!("invalid tag {tag:?}");
        };
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(workdir).await?;
            return Ok(());
        }
        tokio::fs::rename(workdir, &dest)
            .await
            .with_context(|| format!("cannot store {}", dest.display()))
    }

    /// Remove what no row refers to (older than an hour), and stale
    /// temporary files. `images` and `subtitles` are the tags in use,
    /// `trickplay` the preview sets in use.
    pub fn collect(
        &self,
        images: &HashSet<String>,
        subtitles: &HashSet<String>,
        trickplay: &HashSet<String>,
    ) -> anyhow::Result<Collected> {
        let mut out = Collected::default();
        let old = |m: &std::fs::Metadata| {
            m.modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > GRACE)
        };
        for (kind, used) in [(Kind::Image, images), (Kind::Subtitle, subtitles)] {
            let base = self.root.join(kind.dir());
            for fan in std::fs::read_dir(&base)?.flatten() {
                for f in std::fs::read_dir(fan.path())?.flatten() {
                    let name = f.file_name().to_string_lossy().into_owned();
                    let Ok(m) = f.metadata() else { continue };
                    if !used.contains(&name) && old(&m) {
                        std::fs::remove_file(f.path())?;
                        out.files += 1;
                        out.bytes += m.len();
                    }
                }
                // An emptied fan-out directory goes too.
                let _ = std::fs::remove_dir(fan.path());
            }
        }
        for set in std::fs::read_dir(self.root.join("trickplay"))?.flatten() {
            let name = set.file_name().to_string_lossy().into_owned();
            let Ok(m) = set.metadata() else { continue };
            if !trickplay.contains(&name) && old(&m) {
                let bytes: u64 = std::fs::read_dir(set.path())?
                    .flatten()
                    .filter_map(|f| f.metadata().ok())
                    .map(|m| m.len())
                    .sum();
                std::fs::remove_dir_all(set.path())?;
                out.files += 1;
                out.bytes += bytes;
            }
        }
        for t in std::fs::read_dir(self.root.join("tmp"))?.flatten() {
            if t.metadata().is_ok_and(|m| old(&m)) {
                let p = t.path();
                let _ = if p.is_dir() {
                    std::fs::remove_dir_all(p)
                } else {
                    std::fs::remove_file(p)
                };
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_by_content_and_collects() {
        let root = std::env::temp_dir().join(format!("fc-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = Assets::open(&root).unwrap();
        let tag = a.put(Kind::Image, b"jpeg bytes").await.unwrap();
        assert!(valid_tag(&tag));
        assert_eq!(a.put(Kind::Image, b"jpeg bytes").await.unwrap(), tag);
        assert_eq!(a.read(Kind::Image, &tag).await.unwrap(), b"jpeg bytes");
        let path = a.path(Kind::Image, &tag).unwrap();
        assert!(path.starts_with(root.join("images").join(&tag[..2])));
        // Tags are never paths.
        assert!(a.path(Kind::Image, "../../etc/passwd").is_none());
        assert!(a.read(Kind::Subtitle, "0123456789abcdeF").await.is_err());

        let work = a.trickplay_workdir().await.unwrap();
        std::fs::write(work.join("1.jpg"), b"sheet").unwrap();
        a.adopt_trickplay(&work, "00000000000000aa").await.unwrap();
        assert_eq!(
            std::fs::read(a.trickplay_sheet("00000000000000aa", 1).unwrap()).unwrap(),
            b"sheet"
        );

        // Unreferenced but recent: kept.
        let none = HashSet::new();
        assert_eq!(a.collect(&none, &none, &none).unwrap(), Collected::default());
        // Old and unreferenced: removed; referenced: kept.
        let past = SystemTime::now() - Duration::from_secs(7200);
        let other = a.put(Kind::Image, b"other").await.unwrap();
        for p in [path.clone(), a.path(Kind::Image, &other).unwrap()] {
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(past)
                .unwrap();
        }
        std::fs::File::open(a.trickplay_dir("00000000000000aa").unwrap())
            .unwrap()
            .set_modified(past)
            .unwrap();
        let used: HashSet<String> = [tag.clone()].into();
        let c = a.collect(&used, &none, &none).unwrap();
        assert_eq!(c.files, 2, "{c:?}");
        assert!(path.exists() && !a.path(Kind::Image, &other).unwrap().exists());
        assert!(!a.trickplay_dir("00000000000000aa").unwrap().exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
