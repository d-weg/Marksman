//! movefix — the Swift §8 hooks ([`ci_edit::moves::MoveModel`]) behind the shared move engine.
//!
//! Swift is the DEGENERATE-CASE validator for the §8 extraction (rollout spec, Swift section): it
//! proves the hooks may legally be NO-OPS. The reason is the module model — Swift imports are
//! MODULE-level (`import Foundation`), not file-level, and a SwiftPM target GLOBS its directory.
//! Consequences:
//! - **`ref_occurrences`**: within a target there are no file→file path references to rewrite (a
//!   type is visible to its whole module without an `import`), so this returns EMPTY. There is
//!   nothing for a move to retarget, and nothing for the deletion pass to flag.
//! - **`membership_edits`**: a `.swift` file is a member of its target by LIVING in the globbed
//!   directory — there is no `mod x;` / `package p;` / barrel to maintain. So this returns an
//!   EMPTY vec (a handled move needing no declaration work), never `None`.
//! - **`file_to_ref`**: a `.swift` path maps to its file stem — a stable key so a pure rename
//!   (`A.swift` → `B.swift`, different stems) is a move the engine handles, while a cross-dir move
//!   keeping the name declines cleanly (identical refs → the engine returns `None`); either way
//!   the PHYSICAL move still happens in the spine and `swift build` gates the result.
//!
//! Cross-TARGET moves are the one case that would touch `Package.swift` membership; sourcekit-lsp
//! refutes `willRenameFiles`, so that edit would live here. It is intentionally NOT implemented in
//! this phase (the rollout spec scopes the within-target no-op form). Rather than emit a false
//! "handled" empty edit for a cross-target move, the model DECLINES it (`None`) so the engine
//! reports "not handled" honestly — the physical move + the `swift build` gate remain the guard
//! (a cross-target move that strands a target fails the build).
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use std::path::Path;

/// The Swift [`MoveModel`]: the degenerate within-target form (no path references, no membership
/// declaration — files are members by directory glob). A unit struct — the within-target hooks are
/// rootless (no disk to consult); the future cross-target `Package.swift` rewriter will reintroduce
/// a root when it actually needs one.
pub(crate) struct SwiftMoveModel;

/// The SwiftPM target a source path belongs to, by the `Sources/<Target>/…` convention (the first
/// segment under `Sources/`). `None` when the path isn't under `Sources/` — an unclassifiable move
/// then declines, the gate remaining the guard. A custom-`path:` manifest can defeat the heuristic,
/// but the only consequence is a decline-vs-empty-edit label difference; `swift build` still gates.
fn swift_target(rel: &str) -> Option<&str> {
    rel.strip_prefix("Sources/").and_then(|r| r.split('/').next()).filter(|s| !s.is_empty())
}

impl MoveModel for SwiftMoveModel {
    /// A `.swift` path's reference key is its file stem — enough to distinguish a rename from a
    /// same-name relocation, which is all the engine needs to decide whether to run.
    fn file_to_ref(&self, rel: &str) -> Option<String> {
        if !rel.ends_with(".swift") {
            return None;
        }
        std::path::Path::new(rel).file_stem()?.to_str().map(str::to_string)
    }

    /// No within-target file→file references: module-level visibility means a type is reachable
    /// without an `import` naming its file, so there is nothing to rewrite or flag on deletion.
    fn ref_occurrences(&self, _rel: &str, _content: &str) -> Vec<RefOccurrence> {
        Vec::new()
    }

    /// Within a target, files are members by directory glob — no declaration to maintain, so an
    /// EMPTY vec (a handled move needing no membership work). A CROSS-TARGET move genuinely needs
    /// `Package.swift` membership work this model does not do, so it DECLINES (`None`) — the engine
    /// then reports "not handled" honestly instead of a false empty "handled" edit, and the
    /// physical move + `swift build` gate guard the result. (A path outside `Sources/<target>/`
    /// can't be classified → also declines.)
    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
        match (swift_target(from), swift_target(to)) {
            (Some(a), Some(b)) if a == b => Some(Vec::new()),
            _ => None,
        }
    }

    fn is_source(&self, rel: &str) -> bool {
        rel.ends_with(".swift")
    }
}

/// The move's `WorkspaceEdit` over [`SwiftMoveModel`]. Within a target this is empty (no
/// references, no membership) — the physical file move in the spine is the whole change, and
/// `swift build` gates it. `None` when the move shape is outside what the model addresses.
pub(crate) fn move_workspace_edit(root: &Path, from: &str, to: &str) -> Option<serde_json::Value> {
    ci_edit::moves::move_workspace_edit(root, from, to, &SwiftMoveModel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_to_ref_is_the_stem_and_non_swift_declines() {
        let m = SwiftMoveModel;
        assert_eq!(m.file_to_ref("Sources/App/Util.swift").as_deref(), Some("Util"));
        assert_eq!(m.file_to_ref("README.md"), None);
    }

    // The degenerate move: no references, no membership — a within-target move produces an empty
    // (or declined) WorkspaceEdit, and the physical move (spine) carries the change. A same-name
    // relocation declines (identical stem refs); a rename to a new name is handled with an empty
    // documentChanges list (nothing to rewrite).
    #[test]
    fn within_target_move_has_no_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Sources/App/Sub")).unwrap();
        std::fs::write(root.join("Sources/App/Util.swift"), "struct Util {}\n").unwrap();
        std::fs::write(root.join("Sources/App/main.swift"), "print(Util())\n").unwrap();

        // Same-name relocation: identical stem refs -> the engine declines (the physical move
        // still happens in the spine; nothing to rewrite).
        assert!(
            move_workspace_edit(root, "Sources/App/Util.swift", "Sources/App/Sub/Util.swift").is_none(),
            "same-name relocation needs no rewrite -> declines"
        );

        // A within-target rename to a new name IS a move the engine handles, but with no
        // reference/membership rewrites: an empty documentChanges list.
        let we = move_workspace_edit(root, "Sources/App/Util.swift", "Sources/App/Helper.swift")
            .expect("within-target rename is handled");
        let changes = we.get("documentChanges").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        assert!(changes.is_empty(), "no within-target references to rewrite: {we}");

        // A CROSS-TARGET move (different `Sources/<target>` dir, different name) DECLINES rather
        // than emitting a false empty "handled" edit — it needs Package.swift work the model
        // doesn't do, so the engine defers to the physical move + `swift build` gate.
        assert!(
            move_workspace_edit(root, "Sources/App/Util.swift", "Sources/Lib/Helper.swift").is_none(),
            "cross-target move declines (Package.swift membership not modeled)"
        );
    }

    // Membership within a target is an empty vec (members by glob); a CROSS-TARGET move declines
    // honestly (`None`) rather than reporting a false empty "handled" edit.
    #[test]
    fn membership_within_target_empty_cross_target_declines() {
        let m = SwiftMoveModel;
        assert_eq!(
            m.membership_edits("Sources/App/A.swift", "Sources/App/Sub/A.swift"),
            Some(Vec::new()),
            "within a target: members by directory glob, no declaration to maintain"
        );
        assert_eq!(
            m.membership_edits("Sources/App/A.swift", "Sources/Lib/A.swift"),
            None,
            "cross-target: needs Package.swift work this model doesn't do -> declines honestly"
        );
    }
}
