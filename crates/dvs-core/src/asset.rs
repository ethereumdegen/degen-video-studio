//! Content-addressed media store.
//!
//! Every blob an edit refers to — imported footage, music, fonts, LUTs, AI generations —
//! is stored under the blake3 hash of its bytes. That buys four things the video case needs
//! badly. `project.json` stays small and diffable because it holds a hash, not pixels. Undo
//! snapshots copy JSON, never gigabytes. Importing the same clip twice, which an agent
//! loop does constantly, costs one copy. And a render cache keyed on content is correct by
//! construction: the same bytes cannot be two different sources.
//!
//! Hashes are stored and passed around in the full `blake3:<hex>` form, because a bare hex
//! string in a document says nothing about which algorithm produced it and this project
//! expects to outlive that choice. Paths shard on the first two hex digits so a project
//! with a hundred thousand generated frames does not put them in one directory.

use crate::error::{Error, Result};
use crate::vfs::Vfs;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The algorithm tag every hash in the document carries.
pub const HASH_PREFIX: &str = "blake3:";

/// Streaming read size for [`AssetStore::hash_file`]. Large enough that syscall overhead
/// disappears against disk throughput, small enough to stay in L2.
const HASH_CHUNK: usize = 64 * 1024;

#[derive(Clone)]
pub struct AssetStore {
    dir: PathBuf,
    vfs: Arc<dyn Vfs>,
}

impl std::fmt::Debug for AssetStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// The hex digits of a hash, with or without the `blake3:` tag. Accepting both means a
/// caller that read a hash out of a file name is not forced to re-tag it.
fn hex_of(hash: &str) -> &str {
    hash.trim().strip_prefix(HASH_PREFIX).unwrap_or(hash.trim())
}

/// Two hex digits, or whatever is available for a hash too short to shard. `path_for` is
/// infallible by contract, so a malformed hash yields a path that simply will not be found
/// rather than an error at the wrong layer.
fn shard_of(hex: &str) -> &str {
    &hex[..2.min(hex.len())]
}

