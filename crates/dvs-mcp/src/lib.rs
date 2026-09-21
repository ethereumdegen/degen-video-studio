//! The agent surface: one registry, two front ends.
//!
//! `dvs-cli` and this MCP server are the same capability list wearing different clothes.
//! The list itself is [`full_registry`], and it lives here rather than in the CLI because
//! the failure this crate exists to prevent is *surface drift*: an op registered for the
//! command line but missing from the MCP tool list, or a tool whose arguments no longer
//! match the op behind it. Both front ends call one function, so a new op appears in both
//! or in neither.
//!
//! On top of the generated per-op tools sit the loop tools from `PLAN.md` §5
//! ([`tools`]): `dvs_overview`, `dvs_apply`, `dvs_render`, `dvs_lint`, `dvs_frame`,
//! `dvs_transcript`, `dvs_history`. They exist because an agent's round trip is the
//! expensive resource — thirty edits and one digest should cost one call, not thirty-one.
//! Each is a plain synchronous function the CLI calls too, so the two surfaces cannot
//! disagree about what "render and tell me what happened" means.

pub mod server;
pub mod tools;

use dvs_core::op::Registry;

pub use server::{serve, DvsServer};
pub use tools::{full_catalog, op_for_tool, parse_span, tool_name, LOOP_TOOLS};

/// Every op in the product: the document ops from `dvs-core` plus the ops each capability
/// crate registers for itself.
///
/// Order is irrelevant — [`Registry`] is keyed by id and panics on a duplicate — but the
/// completeness is not: this is the only list, and `dvs op --list`, `dvs schema`, the MCP
/// `tools/list` response and the generated reference all read from it.
pub fn full_registry() -> Registry {
    let mut registry = dvs_core::registry();
    dvs_media::register(&mut registry);
    dvs_comp::ops::register(&mut registry);
    dvs_audio::register(&mut registry);
    dvs_text::register(&mut registry);
    dvs_interop::register(&mut registry);
    dvs_inspect::register(&mut registry);
    dvs_ai::register(&mut registry);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Two ops mapping to one tool name would make `tools/call` ambiguous, and the mapping
    /// is lossy on purpose (`.` and `-` both become `_`), so the collision is possible in
    /// principle: `title.set-text` and a hypothetical `title.set.text` would collide.
    #[test]
    fn tool_names_are_unique_and_valid_identifiers() {
        let registry = full_registry();
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        for id in registry.ids() {
            let name = tool_name(id);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "tool name '{name}' for op '{id}' is not [a-z0-9_]+"
            );
            assert!(!name.is_empty(), "op '{id}' produced an empty tool name");
            if let Some(other) = seen.insert(name.clone(), id) {
                panic!("ops '{other}' and '{id}' both map to tool name '{name}'");
            }
        }
    }

    /// The loop tools share the namespace with the generated ones; a `dvs_*` op id would
    /// shadow one of them silently.
    #[test]
    fn loop_tools_do_not_collide_with_op_tools() {
        let registry = full_registry();
        for id in registry.ids() {
            let name = tool_name(id);
            assert!(
                !LOOP_TOOLS.contains(&name.as_str()),
                "op '{id}' maps onto loop tool '{name}'"
            );
        }
    }

    /// Every advertised tool must route back to something callable: either an op or a loop
    /// tool. A tool in `tools/list` that `tools/call` rejects is worse than no tool.
    #[test]
    fn every_advertised_tool_resolves() {
        let registry = full_registry();
        let catalog = full_catalog(&registry);
        assert!(catalog.len() > registry.len(), "loop tools are missing");
        for tool in &catalog {
            let name = tool.name.as_ref();
            let known =
                LOOP_TOOLS.contains(&name) || op_for_tool(&registry, name).is_ok();
            assert!(known, "advertised tool '{name}' resolves to nothing");
        }
    }
}
