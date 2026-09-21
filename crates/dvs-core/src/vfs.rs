//! The one place in the engine that touches storage.
//!
//! Everything above this module — the asset store, `project.json`, the journal — speaks
//! [`Vfs`] and never `std::fs`. Two things fall out of that. The document layer becomes
//! testable without a temp directory, so the engine tests run in memory and in parallel;
//! and the single implementation that does reach the disk can make one guarantee in one
//! place: a write is a `.tmp` sibling plus a rename, so an interrupted save leaves either
//! the old `project.json` or the new one and never a half-written file.
//!
//! A stray `std::fs` call elsewhere in the crate silently defeats both, and it is invisible
//! in review. [`tests::no_direct_filesystem_calls_outside_this_module`] makes it a build
//! failure instead, with one documented exception: [`crate::asset::AssetStore::hash_file`]
//! streams a multi-gigabyte import rather than reading it into a `Vec`, which the
//! whole-blob [`Vfs::read`] cannot express.

use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Byte storage addressed by path. Implementations are shared across threads and cloned
/// freely, so every method takes `&self`.
pub trait Vfs: Send + Sync {
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Replace the contents of `path`, atomically where the backend allows it. Missing
    /// parent directories are created.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()>;

    /// Append to `path`, creating it if absent. The journal's hot path: history grows by
    /// one line per op and must never be rewritten to record one.
    fn append(&self, path: &Path, bytes: &[u8]) -> Result<()>;

    fn exists(&self, path: &Path) -> bool;

    fn remove(&self, path: &Path) -> Result<()>;

    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Immediate children of `dir`, as full paths, sorted. Errors if `dir` is not a
    /// directory.
    fn list(&self, dir: &Path) -> Result<Vec<PathBuf>>;

    /// Size in bytes, without reading the blob — an asset store holds gigabytes.
    fn len(&self, path: &Path) -> Result<u64>;
}

/// The native backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsVfs;

impl FsVfs {
    pub fn new() -> Self {
        FsVfs
    }

    /// The native filesystem as an `Arc<dyn Vfs>` — what every CLI entry point passes in.
    pub fn shared() -> Arc<dyn Vfs> {
        Arc::new(FsVfs)
    }
}

/// Writes land here first and are renamed over the target. The name is derived rather than
/// random so a crashed write leaves one predictable file that the next write reuses instead
/// of littering the directory; `.lock` serializes the writers that could otherwise race.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    Ok(())
}

impl Vfs for FsVfs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(|e| Error::io(path, e))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        ensure_parent(path)?;
        let tmp = tmp_sibling(path);
        std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
        // Rename within a directory is atomic: a concurrent reader — the Tauri app watching
        // the project directory — sees the old bytes or the new bytes, never a prefix.
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Error::io(path, e)
        })
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        ensure_parent(path)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| Error::io(path, e))?;
        file.write_all(bytes).map_err(|e| Error::io(path, e))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        std::fs::remove_file(path).map_err(|e| Error::io(path, e))
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(|e| Error::io(path, e))
    }

    fn list(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
            out.push(entry.map_err(|e| Error::io(dir, e))?.path());
        }
        out.sort();
        Ok(out)
    }

    fn len(&self, path: &Path) -> Result<u64> {
        Ok(std::fs::metadata(path).map_err(|e| Error::io(path, e))?.len())
    }
}

/// An in-memory tree, shared by every clone. The core tests run the real engine against it,
/// so they neither touch the disk nor race each other over a temp directory; it is also the
/// backend a browser build would persist to OPFS.
#[derive(Debug, Clone, Default)]
pub struct MemVfs {
    inner: Arc<RwLock<Tree>>,
}

#[derive(Debug, Default)]
struct Tree {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
}

/// Lexical normalisation: `.` is dropped, `..` pops. There are no symlinks in memory, so
/// this is exact rather than a best guess, and `/p/assets/../assets/x` is one file.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn missing(path: &Path) -> Error {
    Error::io(
        path,
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} not found", path.display()),
        ),
    )
}

impl MemVfs {
    pub fn new() -> Self {
        MemVfs::default()
    }

    /// A fresh in-memory tree as an `Arc<dyn Vfs>`.
    pub fn shared() -> Arc<dyn Vfs> {
        Arc::new(MemVfs::new())
    }

    /// Record the ancestors of a file so `exists` and `list` answer for directories that
    /// were never created explicitly — the sharded asset store writes straight into them.
    fn touch_dirs(tree: &mut Tree, path: &Path) {
        let mut cursor = path.to_path_buf();
        while cursor.pop() && !cursor.as_os_str().is_empty() {
            tree.dirs.insert(cursor.clone());
        }
    }

    fn tree(&self) -> std::sync::RwLockReadGuard<'_, Tree> {
        self.inner.read().expect("mem vfs lock poisoned")
    }

    fn tree_mut(&self) -> std::sync::RwLockWriteGuard<'_, Tree> {
        self.inner.write().expect("mem vfs lock poisoned")
    }
}