/// `mp4`, from `.MP4`, `mp4`, or nothing at all.
fn normalize_ext(ext: &str) -> String {
    let cleaned: String = ext
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

impl AssetStore {
    /// `dir` is the store directory itself — `ProjectPaths::assets_dir()` for a project on
    /// disk, so the layout under a project root reads `assets/<ab>/<hex>.<ext>`.
    pub fn new(dir: PathBuf, vfs: Arc<dyn Vfs>) -> Self {
        AssetStore { dir, vfs }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The storage the ops layer writes proxies, waveforms and transcripts through, so that
    /// nothing above this module needs its own filesystem handle.
    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        &self.vfs
    }

    pub fn hash_bytes(bytes: &[u8]) -> String {
        format!("{HASH_PREFIX}{}", blake3::hash(bytes).to_hex())
    }

    /// Hash a file on the host filesystem without loading it.
    ///
    /// This is the one place in `dvs-core` that calls `std::fs` outside
    /// [`crate::vfs`], and the reason is size: media is the whole point of this program,
    /// a 4 GB camera file is ordinary, and [`Vfs::read`] returns a `Vec<u8>` — hashing an
    /// import through it would mean a multi-gigabyte allocation to produce 32 bytes.
    /// Streaming needs a `Read`, which the whole-blob `Vfs` deliberately does not expose,
    /// so this function is native-only; a hypothetical browser build hashes the `File`
    /// bytes it already holds with [`AssetStore::hash_bytes`] instead.
    pub fn hash_file(path: &Path) -> Result<String> {
        use std::io::Read;
        let mut file = std::fs::File::open(path) // vfs-exempt: streaming hash, see above
            .map_err(|e| Error::io(path, e))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; HASH_CHUNK];
        loop {
            let read = file.read(&mut buffer).map_err(|e| Error::io(path, e))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(format!("{HASH_PREFIX}{}", hasher.finalize().to_hex()))
    }

    /// `<dir>/<ab>/<hex>.<ext>`.
    pub fn path_for(&self, hash: &str, ext: &str) -> PathBuf {
        let hex = hex_of(hash);
        self.dir
            .join(shard_of(hex))
            .join(format!("{hex}.{}", normalize_ext(ext)))
    }

    /// Locate a blob when the caller knows the hash but not the extension — which is the
    /// normal case, because the document stores the hash and the file name separately.
    pub fn find(&self, hash: &str) -> Result<PathBuf> {
        let hex = hex_of(hash);
        let shard = self.dir.join(shard_of(hex));
        if self.vfs.exists(&shard) {
            for entry in self.vfs.list(&shard)? {
                // Stem comparison rejects the `.tmp` staging file of an interrupted write,
                // whose stem still carries the real extension.
                if entry.file_stem().is_some_and(|stem| stem == hex) {
                    return Ok(entry);
                }
            }
        }
        Err(Error::op(format!(
            "asset blob {hash} is not in the store at {}",
            self.dir.display()
        )))
    }

    pub fn exists(&self, hash: &str) -> bool {
        self.find(hash).is_ok()
    }

    pub fn read(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.find(hash)?;
        self.vfs.read(&path)
    }

    pub fn len(&self, hash: &str) -> Result<u64> {
        let path = self.find(hash)?;
        self.vfs.len(&path)
    }

    /// Store bytes, returning their hash. Storing identical bytes again is a no-op even if
    /// the extension differs: the bytes are the identity, the extension is a label.
    pub fn import_bytes(&self, bytes: &[u8], ext: &str) -> Result<String> {
        let hash = Self::hash_bytes(bytes);
        if self.exists(&hash) {
            return Ok(hash);
        }
        self.vfs.write(&self.path_for(&hash, ext), bytes)?;
        Ok(hash)
    }

    /// Copy a file into the store, keeping its extension so `find` returns something
    /// ffprobe and the interop writers can name.
    pub fn import_path(&self, path: &Path) -> Result<String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let bytes = self.vfs.read(path)?;
        self.import_bytes(&bytes, ext)
    }

    /// Every blob in the store, as `(hash, path)`, sorted by hash.
    fn blobs(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        if !self.vfs.exists(&self.dir) {
            return Ok(out);
        }
        for shard in self.vfs.list(&self.dir)? {
            // A file directly under the store root is not ours; skip rather than fail, so
            // a stray `.DS_Store` cannot break garbage collection.
            let Ok(entries) = self.vfs.list(&shard) else {
                continue;
            };
            for entry in entries {
                let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                    out.push((format!("{HASH_PREFIX}{stem}"), entry));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Every hash currently stored, sorted. The complement of the document's
    /// [`crate::project::Project::referenced_hashes`] is what [`AssetStore::gc`] prunes.
    pub fn list(&self) -> Result<Vec<String>> {
        Ok(self.blobs()?.into_iter().map(|(hash, _)| hash).collect())
    }

    /// Delete every blob whose hash is not in `keep`, returning what went. Media is the
    /// expensive thing in a video project and an agent generates a lot of it, so this is a
    /// routine operation rather than a rescue tool.
    pub fn gc(&self, keep: &[String]) -> Result<Vec<String>> {
        let keep: BTreeSet<&str> = keep.iter().map(|hash| hex_of(hash)).collect();
        let mut removed = Vec::new();
        for (hash, path) in self.blobs()? {
            if !keep.contains(hex_of(&hash)) {
                self.vfs.remove(&path)?;
                removed.push(hash);
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FsVfs, MemVfs};

    fn store() -> AssetStore {
        AssetStore::new(PathBuf::from("/promo/assets"), MemVfs::shared())
    }

    #[test]
    fn a_blob_is_found_by_hash_alone_whatever_its_extension() {
        let store = store();
        let hash = store.import_bytes(b"moov atom", "MP4").unwrap();

        let found = store.find(&hash).unwrap();
        assert_eq!(
            found,
            store.path_for(&hash, "mp4"),
            "the extension must be normalised to lowercase"
        );
        assert!(
            found.parent().unwrap().ends_with(&hex_of(&hash)[..2]),
            "blobs shard on the first two hex digits: {found:?}"
        );
        assert_eq!(store.read(&hash).unwrap(), b"moov atom");
        assert_eq!(store.len(&hash).unwrap(), 9);

        // The caller that only has the hash — the digest, the renderer, `relink` — never
        // learns the extension, so lookup must not need it.
        assert_eq!(store.find(hex_of(&hash)).unwrap(), found);
        let err = store.find("blake3:deadbeef").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::exit::OP_ERROR);
        assert!(!store.exists("blake3:deadbeef"));
    }

    #[test]
    fn importing_the_same_file_twice_yields_one_blob_and_one_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("talk.mp4");
        let second = tmp.path().join("copies/talk-copy.mp4");
        let bytes = b"the same twelve hundred megabytes, abridged".to_vec();
        let vfs = FsVfs::shared();
        vfs.write(&first, &bytes).unwrap();
        vfs.write(&second, &bytes).unwrap();

        let store = AssetStore::new(tmp.path().join("store"), Arc::clone(&vfs));
        let a = store.import_path(&first).unwrap();
        let b = store.import_path(&second).unwrap();

        assert_eq!(a, b, "identical bytes are one asset regardless of file name");
        assert_eq!(a, AssetStore::hash_bytes(&bytes));
        assert_eq!(
            store.list().unwrap(),
            vec![a.clone()],
            "the second import must not create a second copy"
        );
        assert_eq!(store.read(&a).unwrap(), bytes);
    }

    #[test]
    fn a_second_extension_for_known_bytes_does_not_duplicate_the_blob() {
        let store = store();
        let first = store.import_bytes(b"ID3 tags", "mp3").unwrap();
        let second = store.import_bytes(b"ID3 tags", "wav").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.find(&first).unwrap(),
            store.path_for(&first, "mp3"),
            "the first extension wins; the bytes are the identity"
        );
    }

    #[test]
    fn gc_removes_exactly_the_unreferenced_blobs() {
        let store = store();
        let used_video = store.import_bytes(b"footage", "mp4").unwrap();
        let used_music = store.import_bytes(b"music", "mp3").unwrap();
        let orphan_a = store.import_bytes(b"an abandoned generation", "mp4").unwrap();
        let orphan_b = store.import_bytes(b"a superseded render", "mov").unwrap();

        let removed = store
            .gc(&[used_video.clone(), used_music.clone()])
            .unwrap();

        assert_eq!(
            removed.iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([&orphan_a, &orphan_b])
        );
        assert_eq!(
            store.list().unwrap().into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([used_video.clone(), used_music.clone()])
        );
        assert_eq!(store.read(&used_video).unwrap(), b"footage");
        assert!(!store.exists(&orphan_a));

        // Idempotent: a second pass with the same keep-list has nothing left to do.
        assert!(store.gc(&[used_video, used_music]).unwrap().is_empty());
    }

    #[test]
    fn gc_keeps_nothing_when_the_document_references_nothing() {
        let store = store();
        store.import_bytes(b"a", "mp4").unwrap();
        store.import_bytes(b"b", "mp4").unwrap();
        assert_eq!(store.gc(&[]).unwrap().len(), 2);
        assert!(store.list().unwrap().is_empty());
    }

    /// The streaming hash must agree with the one-shot hash for content that spans several
    /// buffer fills and ends mid-buffer — the shape that catches a hasher reset per chunk,
    /// a dropped final partial read, or a stale-tail bug from reusing the buffer.
    #[test]
    fn the_streamed_hash_of_a_large_file_matches_the_in_memory_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.raw");
        let bytes: Vec<u8> = (0..HASH_CHUNK * 3 + 517)
            .map(|i| (i % 251) as u8)
            .collect();
        FsVfs.write(&path, &bytes).unwrap();

        assert_eq!(
            AssetStore::hash_file(&path).unwrap(),
            AssetStore::hash_bytes(&bytes)
        );
        assert!(AssetStore::hash_file(&tmp.path().join("absent")).is_err());
    }

    #[test]
    fn an_empty_store_directory_is_not_an_error() {
        let store = store();
        assert!(store.list().unwrap().is_empty());
        assert!(store.gc(&["blake3:whatever".to_string()]).unwrap().is_empty());
    }
}
