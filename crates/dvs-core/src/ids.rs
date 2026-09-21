//! Typed, stable, prefixed ids.
//!
//! Indices are not addresses: an agent that says "clip 3" is wrong the moment a ripple
//! insert happens. Every addressable thing carries a ULID with a type prefix, which is
//! monotonic (so ids sort by creation), copy-pasteable, and self-describing in an error
//! message. Names exist too, but they are labels — the id is the identity.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::{LazyLock, Mutex};
use std::fmt;
use std::str::FromStr;

/// One process-wide monotonic ULID source.
///
/// `Ulid::new()` is only ordered *across* milliseconds: two ids minted in the same
/// millisecond get independent random tails, so half the time the later one sorts first.
/// That matters here because ids are read by humans and agents as a creation order —
/// "which clip did the last insert make?" — and because a journal or a bin listing sorted
/// by id should match the order things happened. `Generator` keeps the timestamp and
/// increments the random field instead, which restores the ordering guarantee.
static IDS: LazyLock<Mutex<ulid::Generator>> =
    LazyLock::new(|| Mutex::new(ulid::Generator::new()));

fn next_ulid() -> ulid::Ulid {
    match IDS.lock() {
        // The generator only fails if a millisecond's 80-bit space is exhausted (2^80 ids)
        // or the clock jumped backwards; a fresh random ULID is still unique, it just may
        // not sort after its predecessor.
        Ok(mut generator) => generator.generate().unwrap_or_else(|_| ulid::Ulid::new()),
        Err(poisoned) => poisoned.into_inner().generate().unwrap_or_else(|_| ulid::Ulid::new()),
    }
}

macro_rules! id_type {
    ($name:ident, $prefix:literal, $what:literal) => {
        #[doc = concat!("Identifier for a ", $what, ", `", $prefix, "_<ulid>`.")]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// A fresh id. Monotonic: ids created later sort later, within a process.
            pub fn new() -> Self {
                $name(format!("{}_{}", $prefix, next_ulid()))
            }

            /// Wrap an existing string without validating: for deserialization of documents
            /// written by older versions, and for tests that want readable ids.
            pub fn from_raw(raw: impl Into<String>) -> Self {
                $name(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn has_prefix(&self) -> bool {
                self.0.starts_with(concat!($prefix, "_"))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(text: &str) -> Result<Self> {
                if text.trim().is_empty() {
                    return Err(Error::bad_args(concat!("empty ", $what, " id")));
                }
                Ok($name(text.trim().to_string()))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(text: &str) -> Self {
                $name(text.to_string())
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "description": concat!($what, " id, '", $prefix, "_<ulid>'")
                })
            }
        }
    };
}

id_type!(ProjectId, "prj", "project");
id_type!(AssetId, "ast", "media asset");
id_type!(SequenceId, "seq", "sequence");
id_type!(TrackId, "trk", "track");
id_type!(ClipId, "clp", "clip");
id_type!(EffectId, "fx", "effect");
id_type!(MarkerId, "mk", "marker");
id_type!(CueId, "cue", "caption cue");
id_type!(StyleId, "sty", "caption style");
id_type!(TitleId, "ttl", "title document");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_monotonic() {
        let first = ClipId::new();
        let second = ClipId::new();
        assert!(first.has_prefix() && second.has_prefix());
        assert!(first < second, "{first} should sort before {second}");
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = AssetId::from_raw("ast_talk");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"ast_talk\"");
        assert_eq!(
            serde_json::from_str::<AssetId>("\"ast_talk\"").unwrap(),
            id
        );
    }
}
