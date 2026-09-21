//! The engine: applies ops transactionally, journals them, and saves.
//!
//! One rule governs everything here: **validate fully, then mutate.** An op runs against a
//! clone of the document; only a run that succeeds *and* leaves every sequence valid is
//! committed, journaled and written. A failure anywhere — including the seventeenth op of
//! a batch — leaves `project.json` and `history.jsonl` exactly as they were. That matters
//! more for an agent than for a human: a human notices a half-applied edit on screen, an
//! agent reads back a document it believes in and compounds the damage.
//!
//! The post-apply check is [`crate::project::Sequence::validate`] over every sequence, not
//! just the one the op touched, because ops move clips between tracks and nest sequences.
//! It is cheap next to the JSON diff and it is the last gate before a corrupt document
//! reaches disk, where the next `dvs` invocation would inherit it.
//!
//! `modified` is deliberately outside the patch chain: it is a write stamp, like an mtime,
//! so undo does not rewind the clock and the journal contains no entries that only bump a
//! timestamp.

use crate::asset::AssetStore;
use crate::error::{Error, Result};
use crate::journal::{Actor, Journal, OP_REDO, OP_UNDO};
use crate::op::{Op, OpCx, OpEffect, Registry};
use crate::paths::ProjectPaths;
use crate::project::Project;
use crate::vfs::{FsVfs, Vfs};
use json_patch::Patch;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

/// An open project: the document plus the three things on disk that belong to it.
pub struct Workspace {
    pub project: Project,
    pub paths: ProjectPaths,
    pub assets: AssetStore,
    pub journal: Journal,
    vfs: Arc<dyn Vfs>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.paths.root())
            .field("project", &self.project.id)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Lay out a new project directory and write the document into it.
    ///
    /// The three directories are created up front rather than on first use so that a fresh
    /// project is immediately recognisable — and rsync-able, and committable — as a project
    /// rather than a lone JSON file.
    pub fn create(paths: ProjectPaths, project: Project, vfs: Arc<dyn Vfs>) -> Result<Workspace> {
        if vfs.exists(&paths.project_json()) {
            return Err(Error::op(format!(
                "{} already contains a project; open it instead of creating one over it",
                paths.root().display()
            )));
        }
        vfs.create_dir_all(paths.root())?;
        vfs.create_dir_all(&paths.assets_dir())?;
        vfs.create_dir_all(&paths.transcripts_dir())?;
        vfs.create_dir_all(&paths.cache_dir())?;

        let assets = AssetStore::new(paths.assets_dir(), Arc::clone(&vfs));
        let journal = Journal::open(paths.history(), Arc::clone(&vfs))?;
        let mut workspace = Workspace {
            project,
            paths,
            assets,
            journal,
            vfs,
        };
        workspace.save()?;
        Ok(workspace)
    }

    pub fn open(paths: ProjectPaths, vfs: Arc<dyn Vfs>) -> Result<Workspace> {
        let file = paths.project_json();
        let bytes = vfs.read(&file)?;
        let project: Project =
            serde_json::from_slice(&bytes).map_err(|e| Error::json(&file, e))?;
        // A document from a newer build may use fields this one would drop on the next
        // save, so refusing to open is refusing to destroy someone's edit.
        project.check_format()?;

        let assets = AssetStore::new(paths.assets_dir(), Arc::clone(&vfs));
        let journal = Journal::open(paths.history(), Arc::clone(&vfs))?;
        Ok(Workspace {
            project,
            paths,
            assets,
            journal,
            vfs,
        })
    }

    /// Open the project containing `dir`, walking up the way `git` does, on the host
    /// filesystem. This is the entry point for the CLI and the Tauri shell.
    pub fn open_native(dir: impl AsRef<Path>) -> Result<Workspace> {
        let dir = dir.as_ref();
        let paths = ProjectPaths::discover(dir).ok_or_else(|| {
            Error::op(format!(
                "no dvs project in {} or any parent directory; run 'dvs new' first",
                dir.display()
            ))
        })?;
        Workspace::open(paths, FsVfs::shared())
    }

    /// Write `project.json`. Pretty-printed with a trailing newline because this file is
    /// meant to be read, diffed and committed by humans and agents alike; the atomicity is
    /// the [`Vfs`]'s.
    pub fn save(&mut self) -> Result<()> {
        self.project.touch();
        let file = self.paths.project_json();
        let mut bytes =
            serde_json::to_vec_pretty(&self.project).map_err(|e| Error::json(&file, e))?;
        bytes.push(b'\n');
        self.vfs.write(&file, &bytes)
    }

    /// The storage this project is open on, for ops that write proxies, transcripts and
    /// render output beside the document.
    pub fn vfs(&self) -> Arc<dyn Vfs> {
        Arc::clone(&self.vfs)
    }
}

