//! Append-only op journal: `history.jsonl`, one JSON object per line.
//!
//! Undo is a stored RFC-6902 inverse patch rather than a hand-written `undo()` per op, so
//! the catalog can grow without growing the undo surface. Two decisions make this file
//! more than a log.
//!
//! **It is append-only, including undo.** An undo does not flip a flag on an earlier
//! record; it appends a `project.undo` entry pointing at the record it reversed, and redo
//! appends a `project.redo`. The consequence is the one an agent needs: `history.jsonl`
//! read top to bottom is the literal sequence of events, so a human clicking undo in the
//! Tauri app over an agent's edit shows up in the same stream as the edit, with a cause and
//! a timestamp. A flag-based journal silently rewrites the past and two writers racing on
//! it lose edits.
//!
//! **A crash mid-append is recoverable.** The last line may be a partial record; [`Journal::open`]
//! drops it and rewrites the file, because appending after a truncated line would glue the
//! next record onto the garbage and turn a recoverable tail into an unparseable middle.

use crate::error::{Error, Result};
use crate::vfs::Vfs;
use chrono::{DateTime, Utc};
use json_patch::Patch;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The op id appended when an entry is reversed. Also a registered op, so undo is a normal
/// CLI verb and MCP tool rather than a special case.
pub const OP_UNDO: &str = "project.undo";

/// The op id appended when an undo is re-applied.
pub const OP_REDO: &str = "project.redo";

/// Who made the edit. Provenance matters here because the document is written by three
/// different kinds of author and "why is this clip 200 ms short" has a different answer
/// for each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// A CLI or MCP call: the default author of everything in an agent session.
    #[default]
    Agent,
    /// A person in the GUI.
    Human,
    /// A model that generated content rather than an operator driving the tool.
    Ai,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// 1-based, dense, assigned on append. Stable: `project.undo` refers to it.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub actor: Actor,
    pub op: String,
    pub args: serde_json::Value,
    /// Before → after. Replaying every entry's `patch` in order reproduces the document.
    pub patch: Patch,
    /// After → before. Applying it is undo.
    pub inverse: Patch,
    /// For `project.undo` and `project.redo`: the `seq` of the entry they act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
}

impl Entry {
    /// Undo and redo records are bookkeeping about other entries; the undo stack walks
    /// past them rather than undoing an undo.
    pub fn is_meta(&self) -> bool {
        self.op == OP_UNDO || self.op == OP_REDO
    }
}

pub struct Journal {
    path: PathBuf,
    vfs: Arc<dyn Vfs>,
    entries: Vec<Entry>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("path", &self.path)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Journal {
    /// Read the whole history. It is loaded eagerly because undo needs the tail, `history`
    /// needs the tail, and a journal of a few thousand lines is smaller than the project.
    pub fn open(path: PathBuf, vfs: Arc<dyn Vfs>) -> Result<Journal> {
        let mut entries = Vec::new();
        let mut truncated = false;
        if vfs.exists(&path) {
            let bytes = vfs.read(&path)?;
            // A complete append ends in a newline; a torn one cannot, since the newline is
            // the last byte written. That is what separates "crashed" from "corrupt".
            let complete = bytes.last() == Some(&b'\n');
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Entry>(line) {
                    Ok(entry) => entries.push(entry),
                    Err(error) => {
                        if index + 1 == lines.len() && !complete {
                            truncated = true;
                        } else {
                            return Err(Error::json(&path, error));
                        }
                    }
                }
            }
        }
        let journal = Journal { path, vfs, entries };
        if truncated {
            journal.rewrite()?;
        }
        Ok(journal)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The newest `n` entries, oldest first — what `dvs project history` prints and what an
    /// agent reads to find out what a human just did in the GUI.
    pub fn tail(&self, n: usize) -> &[Entry] {
        &self.entries[self.entries.len().saturating_sub(n)..]
    }

    pub fn entry(&self, seq: u64) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.seq == seq)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record an op. The engine passes the patches it computed from the committed
    /// transition, so this function never has to see a `Project`.
    pub fn append(
        &mut self,
        op: impl Into<String>,
        args: serde_json::Value,
        patch: Patch,
        inverse: Patch,
        actor: Actor,
        target: Option<u64>,
    ) -> Result<u64> {
        let seq = self.entries.last().map_or(1, |last| last.seq + 1);
        let entry = Entry {
            seq,
            ts: Utc::now(),
            actor,
            op: op.into(),
            args,
            patch,
            inverse,
            target,
        };
        let mut line = serde_json::to_string(&entry).map_err(|e| Error::json(&self.path, e))?;
        line.push('\n');
        self.vfs.append(&self.path, line.as_bytes())?;
        self.entries.push(entry);
        Ok(seq)
    }

