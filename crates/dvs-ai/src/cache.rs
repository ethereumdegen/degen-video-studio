//! Request cache: re-running an edit must not re-pay for a generation.
//!
//! An agent loop is repetitive by nature. It re-applies a batch after an unrelated change,
//! it re-runs a script to produce a fresh project, it retries after a lint failure. If
//! `ai.generate-video` with identical parameters charged every time, the third iteration
//! would cost as much as the first and nobody would notice until the invoice arrived.
//!
//! So a request is identified by its content: `blake3(provider, model, canonical params)`.
//! Canonicalization is the part that actually matters — `serde_json` is built with
//! `preserve_order` in this workspace, so `{"prompt":…,"seed":…}` and `{"seed":…,"prompt":…}`
//! are *different* JSON values with identical meaning, and a key computed over raw
//! serialization would miss the cache half the time depending on how the caller's arguments
//! happened to be ordered.
//!
//! Entries live in `cache/<ai>/<key>/` beside the project's other regenerable data, so
//! `project gc` semantics stay true: deleting `cache/` costs money to rebuild but never
//! loses document state, because the generated media itself is also in the asset store.

use chrono::{DateTime, Utc};
use dvs_core::error::{Error, Result};
use dvs_core::paths::ProjectPaths;
use dvs_core::vfs::Vfs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Subdirectory of `cache/` holding provider responses.
pub const CACHE_SUBDIR: &str = "ai";

/// The record that makes an entry valid. Written last, so an interrupted download is a
/// cache miss rather than a truncated asset.
const RECORD_FILE: &str = "request.json";

/// JSON with object keys sorted at every depth and no insignificant whitespace.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                // A key is a JSON string like any other; going through serde_json keeps
                // escaping identical to the value side.
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Scalars already serialize canonically, and `to_string` on a `Value` cannot fail.
        other => out.push_str(&other.to_string()),
    }
}

/// Identity of a generation request. The domain tag keeps these hashes from ever colliding
/// with an asset hash, which is also blake3 over bytes.
pub fn request_key(provider: &str, model: &str, params: &serde_json::Value) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"dvs-ai/request/1\0");
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(model.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_json(params).as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// What was asked for, what it cost, and where the answer landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedRequest {
    pub provider: String,
    pub model: String,
    pub params: serde_json::Value,
    /// File name of the media inside the entry directory.
    pub media: String,
    /// Where the provider served it from, for forensics only — the document never points
    /// at a remote URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub cost_usd: f64,
    pub created: DateTime<Utc>,
}

/// A served entry.
#[derive(Debug, Clone)]
pub struct CacheHit {
    pub record: CachedRequest,
    /// Local path of the stored media.
    pub media: PathBuf,
}

/// `cache/ai/` inside one project.
pub struct AiCache {
    dir: PathBuf,
    vfs: Arc<dyn Vfs>,
}

impl std::fmt::Debug for AiCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiCache")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl AiCache {
    pub fn new(paths: &ProjectPaths, vfs: Arc<dyn Vfs>) -> AiCache {
        AiCache {
            dir: paths.cache_dir().join(CACHE_SUBDIR),
            vfs,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// The stored response for a key, or `None`.
    ///
    /// A record whose media file is gone counts as a miss: someone pruned `cache/` and the
    /// honest answer is to generate again, not to fail an edit.
    pub fn lookup(&self, key: &str) -> Result<Option<CacheHit>> {
        let record_path = self.entry_dir(key).join(RECORD_FILE);
        if !self.vfs.exists(&record_path) {
            return Ok(None);
        }
        let bytes = self.vfs.read(&record_path)?;
        let record: CachedRequest =
            serde_json::from_slice(&bytes).map_err(|e| Error::json(&record_path, e))?;
        let media = self.entry_dir(key).join(&record.media);
        if !self.vfs.exists(&media) {
            return Ok(None);
        }
        Ok(Some(CacheHit { record, media }))
    }

    /// Path a producer should write its output to. Used by the local TTS path, which writes
    /// a file itself instead of handing back bytes.
    pub fn prepare(&self, key: &str, file_name: &str) -> Result<PathBuf> {
        let dir = self.entry_dir(key);
        self.vfs.create_dir_all(&dir)?;
        Ok(dir.join(sanitize(file_name)))
    }

    /// Mark an entry complete. Separate from [`AiCache::prepare`] because the record is the
    /// commit point: no record, no hit.
    pub fn commit(&self, key: &str, record: &CachedRequest) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| Error::op(format!("cache record is not serializable: {e}")))?;
        self.vfs.write(&self.entry_dir(key).join(RECORD_FILE), &bytes)
    }

    /// Store a downloaded response: media first, record second.
    pub fn store(&self, key: &str, record: &CachedRequest, bytes: &[u8]) -> Result<PathBuf> {
        let media = self.prepare(key, &record.media)?;
        self.vfs.write(&media, bytes)?;
        self.commit(key, record)?;
        Ok(media)
    }
}