/// The result of one op: what it is, where it landed in the history, and what it changed.
/// The effect is flattened so `--json` yields one flat object per op.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub effect: OpEffect,
}

pub struct Engine {
    pub registry: Registry,
    pub workspace: Workspace,
    pub actor: Actor,
}

impl Engine {
    pub fn new(registry: Registry, workspace: Workspace) -> Self {
        Engine {
            registry,
            workspace,
            actor: Actor::Agent,
        }
    }

    /// Attribute subsequent edits to a person. The Tauri app calls this; everything else is
    /// an agent by default, which is the honest default for this tool.
    pub fn as_human(mut self) -> Self {
        self.actor = Actor::Human;
        self
    }

    pub fn apply(
        &mut self,
        op_id: &str,
        args: serde_json::Value,
        sequence: Option<String>,
        dry_run: bool,
    ) -> Result<Applied> {
        let mut applied =
            self.apply_batch(vec![(op_id.to_string(), args, sequence)], dry_run)?;
        Ok(applied.remove(0))
    }

    /// Apply many ops as one transaction. This is what turns a thirty-edit build into one
    /// MCP round trip instead of thirty model turns, and the reason it is safe to do so is
    /// that a failure at any point writes nothing.
    ///
    /// Undo stays per-op: each mutating op gets its own journal entry, with patches sliced
    /// out of the single committed transition, so the history reads the same whether the
    /// edits arrived one at a time or in one call.
    pub fn apply_batch(
        &mut self,
        ops: Vec<(String, serde_json::Value, Option<String>)>,
        dry_run: bool,
    ) -> Result<Vec<Applied>> {
        // Resolve every id before running anything: an unknown verb in position 17 should
        // fail as a lookup, not as a batch that ran sixteen ops and then rolled back.
        let resolved: Vec<Arc<dyn Op>> = ops
            .iter()
            .map(|(id, _, _)| self.registry.get(id).map(Arc::clone))
            .collect::<Result<_>>()?;

        let mut candidate = self.workspace.project.clone();
        // Successive document states; `states[i]` is the input of the i-th journaled op and
        // `states[i + 1]` its output. Keeping them avoids re-serializing for the diff.
        let mut states = vec![self.document(&candidate)?];
        let mut journaled: Vec<Journaled> = Vec::new();
        let mut results = Vec::with_capacity(ops.len());

        for (op, (id, args, sequence)) in resolved.into_iter().zip(ops) {
            let effect = {
                let mut cx = OpCx::new(&self.workspace.paths, &self.workspace.assets)
                    .with_sequence(sequence)
                    .dry_run(dry_run);
                op.apply(&mut candidate, args.clone(), &mut cx)?
            };
            validate(&candidate)?;

            let next = self.document(&candidate)?;
            // A query never enters the history, and neither does an op that asked for the
            // state the document was already in.
            if !op.is_query() && next != *states.last().expect("seeded above") {
                journaled.push(Journaled {
                    result: results.len(),
                    op: id.clone(),
                    args,
                    from: states.len() - 1,
                });
                states.push(next);
            }
            results.push(Applied {
                op: id,
                seq: None,
                effect,
            });
        }

        if dry_run || journaled.is_empty() {
            return Ok(results);
        }

        // Commit. The document is written before the history is extended: if the append
        // then fails, the edit is on disk without a history entry — recoverable — whereas
        // the reverse order would leave an entry whose inverse patch does not fit the
        // document it claims to describe, and undo would be the thing that breaks.
        self.workspace.project = candidate;
        self.workspace.save()?;
        for entry in journaled {
            let before = &states[entry.from];
            let after = &states[entry.from + 1];
            let seq = self.workspace.journal.append(
                entry.op,
                entry.args,
                json_patch::diff(before, after),
                json_patch::diff(after, before),
                self.actor,
                None,
            )?;
            results[entry.result].seq = Some(seq);
        }
        Ok(results)
    }

