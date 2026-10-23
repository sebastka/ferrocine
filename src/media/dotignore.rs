//! `.ignore` files, as Jellyfin reads them: the nearest one at or above a
//! directory (above a file) decides. An empty one, or one without a valid
//! pattern, hides everything below it; otherwise its lines are gitignore
//! patterns, matched without regard to case, relative to its directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub const FILE_NAME: &str = ".ignore";

enum Rules {
    /// Hide everything below.
    All,
    Patterns(Gitignore),
}

/// The `.ignore` files of one library path, read once per directory.
pub struct DotIgnore {
    root: PathBuf,
    /// For each directory looked at: the directory of its nearest `.ignore`, and its rules.
    nearest: HashMap<PathBuf, Option<(PathBuf, Arc<Rules>)>>,
    /// An `.ignore` file exists but could not be read.
    pub unreadable: bool,
}

fn parse(dir: &Path, text: &str) -> Rules {
    let mut builder = GitignoreBuilder::new(dir);
    // Jellyfin matches without case: `SPs` hides `sps/`.
    let _ = builder.case_insensitive(true);
    let mut valid = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // Invalid patterns are skipped, as by Jellyfin.
        if builder.add_line(None, line).is_ok() {
            valid += 1;
        }
    }
    match builder.build() {
        Ok(g) if valid > 0 => Rules::Patterns(g),
        _ => Rules::All,
    }
}

impl DotIgnore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            nearest: HashMap::new(),
            unreadable: false,
        }
    }

    /// Whether `path`, inside the library path, is hidden by an `.ignore` file.
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let start = if is_dir { Some(path) } else { path.parent() };
        let Some((base, rules)) = start.and_then(|d| self.nearest(d)) else {
            return false;
        };
        match &*rules {
            Rules::All => true,
            // The directory holding the patterns is not matched against them.
            Rules::Patterns(_) if path == base => false,
            Rules::Patterns(g) => g.matched_path_or_any_parents(path, is_dir).is_ignore(),
        }
    }

    fn nearest(&mut self, dir: &Path) -> Option<(PathBuf, Arc<Rules>)> {
        if let Some(cached) = self.nearest.get(dir) {
            return cached.clone();
        }
        let found = match std::fs::read_to_string(dir.join(FILE_NAME)) {
            Ok(text) => Some((dir.to_path_buf(), Arc::new(parse(dir, &text)))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match dir.parent() {
                // Up to the library path, not beyond.
                Some(up) if dir != self.root && up.starts_with(&self.root) => self.nearest(up),
                _ => None,
            },
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot read .ignore; nothing will be removed by this scan");
                self.unreadable = true;
                None
            }
        };
        self.nearest.insert(dir.to_path_buf(), found.clone());
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("fc-dotignore-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, content) in files {
            let f = root.join(path);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, content).unwrap();
        }
        root
    }

    #[test]
    fn empty_or_blank_files_hide_everything_below() {
        let root = tree(
            "empty",
            &[
                ("Show/.ignore", ""),
                ("Show/S01/e1.mkv", ""),
                ("Blank/.ignore", " \n\t\n"),
                ("Invalid/.ignore", "[z-a]\na{b\n"),
                ("Movie/m.mkv", ""),
            ],
        );
        let mut d = DotIgnore::new(&root);
        assert!(d.is_ignored(&root.join("Show"), true));
        assert!(d.is_ignored(&root.join("Show/S01/e1.mkv"), false));
        assert!(d.is_ignored(&root.join("Blank"), true));
        // Only invalid patterns: as if empty.
        assert!(d.is_ignored(&root.join("Invalid"), true));
        assert!(!d.is_ignored(&root.join("Movie"), true));
        assert!(!d.is_ignored(&root.join("Movie/m.mkv"), false));
        assert!(!d.unreadable);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patterns_like_jellyfin() {
        let root = tree(
            "patterns",
            &[
                (".ignore", "# extras\nSPs\n!thebestshot.mkv\nvideos/\n*.TMP\n"),
                ("cd/sps/ffffff.mkv", ""),
                ("cd/sps/thebestshot.mkv", ""),
                ("videos/a.mkv", ""),
                ("videos.mkv", ""),
                ("deep/er/x.tmp", ""),
                // The nearest .ignore decides alone: the root's `*.TMP` does not apply here.
                ("own/.ignore", "*.bak\n"),
                ("own/y.tmp", ""),
                ("own/y.bak", ""),
                ("Alien (1979)/Alien (1979).mkv", ""),
            ],
        );
        let mut d = DotIgnore::new(&root);
        assert!(d.is_ignored(&root.join("cd/sps"), true));
        assert!(d.is_ignored(&root.join("cd/sps/ffffff.mkv"), false));
        assert!(!d.is_ignored(&root.join("cd/sps/thebestshot.mkv"), false));
        assert!(d.is_ignored(&root.join("videos"), true));
        assert!(!d.is_ignored(&root.join("videos.mkv"), false));
        assert!(d.is_ignored(&root.join("deep/er/x.tmp"), false));
        assert!(!d.is_ignored(&root.join("own/y.tmp"), false));
        assert!(d.is_ignored(&root.join("own/y.bak"), false));
        assert!(!d.is_ignored(&root.join("own"), true));
        assert!(!d.is_ignored(&root.join("Alien (1979)/Alien (1979).mkv"), false));
        std::fs::remove_dir_all(root).unwrap();
    }
}