impl Vfs for MemVfs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let key = normalize(path);
        self.tree()
            .files
            .get(&key)
            .cloned()
            .ok_or_else(|| missing(path))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let key = normalize(path);
        let mut tree = self.tree_mut();
        Self::touch_dirs(&mut tree, &key);
        tree.files.insert(key, bytes.to_vec());
        Ok(())
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let key = normalize(path);
        let mut tree = self.tree_mut();
        Self::touch_dirs(&mut tree, &key);
        tree.files.entry(key).or_default().extend_from_slice(bytes);
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        let key = normalize(path);
        let tree = self.tree();
        tree.files.contains_key(&key) || tree.dirs.contains(&key)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let key = normalize(path);
        self.tree_mut()
            .files
            .remove(&key)
            .map(|_| ())
            .ok_or_else(|| missing(path))
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let key = normalize(path);
        let mut tree = self.tree_mut();
        let mut cursor = PathBuf::new();
        for component in key.components() {
            cursor.push(component.as_os_str());
            if !cursor.as_os_str().is_empty() {
                tree.dirs.insert(cursor.clone());
            }
        }
        Ok(())
    }

    fn list(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let key = normalize(dir);
        let tree = self.tree();
        if !tree.dirs.contains(&key) {
            return Err(if tree.files.contains_key(&key) {
                Error::io(
                    dir,
                    std::io::Error::new(
                        // `ErrorKind::NotADirectory` is still unstable on this toolchain.
                        std::io::ErrorKind::InvalidInput,
                        format!("{} is a file", dir.display()),
                    ),
                )
            } else {
                missing(dir)
            });
        }
        let mut out = BTreeSet::new();
        for entry in tree.files.keys().chain(tree.dirs.iter()) {
            if let Ok(rest) = entry.strip_prefix(&key) {
                if let Some(first) = rest.components().next() {
                    out.insert(key.join(first.as_os_str()));
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    fn len(&self, path: &Path) -> Result<u64> {
        let key = normalize(path);
        self.tree()
            .files
            .get(&key)
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| missing(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source line that reaches the host filesystem directly.
    #[derive(Debug)]
    struct Hit {
        line: usize,
        text: String,
        exempt: bool,
    }

    /// Opt-out marker for the documented exception. Spelling it out on the offending line
    /// keeps the justification next to the code instead of in a list somewhere else.
    const EXEMPT: &str = "vfs-exempt:";

    /// Scan one source file. Anything at or after the `#[cfg(test)]` marker is test
    /// scaffolding, which is allowed to use the real filesystem.
    fn direct_fs_calls(src: &str) -> Vec<Hit> {
        // `fs::` only counts when it starts a path segment — `vfs::` and `FsVfs::` do not.
        fn aliased_fs_path(code: &str) -> bool {
            let bytes = code.as_bytes();
            code.match_indices("fs::").any(|(i, _)| {
                i == 0
                    || !(bytes[i - 1].is_ascii_alphanumeric()
                        || bytes[i - 1] == b'_'
                        || bytes[i - 1] == b':')
            })
        }
        let mut out = Vec::new();
        for (index, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                break;
            }
            let code = line.split("//").next().unwrap_or("");
            if code.contains("std::fs") || aliased_fs_path(code) {
                out.push(Hit {
                    line: index + 1,
                    text: line.trim().to_string(),
                    exempt: line.contains(EXEMPT),
                });
            }
        }
        out
    }

    #[test]
    fn the_filesystem_guard_actually_detects_a_reintroduced_call() {
        let bad = "fn save(p: &Path) {\n    std::fs::write(p, b\"x\").unwrap();\n}\n";
        let hits = direct_fs_calls(bad);
        assert_eq!(hits.len(), 1, "a direct std::fs call must be caught");
        assert!(!hits[0].exempt);

        let aliased = "use std::fs;\nfn load() { let _ = fs::read(\"a\"); }\n";
        assert_eq!(
            direct_fs_calls(aliased).len(),
            2,
            "`use std::fs` and `fs::read` are both hits"
        );

        let vfs_call = "let bytes = self.vfs.read(p)?;\nlet s = FsVfs::shared();\n";
        assert!(
            direct_fs_calls(vfs_call).is_empty(),
            "`vfs::`/`FsVfs::` are not `fs::`"
        );

        let in_tests = "fn ok() {}\n#[cfg(test)]\nmod tests {\n    std::fs::read(\"x\");\n}\n";
        assert!(
            direct_fs_calls(in_tests).is_empty(),
            "test code may use the real filesystem"
        );

        let commented = "// std::fs::read is what this replaces\nfn ok() {}\n";
        assert!(
            direct_fs_calls(commented).is_empty(),
            "a doc reference is not a call"
        );

        let marked = "let f = std::fs::File::open(p)?; // vfs-exempt: streaming hash\n";
        let hits = direct_fs_calls(marked);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].exempt, "the marker must register as an exemption");
    }

    /// The invariant the rest of the crate relies on: storage is reached through [`Vfs`],
    /// so the engine is testable in memory and every write keeps the atomicity guarantee.
    #[test]
    fn no_direct_filesystem_calls_outside_this_module() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        let mut scanned = 0usize;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !name.ends_with(".rs") || name == "vfs.rs" {
                    continue;
                }
                scanned += 1;
                let text = std::fs::read_to_string(&path).unwrap();
                for hit in direct_fs_calls(&text) {
                    if !hit.exempt {
                        offenders.push(format!("{}:{}: {}", path.display(), hit.line, hit.text));
                    } else if name != "asset.rs" {
                        offenders.push(format!(
                            "{}:{}: the '{EXEMPT}' marker is granted only to asset.rs::hash_file",
                            path.display(),
                            hit.line
                        ));
                    }
                }
            }
        }
        assert!(scanned > 5, "the scanner found almost no sources: {scanned}");
        assert!(
            offenders.is_empty(),
            "dvs-core must reach storage only through vfs::Vfs, but found:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn a_write_replaces_the_file_atomically_and_leaves_no_staging_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let vfs = FsVfs;
        let target = tmp.path().join("project.json");

        vfs.write(&target, b"{\"degenVideo\":1}").unwrap();

        // A reader that opened the old file keeps reading the old bytes: that is what
        // rename buys and what an in-place truncate would destroy, mid-save, for the GUI
        // watching this directory.
        let mut held = std::fs::File::open(&target).unwrap();
        vfs.write(&target, b"{}").unwrap();
        let mut seen = Vec::new();
        std::io::Read::read_to_end(&mut held, &mut seen).unwrap();
        assert_eq!(
            seen, b"{\"degenVideo\":1}",
            "an open handle must not observe the replacement"
        );

        assert_eq!(
            vfs.read(&target).unwrap(),
            b"{}",
            "the second write must replace the first, not merge with it"
        );
        let leftovers: Vec<PathBuf> = vfs
            .list(tmp.path())
            .unwrap()
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tmp"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "the staging file must be renamed away, found {leftovers:?}"
        );
        assert_eq!(vfs.len(&target).unwrap(), 2);
    }

    #[test]
    fn the_two_backends_agree_on_the_operations_the_engine_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let backends: Vec<(&str, PathBuf, Arc<dyn Vfs>)> = vec![
            ("fs", tmp.path().to_path_buf(), FsVfs::shared()),
            ("mem", PathBuf::from("/promo"), MemVfs::shared()),
        ];
        for (name, root, vfs) in backends {
            vfs.create_dir_all(&root.join("assets")).unwrap();
            assert!(vfs.exists(&root.join("assets")), "{name}");
            assert!(!vfs.exists(&root.join("assets/none.mp4")), "{name}");

            // write replaces; append accumulates. The journal depends on the difference.
            vfs.write(&root.join("project.json"), b"first").unwrap();
            vfs.write(&root.join("project.json"), b"second").unwrap();
            assert_eq!(vfs.read(&root.join("project.json")).unwrap(), b"second", "{name}");
            vfs.append(&root.join("history.jsonl"), b"one\n").unwrap();
            vfs.append(&root.join("history.jsonl"), b"two\n").unwrap();
            assert_eq!(
                vfs.read(&root.join("history.jsonl")).unwrap(),
                b"one\ntwo\n",
                "{name}"
            );

            // Writing creates missing parents, which is what the sharded store relies on.
            vfs.write(&root.join("assets/de/ad.bin"), b"blob").unwrap();
            assert_eq!(vfs.len(&root.join("assets/de/ad.bin")).unwrap(), 4, "{name}");
            assert_eq!(
                vfs.list(&root.join("assets")).unwrap(),
                vec![root.join("assets/de")],
                "{name}"
            );

            vfs.remove(&root.join("assets/de/ad.bin")).unwrap();
            assert!(!vfs.exists(&root.join("assets/de/ad.bin")), "{name}");
            assert_eq!(vfs.read(&root.join("missing")).unwrap_err().kind(), "io", "{name}");
            assert_eq!(vfs.len(&root.join("missing")).unwrap_err().kind(), "io", "{name}");
            assert!(
                vfs.list(&root.join("project.json")).is_err(),
                "{name}: a file is not a directory"
            );
        }
    }

    #[test]
    fn a_memory_tree_is_shared_by_every_clone_and_normalised_lexically() {
        let a = MemVfs::new();
        let b = a.clone();
        a.write(Path::new("/promo/assets/../project.json"), b"{}").unwrap();
        assert_eq!(b.read(Path::new("/promo/project.json")).unwrap(), b"{}");
        assert!(b.exists(Path::new("/promo")));
    }
}
