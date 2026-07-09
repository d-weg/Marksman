//! ci-edit — atomic, gated edits. Resolves targets to SCIP node ranges, applies
//! them to a VFS overlay, then gates with an LSP **baseline-diff** (fail only on
//! NEWLY introduced diagnostics) before committing — else rolls back by dropping
//! the VFS. Rename goes through LSP (all references); structural edits use the
//! node's `enclosing_range`. No AST: `set_body` and the fine verbs are refused at
//! Symbol granularity (use `replace_node`).
use ci_core::{CommitResult, Diag, EditOp, EditOpts, Error, FileSummary, Node, Result};
use ci_lsp::LspClient;
use ci_vfs::Vfs;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

mod actions;
mod apply;
mod composed;
pub mod moves;

pub use actions::{action_to_op, Action};
pub use apply::{apply_delete, apply_structural, leading_symbol_name, workspace_edit_is_empty};
pub use composed::{Composed, EngineFactory, FreshDeepener, LiveSummarizer, Prewarmer};

use apply::{apply_move, apply_rename};

/// The type-check engine behind the gate. Abstracts over *how* a language computes
/// diagnostics and cross-file rename/move edits, so the provider can pick the lightest
/// available option (a TS provider drives ts-morph in-process; the generic fallback is an
/// LSP server). All edits flow through the same VFS / baseline-diff / blast-radius logic
/// in `commit_edits` regardless of engine. `rename`/`will_rename` return an LSP-shaped
/// WorkspaceEdit JSON (`changes`/`documentChanges`), consumed by `apply_workspace_edit`.
pub trait GateEngine {
    /// Type-check `files` (repo-relative path + buffer content) → ERROR diagnostics.
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>>;
    /// Cross-file edits to rename the symbol whose name starts at (0-based) `line`/`character`.
    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value>;
    /// Importer rewrites for moving `from` → `to` (does not move the file). Empty if unsupported.
    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value>;
    /// Fresh per-file read info (named symbols + import edges) from the engine's live project,
    /// or `Ok(None)` when this engine can't provide it. Providers whose read index is a build
    /// artifact (SCIP) call this after a committed edit so `structure()`/`import_graph()` stay
    /// true in-session instead of serving pre-edit state until the next reindex.
    fn file_summaries(&mut self, files: &[String]) -> Result<Option<Vec<FileSummary>>> {
        let _ = files;
        Ok(None)
    }
    /// Notify the engine of file-system changes the gate materialized on disk (repo-relative
    /// created/deleted paths). LSP engines forward didClose + didChangeWatchedFiles so the
    /// server's project view tracks the staged state; in-memory engines need nothing.
    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        let _ = (created, deleted);
        Ok(())
    }
    /// Restore every in-memory buffer the engine holds to the CURRENT on-disk content. Called
    /// before computing any edit: a prior dry-run or rejected gate pushed overlay content into
    /// the engine, and a rename computed against that phantom state returns spans that slice
    /// the wrong text on disk (only by luck — equal-length names — does it land). Default no-op
    /// for engines that hold no cross-call state.
    fn sync_disk(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The one prewarm discipline for every provider's warm-engine slot: spawn a background
/// thread that holds the slot lock for the WHOLE construction + warming call, then parks the
/// engine in the slot. Holding the lock is the contract — an `apply_edits` arriving mid-warm
/// blocks on the slot and reuses the warmed engine instead of racing in a second cold one.
/// No-op if the slot is already warm. `make` constructs the engine and issues its warming
/// call, returning `None` when the engine can't start (the slot stays empty and the provider
/// starts one lazily on first use, surfacing the error there).
pub fn spawn_prewarm<E: Send + 'static>(
    slot: Arc<Mutex<Option<E>>>,
    make: impl FnOnce() -> Option<E> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(mut guard) = slot.lock() else { return };
        if guard.is_some() {
            return; // already warm
        }
        *guard = make();
    })
}

/// LSP request errors that mean "the server is still loading the project" rather than a real
/// failure — worth retrying with backoff. rust-analyzer mid-index returns these transiently
/// (JSON-RPC `-32602`/`-32801`, "content modified", "still loading", …).
/// Diagnostic code token for reject text: `TS2554 ` for numeric codes (the TypeScript
/// convention agents pattern-match), empty when the engine's code wasn't numeric
/// (rust-analyzer's `E0583` already lives in the message — "TS0" was noise).
fn code_token(code: i64) -> String {
    if code > 0 { format!("TS{code} ") } else { String::new() }
}

fn is_transient_lsp_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("-32602")
        || m.contains("-32801")
        || m.contains("-32802") // ServerCancelled: the server asks for a retry
        || m.contains("server cancelled")
        || m.contains("content modified")
        || m.contains("not ready")
        || m.contains("loading")
        || m.contains("waiting")
}

/// The generic LSP engine: any language with a language server.
impl GateEngine for LspClient {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        LspClient::diagnostics(self, files)
    }
    fn sync_disk(&mut self) -> Result<()> {
        LspClient::sync_disk(self)
    }
    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        LspClient::fs_events(self, created, deleted)
    }
    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        // Warm: opening the file loads the project, so rename sees every reference (a cold
        // server returns an EMPTY edit — a silent no-op rename).
        if let Ok(content) = std::fs::read_to_string(self.root().join(file)) {
            let _ = LspClient::diagnostics(self, &[(file.to_string(), content)]);
        }
        let uri = format!("file://{}", self.root().join(file).to_string_lossy());
        let params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character},
            "newName": new_name,
        });
        // A server still loading the project (notably rust-analyzer mid-index) returns a
        // transient error ("no references found", "content modified", -32602/-32801) until it's
        // analyzed. Retry with backoff rather than fail a rename that will work in a second.
        for attempt in 0..8 {
            match self.request("textDocument/rename", params.clone()) {
                Ok(we) => return Ok(we),
                Err(e) => {
                    // Rename also treats "no references found" as transient (a cold server
                    // hasn't indexed references yet) — willRename handles empties separately.
                    let m = e.to_string();
                    let transient = attempt < 7
                        && (is_transient_lsp_error(&m) || m.to_lowercase().contains("references"));
                    if !transient {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                }
            }
        }
        self.request("textDocument/rename", params)
    }
    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        if let Ok(content) = std::fs::read_to_string(self.root().join(from)) {
            let _ = LspClient::diagnostics(self, &[(from.to_string(), content)]);
        }
        let old_uri = format!("file://{}", self.root().join(from).to_string_lossy());
        let new_uri = format!("file://{}", self.root().join(to).to_string_lossy());
        let params = json!({ "files": [{ "oldUri": old_uri, "newUri": new_uri }] });
        // A cold server (rust-analyzer mid-index) returns EMPTY until analyzed; retry until it
        // produces the importer rewrites (mod-decl / use-path edits). An unimported file
        // legitimately yields empty — that rare case just pays the retries once, then proceeds.
        let mut last = json!({});
        for attempt in 0..8 {
            match self.request("workspace/willRenameFiles", params.clone()) {
                Ok(we) if !workspace_edit_is_empty(&we) => return Ok(we),
                Ok(we) => last = we,
                Err(e) => {
                    if !is_transient_lsp_error(&e.to_string()) {
                        return Err(e);
                    }
                }
            }
            if attempt < 7 {
                std::thread::sleep(std::time::Duration::from_millis(1200));
            }
        }
        Ok(last)
    }
}