    /// Reverse the newest op still in effect. `Ok(None)` when there is nothing to undo.
    pub fn undo(&mut self) -> Result<Option<String>> {
        let Some(entry) = self.workspace.journal.next_undoable() else {
            return Ok(None);
        };
        let target = entry.seq;
        let op = entry.op.clone();
        let forward = entry.patch.clone();
        let inverse = entry.inverse.clone();

        self.repatch(&inverse, &format!("undo of '{op}' (#{target})"))?;
        self.workspace.save()?;
        // The undo is itself an op in the stream, carrying the patch it applied, so a
        // replay of the history reproduces the document and a human's GUI undo of an
        // agent's edit is visible to the agent as an event with a cause.
        self.workspace.journal.append(
            OP_UNDO,
            serde_json::json!({ "seq": target, "op": op }),
            inverse,
            forward,
            self.actor,
            Some(target),
        )?;
        Ok(Some(op))
    }

    /// Re-apply the newest undone op. `Ok(None)` when there is nothing to redo.
    pub fn redo(&mut self) -> Result<Option<String>> {
        let Some(undone) = self.workspace.journal.next_redoable() else {
            return Ok(None);
        };
        let target = undone
            .target
            .expect("next_redoable only returns entries that name a target");
        let original = self.workspace.journal.entry(target).ok_or_else(|| {
            Error::op(format!("history entry #{target} is missing; cannot redo it"))
        })?;
        let op = original.op.clone();
        let forward = original.patch.clone();
        let inverse = original.inverse.clone();

        self.repatch(&forward, &format!("redo of '{op}' (#{target})"))?;
        self.workspace.save()?;
        self.workspace.journal.append(
            OP_REDO,
            serde_json::json!({ "seq": target, "op": op }),
            forward,
            inverse,
            self.actor,
            Some(target),
        )?;
        Ok(Some(op))
    }

    /// Apply a stored patch to the live document. The result goes through the same
    /// validation as an op: a patch that no longer fits — because the document was edited
    /// by hand between sessions — must fail loudly rather than half-apply.
    fn repatch(&mut self, patch: &Patch, what: &str) -> Result<()> {
        let mut value = self.document(&self.workspace.project)?;
        json_patch::patch(&mut value, patch)
            .map_err(|e| Error::op(format!("{what} does not fit the current document: {e}")))?;
        let restored: Project = serde_json::from_value(value)
            .map_err(|e| Error::json(self.workspace.paths.project_json(), e))?;
        validate(&restored)?;
        self.workspace.project = restored;
        Ok(())
    }

    fn document(&self, project: &Project) -> Result<serde_json::Value> {
        serde_json::to_value(project)
            .map_err(|e| Error::json(self.workspace.paths.project_json(), e))
    }
}

/// One op that earned a history entry, and where to find its before/after states.
struct Journaled {
    result: usize,
    op: String,
    args: serde_json::Value,
    from: usize,
}