    /// The entry an `undo` would reverse: the newest real op whose effect is still in the
    /// document. Meta entries are skipped, and an op that was undone and not redone is
    /// already reversed.
    pub fn next_undoable(&self) -> Option<&Entry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| !entry.is_meta() && self.is_applied(entry.seq))
    }

    /// The `project.undo` entry a `redo` would reverse: the newest one that no later
    /// `project.redo` has already answered.
    pub fn next_redoable(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|entry| {
            entry.op == OP_UNDO
                && entry.target.is_some()
                && !self.entries.iter().any(|later| {
                    later.op == OP_REDO && later.target == entry.target && later.seq > entry.seq
                })
        })
    }

    /// Whether the effect of entry `seq` is currently in the document. The last meta entry
    /// pointing at it decides; with none, it was never undone.
    fn is_applied(&self, seq: u64) -> bool {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.is_meta() && entry.target == Some(seq))
            .is_none_or(|entry| entry.op == OP_REDO)
    }

    /// Rewrite the whole file. Only used to drop a torn tail: the write is atomic, so the
    /// repair itself cannot lose history.
    fn rewrite(&self) -> Result<()> {
        let mut buffer = String::new();
        for entry in &self.entries {
            buffer.push_str(&serde_json::to_string(entry).map_err(|e| Error::json(&self.path, e))?);
            buffer.push('\n');
        }
        self.vfs.write(&self.path, buffer.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemVfs;
    use serde_json::json;

    fn patch(from: serde_json::Value, to: serde_json::Value) -> (Patch, Patch) {
        (json_patch::diff(&from, &to), json_patch::diff(&to, &from))
    }

    fn record(journal: &mut Journal, op: &str, name: &str) -> u64 {
        let (forward, inverse) = patch(json!({ "name": "before" }), json!({ "name": name }));
        journal
            .append(op, json!({ "name": name }), forward, inverse, Actor::Agent, None)
            .unwrap()
    }

    fn journal(vfs: &Arc<dyn Vfs>) -> Journal {
        Journal::open(PathBuf::from("/promo/history.jsonl"), Arc::clone(vfs)).unwrap()
    }

    #[test]
    fn a_torn_final_line_is_dropped_and_the_history_continues() {
        let vfs: Arc<dyn Vfs> = MemVfs::shared();
        let path = PathBuf::from("/promo/history.jsonl");
        {
            let mut journal = journal(&vfs);
            record(&mut journal, "clip.split", "one");
            record(&mut journal, "clip.trim", "two");
        }

        // Simulate a crash partway through the third append.
        let mut bytes = vfs.read(&path).unwrap();
        bytes.extend_from_slice(
            br#"{"seq":3,"ts":"2026-09-21T00:00:00Z","actor":"agent","op":"seq.ne"#,
        );
        vfs.write(&path, &bytes).unwrap();

        let mut journal = journal(&vfs);
        assert_eq!(
            journal.entries().iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2],
            "the readable prefix of the history must survive"
        );
        assert_eq!(journal.entries()[1].op, "clip.trim");
        let repaired = vfs.read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&repaired).contains("seq.ne"),
            "the torn record must be removed from the file, not just skipped in memory"
        );
        assert_eq!(repaired.last(), Some(&b'\n'));

        assert_eq!(record(&mut journal, "clip.remove", "three"), 3);
        let reopened = journal_at(&vfs, &path);
        assert_eq!(
            reopened.entries().iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the append after the repair must be readable"
        );
        assert_eq!(reopened.entries()[2].op, "clip.remove");
    }

    fn journal_at(vfs: &Arc<dyn Vfs>, path: &Path) -> Journal {
        Journal::open(path.to_path_buf(), Arc::clone(vfs)).unwrap()
    }

    #[test]
    fn a_corrupt_line_in_the_middle_is_an_error_rather_than_silent_data_loss() {
        let vfs: Arc<dyn Vfs> = MemVfs::shared();
        let path = PathBuf::from("/promo/history.jsonl");
        {
            let mut journal = journal(&vfs);
            record(&mut journal, "clip.split", "one");
            record(&mut journal, "clip.trim", "two");
        }
        let text = String::from_utf8(vfs.read(&path).unwrap()).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[0] = "{not json";
        vfs.write(&path, format!("{}\n", lines.join("\n")).as_bytes())
            .unwrap();

        let err = Journal::open(path, Arc::clone(&vfs)).unwrap_err();
        assert_eq!(err.kind(), "json");
    }

    #[test]
    fn the_undo_stack_walks_real_ops_and_ignores_its_own_bookkeeping() {
        let vfs: Arc<dyn Vfs> = MemVfs::shared();
        let mut journal = journal(&vfs);
        record(&mut journal, "clip.split", "one");
        record(&mut journal, "clip.trim", "two");
        record(&mut journal, "clip.move", "three");

        assert_eq!(journal.next_undoable().unwrap().seq, 3);
        assert!(journal.next_redoable().is_none());

        let undo = |journal: &mut Journal, target: u64| {
            journal
                .append(
                    OP_UNDO,
                    json!({ "seq": target }),
                    Patch::default(),
                    Patch::default(),
                    Actor::Human,
                    Some(target),
                )
                .unwrap()
        };
        let redo = |journal: &mut Journal, target: u64| {
            journal
                .append(
                    OP_REDO,
                    json!({ "seq": target }),
                    Patch::default(),
                    Patch::default(),
                    Actor::Agent,
                    Some(target),
                )
                .unwrap()
        };

        let first_undo = undo(&mut journal, 3);
        assert_eq!(
            journal.next_undoable().unwrap().seq,
            2,
            "an undone op is no longer the thing to undo"
        );
        assert_eq!(journal.next_redoable().unwrap().seq, first_undo);

        undo(&mut journal, 2);
        assert_eq!(journal.next_undoable().unwrap().seq, 1);
        assert_eq!(
            journal.next_redoable().unwrap().target,
            Some(2),
            "redo is last-in-first-out over the undos"
        );

        redo(&mut journal, 2);
        assert_eq!(journal.next_undoable().unwrap().seq, 2);
        assert_eq!(
            journal.next_redoable().unwrap().target,
            Some(3),
            "the redone undo must drop out of the redo stack"
        );

        redo(&mut journal, 3);
        assert_eq!(journal.next_undoable().unwrap().seq, 3);
        assert!(journal.next_redoable().is_none());

        // Undoing the same entry a second time after a redo must work: the *last* meta
        // entry decides, not the first.
        undo(&mut journal, 3);
        assert_eq!(journal.next_undoable().unwrap().seq, 2);
        assert_eq!(journal.next_redoable().unwrap().target, Some(3));
    }

    #[test]
    fn entries_carry_the_author_and_the_tail_is_the_newest_slice() {
        let vfs: Arc<dyn Vfs> = MemVfs::shared();
        let mut journal = journal(&vfs);
        record(&mut journal, "clip.split", "one");
        journal
            .append(
                "clip.trim",
                json!({}),
                Patch::default(),
                Patch::default(),
                Actor::Human,
                None,
            )
            .unwrap();

        assert_eq!(journal.tail(1).len(), 1);
        assert_eq!(journal.tail(1)[0].actor, Actor::Human);
        assert_eq!(journal.tail(99).len(), 2, "tail clamps to what exists");
        assert_eq!(journal.entry(1).unwrap().actor, Actor::Agent);

        let line = String::from_utf8(vfs.read(journal.path()).unwrap()).unwrap();
        assert!(
            line.lines().nth(1).unwrap().contains("\"actor\":\"human\""),
            "the author is part of the on-disk record: {line}"
        );
    }
}