/// Find a node id by name within a file's structure (first match).
pub fn resolve_in(nodes: &[Node], name: &str) -> Option<String> {
    fn rec<'a>(nodes: &'a [Node], name: &str) -> Option<&'a Node> {
        for n in nodes {
            if n.name.as_deref() == Some(name) {
                return Some(n);
            }
            if let Some(f) = rec(&n.children, name) {
                return Some(f);
            }
        }
        None
    }
    rec(nodes, name).map(|n| n.id.clone())
}

/// EVERY node id whose name matches, depth-first — the collision-aware sibling of [`resolve_in`].
/// The single-match `resolve_in` silently returns the FIRST of several same-named symbols in one
/// file (two interface fields named `nodeId`, an overload pair, a method + a top-level fn), so an
/// edit-by-bare-name would land on whichever came first with no warning. Callers that must never
/// guess use this and treat `len > 1` as ambiguous — surfacing the candidate ids to pick from.
/// Sub-nodes (`:body`/`:param.N`/`:return`) carry `name: None`, so only real symbols match.
pub fn resolve_all_in(nodes: &[Node], name: &str) -> Vec<String> {
    fn rec(nodes: &[Node], name: &str, out: &mut Vec<String>) {
        for n in nodes {
            if n.name.as_deref() == Some(name) {
                out.push(n.id.clone());
            }
            rec(&n.children, name, out);
        }
    }
    let mut out = Vec::new();
    rec(nodes, name, &mut out);
    out
}

pub(crate) fn find<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = find(&n.children, id) {
            return Some(f);
        }
    }
    None
}

pub(crate) fn file_of(node_id: &str) -> &str {
    node_id.split('#').next().unwrap_or(node_id)
}

/// The repo-relative paths an op reads/writes — checked for root-containment before it runs.
fn op_paths(op: &EditOp) -> Vec<PathBuf> {
    match op {
        EditOp::CreateFile { path, .. } | EditOp::DeleteFile { path } => vec![path.clone()],
        EditOp::ReplaceInFile { path, .. } | EditOp::AddSymbol { path, .. } => vec![path.clone()],
        EditOp::MoveFile { from, to } => vec![from.clone(), to.clone()],
        EditOp::ReplaceNode { node_id, .. }
        | EditOp::InsertBefore { node_id, .. }
        | EditOp::ReplaceText { node_id, .. }
        | EditOp::SetBody { node_id, .. }
        | EditOp::InsertInBody { node_id, .. }
        | EditOp::DeleteInBody { node_id, .. }
        | EditOp::InsertMember { node_id, .. }
        | EditOp::AddParameter { node_id, .. }
        | EditOp::SetReturnType { node_id, .. }
        | EditOp::Rename { node_id, .. } => vec![PathBuf::from(file_of(node_id))],
    }
}

/// Reject a write whose path escapes `root` — the edit-layer trust boundary. An agent (or content
/// it was injected with) must not create/move/delete outside the repo. Two layers:
///   1. lexical — no absolute path, no `..` that climbs above root (catches `../../etc/x`);
///   2. symlink — the target's nearest existing ancestor must canonicalize under root (catches a
///      symlinked dir pointing out). Best-effort: skipped only if `root` itself can't canonicalize.
fn ensure_within_root(root: &Path, rel: &Path) -> Result<()> {
    let escaped = |why: &str| Err(Error::Driver(format!("path escapes repo root ({why}): {}", rel.display())));
    let mut depth: i32 = 0;
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return escaped("absolute"),
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return escaped("..");
                }
            }
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
        }
    }
    if let Ok(root_c) = root.canonicalize() {
        // Walk up to the first ancestor that exists on disk and canonicalize it (a not-yet-created
        // file has no canonical form of its own); it must still live under the canonical root.
        let abs = root.join(rel);
        let mut probe = abs.as_path();
        loop {
            match probe.canonicalize() {
                Ok(p) => {
                    if !p.starts_with(&root_c) {
                        return escaped("symlink");
                    }
                    break;
                }
                Err(_) => match probe.parent() {
                    Some(par) => probe = par,
                    None => break,
                },
            }
        }
    }
    Ok(())
}

/// Whether `op` targets `rel` (any of its paths) — used by providers to detect that a batch
/// already handles a file (e.g. lang-rust skips synthesizing a module declaration when the
/// agent's own batch edits the parent decl file).
pub fn op_touches_file(op: &EditOp, rel: &str) -> bool {
    op_paths(op).iter().any(|p| p.to_string_lossy().replace('\\', "/") == rel)
}

fn op_node_id(op: &EditOp) -> Option<&str> {
    match op {
        EditOp::ReplaceNode { node_id, .. }
        | EditOp::InsertBefore { node_id, .. }
        | EditOp::ReplaceText { node_id, .. }
        | EditOp::SetBody { node_id, .. }
        | EditOp::InsertInBody { node_id, .. }
        | EditOp::DeleteInBody { node_id, .. }
        | EditOp::InsertMember { node_id, .. }
        | EditOp::AddParameter { node_id, .. }
        | EditOp::SetReturnType { node_id, .. }
        | EditOp::Rename { node_id, .. } => Some(node_id),
        _ => None,
    }
}

/// Attribute a diagnostic to the op that introduced it: the node-targeted op in the
/// same file whose node starts nearest above the diagnostic's line. Makes a
/// rejection actionable — the agent re-emits just that op (scoped repair) instead
/// of the whole batch.
/// The INNERMOST named symbol whose line range contains `line`. Lets a blast-radius diagnostic name
/// the symbol an agent should edit (its construction site / caller), sparing it a `list_anchors`
/// round-trip to map `file:line` back to a node. Innermost wins so a method beats its class.
fn enclosing_symbol(nodes: &[Node], line: u32) -> Option<String> {
    fn walk<'a>(nodes: &'a [Node], line: u32, best: &mut Option<(&'a Node, u32)>) {
        for n in nodes {
            if n.name.is_some() && n.range.start_line <= line && line <= n.range.end_line {
                let span = n.range.end_line - n.range.start_line;
                if best.is_none_or(|(_, s)| span <= s) {
                    *best = Some((n, span));
                }
            }
            walk(&n.children, line, best);
        }
    }
    let mut best = None;
    walk(nodes, line, &mut best);
    best.map(|(n, _)| n.id.clone())
}

/// A ready-to-copy `insert_in_body` action for one blast-radius site — the reject hands the
/// agent the whole edit except `value`, so fixing a fan-out (a newly-required member at N
/// construction sites) needs no read_node per site and no anchor construction: the server picks
/// the anchor from the site's own post-edit source (`window` = offending line + up to 2
/// following). When the offending line opens a `{ …` block the anchor is the first line INSIDE
/// it, so the insert lands among the members at their indent (`after` is substring-matched and
/// auto-indented, so whitespace never matters). The suggestion is serialized JSON — exactly the
/// object to drop into `actions`, with quoting the agent can't get wrong.
fn suggest_fix(node_id: &str, window: &[&str]) -> Option<String> {
    let first = window.first()?.trim();
    let opens_block = first.matches('{').count() > first.matches('}').count();
    let anchor = if opens_block {
        window.get(1).map(|l| l.trim()).filter(|l| !l.is_empty()).unwrap_or(first)
    } else {
        first
    };
    if anchor.is_empty() {
        return None;
    }
    Some(format!(
        "\nfix (set `value` to the code this site needs, then batch): {}",
        serde_json::json!({"action": "insert_in_body", "name": node_id, "oldText": anchor, "value": "<FILL IN>"})
    ))
}

