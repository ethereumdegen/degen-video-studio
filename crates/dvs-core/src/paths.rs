//! Where things live inside a project directory.
//!
//! One type owns the layout so no other module hardcodes a path, and so the directory can
//! be relocated or mounted read-only without a search-and-replace.

use std::path::{Path, PathBuf};

/// Project directory layout:
///
/// ```text
/// myproject/
/// ├── project.json      canonical document
/// ├── history.jsonl     journal: {op, args, patch, ts, actor}
/// ├── assets/           content-addressed media, by blake3
/// ├── transcript/       word-timestamped transcripts, per asset
/// ├── cache/            proxies, waveforms, thumbnails, rendered segments (regenerable)
/// └── .lock             writer lock
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    root: PathBuf,
}

impl ProjectPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ProjectPaths { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn project_json(&self) -> PathBuf {
        self.root.join("project.json")
    }

    pub fn history(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn transcripts_dir(&self) -> PathBuf {
        self.root.join("transcript")
    }

    pub fn transcript(&self, asset_id: &str) -> PathBuf {
        self.transcripts_dir().join(format!("{asset_id}.json"))
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn proxy_dir(&self) -> PathBuf {
        self.cache_dir().join("proxy")
    }

    pub fn segment_dir(&self) -> PathBuf {
        self.cache_dir().join("segments")
    }

    pub fn thumb_dir(&self) -> PathBuf {
        self.cache_dir().join("thumb")
    }

    pub fn waveform_dir(&self) -> PathBuf {
        self.cache_dir().join("waveform")
    }

    pub fn lock(&self) -> PathBuf {
        self.root.join(".lock")
    }

    /// Make a path relative to the project root when it is inside it, so `project.json`
    /// stores `cache/proxy/x.mp4` and a moved project directory still resolves.
    pub fn relativize(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Resolve a path stored in the document, which may be project-relative or absolute.
    pub fn resolve(&self, stored: &str) -> PathBuf {
        let path = Path::new(stored);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    /// Walk up from a directory looking for a `project.json`, so `dvs` works from a
    /// subdirectory the way `git` does.
    pub fn discover(start: impl AsRef<Path>) -> Option<ProjectPaths> {
        let mut current = start.as_ref().to_path_buf();
        loop {
            if current.join("project.json").is_file() {
                return Some(ProjectPaths::new(current));
            }
            if !current.pop() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_paths_are_project_relative_and_round_trip() {
        let paths = ProjectPaths::new("/tmp/promo");
        let proxy = paths.proxy_dir().join("ast_x.mp4");
        let stored = paths.relativize(&proxy);
        assert_eq!(stored, "cache/proxy/ast_x.mp4");
        assert_eq!(paths.resolve(&stored), proxy);
    }

    #[test]
    fn absolute_stored_paths_are_left_alone() {
        let paths = ProjectPaths::new("/tmp/promo");
        assert_eq!(
            paths.resolve("/mnt/media/talk.mp4"),
            PathBuf::from("/mnt/media/talk.mp4")
        );
    }

    #[test]
    fn discovery_walks_up_to_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("promo");
        let nested = root.join("cache").join("proxy");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("project.json"), "{}").unwrap();
        let found = ProjectPaths::discover(&nested).expect("should find project root");
        assert_eq!(found.root(), root);
        assert!(ProjectPaths::discover(dir.path()).is_none());
    }
}