/// A provider-supplied file name reduced to something that cannot escape the entry
/// directory or surprise a shell.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', '-']).to_string();
    if cleaned.is_empty() {
        "media.bin".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::vfs::MemVfs;

    fn params(prompt: &str, seed: i64) -> serde_json::Value {
        serde_json::json!({ "prompt": prompt, "seed": seed, "duration": 5 })
    }

    #[test]
    fn parameter_order_does_not_change_the_key() {
        let ordered = serde_json::json!({ "a": 1, "b": { "x": true, "y": [1, 2] } });
        let shuffled = serde_json::json!({ "b": { "y": [1, 2], "x": true }, "a": 1 });

        assert_ne!(
            ordered.to_string(),
            shuffled.to_string(),
            "preserve_order is on, so the raw serializations differ and canonicalization \
             is the only thing making the keys agree"
        );
        assert_eq!(
            request_key("fal", "m", &ordered),
            request_key("fal", "m", &shuffled)
        );
    }

    #[test]
    fn prompt_model_and_seed_each_change_the_key() {
        let base = request_key("fal", "fal-ai/one", &params("a cat", 7));
        assert_eq!(base, request_key("fal", "fal-ai/one", &params("a cat", 7)));
        assert_ne!(base, request_key("fal", "fal-ai/one", &params("a dog", 7)));
        assert_ne!(base, request_key("fal", "fal-ai/two", &params("a cat", 7)));
        assert_ne!(base, request_key("fal", "fal-ai/one", &params("a cat", 8)));
        assert_ne!(base, request_key("other", "fal-ai/one", &params("a cat", 7)));
    }

    #[test]
    fn array_order_is_meaningful_and_is_not_canonicalized_away() {
        let forward = serde_json::json!({ "loras": ["a", "b"] });
        let reverse = serde_json::json!({ "loras": ["b", "a"] });
        assert_ne!(
            request_key("fal", "m", &forward),
            request_key("fal", "m", &reverse)
        );
    }

    #[test]
    fn an_entry_round_trips_and_a_pruned_media_file_is_a_miss() {
        let vfs = MemVfs::shared();
        let paths = ProjectPaths::new("/p");
        let cache = AiCache::new(&paths, vfs.clone());
        let key = request_key("fal", "m", &params("a cat", 1));
        let record = CachedRequest {
            provider: "fal".into(),
            model: "m".into(),
            params: params("a cat", 1),
            media: "media.mp4".into(),
            source_url: Some("https://fal.media/x.mp4".into()),
            cost_usd: 0.4,
            created: Utc::now(),
        };

        assert!(cache.lookup(&key).unwrap().is_none());
        let media = cache.store(&key, &record, b"not really mp4").unwrap();
        let hit = cache.lookup(&key).unwrap().expect("stored entry is found");
        assert_eq!(hit.record.cost_usd, 0.4);
        assert_eq!(hit.media, media);
        assert_eq!(vfs.read(&hit.media).unwrap(), b"not really mp4");

        vfs.remove(&media).unwrap();
        assert!(
            cache.lookup(&key).unwrap().is_none(),
            "a record without its media must not be served"
        );
    }

    #[test]
    fn a_hostile_file_name_cannot_escape_the_entry_directory() {
        let vfs = MemVfs::shared();
        let cache = AiCache::new(&ProjectPaths::new("/p"), vfs);
        let path = cache.prepare("abc", "../../etc/passwd").unwrap();
        assert_eq!(path.parent().unwrap(), cache.entry_dir("abc"));
        assert_eq!(path.file_name().unwrap(), "etc-passwd");
    }
}