fn anchor(diag: &Diag, ops: &[EditOp], structure_of: &impl Fn(&str) -> Vec<Node>) -> Option<(usize, String)> {
    let mut candidates: Vec<(usize, String, u32)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let Some(node_id) = op_node_id(op) else { continue };
        if file_of(node_id) != diag.file {
            continue;
        }
        let nodes = structure_of(file_of(node_id));
        if let Some(node) = find(&nodes, node_id) {
            // The diag must fall WITHIN the op's node, not merely below its start: without the
            // end check, a same-file op swallows every blast-radius diagnostic under it (bench
            // T5: an insert into `makeEntry` claimed a missing-member error 60 lines below, in a
            // function no op touched) — and the op-anchored branch never emits the ready-to-copy
            // `fix:`, so the agent had to re-read the site. Un-anchored is the better failure:
            // the enclosing-symbol branch names the real site and hands over the fix.
            if node.range.start_line <= diag.line && diag.line <= node.range.end_line {
                candidates.push((i, node_id.to_string(), node.range.start_line));
            }
        }
    }
    candidates.into_iter().max_by_key(|(_, _, sl)| *sl).map(|(i, id, _)| (i, id))
}

fn diag_key(d: &Diag) -> String {
    format!("{}:{}:{}", d.file, d.code, d.message)
}

/// Compact per-file line diff between DISK (before) and the VFS overlay (after) for the
/// changed set — the rewrite receipt a rename/move/delete commit carries. Multiset line diff
/// (order-preserving, `-`/`+` per excess occurrence), capped hard: it must stay a receipt,
/// not a dump. Files the batch created transiently read as `created` (their disk content is
/// already the after-state); files the overlay deletes read as `deleted`.
fn rewrite_receipt(root: &Path, changed: &[String], vfs: &Vfs, created: &HashSet<String>) -> String {
    let mut out: Vec<String> = Vec::new();
    for rel in changed.iter().take(8) {
        let after = vfs.read(Path::new(rel));
        let before = if created.contains(rel) { None } else { std::fs::read_to_string(root.join(rel)).ok() };
        match (before, after) {
            (None, Some(_)) => out.push(format!("  {rel}: created")),
            (Some(_), None) | (None, None) => out.push(format!("  {rel}: deleted")),
            (Some(b), Some(a)) => {
                let mut bcount: HashMap<&str, i64> = HashMap::new();
                for l in b.lines() {
                    *bcount.entry(l).or_default() += 1;
                }
                let mut acount: HashMap<&str, i64> = HashMap::new();
                for l in a.lines() {
                    *acount.entry(l).or_default() += 1;
                }
                let clip = |l: &str| -> String {
                    let t = l.trim_end();
                    if t.len() > 120 { format!("{}…", &t[..120]) } else { t.to_string() }
                };
                let mut lines: Vec<String> = Vec::new();
                let mut seen_minus: HashMap<&str, i64> = HashMap::new();
                for l in b.lines() {
                    let excess = bcount.get(l).unwrap_or(&0) - acount.get(l).unwrap_or(&0);
                    let s = seen_minus.entry(l).or_default();
                    if *s < excess {
                        *s += 1;
                        lines.push(format!("    - {}", clip(l)));
                    }
                }
                let mut seen_plus: HashMap<&str, i64> = HashMap::new();
                for l in a.lines() {
                    let excess = acount.get(l).unwrap_or(&0) - bcount.get(l).unwrap_or(&0);
                    let s = seen_plus.entry(l).or_default();
                    if *s < excess {
                        *s += 1;
                        lines.push(format!("    + {}", clip(l)));
                    }
                }
                if lines.is_empty() {
                    continue;
                }
                let extra = lines.len().saturating_sub(6);
                lines.truncate(6);
                if extra > 0 {
                    lines.push(format!("    … and {extra} more changed line(s)"));
                }
                out.push(format!("  {rel}:\n{}", lines.join("\n")));
            }
        }
    }
    if changed.len() > 8 {
        out.push(format!("  … and {} more file(s)", changed.len() - 8));
    }
    out.join("\n")
}