/// Every invariant the document model promises, checked over the whole project.
fn validate(project: &Project) -> Result<()> {
    for sequence in project.sequences.values() {
        sequence.validate()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::exit;
    use crate::ids::SequenceId;
    use crate::op::args;
    use crate::project::{Clip, Generator, Source, Track, TrackKind};
    use crate::time::{Fps, Time};
    use crate::vfs::MemVfs;
    use serde_json::json;

    /// Renames the project: the smallest possible real mutation, so the tests are about
    /// the engine rather than about an op.
    struct Rename;
    impl Op for Rename {
        fn id(&self) -> &'static str {
            "test.rename"
        }
        fn about(&self) -> &'static str {
            "rename the project"
        }
        fn schema(&self) -> serde_json::Value {
            json!({ "type": "object", "properties": { "name": { "type": "string" } } })
        }
        fn apply(
            &self,
            project: &mut Project,
            args: serde_json::Value,
            _cx: &mut OpCx,
        ) -> Result<OpEffect> {
            args::reject_unknown(&args, &["name"])?;
            project.name = args::str_field(&args, "name")?.to_string();
            Ok(OpEffect::new().changed(project.id.clone()))
        }
    }

    /// Mutates and then fails, which is the case a "check first, write later" engine gets
    /// wrong: the partial mutation must not survive.
    struct Explode;
    impl Op for Explode {
        fn id(&self) -> &'static str {
            "test.explode"
        }
        fn about(&self) -> &'static str {
            "mutate, then fail"
        }
        fn schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        fn apply(
            &self,
            project: &mut Project,
            _args: serde_json::Value,
            _cx: &mut OpCx,
        ) -> Result<OpEffect> {
            project.name = "corrupted".to_string();
            Err(Error::op("the encoder fell over"))
        }
    }

    /// Produces two clips occupying the same time on one track — the invariant the document
    /// model says ops cannot produce.
    struct Overlap;
    impl Op for Overlap {
        fn id(&self) -> &'static str {
            "test.overlap"
        }
        fn about(&self) -> &'static str {
            "place a clip on top of another"
        }
        fn schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        fn apply(
            &self,
            project: &mut Project,
            _args: serde_json::Value,
            cx: &mut OpCx,
        ) -> Result<OpEffect> {
            let sequence = cx.sequence(project)?;
            let track = project
                .sequence_mut(&sequence)?
                .tracks
                .first_mut()
                .ok_or_else(|| Error::op("no track"))?;
            let (source, start) = {
                let first = track.clips.first().ok_or_else(|| Error::op("no clip"))?;
                (first.source.clone(), first.start)
            };
            let clip = Clip::new(source, start + Time::from_secs(1), Time::from_secs(4));
            let id = clip.id.clone();
            track.clips.push(clip);
            Ok(OpEffect::new().created(id))
        }
    }

    /// A read-only op, to prove queries never reach the history.
    struct CountTracks;
    impl Op for CountTracks {
        fn id(&self) -> &'static str {
            "test.count"
        }
        fn about(&self) -> &'static str {
            "count tracks"
        }
        fn is_query(&self) -> bool {
            true
        }
        fn schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        fn apply(
            &self,
            project: &mut Project,
            _args: serde_json::Value,
            cx: &mut OpCx,
        ) -> Result<OpEffect> {
            let sequence = cx.sequence(project)?;
            let count = project.sequence(&sequence)?.tracks.len();
            Ok(OpEffect::new().data(json!({ "tracks": count })))
        }
    }

    fn sample_project() -> Project {
        let mut project = Project::new("promo", Fps::new(30, 1).unwrap(), [1920, 1080], 48000);
        let sequence: SequenceId = project.active_sequence.clone();
        let mut track = Track::new("V1", TrackKind::Video);
        track.clips.push(Clip::new(
            Source::Generator {
                generator: Generator::Bars,
                params: serde_json::Map::new(),
            },
            Time::ZERO,
            Time::from_secs(4),
        ));
        project.sequence_mut(&sequence).unwrap().tracks.push(track);
        project
    }

    fn fixture() -> (Arc<MemVfs>, ProjectPaths, Engine) {
        let vfs = Arc::new(MemVfs::new());
        let paths = ProjectPaths::new("/promo");
        let workspace =
            Workspace::create(paths.clone(), sample_project(), vfs.clone()).unwrap();
        let mut registry = Registry::new();
        registry
            .register(Rename)
            .register(Explode)
            .register(Overlap)
            .register(CountTracks);
        (vfs, paths, Engine::new(registry, workspace))
    }

    /// `modified` is a write stamp rather than document state, so comparisons of "did we
    /// get back to where we were" ignore it.
    fn document(project: &Project) -> serde_json::Value {
        let mut value = serde_json::to_value(project).unwrap();
        value.as_object_mut().unwrap().remove("modified");
        value
    }

    #[test]
    fn a_new_workspace_is_a_directory_layout_and_a_readable_document() {
        let vfs = Arc::new(MemVfs::new());
        let paths = ProjectPaths::new("/promo");
        let created =
            Workspace::create(paths.clone(), sample_project(), vfs.clone()).unwrap();

        for dir in [
            paths.assets_dir(),
            paths.transcripts_dir(),
            paths.cache_dir(),
        ] {
            assert!(vfs.exists(&dir), "{} must exist", dir.display());
        }
        let bytes = vfs.read(&paths.project_json()).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "the document ends in a newline so a diff is one line, not the whole file"
        );
        assert!(
            text.contains("\n  \"degenVideo\": 1"),
            "the document is pretty-printed: {text:.80}"
        );

        let reopened = Workspace::open(paths.clone(), vfs.clone()).unwrap();
        assert_eq!(reopened.project.id, created.project.id);
        assert_eq!(reopened.project.sequences.len(), 1);

        let err = Workspace::create(paths, sample_project(), vfs).unwrap_err();
        assert!(
            err.to_string().contains("already contains a project"),
            "creating over an existing project must refuse: {err}"
        );
    }

    #[test]
    fn a_document_from_a_newer_build_is_refused_rather_than_reinterpreted() {
        let vfs = Arc::new(MemVfs::new());
        let paths = ProjectPaths::new("/promo");
        Workspace::create(paths.clone(), sample_project(), vfs.clone()).unwrap();

        let mut value: serde_json::Value =
            serde_json::from_slice(&vfs.read(&paths.project_json()).unwrap()).unwrap();
        value["degenVideo"] = json!(99);
        vfs.write(
            &paths.project_json(),
            serde_json::to_vec(&value).unwrap().as_slice(),
        )
        .unwrap();

        let err = Workspace::open(paths, vfs).unwrap_err();
        assert!(err.to_string().contains("newer than this build"), "{err}");
    }

    #[test]
    fn open_native_finds_the_project_from_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("promo");
        let paths = ProjectPaths::new(&root);
        Workspace::create(paths.clone(), sample_project(), FsVfs::shared()).unwrap();

        let opened = Workspace::open_native(paths.proxy_dir()).unwrap();
        assert_eq!(opened.paths.root(), root);
        assert_eq!(opened.project.name, "promo");
        assert!(Workspace::open_native(tmp.path().join("elsewhere")).is_err());
    }

    #[test]
    fn applying_an_op_persists_the_document_and_reports_a_flat_result() {
        let (vfs, paths, mut engine) = fixture();

        let applied = engine
            .apply("test.rename", json!({ "name": "promo v2" }), None, false)
            .unwrap();

        assert_eq!(applied.seq, Some(1));
        let reported = serde_json::to_value(&applied).unwrap();
        assert_eq!(reported["op"], "test.rename");
        assert_eq!(reported["seq"], 1);
        assert!(
            reported["changed"].is_array(),
            "the effect is flattened into the result: {reported}"
        );

        let reopened = Workspace::open(paths, vfs).unwrap();
        assert_eq!(reopened.project.name, "promo v2");
        assert_eq!(reopened.journal.entries().len(), 1);
        assert_eq!(reopened.journal.entries()[0].actor, Actor::Agent);
    }

    #[test]
    fn a_dry_run_reports_what_would_happen_and_writes_nothing() {
        let (vfs, paths, mut engine) = fixture();
        let before = vfs.read(&paths.project_json()).unwrap();

        let applied = engine
            .apply("test.rename", json!({ "name": "ghost" }), None, true)
            .unwrap();

        assert_eq!(applied.seq, None);
        assert_eq!(applied.effect.changed.len(), 1);
        assert_eq!(engine.workspace.project.name, "promo");
        assert_eq!(vfs.read(&paths.project_json()).unwrap(), before);
        assert!(!vfs.exists(&paths.history()));
    }

    #[test]
    fn a_query_op_answers_without_touching_the_document_or_the_history() {
        let (vfs, paths, mut engine) = fixture();
        let before = vfs.read(&paths.project_json()).unwrap();

        let applied = engine.apply("test.count", json!({}), None, false).unwrap();

        assert_eq!(applied.effect.data, Some(json!({ "tracks": 1 })));
        assert_eq!(applied.seq, None);
        assert!(engine.workspace.journal.is_empty());
        assert_eq!(vfs.read(&paths.project_json()).unwrap(), before);
    }

    #[test]
    fn a_failure_in_the_middle_of_a_batch_leaves_both_files_untouched() {
        let (vfs, paths, mut engine) = fixture();
        engine
            .apply("test.rename", json!({ "name": "first" }), None, false)
            .unwrap();
        let project_before = vfs.read(&paths.project_json()).unwrap();
        let history_before = vfs.read(&paths.history()).unwrap();

        let err = engine
            .apply_batch(
                vec![
                    ("test.rename".into(), json!({ "name": "second" }), None),
                    ("test.explode".into(), json!({}), None),
                    ("test.rename".into(), json!({ "name": "third" }), None),
                ],
                false,
            )
            .unwrap_err();

        assert_eq!(err.exit_code(), exit::OP_ERROR);
        assert_eq!(
            engine.workspace.project.name, "first",
            "the ops that did run must not survive the failure"
        );
        assert_eq!(vfs.read(&paths.project_json()).unwrap(), project_before);
        assert_eq!(vfs.read(&paths.history()).unwrap(), history_before);
        assert_eq!(engine.workspace.journal.entries().len(), 1);
    }

    #[test]
    fn a_batch_that_succeeds_journals_one_undoable_entry_per_op() {
        let (_vfs, _paths, mut engine) = fixture();

        let applied = engine
            .apply_batch(
                vec![
                    ("test.rename".into(), json!({ "name": "one" }), None),
                    ("test.count".into(), json!({}), None),
                    ("test.rename".into(), json!({ "name": "two" }), None),
                ],
                false,
            )
            .unwrap();

        assert_eq!(
            applied.iter().map(|a| a.seq).collect::<Vec<_>>(),
            vec![Some(1), None, Some(2)],
            "the query in the middle must not consume a sequence number"
        );
        assert_eq!(engine.workspace.project.name, "two");

        // Undo is per-op even though the batch committed once.
        assert_eq!(engine.undo().unwrap().as_deref(), Some("test.rename"));
        assert_eq!(engine.workspace.project.name, "one");
        assert_eq!(engine.undo().unwrap().as_deref(), Some("test.rename"));
        assert_eq!(engine.workspace.project.name, "promo");
        assert!(engine.undo().unwrap().is_none());
    }

    #[test]
    fn undo_then_redo_returns_the_document_and_both_appear_in_the_history() {
        let (vfs, paths, mut engine) = fixture();
        let original = document(&engine.workspace.project);

        engine
            .apply("test.rename", json!({ "name": "promo v2" }), None, false)
            .unwrap();
        let edited = document(&engine.workspace.project);

        assert_eq!(engine.undo().unwrap().as_deref(), Some("test.rename"));
        assert_eq!(document(&engine.workspace.project), original);
        assert_eq!(engine.redo().unwrap().as_deref(), Some("test.rename"));
        assert_eq!(document(&engine.workspace.project), edited);
        assert!(engine.redo().unwrap().is_none(), "nothing left to redo");

        let ops: Vec<&str> = engine
            .workspace
            .journal
            .entries()
            .iter()
            .map(|entry| entry.op.as_str())
            .collect();
        assert_eq!(ops, vec!["test.rename", OP_UNDO, OP_REDO]);
        assert_eq!(engine.workspace.journal.entries()[1].target, Some(1));
        assert_eq!(engine.workspace.journal.entries()[2].target, Some(1));

        // The history on disk is the channel the GUI shares, so it must say the same.
        let reopened = Workspace::open(paths.clone(), vfs.clone()).unwrap();
        assert_eq!(reopened.project.name, "promo v2");
        assert_eq!(reopened.journal.entries().len(), 3);
        assert_eq!(
            String::from_utf8(vfs.read(&paths.history()).unwrap())
                .unwrap()
                .lines()
                .count(),
            3
        );

        // And the undone op is undoable again, from the reopened state.
        assert_eq!(engine.undo().unwrap().as_deref(), Some("test.rename"));
        assert_eq!(document(&engine.workspace.project), original);
    }

    #[test]
    fn an_op_that_breaks_a_track_invariant_is_rejected_before_anything_is_written() {
        let (vfs, paths, mut engine) = fixture();
        engine
            .apply("test.rename", json!({ "name": "first" }), None, false)
            .unwrap();
        let project_before = vfs.read(&paths.project_json()).unwrap();
        let history_before = vfs.read(&paths.history()).unwrap();

        let err = engine
            .apply("test.overlap", json!({}), None, false)
            .unwrap_err();

        assert!(
            err.to_string().contains("overlap"),
            "the post-apply check must name the invariant: {err}"
        );
        let clips = &engine
            .workspace
            .project
            .sequences
            .values()
            .next()
            .unwrap()
            .tracks[0]
            .clips;
        assert_eq!(clips.len(), 1, "the overlapping clip must not be committed");
        assert_eq!(vfs.read(&paths.project_json()).unwrap(), project_before);
        assert_eq!(vfs.read(&paths.history()).unwrap(), history_before);

        // A dry run of the same op fails the same way: validation is not a save-time step.
        assert!(engine.apply("test.overlap", json!({}), None, true).is_err());
    }

    #[test]
    fn an_unknown_op_in_a_batch_fails_before_the_earlier_ops_run() {
        let (vfs, paths, mut engine) = fixture();
        let before = vfs.read(&paths.project_json()).unwrap();

        let err = engine
            .apply_batch(
                vec![
                    ("test.rename".into(), json!({ "name": "one" }), None),
                    ("test.renme".into(), json!({}), None),
                ],
                false,
            )
            .unwrap_err();

        assert_eq!(err.exit_code(), exit::NO_MATCH);
        assert!(err.to_string().contains("test.rename"), "{err}");
        assert_eq!(engine.workspace.project.name, "promo");
        assert_eq!(vfs.read(&paths.project_json()).unwrap(), before);
    }

    #[test]
    fn an_op_that_changes_nothing_does_not_grow_the_history() {
        let (_vfs, _paths, mut engine) = fixture();
        engine
            .apply("test.rename", json!({ "name": "promo" }), None, false)
            .unwrap();
        assert!(
            engine.workspace.journal.is_empty(),
            "a no-op rename has nothing to undo, so it earns no entry"
        );
    }

    #[test]
    fn the_author_of_an_edit_is_recorded() {
        let (_vfs, _paths, engine) = fixture();
        let mut engine = engine.as_human();
        engine
            .apply("test.rename", json!({ "name": "by hand" }), None, false)
            .unwrap();
        assert_eq!(engine.workspace.journal.entries()[0].actor, Actor::Human);
        engine.undo().unwrap();
        assert_eq!(engine.workspace.journal.entries()[1].actor, Actor::Human);
    }
}