/// Apply `ops` atomically behind the LSP type-check gate. On any NEW diagnostic
/// nothing is written (VFS dropped). On success the VFS is committed to disk when
/// `write && !dry_run`.
pub fn commit_edits(
    root: &Path,
    ops: &[EditOp],
    structure_of: &impl Fn(&str) -> Vec<Node>,
    engine: &mut dyn GateEngine,
    opts: &EditOpts,
    reverse_imports: &impl Fn(&str) -> Vec<String>,
) -> Result<CommitResult> {
    let mut vfs = Vfs::new(root);

    // The engine must see DISK truth before we compute anything: a prior dry-run/rejected gate
    // left overlay content in its buffers, and rename/willRename edits computed against that
    // phantom state produce wrong spans (silently, when name lengths happen to match).
    engine.sync_disk()?;

    // Structural ops resolve their spans from PRE-batch `structure_of` (disk truth), so within
    // one file an edit higher up shifts the disk-truth span of every op below it — and the tool's
    // own reject flow invites exactly that batch (a schema op + its ready-to-copy fixes, one of
    // which may sit in the same file, e.g. a default-constructor right under its interface).
    // Apply same-file structural ops BOTTOM-UP (descending node start), the same trick the rename
    // path uses for its workspace edits: every disk-truth span stays valid. Only contiguous runs
    // of structural ops are permuted — rename/move/delete/create keep their stated order — and
    // rejections report the op's ORIGINAL index.
    let is_structural = |op: &EditOp| {
        !matches!(
            op,
            EditOp::Rename { .. }
                | EditOp::MoveFile { .. }
                | EditOp::DeleteFile { .. }
                | EditOp::CreateFile { .. }
                | EditOp::ReplaceInFile { .. }
                | EditOp::AddSymbol { .. }
        )
    };
    let mut order: Vec<usize> = (0..ops.len()).collect();
    let mut lo = 0;
    while lo < order.len() {
        if !is_structural(&ops[order[lo]]) {
            lo += 1;
            continue;
        }
        let mut hi = lo;
        while hi < order.len() && is_structural(&ops[order[hi]]) {
            hi += 1;
        }
        // Stable sort: (file, descending start). An op whose node doesn't resolve keeps a neutral
        // key — it will reject at apply time with its own error regardless of position.
        order[lo..hi].sort_by_cached_key(|&k| {
            op_node_id(&ops[k])
                .and_then(|id| {
                    find(&structure_of(file_of(id)), id)
                        .map(|n| (file_of(id).to_string(), std::cmp::Reverse(n.range.start_line)))
                })
                .unwrap_or_else(|| (String::new(), std::cmp::Reverse(0)))
        });
        lo = hi;
    }

    let mut redundant_ops = 0usize;
    for i in order {
        let op = &ops[i];
        // Trust boundary: reject before any VFS mutation if the op targets a path outside the repo.
        for p in op_paths(op) {
            if let Err(e) = ensure_within_root(root, &p) {
                return Ok(CommitResult::Rejected { failed_op_index: i as i64, feedback: e.to_string() });
            }
        }
        // Redundancy detection, GENERIC across every satisfied-op semantics (present and
        // future): a content op that returns Ok while changing nothing in the VFS had its end
        // state produced by an earlier op in the batch (typically a rename/move's automation).
        // The count reaches the response so the agent LEARNS the helpers were free riders —
        // the hedge is priced by evidence, not argued against in prose.
        let before: Option<(PathBuf, String)> = match op {
            EditOp::Rename { .. } | EditOp::MoveFile { .. } | EditOp::DeleteFile { .. } => None,
            other => op_paths(other).first().and_then(|p| vfs.read(p).map(|c| (p.clone(), c))),
        };
        let res = match op {
            EditOp::Rename { node_id, new_name } => {
                apply_rename(&mut vfs, node_id, new_name, root, structure_of, engine)
            }
            EditOp::MoveFile { from, to } => apply_move(&mut vfs, from, to, root, engine),
            EditOp::DeleteFile { path } => apply_delete(&mut vfs, path, reverse_imports),
            other => apply_structural(&mut vfs, other, structure_of),
        };
        if let Err(e) = res {
            return Ok(CommitResult::Rejected { failed_op_index: i as i64, feedback: e.to_string() });
        }
        if let Some((p, b)) = before {
            if vfs.read(&p).as_deref() == Some(b.as_str()) {
                redundant_ops += 1;
            }
        }
    }

    if vfs.is_empty() {
        return Ok(CommitResult::Ok { applied_ops: ops.len(), changed_files: vec![], repair_rounds: 0, preexisting_in_radius: vec![], redundant_ops, rewrite_summary: String::new() });
    }

    let changed = vfs.changed();
    let changed_rel: Vec<String> = changed.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
    let changed_set: HashSet<String> = changed_rel.iter().cloned().collect();

    // Gate over the BLAST RADIUS, not just the edited files: an edit (e.g. a signature change)
    // can break a CALLER in a file we never touched. "Affected" = the changed files + their
    // direct reverse-import dependents — a bounded slice of the import graph, never the whole
    // project. (A rename already rewrites every reference into `changed`; this matters for
    // replace_node / structural edits whose ripple reaches importers.)
    // One hop is deeper than it looks for TS: the graph comes from SEMANTIC scip references, so
    // a consumer importing through a barrel (`export { x } from './a'`) edges DIRECTLY to a.ts
    // (verified: app->barrel->math yields app -> [barrel, math]) — re-exports never hide a
    // consumer. The accepted residual is the type-INFERENCE ripple (A changes B's inferred
    // type, which breaks C two hops out): closing it needs transitive expansion on every edit's
    // gate — the hot path — for a much rarer miss. Rust's tree-sitter mod graph is syntactic,
    // so its `pub use` re-exports do not get this flattening.
    let mut affected: Vec<String> = changed_rel.clone();
    {
        let mut seen = changed_set.clone();
        for c in &changed_rel {
            for importer in reverse_imports(c) {
                if seen.insert(importer.clone()) {
                    affected.push(importer);
                }
            }
        }
    }
    // Files the batch CREATES (create_file / a move's destination) exist only in the overlay —
    // and a didOpen buffer at a path that isn't on disk is NOT enough for the gate: language
    // servers assign open buffers to a project by enumerating the FILE SYSTEM (tsserver's
    // configured project, rust-analyzer's crate graph), so importers of the new path gate on a
    // phantom "cannot find module". Materialize those paths transiently for the check; the
    // drop-guard removes them again on any reject/error path, and a committing transaction
    // defuses it (vfs.commit rewrites the same content). Only the ts-morph engine, with its own
    // in-memory project, never needed this.
    struct TransientCreates(Vec<PathBuf>);
    impl Drop for TransientCreates {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let mut transient = TransientCreates(Vec::new());
    let mut transient_rels: HashSet<String> = HashSet::new();
    for rel in &changed_rel {
        let abs = root.join(rel);
        if !abs.exists() {
            if let Some(content) = vfs.read(Path::new(rel)) {
                if let Some(parent) = abs.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(&abs, &content).is_ok() {
                    transient.0.push(abs);
                    transient_rels.insert(rel.clone());
                }
            }
        }
    }

    // Baseline inputs, shared by the lazy diff below — built BEFORE any staged deletion
    // leaves the disk: afterwards the pre-state error set would include the deletion's own
    // fallout and excuse it (a false clean via the baseline itself).
    let baseline_files: Vec<(String, String)> = affected
        .iter()
        .filter(|rel| !transient_rels.contains(*rel))
        .filter_map(|rel| std::fs::read_to_string(root.join(rel)).ok().map(|c| (rel.clone(), c)))
        .collect();

    // Symmetric to TransientCreates: files the batch DELETES (delete_file / a move's source)
    // still exist on DISK during the gate, and language servers resolve against the file
    // system — rust-analyzer kept resolving a moved module's OLD path, so a move that broke
    // the build gated "clean" (the R2 bench false-clean). Take them off disk for the check;
    // the drop-guard restores them on any reject/error path (the server's own watcher sees
    // the restoration), and a committing transaction DEFUSES the guard — committed deletions
    // must stay deleted.
    struct TransientDeletes(Vec<(PathBuf, String)>);
    impl Drop for TransientDeletes {
        fn drop(&mut self) {
            for (p, content) in &self.0 {
                let _ = std::fs::write(p, content);
            }
        }
    }
    let deleted_rels: Vec<String> = changed_rel
        .iter()
        .filter(|rel| root.join(rel.as_str()).exists() && vfs.read(Path::new(rel.as_str())).is_none())
        .cloned()
        .collect();
    // The baseline must be measured while the deleted files still exist — eagerly, only for
    // deletion batches (everything else keeps the lazy happy path).
    let baseline_early: Option<Vec<Diag>> =
        if deleted_rels.is_empty() { None } else { Some(engine.diagnostics(&baseline_files)?) };
    let mut transient_del = TransientDeletes(Vec::new());
    for rel in &deleted_rels {
        let abs = root.join(rel);
        if let Ok(content) = std::fs::read_to_string(&abs) {
            if std::fs::remove_file(&abs).is_ok() {
                transient_del.0.push((abs, content));
            }
        }
    }
    // Tell the engine what the gate just staged on disk: close deleted buffers (an open
    // buffer SHADOWS the file system) and push watched-file events so servers with their own
    // watchers (rust-analyzer) re-derive the project without racing the OS notifier.
    if !transient_rels.is_empty() || !deleted_rels.is_empty() {
        let created_rels: Vec<String> = transient_rels.iter().cloned().collect();
        engine.fs_events(&created_rels, &deleted_rels)?;
    }

    // After = overlay (edited) content for the changed files; disk content for the dependents
    // (their source is unchanged, but tsserver re-checks them against the overlaid changed
    // files, so a freshly-broken caller surfaces here).
    let mut after_files: Vec<(String, String)> = affected
        .iter()
        .filter_map(|rel| {
            if changed_set.contains(rel) {
                vfs.read(Path::new(rel))
            } else {
                std::fs::read_to_string(root.join(rel)).ok()
            }
            .map(|c| (rel.clone(), c))
        })
        .collect();
    // Deleted files ride along as EMPTY open buffers: didOpen is the one project-view update
    // every server applies SYNCHRONOUSLY (ordered with the pull), so consumers of a deleted/
    // moved file fail deterministically even while the server's ASYNC file watcher is still
    // ingesting the disk-level deletion (bench move-rust round 4: the E0432s raced the notify
    // loader and gated "clean"). The transient disk removal + watched-file events remain the
    // durable signal; this is the synchronous one.
    for rel in &deleted_rels {
        after_files.push((rel.clone(), String::new()));
    }
    let after = engine.diagnostics(&after_files)?;
    // Gate as a diff, computed LAZILY: if the post-edit state is clean, there can be no newly
    // introduced error, so we commit WITHOUT the baseline pass — the happy path (a good edit), which
    // halves the gate's type-check work. Only when `after` has errors do we pay the baseline pass, to
    // tell a freshly-introduced error from one that was already there (disk is untouched until commit).
    let mut preexisting: Vec<Diag> = Vec::new();
    let new: Vec<&Diag> = if after.is_empty() {
        Vec::new()
    } else {
        // (The transiently-materialized creations stayed INVISIBLE to `baseline_files` — they
        // hold after-content, so including them would let a broken new file excuse its own
        // errors. Deletion batches captured the baseline eagerly above, pre-removal.)
        let baseline_diags = match baseline_early {
            Some(b) => b,
            None => engine.diagnostics(&baseline_files)?,
        };
        // COUNT baseline occurrences per key, don't just collect the key set: the key has no line
        // (edits shift lines, so it can't), which means one pre-existing error would otherwise
        // excuse EVERY new instance with the same code+message in that file — a false accept.
        // Each after-instance beyond the baseline count is new. (When instances are excused, the
        // FIRST ones in diagnostic order are — so a flagged line can occasionally be the old
        // site rather than the new one; the reject still surfaces the error either way.)
        let mut baseline_counts: HashMap<String, usize> = HashMap::new();
        for d in baseline_diags {
            *baseline_counts.entry(diag_key(&d)).or_default() += 1;
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut split: Vec<&Diag> = Vec::new();
        for d in &after {
            let k = diag_key(d);
            let n = seen.entry(k.clone()).or_default();
            *n += 1;
            if *n > baseline_counts.get(&k).copied().unwrap_or(0) {
                split.push(d);
            } else {
                // Excused by the baseline: PRE-EXISTING breakage in the radius. Legal to
                // commit past (clause 5), but the result must CARRY it — a "clean/COMPLETE"
                // claim over a still-broken radius sent agents away from real errors
                // (bench move-rust round 4).
                preexisting.push(d.clone());
            }
        }
        split
    };

    if !new.is_empty() {
        // Post-edit source of every affected file, so each diagnostic can carry the OFFENDING LINE
        // inline — the agent sees the exact code to fix (its object literal, caller, …) without a
        // follow-up read_node per site. Lightweight: just the one line, trimmed (no ANSI, no context
        // block that would balloon a many-site rejection).
        let after_src: HashMap<&str, &str> = after_files.iter().map(|(f, c)| (f.as_str(), c.as_str())).collect();
        // A small SOURCE WINDOW around the offending line (the line + up to 2 following) — enough for
        // the agent to pick a unique `replace_text` anchor and see the indentation of the site it must
        // fix, so it can write the fix straight from the reject WITHOUT a read_node per site. Lines are
        // VERBATIM (leading indentation intact, only trailing whitespace trimmed) so the agent can copy
        // one straight into `oldText` and have it match — a display prefix would inflate the indent and
        // every `replace_text` would miss. Fenced so the code is visually distinct from the diagnostics.
        // The site's source window: the offending line, extended to where its opened block
        // closes (capped). Three lines showed the anchor but not the site's SCOPE — the agent
        // still read_node'd each site to learn what variables the new member could be built
        // from (bench T5: is there a `name` in scope?). Showing the whole literal makes the
        // fix's `value` derivable in place; the cap keeps a many-site rejection bounded.
        let window = |file: &str, line: u32| -> Vec<&str> {
            let Some(c) = after_src.get(file) else { return Vec::new() };
            let mut out: Vec<&str> = Vec::new();
            let mut depth: i64 = 0;
            for (i, l) in c.lines().skip(line.saturating_sub(1) as usize).take(8).enumerate() {
                let l = l.trim_end();
                depth += l.bytes().filter(|b| matches!(b, b'{' | b'(' | b'[')).count() as i64;
                depth -= l.bytes().filter(|b| matches!(b, b'}' | b')' | b']')).count() as i64;
                out.push(l);
                if i >= 2 && depth <= 0 {
                    break; // at least the old 3-line window; stop once the site's block closes
                }
            }
            out
        };
        let snippet =
            |w: &[&str]| if w.is_empty() { String::new() } else { format!("\n```\n{}\n```", w.join("\n")) };
        // ONE entry per site: compilers stack multiple codes on the same expression (cargo:
        // E0308 + E0277 for one bad operand), and each duplicate re-prints the same source
        // window + fix template — a 4x reject for one mistake (bench locate-edit: 4.3KB).
        // Keep the first diagnostic per (file, line); the fix is identical either way.
        let mut seen_sites: HashSet<(String, u32)> = HashSet::new();
        let new: Vec<&Diag> =
            new.into_iter().filter(|d| seen_sites.insert((d.file.clone(), d.line))).collect();
        // Anchor each new diagnostic to the op that introduced it (scoped repair).
        let mut anchored_op: i64 = -1;
        let feedback = new
            .iter()
            .map(|d| {
                let w = window(&d.file, d.line);
                let (head, fix) = match anchor(d, ops, structure_of) {
                    Some((i, node_id)) => {
                        if anchored_op < 0 {
                            anchored_op = i as i64;
                        }
                        (format!("op #{i} ({node_id}) -> {}:{} {}{}", d.file, d.line, code_token(d.code), d.message), None)
                    }
                    // A blast-radius error at a site NO op touched (e.g. a construction site that must
                    // now set a newly-required field). Name its enclosing symbol so the agent edits it
                    // directly in the next batch instead of a list_anchors hunt to map line -> node —
                    // and hand it the ready-to-copy insert_in_body for that site (only `value` left).
                    None => match enclosing_symbol(&structure_of(&d.file), d.line) {
                        Some(id) => {
                            let fix = suggest_fix(&id, &w);
                            (format!("{}:{} (in {id}) {}{}", d.file, d.line, code_token(d.code), d.message), fix)
                        }
                        None => (format!("{}:{} {}{}", d.file, d.line, code_token(d.code), d.message), None),
                    },
                };
                format!("{head}{}{}", snippet(&w), fix.unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Point-of-use instruction, only when ready-to-copy fixes were handed out: the upfront
        // tool description tells the agent not to re-read the sites, but the moment of decision
        // is HERE, right after this text — and agents demonstrably heed the success message's
        // equivalent ("do not grep to verify"). Say it where it lands.
        let feedback = if feedback.contains("\nfix (set") {
            format!(
                "{feedback}\n\nThis list is COMPLETE (the type-checker found every affected site) and each window \
                 above is that site's current source — the variables in scope are visible in it. Re-issue ONE batch \
                 now: your original edit(s) + each `fix:` action with `value` filled from its window. Do NOT \
                 read_node/Read/list_anchors the sites first; that would only re-fetch what is already shown here."
            )
        } else {
            feedback
        };
        return Ok(CommitResult::Rejected { failed_op_index: anchored_op, feedback });
    }

    // Rewrite receipt for structural file ops (rename/move/delete): the exact lines the
    // automation changed, computed while BOTH states exist (disk = before, overlay = after).
    // This is what the agent's pre-move survey was trying to learn — shown after the fact,
    // it proves the bare op's completeness with evidence (bench move-rust: the insurance
    // hedge survives description prose; a visible diff is the counterfactual it can't argue
    // with). Symbol-anchored content edits are excluded — the MCP layer already echoes those
    // blocks — so this stays scoped to the ops whose rewrites are otherwise invisible.
    let rewrite_summary = if ops.iter().any(|o| matches!(o, EditOp::Rename { .. } | EditOp::MoveFile { .. } | EditOp::DeleteFile { .. })) {
        rewrite_receipt(root, &changed_rel, &vfs, &transient_rels)
    } else {
        String::new()
    };

    if opts.write && !opts.dry_run {
        vfs.commit()?;
        // The transaction landed — the materialized creations are now REAL files (vfs.commit
        // just rewrote them) and the staged deletions are now REAL deletions; defuse both
        // guards so their drops neither delete the former nor resurrect the latter.
        transient.0.clear();
        transient_del.0.clear();
        // Close the empty stand-in buffers for deleted files: left open, they'd shadow the
        // REAL deletion (an empty module resolves; an absent one errors) for every later gate.
        let _ = engine.fs_events(&[], &deleted_rels);
    }
    Ok(CommitResult::Ok { applied_ops: ops.len(), changed_files: changed, repair_rounds: 0, preexisting_in_radius: preexisting, redundant_ops, rewrite_summary })
}

#[cfg(test)]
pub(crate) mod testutil {
    use ci_core::{Node, NodeKind, Range, SymbolKind};

    pub(crate) fn fn_node(file: &str, name: &str, sl: u32, el: u32) -> Node {
        Node {
            id: format!("{file}#{name}"),
            name: Some(name.into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: Range { start_line: sl, start_char: 0, end_line: el, end_char: 1 },
            name_range: Some(Range { start_line: sl, start_char: 16, end_line: sl, end_char: 19 }),
            children: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fn_node;
    use ci_core::{NodeKind, Range, SymbolKind};
    use std::fs;

    // The prewarm contract: the warming thread holds the slot lock for the whole
    // construction+warming call, so an `apply_edits` arriving mid-warm (its discipline: lock
    // the slot, cold-start only if empty) blocks and finds the WARMED engine — never a
    // second cold start.
    #[test]
    fn apply_during_inflight_prewarm_waits_for_the_warm_engine() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let slot: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let cold_starts = Arc::new(AtomicUsize::new(0));
        let (warming_tx, warming_rx) = std::sync::mpsc::channel();
        let handle = spawn_prewarm(slot.clone(), move || {
            warming_tx.send(()).unwrap(); // the slot lock is held from before this point
            std::thread::sleep(std::time::Duration::from_millis(150));
            Some("warm")
        });
        warming_rx.recv().unwrap();
        let mut guard = slot.lock().unwrap();
        if guard.is_none() {
            cold_starts.fetch_add(1, Ordering::SeqCst);
            *guard = Some("cold");
        }
        assert_eq!(*guard, Some("warm"), "mid-warm arrival must reuse the warmed engine");
        assert_eq!(cold_starts.load(Ordering::SeqCst), 0, "must wait, not double-start");
        drop(guard);
        handle.join().unwrap();
    }

    // An already-warm slot makes a second prewarm a no-op (the `is_some()` guard), and a
    // failed construction leaves the slot empty so first use starts lazily and surfaces
    // the real error.
    #[test]
    fn prewarm_guards_warm_slots_and_tolerates_failed_starts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let slot: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let starts = Arc::new(AtomicUsize::new(0));

        let s = starts.clone();
        spawn_prewarm(slot.clone(), move || {
            s.fetch_add(1, Ordering::SeqCst);
            None // engine failed to start
        })
        .join()
        .unwrap();
        assert_eq!(*slot.lock().unwrap(), None, "failed start leaves the slot cold");

        let s = starts.clone();
        spawn_prewarm(slot.clone(), move || {
            s.fetch_add(1, Ordering::SeqCst);
            Some(1)
        })
        .join()
        .unwrap();
        let s = starts.clone();
        spawn_prewarm(slot.clone(), move || {
            s.fetch_add(1, Ordering::SeqCst);
            Some(2)
        })
        .join()
        .unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(1), "warm slot is never replaced");
        assert_eq!(starts.load(Ordering::SeqCst), 2, "third prewarm never constructs");
    }

    /// `resolve_all_in` returns EVERY same-named symbol (incl. two in the same scope depth), while
    /// `resolve_in` only sees the first — the difference that lets the MCP flag same-file collisions
    /// instead of silently editing whichever came first.
    #[test]
    fn resolve_all_in_surfaces_same_file_collisions() {
        // A named symbol with a `name`d child, ids given explicitly (mirrors real structure ids).
        let scope = |iface: &str, line: u32| {
            let mut field = fn_node("t.ts", "nodeId", line + 1, line + 1);
            field.id = format!("t.ts#{iface}.nodeId");
            let mut s = fn_node("t.ts", iface, line, line + 2);
            s.id = format!("t.ts#{iface}");
            s.children = vec![field];
            s
        };
        // Two distinct symbols named `nodeId`, e.g. a field on each of two interfaces.
        let nodes = vec![scope("IfaceA", 1), scope("IfaceB", 5)];

        let all = resolve_all_in(&nodes, "nodeId");
        assert_eq!(all, vec!["t.ts#IfaceA.nodeId", "t.ts#IfaceB.nodeId"], "both collisions returned, in source order");
        // The old single-match helper hides the second one.
        assert_eq!(resolve_in(&nodes, "nodeId").as_deref(), Some("t.ts#IfaceA.nodeId"));
        // A unique name resolves to exactly one.
        assert_eq!(resolve_all_in(&nodes, "IfaceB"), vec!["t.ts#IfaceB"]);
        assert!(resolve_all_in(&nodes, "missing").is_empty());
    }

    #[test]
    fn enclosing_symbol_picks_the_innermost() {
        // A class spanning 1-20 with a method spanning 5-10; line 7 is inside the method.
        let method = fn_node("t.ts", "run", 5, 10);
        let mut class = fn_node("t.ts", "Svc", 1, 20);
        class.id = "t.ts#Svc".into();
        class.children = vec![{
            let mut m = method;
            m.id = "t.ts#Svc.run".into();
            m
        }];
        let nodes = vec![class];
        assert_eq!(enclosing_symbol(&nodes, 7).as_deref(), Some("t.ts#Svc.run")); // innermost
        assert_eq!(enclosing_symbol(&nodes, 3).as_deref(), Some("t.ts#Svc")); // only the class here
        assert_eq!(enclosing_symbol(&nodes, 99), None); // out of range
    }

    // Regression (bench T5 trajectory variance): a blast-radius diagnostic BELOW a same-file
    // op's node must NOT be attributed to that op — the op-anchored branch never emits the
    // ready-to-copy `fix:`, so a swallowed site forces the agent back into read/list_anchors.
    // Un-anchored is the designed path: enclosing symbol + fix.
    #[test]
    fn anchor_ignores_diags_outside_the_ops_node_range() {
        let ops = vec![EditOp::InsertInBody { node_id: "a.ts#makeEntry".into(), code: "x".into(), after: None }];
        let structure_of = |_f: &str| vec![fn_node("a.ts", "makeEntry", 50, 70), fn_node("a.ts", "extractAll", 100, 140)];
        // Same file, below the op's node: un-anchored (falls to the enclosing-symbol + fix path).
        let below = Diag { file: "a.ts".into(), code: 2741, message: "missing".into(), line: 135 };
        assert_eq!(anchor(&below, &ops, &structure_of), None);
        // Inside the op's node: anchored to it, as before.
        let inside = Diag { file: "a.ts".into(), code: 2741, message: "missing".into(), line: 60 };
        assert_eq!(anchor(&inside, &ops, &structure_of), Some((0, "a.ts#makeEntry".into())));
    }

    #[test]
    fn anchor_attributes_diagnostic_to_the_right_op() {
        let ops = vec![
            EditOp::ReplaceNode { node_id: "a.ts#foo".into(), code: "".into() },
            EditOp::ReplaceNode { node_id: "a.ts#bar".into(), code: "".into() },
        ];
        let structure_of = |_f: &str| vec![fn_node("a.ts", "foo", 1, 3), fn_node("a.ts", "bar", 5, 7)];

        let d_in_bar = Diag { file: "a.ts".into(), code: 2322, message: "x".into(), line: 6 };
        assert_eq!(anchor(&d_in_bar, &ops, &structure_of), Some((1, "a.ts#bar".into())));

        let d_in_foo = Diag { file: "a.ts".into(), code: 2322, message: "y".into(), line: 2 };
        assert_eq!(anchor(&d_in_foo, &ops, &structure_of), Some((0, "a.ts#foo".into())));

        // a diagnostic in another file isn't attributed to these ops
        let d_other = Diag { file: "b.ts".into(), code: 1, message: "z".into(), line: 1 };
        assert_eq!(anchor(&d_other, &ops, &structure_of), None);
    }

    #[test]
    fn write_outside_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let structure_of = |_f: &str| Vec::<Node>::new();
        let no_imports = |_: &str| Vec::<String>::new();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let mut engine = NoopEngine;

        for path in ["../evil.ts", "../../etc/x.ts", "/etc/passwd"] {
            let res = commit_edits(
                root,
                &[EditOp::CreateFile { path: path.into(), code: "x".into() }],
                &structure_of,
                &mut engine,
                &opts,
                &no_imports,
            )
            .unwrap();
            match res {
                CommitResult::Rejected { feedback, .. } => {
                    assert!(feedback.contains("escapes repo root"), "unexpected feedback: {feedback}");
                }
                other => panic!("expected rejection for {path:?}, got {other:?}"),
            }
        }
        // A legitimate in-repo create still succeeds (guard doesn't over-reject).
        let ok = commit_edits(
            root,
            &[EditOp::CreateFile { path: "src/new.ts".into(), code: "export const x = 1;\n".into() }],
            &structure_of,
            &mut engine,
            &opts,
            &no_imports,
        )
        .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "in-repo create should commit: {ok:?}");
        assert!(root.join("src/new.ts").exists(), "in-repo file written");
    }

    // A gate engine that reports no diagnostics — for path-guard tests that must reach commit.
    struct NoopEngine;
    impl GateEngine for NoopEngine {
        fn diagnostics(&mut self, _files: &[(String, String)]) -> Result<Vec<Diag>> {
            Ok(vec![])
        }
        fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    // File-level replace: `path` + a UNIQUE oldText addresses text OUTSIDE every symbol
    // anchor (imports, `mod` decls). Unique commits; ambiguous rejects with a count.
    #[test]
    fn replace_in_file_unique_commits_ambiguous_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lib.rs"), "pub mod tokenize;\npub mod store;\n").unwrap();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let structure_of = |_: &str| Vec::new();
        let no_rev = |_: &str| Vec::new();

        let ok = commit_edits(
            root,
            &[EditOp::ReplaceInFile { path: "lib.rs".into(), old_text: "pub mod tokenize;".into(), new_text: "pub mod text;".into() }],
            &structure_of,
            &mut NoopEngine,
            &opts,
            &no_rev,
        )
        .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "unique file-level replace commits: {ok:?}");
        assert!(std::fs::read_to_string(root.join("lib.rs")).unwrap().contains("pub mod text;"));

        let bad = commit_edits(
            root,
            &[EditOp::ReplaceInFile { path: "lib.rs".into(), old_text: "pub mod".into(), new_text: "mod".into() }],
            &structure_of,
            &mut NoopEngine,
            &opts,
            &no_rev,
        )
        .unwrap();
        match bad {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("2 times") || feedback.contains("occurs"), "counts the matches: {feedback}")
            }
            other => panic!("ambiguous oldText must reject: {other:?}"),
        }
    }

    #[test]
    fn suggest_fix_picks_an_inside_anchor_for_block_openers() {
        // Offending line opens a `{ …` block -> anchor on the first line INSIDE it, trimmed.
        let w = vec!["  return {", "    id: symbolId(name),", "    name,"];
        let fix = suggest_fix("b.ts#makeEntry", &w).unwrap();
        assert!(fix.contains(r#""oldText":"id: symbolId(name),""#), "inside-block anchor: {fix}");
        assert!(fix.contains(r#""name":"b.ts#makeEntry""#));
        // Plain offending line -> anchor on the line itself.
        let w = vec!["  const x = build(entry);"];
        let fix = suggest_fix("b.ts#run", &w).unwrap();
        assert!(fix.contains(r#""oldText":"const x = build(entry);""#), "own-line anchor: {fix}");
        // No source window -> no suggestion.
        assert_eq!(suggest_fix("b.ts#run", &[]), None);
    }

    // A gate engine that flags b.ts once the edit's marker text is in play — after-pass sees the
    // overlay (marker present -> diag), baseline-pass sees disk (absent -> clean), so the diag is
    // "new" and the rejection feedback is built.
    struct FanoutEngine;
    impl GateEngine for FanoutEngine {
        fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
            if files.iter().any(|(_, c)| c.contains("nameLower")) {
                return Ok(vec![Diag {
                    file: "b.ts".into(),
                    code: 2741,
                    message: "Property 'nameLower' is missing in type".into(),
                    line: 2,
                }]);
            }
            Ok(vec![])
        }
        fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    // Baseline: ONE instance of an error. After the edit: TWO instances with the identical
    // key (same file+code+message — the key has no line, since edits shift lines). Set-based
    // diffing excused BOTH as pre-existing — a false accept; count-based diffing flags the
    // second instance as new and rejects.
    struct DupDiagEngine;
    impl GateEngine for DupDiagEngine {
        fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
            let d = |line| Diag {
                file: "a.ts".into(),
                code: 2322,
                message: "Type 'string' is not assignable to type 'number'".into(),
                line,
            };
            // Baseline pass sees disk content; the after pass sees the overlay with the marker.
            if files.iter().any(|(_, c)| c.contains("NEW_BAD_SITE")) {
                Ok(vec![d(2), d(5)])
            } else {
                Ok(vec![d(2)])
            }
        }
        fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    #[test]
    fn duplicate_of_a_preexisting_error_is_still_new() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export function foo(): void {\n  const x: number = 'old';\n  return;\n}\n").unwrap();

        let structure_of = |f: &str| if f == "a.ts" { vec![fn_node("a.ts", "foo", 1, 4)] } else { vec![] };
        let no_imports = |_: &str| Vec::<String>::new();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = commit_edits(
            root,
            &[EditOp::ReplaceText {
                node_id: "a.ts#foo".into(),
                old_text: "return;".into(),
                new_text: "const y: number = 'NEW_BAD_SITE';\n  return;".into(),
            }],
            &structure_of,
            &mut DupDiagEngine,
            &opts,
            &no_imports,
        )
        .unwrap();

        let CommitResult::Rejected { feedback, .. } = res else {
            panic!("a second instance of a pre-existing error must reject, got {res:?}");
        };
        assert!(feedback.contains("TS2322"), "the new instance is surfaced: {feedback}");
        // Exactly ONE instance is new — the baseline one stays excused.
        assert_eq!(feedback.matches("TS2322").count(), 1, "only the extra instance is new: {feedback}");
    }

    #[test]
    fn rejection_carries_a_ready_to_copy_fix_per_blast_radius_site() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export interface SymbolEntry {\n  id: string;\n  name: string;\n}\n").unwrap();
        fs::write(
            root.join("b.ts"),
            "export function makeEntry(name: string): SymbolEntry {\n  return {\n    id: symbolId(name),\n    name,\n  };\n}\n",
        )
        .unwrap();

        let structure_of = |f: &str| match f {
            "a.ts" => vec![fn_node("a.ts", "SymbolEntry", 1, 4)],
            "b.ts" => vec![fn_node("b.ts", "makeEntry", 1, 6)],
            _ => vec![],
        };
        let reverse = |f: &str| if f == "a.ts" { vec!["b.ts".to_string()] } else { vec![] };
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = commit_edits(
            root,
            &[EditOp::ReplaceText {
                node_id: "a.ts#SymbolEntry".into(),
                old_text: "name: string;".into(),
                new_text: "name: string;\n  nameLower: string;".into(),
            }],
            &structure_of,
            &mut FanoutEngine,
            &opts,
            &reverse,
        )
        .unwrap();

        let CommitResult::Rejected { feedback, .. } = res else { panic!("expected rejection, got {res:?}") };
        // The untouched construction site is named by its enclosing symbol …
        assert!(feedback.contains("(in b.ts#makeEntry)"), "enclosing symbol tag: {feedback}");
        // … its source window is shown verbatim …
        assert!(feedback.contains("    id: symbolId(name),"), "verbatim snippet: {feedback}");
        // … and the ready-to-copy action carries the symbol + an INSIDE-the-literal anchor, so the
        // agent only fills in `value` (no read_node, no indentation reasoning).
        assert!(feedback.contains(r#""action":"insert_in_body""#), "fix action: {feedback}");
        assert!(feedback.contains(r#""name":"b.ts#makeEntry""#), "fix target: {feedback}");
        assert!(feedback.contains(r#""oldText":"id: symbolId(name),""#), "fix anchor: {feedback}");
        // Nothing was written — the reject left disk untouched.
        assert!(!fs::read_to_string(root.join("a.ts")).unwrap().contains("nameLower"));
    }

    // A same-file batch (the reject-then-batch flow: a schema op + a ready fix whose site lives
    // in the SAME file, e.g. a default-constructor right under its interface): every op's span
    // comes from PRE-batch disk truth, so commit_edits must apply same-file structural ops
    // bottom-up or the top op shifts the lower op's span and the batch corrupts.
    #[test]
    fn same_file_batch_applies_bottom_up_so_disk_truth_spans_survive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.ts"),
            "export interface Policy {\n  name: string;\n}\n\nexport function defaultPolicy(): Policy {\n  return { name: \"x\" };\n}\n",
        )
        .unwrap();

        let structure_of = |f: &str| {
            if f != "a.ts" {
                return vec![];
            }
            let mut iface = fn_node("a.ts", "Policy", 1, 3);
            iface.kind = NodeKind::Symbol(SymbolKind::Interface);
            let mut func = fn_node("a.ts", "defaultPolicy", 5, 7);
            let mut body = fn_node("a.ts", "defaultPolicy:body", 5, 7);
            body.id = "a.ts#defaultPolicy:body".into();
            body.range = Range { start_line: 5, start_char: 40, end_line: 7, end_char: 1 };
            func.children = vec![body];
            vec![iface, func]
        };
        let no_imports = |_: &str| Vec::<String>::new();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = commit_edits(
            root,
            &[
                // Stated top-first — the order an agent naturally writes (schema op, then fixes).
                EditOp::InsertMember { node_id: "a.ts#Policy".into(), code: "burst: number;".into() },
                EditOp::InsertInBody { node_id: "a.ts#defaultPolicy".into(), code: "void 0;".into(), after: None },
            ],
            &structure_of,
            &mut NoopEngine,
            &opts,
            &no_imports,
        )
        .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "same-file batch must commit: {res:?}");
        let out = fs::read_to_string(root.join("a.ts")).unwrap();
        assert!(out.contains("{\n  burst: number;\n  name: string;\n}"), "member landed first in the block:\n{out}");
        assert!(out.contains("void 0;"), "body insert landed:\n{out}");
        assert!(out.contains("return { name: \"x\" };"), "function body intact:\n{out}");
    }

    /// Serializes tsls STARTUP across the two real-LSP tests in this binary: both spawn
    /// `npx --yes` against the same npm cache, and concurrent npx installs corrupt it —
    /// the loser dies at spawn ("lsp server disconnected"). Same contention lang-ts fixed
    /// for scip/ts-morph behind its cache lock; here an in-process mutex suffices (cargo
    /// runs test binaries sequentially). Held only across start — a warm cache is fine to
    /// read concurrently.
    static TSLS_START: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn start_tsls(root: &Path) -> ci_lsp::LspClient {
        let _serialize = TSLS_START.lock().unwrap();
        let mut cmd = std::process::Command::new("npx");
        cmd.args(["--yes", "-p", "typescript-language-server", "-p", "typescript", "typescript-language-server", "--stdio"])
            .env("npm_config_cache", std::env::var("CI_NPM_CACHE").unwrap_or_else(|_| "/tmp/ci-npm-cache".into()));
        ci_lsp::LspClient::start(root, cmd).expect("start tsls")
    }

    // Real move via LSP willRenameFiles. #[ignore]; run with `--ignored`.
    #[test]
    #[ignore]
    fn move_file_rewrites_importers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#).unwrap();
        fs::create_dir_all(root.join("src/util")).unwrap();
        fs::write(root.join("src/math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n").unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from \"./math.js\";\nexport const r = add(1, 2);\n").unwrap();

        let mut lsp = start_tsls(root);
        let structure_of = |_f: &str| Vec::new();
        let no_imports = |_: &str| Vec::<String>::new();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = commit_edits(
            root,
            &[EditOp::MoveFile { from: "src/math.ts".into(), to: "src/util/math.ts".into() }],
            &structure_of,
            &mut lsp,
            &opts,
            &no_imports,
        )
        .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "move should commit: {res:?}");
        assert!(root.join("src/util/math.ts").exists(), "file moved to new path");
        assert!(!root.join("src/math.ts").exists(), "old path removed");
        let app = fs::read_to_string(root.join("src/app.ts")).unwrap();
        assert!(app.contains("util/math"), "importer rewritten by willRenameFiles, got: {app}");
    }

    // Real gate end-to-end: spawns typescript-language-server. #[ignore]; run with
    // `cargo test -p ci-edit -- --ignored`.
    #[test]
    #[ignore]
    fn gate_rejects_type_error_accepts_clean() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let original = "export function add(): number {\n  return 1;\n}\n";
        fs::write(root.join("src/a.ts"), original).unwrap();

        let structure_of = |_f: &str| vec![fn_node("src/a.ts", "add", 1, 3)];
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let mut lsp = start_tsls(root);
        let no_imports = |_: &str| Vec::<String>::new();

        // 1. A type-breaking replace_node is REJECTED; disk unchanged.
        let bad = commit_edits(
            root,
            &[EditOp::ReplaceNode {
                node_id: "src/a.ts#add".into(),
                code: "export function add(): number {\n  return \"no\";\n}".into(),
            }],
            &structure_of,
            &mut lsp,
            &opts,
            &no_imports,
        )
        .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type error must be rejected, got {bad:?}");
        assert_eq!(fs::read_to_string(root.join("src/a.ts")).unwrap(), original, "disk must be untouched");

        // 2. A clean replace_node is COMMITTED to disk.
        let ok = commit_edits(
            root,
            &[EditOp::ReplaceNode {
                node_id: "src/a.ts#add".into(),
                code: "export function add(): number {\n  return 2;\n}".into(),
            }],
            &structure_of,
            &mut lsp,
            &opts,
            &no_imports,
        )
        .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit must pass, got {ok:?}");
        assert!(fs::read_to_string(root.join("src/a.ts")).unwrap().contains("return 2;"));
    }
}
