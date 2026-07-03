//! ci-edit — atomic, gated edits. Resolves targets to SCIP node ranges, applies
//! them to a VFS overlay, then gates with an LSP **baseline-diff** (fail only on
//! NEWLY introduced diagnostics) before committing — else rolls back by dropping
//! the VFS. Rename goes through LSP (all references); structural edits use the
//! node's `enclosing_range`. No AST: `set_body` and the fine verbs are refused at
//! Symbol granularity (use `replace_node`).
use ci_core::{CommitResult, Diag, EditOp, EditOpts, Error, FileSummary, Node, Range, Result};
use ci_lsp::LspClient;
use ci_vfs::Vfs;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

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
    /// Restore every in-memory buffer the engine holds to the CURRENT on-disk content. Called
    /// before computing any edit: a prior dry-run or rejected gate pushed overlay content into
    /// the engine, and a rename computed against that phantom state returns spans that slice
    /// the wrong text on disk (only by luck — equal-length names — does it land). Default no-op
    /// for engines that hold no cross-call state.
    fn sync_disk(&mut self) -> Result<()> {
        Ok(())
    }
}

/// LSP request errors that mean "the server is still loading the project" rather than a real
/// failure — worth retrying with backoff. rust-analyzer mid-index returns these transiently
/// (JSON-RPC `-32602`/`-32801`, "content modified", "still loading", …).
fn is_transient_lsp_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("-32602")
        || m.contains("-32801")
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

/// Structured rich action payload (the MCP/wrapper input shape):
/// `{action, target, name, value, old_text?, new_text?}`.
#[derive(Debug, Clone, Default)]
pub struct Action {
    pub path: String,
    pub action: String,
    pub target: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    /// For `replace_text`: the exact substring to replace (must be unique within the target node).
    pub old_text: Option<String>,
    /// For `replace_text`: its replacement.
    pub new_text: Option<String>,
}

/// Map a structured action to an [`EditOp`]. `resolve(path, target, name) -> node_id`
/// turns target-kind + name addressing into a node id (via the structure tree).
pub fn action_to_op(
    a: &Action,
    resolve: impl Fn(&str, Option<&str>, Option<&str>) -> Option<String>,
) -> Result<EditOp> {
    let node = || {
        resolve(&a.path, a.target.as_deref(), a.name.as_deref())
            .ok_or_else(|| Error::Anchor(format!("{}#{}", a.path, a.name.clone().unwrap_or_default())))
    };
    // The resolved symbol id, optionally NARROWED to a sub-node anchor when `target` names one
    // (`body` / `return` / `param.N`). For a surgical edit the agent targets a sub-symbol range
    // — its body or return type or one parameter — instead of re-emitting the whole definition.
    // An UNRECOGNIZED target is an error, never a silent fallthrough: falling back to the whole
    // symbol would apply sub-node code (a body, a type) over the entire declaration — a
    // silently-wrong edit is worse than one clear retry. The narrowed id (`f.ts#foo:body`) is
    // validated against the structure tree in `apply_structural`.
    let targeted = || -> Result<String> {
        let base = node()?;
        Ok(match a.target.as_deref() {
            None | Some("") => base,
            Some("body") => format!("{base}:body"),
            Some("return") | Some("returnType") => format!("{base}:return"),
            Some("doc") | Some("comment") | Some("docstring") => format!("{base}:doc"),
            Some(t) if t.starts_with("param.") && t["param.".len()..].parse::<u32>().is_ok() => {
                format!("{base}:{t}")
            }
            Some(t) => {
                return Err(Error::Other(format!(
                    "unknown target {t:?} — use `body`, `return`, `doc`, or `param.N` (0-based), or omit \
                     `target` to address the whole symbol"
                )))
            }
        })
    };
    let value = || a.value.clone().ok_or_else(|| Error::Other(format!("{} needs a value", a.action)));
    Ok(match a.action.as_str() {
        "rename" => EditOp::Rename { node_id: node()?, new_name: value()? },
        "replace_node" => EditOp::ReplaceNode { node_id: targeted()?, code: value()? },
        // `replace_text` swaps an exact substring INSIDE a node (optionally a sub-node via
        // `target`) — the cheapest precise edit: the agent sends only old→new, not the whole
        // body. `old_text` must be unique within the node. Gated like any structural edit.
        "replace_text" => EditOp::ReplaceText {
            node_id: targeted()?,
            old_text: a.old_text.clone().ok_or_else(|| Error::Other("replace_text needs oldText".into()))?,
            new_text: a.new_text.clone().ok_or_else(|| Error::Other("replace_text needs newText".into()))?,
        },
        // `set_body` is sugar for replacing the `:body` anchor — re-draft a function/method body
        // without retyping its signature. Gated like any other structural edit.
        "set_body" => EditOp::SetBody { node_id: node()?, body: value()? },
        "insert_before" => EditOp::InsertBefore { node_id: targeted()?, code: value()? },
        // Statement-level body edits. `value` is the statement; `oldText` locates a line inside the
        // body (the `after` anchor for insert; the statement to remove for delete).
        "insert_in_body" => {
            EditOp::InsertInBody { node_id: node()?, code: value()?, after: a.old_text.clone() }
        }
        "insert_member" => EditOp::InsertMember { node_id: node()?, code: value()? },
        "delete_in_body" => EditOp::DeleteInBody {
            node_id: node()?,
            text: a.old_text.clone().or_else(|| a.value.clone()).ok_or_else(|| {
                Error::Other("delete_in_body needs oldText (the statement fragment to remove)".into())
            })?,
        },
        // Signature edits at an insertion point (no existing sub-node anchor). `value` is the new
        // parameter / return type.
        "add_parameter" => EditOp::AddParameter { node_id: node()?, param: value()? },
        "set_return_type" => EditOp::SetReturnType { node_id: node()?, ty: value()? },
        "create_file" => EditOp::CreateFile { path: a.path.clone().into(), code: value()? },
        "move_file" => EditOp::MoveFile { from: a.path.clone().into(), to: value()?.into() },
        "delete_file" => EditOp::DeleteFile { path: a.path.clone().into() },
        other => {
            return Err(Error::Driver(format!(
                "unsupported action {other:?} — valid actions: rename, replace_text, replace_node, set_body, \
                 insert_in_body, delete_in_body, insert_member, add_parameter, set_return_type, insert_before, \
                 move_file, create_file, delete_file"
            )))
        }
    })
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

fn find<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
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

/// Resolve a node id to its structure node (owned), or an `Anchor` error naming the id. The
/// structure tree is rebuilt per call (`structure_of`), so the returned `Node` is cloned out.
fn node_by_id(node_id: &str, structure_of: &impl Fn(&str) -> Vec<Node>) -> Result<Node> {
    let nodes = structure_of(file_of(node_id));
    find(&nodes, node_id).cloned().ok_or_else(|| Error::Anchor(node_id.to_string()))
}

fn file_of(node_id: &str) -> &str {
    node_id.split('#').next().unwrap_or(node_id)
}

/// The repo-relative paths an op reads/writes — checked for root-containment before it runs.
fn op_paths(op: &EditOp) -> Vec<PathBuf> {
    match op {
        EditOp::CreateFile { path, .. } | EditOp::DeleteFile { path } => vec![path.clone()],
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

/// Apply a structural (non-rename) op to the VFS using the node's range.
pub fn apply_structural(
    vfs: &mut Vfs,
    op: &EditOp,
    structure_of: &impl Fn(&str) -> Vec<Node>,
) -> Result<()> {
    match op {
        EditOp::ReplaceNode { node_id, code } => {
            let node = node_by_id(node_id, structure_of)?;
            vfs.replace_range(Path::new(file_of(node_id)), &node.range, code)
        }
        EditOp::InsertBefore { node_id, code } => {
            let node = node_by_id(node_id, structure_of)?;
            vfs.insert_before(Path::new(file_of(node_id)), &node.range, &format!("{code}\n\n"))
        }
        EditOp::ReplaceText { node_id, old_text, new_text } => {
            let file = file_of(node_id);
            let node = node_by_id(node_id, structure_of)?;
            let text = vfs
                .read_range(Path::new(file), &node.range)
                .ok_or_else(|| Error::Other("node text unavailable".into()))?;
            match text.matches(old_text.as_str()).count() {
                // Echo the node's ACTUAL text so the agent can fix oldText in one retry instead
                // of spiraling into read_node/Read calls to discover what it should have been.
                0 => {
                    return Err(Error::Other(format!(
                        "REPLACE_TEXT: oldText {old_text:?} not found in node '{node_id}'. Its current text is:\n{text}"
                    )))
                }
                1 => {}
                _ => {
                    return Err(Error::Other(format!(
                        "REPLACE_TEXT: oldText {old_text:?} is not unique in node '{node_id}' — include more surrounding text to disambiguate."
                    )))
                }
            }
            vfs.replace_range(Path::new(file), &node.range, &text.replacen(old_text, new_text, 1))
        }
        EditOp::CreateFile { path, code } => vfs.create(path, code.clone()),
        EditOp::SetBody { node_id, body } => {
            // Replace just the function/method `:body` anchor (the `{ … }` block), keeping the
            // signature. Requires AST granularity (the body sub-node must exist in `structure()`).
            let file = file_of(node_id);
            let nodes = structure_of(file);
            let body_id = format!("{node_id}:body");
            let node = find(&nodes, &body_id).ok_or_else(|| {
                Error::Anchor(format!(
                    "{body_id} — no body anchor (symbol has no editable body, or this provider lacks AST granularity; use replace_node)"
                ))
            })?;
            vfs.replace_range(Path::new(file), &node.range, body)
        }
        // Statement-level body edits: transform the `:body` sub-node's text and write it back. Pure
        // string surgery on the block, so it's language-generic (braces or a Python suite alike).
        EditOp::InsertInBody { node_id, code, after } => {
            let file = file_of(node_id);
            let range = subnode_range(node_id, "body", structure_of, "no editable body")?;
            let text = vfs
                .read_range(Path::new(file), &range)
                .ok_or_else(|| Error::Other("body text unavailable".into()))?;
            // A Python-suite body starts mid-indent, so its first line's indentation isn't in the
            // text; the body's start column supplies it. (Brace bodies keep indents in the text.)
            let base_indent = " ".repeat(range.start_char as usize);
            let new_body = insert_stmt_in_body(&text, code, after.as_deref(), &base_indent)?;
            vfs.replace_range(Path::new(file), &range, &new_body)
        }
        EditOp::DeleteInBody { node_id, text: needle } => {
            let file = file_of(node_id);
            let range = subnode_range(node_id, "body", structure_of, "no editable body")?;
            let text = vfs
                .read_range(Path::new(file), &range)
                .ok_or_else(|| Error::Other("body text unavailable".into()))?;
            let new_body = delete_stmt_in_body(&text, needle)?;
            vfs.replace_range(Path::new(file), &range, &new_body)
        }
        // Insert a member at the TOP of the container's `{ … }` block (interface field, class
        // member, object property). Works off the container node's own text — no `:body` sub-node
        // needed, so it targets a plain interface/type/class/object symbol directly. Landing first
        // (right after `{`) means our member carries its own separator and never needs the PRIOR
        // item to gain a trailing comma, so it stays valid for both `;`-separated interface members
        // and `,`-separated object properties. The type-check gate verifies the result.
        EditOp::InsertMember { node_id, code } => {
            let file = file_of(node_id);
            let node = node_by_id(node_id, structure_of)?;
            let text = vfs
                .read_range(Path::new(file), &node.range)
                .ok_or_else(|| Error::Other("node text unavailable".into()))?;
            let open = block_open(&text).ok_or_else(|| {
                Error::Other(format!("INSERT_MEMBER: node '{node_id}' has no `{{ … }}` block to insert into"))
            })?;
            let member_indent = format!("{}  ", " ".repeat(node.range.start_char as usize));
            let (head, tail) = text.split_at(open + 1); // just past the opening brace
            let new_text = format!("{head}\n{member_indent}{}{tail}", code.trim());
            vfs.replace_range(Path::new(file), &node.range, &new_text)
        }
        // Append a parameter: rewrite the `:params` `(...)` list, inserting before the `)`.
        EditOp::AddParameter { node_id, param } => {
            let file = file_of(node_id);
            let range = subnode_range(node_id, "params", structure_of, "no parameter list")?;
            let text = vfs
                .read_range(Path::new(file), &range)
                .ok_or_else(|| Error::Other("parameter list text unavailable".into()))?;
            vfs.replace_range(Path::new(file), &range, &insert_param(&text, param)?)
        }
        // Add a return type where none exists, at the language's insertion point (right after the
        // `)`). If one already exists we refuse — the agent should replace_node target:return.
        EditOp::SetReturnType { node_id, ty } => {
            let file = file_of(node_id);
            let nodes = structure_of(file);
            if find(&nodes, &format!("{node_id}:return")).is_some() {
                return Err(Error::Other(format!(
                    "SET_RETURN_TYPE: '{node_id}' already has a return type — use replace_node target:return to change it"
                )));
            }
            let params = find(&nodes, &format!("{node_id}:params"))
                .ok_or_else(|| Error::Anchor(format!("{node_id}:params — no parameter list to anchor a return type after")))?;
            // Insert at the params' end position (immediately after `)`).
            let at = Range {
                start_line: params.range.end_line,
                start_char: params.range.end_char,
                end_line: params.range.end_line,
                end_char: params.range.end_char,
            };
            vfs.insert_before(Path::new(file), &at, &format!("{}{ty}", return_delim(file)))
        }
        EditOp::MoveFile { .. } | EditOp::DeleteFile { .. } => {
            Err(Error::Driver("file ops (move/delete) land in P3".into()))
        }
        EditOp::Rename { .. } => Err(Error::Driver("rename must go through apply_rename".into())),
    }
}

/// Byte index of the block-opening `{` in a container's text — the first `{` that isn't inside a
/// generic `<…>` or a parameter `(…)`, so `interface Foo extends Bar<{ x }> {` finds the BODY
/// brace, not the one in the type argument. `None` if there's no such brace (e.g. a `type X = number`
/// alias with no block). Braces inside strings/comments are a theoretical edge the gate would catch.
fn block_open(text: &str) -> Option<usize> {
    let (mut angle, mut paren) = (0i32, 0i32);
    for (i, c) in text.char_indices() {
        match c {
            '<' => angle += 1,
            '>' if angle > 0 => angle -= 1,
            '(' => paren += 1,
            ')' if paren > 0 => paren -= 1,
            '{' if angle == 0 && paren == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Resolve `{node_id}:{sub}` to its range, or an `Anchor` error carrying `why` (the symbol lacks
/// that sub-node — no body/params, or the provider has no AST granularity).
fn subnode_range(
    node_id: &str,
    sub: &str,
    structure_of: &impl Fn(&str) -> Vec<Node>,
    why: &str,
) -> Result<Range> {
    let nodes = structure_of(file_of(node_id));
    let sub_id = format!("{node_id}:{sub}");
    find(&nodes, &sub_id).map(|n| n.range.clone()).ok_or_else(|| {
        Error::Anchor(format!(
            "{sub_id} — {why} (symbol has no {sub} anchor, or this provider lacks AST granularity)"
        ))
    })
}

/// The leading whitespace (indent) of a line.
fn indent_of(line: &str) -> String {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// Prefix every non-empty line of `code` with `indent`, preserving its own relative indentation —
/// so a single- or multi-line insert aligns with the statements around it.
fn reindent(code: &str, indent: &str) -> String {
    code.split('\n')
        .map(|l| if l.is_empty() { String::new() } else { format!("{indent}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Insert `code` as a statement inside a body block's text. With `after`, it lands on the line
/// after the unique line containing that fragment; without it, at the end of the body — before a
/// trailing lone `}` for brace languages, else after the last statement (Python suite). A line
/// whose leading whitespace isn't in the text (a suite's first line) falls back to `base_indent`.
fn insert_stmt_in_body(body: &str, code: &str, after: Option<&str>, base_indent: &str) -> Result<String> {
    // The indentation to align an insert with `line`, falling back to `base_indent` when the line
    // carries none in the text (the first statement of a Python suite).
    let line_indent = |line: &str| {
        let i = indent_of(line);
        if i.is_empty() { base_indent.to_string() } else { i }
    };
    let mut lines: Vec<String> = body.split('\n').map(String::from).collect();
    if let Some(anchor) = after {
        let hits: Vec<usize> =
            lines.iter().enumerate().filter(|(_, l)| l.contains(anchor)).map(|(i, _)| i).collect();
        if hits.len() != 1 {
            return Err(Error::Other(format!(
                "INSERT_IN_BODY: `after` {anchor:?} {} the body — give an exact, unique line fragment",
                if hits.is_empty() { "was not found in" } else { "is not unique in" }
            )));
        }
        let i = hits[0];
        let indent = line_indent(&lines[i]);
        lines.insert(i + 1, reindent(code, &indent));
        return Ok(lines.join("\n"));
    }
    match lines.iter().rposition(|l| !l.trim().is_empty()) {
        None => Ok(code.to_string()), // empty body — insert as-is
        Some(i) if lines[i].trim() == "}" => {
            // Insert before the closing brace, matching a sibling statement's indent (else one
            // level past the brace).
            let sib = lines[..i].iter().rev().find(|l| !l.trim().is_empty()).map(|l| line_indent(l));
            let indent = sib.unwrap_or_else(|| format!("{}    ", indent_of(&lines[i])));
            lines.insert(i, reindent(code, &indent));
            Ok(lines.join("\n"))
        }
        Some(i) => {
            let indent = line_indent(&lines[i]);
            lines.insert(i + 1, reindent(code, &indent));
            Ok(lines.join("\n"))
        }
    }
}

/// Delete the unique statement line containing `needle` from a body block's text.
fn delete_stmt_in_body(body: &str, needle: &str) -> Result<String> {
    let lines: Vec<String> = body.split('\n').map(String::from).collect();
    let hits: Vec<usize> =
        lines.iter().enumerate().filter(|(_, l)| l.contains(needle)).map(|(i, _)| i).collect();
    if hits.len() != 1 {
        return Err(Error::Other(format!(
            "DELETE_IN_BODY: {needle:?} {} the body — give an exact, unique line fragment",
            if hits.is_empty() { "was not found in" } else { "is not unique in" }
        )));
    }
    let drop = hits[0];
    Ok(lines.into_iter().enumerate().filter(|(j, _)| *j != drop).map(|(_, l)| l).collect::<Vec<_>>().join("\n"))
}

/// Insert `param` into a `(...)` parameter-list text, before the closing `)`, prefixing `, ` when
/// the list already has parameters.
fn insert_param(params: &str, param: &str) -> Result<String> {
    let open = params.find('(').ok_or_else(|| Error::Other("ADD_PARAMETER: no '(' in the parameter list".into()))?;
    let close = params.rfind(')').ok_or_else(|| Error::Other("ADD_PARAMETER: no ')' in the parameter list".into()))?;
    if close < open {
        return Err(Error::Other("ADD_PARAMETER: malformed parameter list".into()));
    }
    let sep = if params[open + 1..close].trim().is_empty() { "" } else { ", " };
    Ok(format!("{}{sep}{param}{}", &params[..close], &params[close..]))
}

/// The return-type delimiter for a file's language: `-> T` for Rust/Python, `: T` for TS.
fn return_delim(file: &str) -> &'static str {
    if file.ends_with(".rs") || file.ends_with(".py") || file.ends_with(".pyi") {
        " -> "
    } else {
        ": "
    }
}

/// Apply a rename through the gate engine at the symbol's name position, then apply the
/// returned WorkspaceEdit to the VFS (all references). Engine handles its own warmup.
fn apply_rename(
    vfs: &mut Vfs,
    node_id: &str,
    new_name: &str,
    root: &Path,
    structure_of: &impl Fn(&str) -> Vec<Node>,
    engine: &mut dyn GateEngine,
) -> Result<()> {
    let file = file_of(node_id);
    let nodes = structure_of(file);
    let node = find(&nodes, node_id).ok_or_else(|| Error::Anchor(node_id.to_string()))?;
    let nr = node.name_range.as_ref().unwrap_or(&node.range);
    let we = engine.rename(file, nr.start_line.saturating_sub(1), nr.start_char, new_name)?;
    // A rename always rewrites at least its own definition. Zero edits means the position
    // didn't resolve to a renameable symbol — fail loudly instead of silently reporting "no
    // changes," which (with apply_edits' "this is complete, don't verify" message) would let
    // the agent ship a rename that did nothing.
    if workspace_edit_is_empty(&we) {
        return Err(Error::Driver(format!(
            "rename produced no edits — '{node_id}' did not resolve to a renameable symbol; nothing was changed"
        )));
    }
    apply_workspace_edit(vfs, root, &we)
}

/// True when a WorkspaceEdit carries no actual text edits (empty `documentChanges`/`changes`).
fn workspace_edit_is_empty(we: &Value) -> bool {
    let nonempty = |edits: Option<&Vec<Value>>| edits.is_some_and(|e| !e.is_empty());
    if let Some(dc) = we.get("documentChanges").and_then(Value::as_array) {
        return !dc.iter().any(|d| nonempty(d.get("edits").and_then(Value::as_array)));
    }
    if let Some(ch) = we.get("changes").and_then(Value::as_object) {
        return !ch.values().any(|e| nonempty(e.as_array()));
    }
    true
}

/// Move a file: ask the engine to compute importer rewrites (`willRename`), apply them to
/// the VFS, then move the file. If the engine can't compute them, the move still proceeds
/// (the blast-radius gate catches any breakage).
fn apply_move(vfs: &mut Vfs, from: &Path, to: &Path, root: &Path, engine: &mut dyn GateEngine) -> Result<()> {
    let from_rel = from.to_string_lossy().replace('\\', "/");
    let to_rel = to.to_string_lossy().replace('\\', "/");
    if let Ok(we) = engine.will_rename(&from_rel, &to_rel) {
        if std::env::var("CI_LSP_DEBUG").is_ok() {
            eprintln!("willRename -> {we}");
        }
        let _ = apply_workspace_edit(vfs, root, &we);
    }
    vfs.move_file(from, to)
}

/// Delete a file — refused (statically, via the SCIP reverse import graph) if
/// anything still imports it. Matches the Node tool's safety behavior.
pub fn apply_delete(
    vfs: &mut Vfs,
    path: &Path,
    reverse_imports: &impl Fn(&str) -> Vec<String>,
) -> Result<()> {
    let rel = path.to_string_lossy().replace('\\', "/");
    let importers = reverse_imports(&rel);
    if !importers.is_empty() {
        return Err(Error::Driver(format!(
            "DELETE_FILE refused: {rel} is still imported by {}",
            importers.join(", ")
        )));
    }
    vfs.delete(path);
    Ok(())
}

fn apply_workspace_edit(vfs: &mut Vfs, root: &Path, we: &Value) -> Result<()> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(dc) = we.get("documentChanges").and_then(Value::as_array) {
        for d in dc {
            if let (Some(uri), Some(edits)) = (
                d.get("textDocument").and_then(|t| t.get("uri")).and_then(Value::as_str),
                d.get("edits").and_then(Value::as_array),
            ) {
                groups.push((uri.to_string(), edits.clone()));
            }
        }
    } else if let Some(changes) = we.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(arr) = edits.as_array() {
                groups.push((uri.clone(), arr.clone()));
            }
        }
    }

    for (uri, mut edits) in groups {
        let rel = uri_to_rel(&uri, root).ok_or_else(|| Error::Other(format!("uri outside root: {uri}")))?;
        // Descending by start so earlier edits don't shift later offsets.
        edits.sort_by_key(|e| std::cmp::Reverse(edit_start(e)));
        for e in &edits {
            let range = lsp_range(e.get("range"))?;
            let new_text = e.get("newText").and_then(Value::as_str).unwrap_or("");
            vfs.replace_range(Path::new(&rel), &range, new_text)?;
        }
    }
    Ok(())
}

fn edit_start(e: &Value) -> (i64, i64) {
    let s = e.get("range").and_then(|r| r.get("start"));
    (
        s.and_then(|x| x.get("line")).and_then(Value::as_i64).unwrap_or(0),
        s.and_then(|x| x.get("character")).and_then(Value::as_i64).unwrap_or(0),
    )
}

fn lsp_range(r: Option<&Value>) -> Result<Range> {
    let r = r.ok_or_else(|| Error::Other("edit range missing".into()))?;
    let g = |k: &str, f: &str| r.get(k).and_then(|x| x.get(f)).and_then(Value::as_i64).unwrap_or(0) as u32;
    Ok(Range {
        start_line: g("start", "line") + 1,
        start_char: g("start", "character"),
        end_line: g("end", "line") + 1,
        end_char: g("end", "character"),
    })
}

fn uri_to_rel(uri: &str, root: &Path) -> Option<String> {
    let prefix = format!("file://{}/", root.to_string_lossy());
    uri.strip_prefix(&prefix).map(str::to_string)
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
            EditOp::Rename { .. } | EditOp::MoveFile { .. } | EditOp::DeleteFile { .. } | EditOp::CreateFile { .. }
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

    for i in order {
        let op = &ops[i];
        // Trust boundary: reject before any VFS mutation if the op targets a path outside the repo.
        for p in op_paths(op) {
            if let Err(e) = ensure_within_root(root, &p) {
                return Ok(CommitResult::Rejected { failed_op_index: i as i64, feedback: e.to_string() });
            }
        }
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
    }

    if vfs.is_empty() {
        return Ok(CommitResult::Ok { applied_ops: ops.len(), changed_files: vec![], repair_rounds: 0 });
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

    // After = overlay (edited) content for the changed files; disk content for the dependents
    // (their source is unchanged, but tsserver re-checks them against the overlaid changed
    // files, so a freshly-broken caller surfaces here).
    let after_files: Vec<(String, String)> = affected
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
    let after = engine.diagnostics(&after_files)?;
    // Gate as a diff, computed LAZILY: if the post-edit state is clean, there can be no newly
    // introduced error, so we commit WITHOUT the baseline pass — the happy path (a good edit), which
    // halves the gate's type-check work. Only when `after` has errors do we pay the baseline pass, to
    // tell a freshly-introduced error from one that was already there (disk is untouched until commit).
    let new: Vec<&Diag> = if after.is_empty() {
        Vec::new()
    } else {
        // The transiently-materialized creations must stay INVISIBLE to the baseline — they hold
        // after-content, so including them would let a broken new file excuse its own errors.
        let baseline_files: Vec<(String, String)> = affected
            .iter()
            .filter(|rel| !transient_rels.contains(*rel))
            .filter_map(|rel| std::fs::read_to_string(root.join(rel)).ok().map(|c| (rel.clone(), c)))
            .collect();
        // COUNT baseline occurrences per key, don't just collect the key set: the key has no line
        // (edits shift lines, so it can't), which means one pre-existing error would otherwise
        // excuse EVERY new instance with the same code+message in that file — a false accept.
        // Each after-instance beyond the baseline count is new. (When instances are excused, the
        // FIRST ones in diagnostic order are — so a flagged line can occasionally be the old
        // site rather than the new one; the reject still surfaces the error either way.)
        let mut baseline_counts: HashMap<String, usize> = HashMap::new();
        for d in engine.diagnostics(&baseline_files)? {
            *baseline_counts.entry(diag_key(&d)).or_default() += 1;
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        after
            .iter()
            .filter(|d| {
                let k = diag_key(d);
                let n = seen.entry(k.clone()).or_default();
                *n += 1;
                *n > baseline_counts.get(&k).copied().unwrap_or(0)
            })
            .collect()
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
                        (format!("op #{i} ({node_id}) -> {}:{} TS{} {}", d.file, d.line, d.code, d.message), None)
                    }
                    // A blast-radius error at a site NO op touched (e.g. a construction site that must
                    // now set a newly-required field). Name its enclosing symbol so the agent edits it
                    // directly in the next batch instead of a list_anchors hunt to map line -> node —
                    // and hand it the ready-to-copy insert_in_body for that site (only `value` left).
                    None => match enclosing_symbol(&structure_of(&d.file), d.line) {
                        Some(id) => {
                            let fix = suggest_fix(&id, &w);
                            (format!("{}:{} (in {id}) TS{} {}", d.file, d.line, d.code, d.message), fix)
                        }
                        None => (format!("{}:{} TS{} {}", d.file, d.line, d.code, d.message), None),
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

    if opts.write && !opts.dry_run {
        vfs.commit()?;
        // The transaction landed — the materialized creations are now REAL files (vfs.commit
        // just rewrote them); defuse the guard so its drop doesn't delete them.
        transient.0.clear();
    }
    Ok(CommitResult::Ok { applied_ops: ops.len(), changed_files: changed, repair_rounds: 0 })
}

// ── Composed: ReadIndex × GateEngine = LanguageProvider ─────────────────────────────────────

/// Builds the write engine on first use (lazily / off-thread via `prewarm`).
pub type EngineFactory = std::sync::Arc<dyn Fn(&Path) -> Result<Box<dyn GateEngine + Send>> + Send + Sync>;

/// A [`LanguageProvider`] assembled from its two halves: a [`ReadIndex`] (the artifact or
/// live parser the agent PLANS against) and a [`GateEngine`] (the checker its edits run
/// through). The halves talk over exactly three channels, and the wiring POLICY is derived
/// from the reader's advertised properties instead of hand-wired per language:
///
/// 1. **radius** (read -> engine): the reverse-import set fed to [`commit_edits`] — one hop
///    when [`ReadIndex::semantic_edges`] (compiler-accurate graphs flatten barrels),
///    transitive otherwise (bench T9: a syntactic one-hop radius lets a barrel hide its
///    consumers).
/// 2. **freshness** (engine -> read): after a committed edit, artifact readers get overrides
///    from `GateEngine::file_summaries` so reads track the commit until the next reindex;
///    [`ReadIndex::live`] readers skip this — they re-parse current disk by construction.
/// 3. **anchors**: edit ops resolve against the read structure the agent actually saw.
use ci_core::{Granularity, ImportGraph, LanguageProvider, ReadIndex};
use std::sync::{Arc, Mutex};

pub struct Composed<R: ReadIndex> {
    root: PathBuf,
    read: R,
    engine_factory: EngineFactory,
    engine: Arc<Mutex<Option<Box<dyn GateEngine + Send>>>>,
    fresh: Arc<Mutex<HashMap<String, FileSummary>>>,
}

impl<R: ReadIndex> Composed<R> {
    pub fn new(root: &Path, read: R, engine_factory: EngineFactory) -> Self {
        Self {
            root: root.to_path_buf(),
            read,
            engine_factory,
            engine: Arc::new(Mutex::new(None)),
            fresh: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<R: ReadIndex> LanguageProvider for Composed<R> {
    fn granularity(&self) -> Granularity {
        self.read.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        if !self.read.live() {
            let rel = file.to_string_lossy().replace('\\', "/");
            if let Ok(m) = self.fresh.lock() {
                if let Some(s) = m.get(&rel) {
                    return Ok(if s.deleted { vec![] } else { s.nodes.clone() });
                }
            }
        }
        self.read.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let mut g = self.read.import_graph()?;
        if !self.read.live() {
            if let Ok(m) = self.fresh.lock() {
                for s in m.values() {
                    let key = PathBuf::from(&s.path);
                    if s.deleted || s.imports.is_empty() {
                        g.remove(&key);
                    } else {
                        g.insert(key, s.imports.clone());
                    }
                }
            }
        }
        Ok(g)
    }

    fn prewarm(&self) {
        let slot = self.engine.clone();
        let factory = self.engine_factory.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Ok(mut guard) = slot.lock() else { return };
            if guard.is_some() {
                return;
            }
            if let Ok(mut engine) = factory(&root) {
                let _ = engine.diagnostics(&[]);
                *guard = Some(engine);
            }
        });
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some((self.engine_factory)(&self.root)?);
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap().as_mut();

        let structure_of = |f: &str| self.structure(Path::new(f)).unwrap_or_default();
        // Channel 1 — radius policy from the reader's edge semantics.
        let reverse = ci_core::reverse_import_map(&self.import_graph().unwrap_or_default());
        let semantic = self.read.semantic_edges();
        let reverse_imports = |file: &str| {
            if semantic {
                reverse.get(file).cloned().unwrap_or_default()
            } else {
                ci_core::transitive_reverse_imports(&reverse, file)
            }
        };
        let r = commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports);

        // Channel 2 — freshness push-back, artifact readers only (best-effort: a refresh
        // hiccup must never fail an already-committed edit; reads then lag until reindex).
        if !self.read.live() {
            if let Ok(CommitResult::Ok { changed_files, .. }) = &r {
                if opts.write && !opts.dry_run && !changed_files.is_empty() {
                    let rels: Vec<String> =
                        changed_files.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
                    match engine.file_summaries(&rels) {
                        Ok(Some(summaries)) => {
                            if let Ok(mut m) = self.fresh.lock() {
                                for s in summaries {
                                    m.insert(s.path.clone(), s);
                                }
                            }
                        }
                        Ok(None) => {} // engine can't re-describe: reads lag until reindex
                        Err(e) => eprintln!("[composed] post-edit read refresh failed ({e}); reads lag until reindex"),
                    }
                }
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::{NodeKind, SymbolKind};
    use std::fs;

    fn fn_node(file: &str, name: &str, sl: u32, el: u32) -> Node {
        Node {
            id: format!("{file}#{name}"),
            name: Some(name.into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: Range { start_line: sl, start_char: 0, end_line: el, end_char: 1 },
            name_range: Some(Range { start_line: sl, start_char: 16, end_line: sl, end_char: 19 }),
            children: vec![],
        }
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

    #[test]
    fn insert_member_lands_first_in_the_block() {
        use ci_vfs::Vfs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // An interface with a generic-bound `{` in `extends` — block_open must skip it and find the
        // real body brace.
        std::fs::write(
            root.join("t.ts"),
            "export interface Foo extends Bar<{ x: number }> {\n  id: string;\n}\n",
        )
        .unwrap();
        let node = Node {
            id: "t.ts#Foo".into(),
            name: Some("Foo".into()),
            kind: NodeKind::Symbol(SymbolKind::Interface),
            range: Range { start_line: 1, start_char: 0, end_line: 3, end_char: 1 },
            name_range: None,
            children: vec![],
        };
        let structure_of = |_f: &str| vec![node.clone()];
        let mut vfs = Vfs::new(root);
        apply_structural(
            &mut vfs,
            &EditOp::InsertMember { node_id: "t.ts#Foo".into(), code: "tag: string;".into() },
            &structure_of,
        )
        .unwrap();
        let out = vfs.read(Path::new("t.ts")).unwrap();
        // New member is inserted right after the BODY `{` (not the generic one), ahead of `id`.
        assert!(
            out.contains("> {\n  tag: string;\n  id: string;\n}"),
            "member should land first in the body block, got:\n{out}"
        );
    }

    #[test]
    fn insert_member_rejects_a_blockless_node() {
        use ci_vfs::Vfs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("t.ts"), "export type Id = string;\n").unwrap();
        let node = Node {
            id: "t.ts#Id".into(),
            name: Some("Id".into()),
            kind: NodeKind::Symbol(SymbolKind::TypeAlias),
            range: Range { start_line: 1, start_char: 0, end_line: 1, end_char: 24 },
            name_range: None,
            children: vec![],
        };
        let structure_of = |_f: &str| vec![node.clone()];
        let mut vfs = Vfs::new(root);
        let err = apply_structural(
            &mut vfs,
            &EditOp::InsertMember { node_id: "t.ts#Id".into(), code: "x: string;".into() },
            &structure_of,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no `{"), "expected a no-block error, got: {err}");
    }

    #[test]
    fn action_maps_set_body_and_subnode_targets() {
        let resolve = |_p: &str, _t: Option<&str>, n: Option<&str>| n.map(|n| format!("a.ts#{n}"));
        let act = |action: &str, target: Option<&str>| {
            action_to_op(
                &Action {
                    path: "a.ts".into(),
                    action: action.into(),
                    target: target.map(str::to_string),
                    name: Some("add".into()),
                    value: Some("v".into()),
                    ..Default::default()
                },
                resolve,
            )
        };

        // rename targets the whole symbol regardless of `target`.
        assert!(matches!(act("rename", Some("function")).unwrap(), EditOp::Rename { .. }));

        // set_body maps to SET_BODY against the symbol (apply_structural narrows to `:body`).
        match act("set_body", None).unwrap() {
            EditOp::SetBody { node_id, .. } => assert_eq!(node_id, "a.ts#add"),
            o => panic!("expected SetBody, got {o:?}"),
        }

        // replace_node narrows to the sub-node anchor when `target` names one.
        let id = |op| match op {
            EditOp::ReplaceNode { node_id, .. } => node_id,
            o => panic!("expected ReplaceNode, got {o:?}"),
        };
        assert_eq!(id(act("replace_node", Some("body")).unwrap()), "a.ts#add:body");
        assert_eq!(id(act("replace_node", Some("return")).unwrap()), "a.ts#add:return");
        assert_eq!(id(act("replace_node", Some("param.1")).unwrap()), "a.ts#add:param.1");
        assert_eq!(id(act("replace_node", Some("doc")).unwrap()), "a.ts#add:doc");
        // an unknown target is REJECTED (it used to fall through to the whole symbol — which
        // silently applied sub-node code over the entire declaration); no target = whole symbol.
        assert!(act("replace_node", Some("function")).is_err());
        assert_eq!(id(act("replace_node", None).unwrap()), "a.ts#add");

        // replace_text carries oldText/newText and honors `target` for sub-node scoping.
        let rt = action_to_op(
            &Action {
                path: "a.ts".into(),
                action: "replace_text".into(),
                target: Some("body".into()),
                name: Some("add".into()),
                old_text: Some("foo".into()),
                new_text: Some("bar".into()),
                ..Default::default()
            },
            resolve,
        )
        .unwrap();
        match rt {
            EditOp::ReplaceText { node_id, old_text, new_text } => {
                assert_eq!(node_id, "a.ts#add:body");
                assert_eq!(old_text, "foo");
                assert_eq!(new_text, "bar");
            }
            o => panic!("expected ReplaceText, got {o:?}"),
        }
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
    fn action_maps_file_ops() {
        let resolve = |_p: &str, _t: Option<&str>, _n: Option<&str>| None;
        let mv = action_to_op(
            &Action { path: "a.ts".into(), action: "move_file".into(), target: None, name: None, value: Some("b/a.ts".into()), ..Default::default() },
            resolve,
        )
        .unwrap();
        assert!(matches!(mv, EditOp::MoveFile { .. }));
        let del = action_to_op(
            &Action { path: "a.ts".into(), action: "delete_file".into(), target: None, name: None, value: None, ..Default::default() },
            resolve,
        )
        .unwrap();
        assert!(matches!(del, EditOp::DeleteFile { .. }));
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

    #[test]
    fn unknown_target_is_rejected_never_widened() {
        let resolve = |_: &str, _: Option<&str>, _: Option<&str>| Some("a.ts#foo".to_string());
        let act = |target: Option<&str>| Action {
            path: String::new(),
            action: "replace_node".into(),
            target: target.map(str::to_string),
            name: Some("foo".into()),
            value: Some("x".into()),
            old_text: None,
            new_text: None,
        };
        // A bogus target must error — falling back to the whole symbol would apply sub-node code
        // over the entire declaration.
        for bad in ["function", "params", "param 1", "param.x", "first"] {
            let err = action_to_op(&act(Some(bad)), resolve).unwrap_err().to_string();
            assert!(err.contains("unknown target"), "target {bad:?} must be rejected, got: {err}");
        }
        // Valid targets narrow to the sub-node; none/empty stays the whole symbol.
        for (t, want) in [("body", "a.ts#foo:body"), ("returnType", "a.ts#foo:return"), ("param.1", "a.ts#foo:param.1")] {
            match action_to_op(&act(Some(t)), resolve).unwrap() {
                EditOp::ReplaceNode { node_id, .. } => assert_eq!(node_id, want),
                other => panic!("unexpected op: {other:?}"),
            }
        }
        for whole in [None, Some("")] {
            match action_to_op(&act(whole), resolve).unwrap() {
                EditOp::ReplaceNode { node_id, .. } => assert_eq!(node_id, "a.ts#foo"),
                other => panic!("unexpected op: {other:?}"),
            }
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

    #[test]
    fn delete_refused_when_imported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export const x = 1;\n").unwrap();
        let mut vfs = Vfs::new(root);

        // imported -> refused
        let imported = |f: &str| if f == "a.ts" { vec!["b.ts".to_string()] } else { vec![] };
        assert!(apply_delete(&mut vfs, Path::new("a.ts"), &imported).is_err());

        // not imported -> removed from the VFS overlay
        let none = |_: &str| Vec::<String>::new();
        assert!(apply_delete(&mut vfs, Path::new("a.ts"), &none).is_ok());
        assert!(vfs.read(Path::new("a.ts")).is_none());
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

    #[test]
    fn structural_replace_node_in_vfs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export function add() {\n  return 1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_node("a.ts", "add", 1, 3)];
        apply_structural(
            &mut vfs,
            &EditOp::ReplaceNode { node_id: "a.ts#add".into(), code: "export function add() {\n  return 2;\n}".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.ts")).unwrap(), "export function add() {\n  return 2;\n}\n");
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

    fn rng(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range { start_line: sl, start_char: sc, end_line: el, end_char: ec }
    }
    fn sub(id: &str, r: Range) -> Node {
        Node { id: id.into(), name: None, kind: NodeKind::Syntax("x".into()), range: r, name_range: None, children: vec![] }
    }
    /// A function symbol node carrying the given sub-node children (`:body`/`:params`/`:return`).
    fn fn_with(id: &str, sym: Range, children: Vec<Node>) -> Node {
        Node {
            id: id.into(),
            name: Some(id.split('#').nth(1).unwrap_or(id).into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: sym,
            name_range: None,
            children,
        }
    }

    #[test]
    fn insert_in_body_appends_before_closing_brace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {\n    let x = 1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        // body spans `{ … }` (line1 col9 .. line3 col1).
        let structure_of = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 3, 1), vec![sub("a.rs#foo:body", rng(1, 9, 3, 1))])];
        apply_structural(
            &mut vfs,
            &EditOp::InsertInBody { node_id: "a.rs#foo".into(), code: "let y = 2;".into(), after: None },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo() {\n    let x = 1;\n    let y = 2;\n}\n");
    }

    #[test]
    fn insert_in_body_after_anchor_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {\n    let a = 1;\n    let b = 2;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 4, 1), vec![sub("a.rs#foo:body", rng(1, 9, 4, 1))])];
        // insert after the `let a` line
        apply_structural(
            &mut vfs,
            &EditOp::InsertInBody { node_id: "a.rs#foo".into(), code: "let mid = 0;".into(), after: Some("let a".into()) },
            &structure_of,
        )
        .unwrap();
        assert_eq!(
            vfs.read(Path::new("a.rs")).unwrap(),
            "fn foo() {\n    let a = 1;\n    let mid = 0;\n    let b = 2;\n}\n"
        );
    }

    #[test]
    fn delete_in_body_removes_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {\n    let a = 1;\n    let b = 2;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 4, 1), vec![sub("a.rs#foo:body", rng(1, 9, 4, 1))])];
        apply_structural(
            &mut vfs,
            &EditOp::DeleteInBody { node_id: "a.rs#foo".into(), text: "let b".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo() {\n    let a = 1;\n}\n");
    }

    #[test]
    fn add_parameter_into_empty_and_nonempty_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {}\n").unwrap();
        let mut vfs = Vfs::new(root);
        // params `()` at line1 col6..col8.
        let structure_of = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 1, 11), vec![sub("a.rs#foo:params", rng(1, 6, 1, 8))])];
        apply_structural(
            &mut vfs,
            &EditOp::AddParameter { node_id: "a.rs#foo".into(), param: "x: i32".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo(x: i32) {}\n");

        // Now the list is non-empty: a second add prefixes ", ". params now span col6..col14.
        let structure_of2 = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 1, 17), vec![sub("a.rs#foo:params", rng(1, 6, 1, 14))])];
        apply_structural(
            &mut vfs,
            &EditOp::AddParameter { node_id: "a.rs#foo".into(), param: "y: i32".into() },
            &structure_of2,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo(x: i32, y: i32) {}\n");
    }

    #[test]
    fn set_return_type_inserts_after_params_and_refuses_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Rust: `-> T` after `)`.
        fs::write(root.join("a.rs"), "fn foo() {}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_with("a.rs#foo", rng(1, 0, 1, 11), vec![sub("a.rs#foo:params", rng(1, 6, 1, 8))])];
        apply_structural(
            &mut vfs,
            &EditOp::SetReturnType { node_id: "a.rs#foo".into(), ty: "i32".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo() -> i32 {}\n");

        // TS: `: T` after `)`.
        fs::write(root.join("b.ts"), "function bar() {}\n").unwrap();
        let ts_struct = |_f: &str| vec![fn_with("b.ts#bar", rng(1, 0, 1, 17), vec![sub("b.ts#bar:params", rng(1, 12, 1, 14))])];
        apply_structural(
            &mut vfs,
            &EditOp::SetReturnType { node_id: "b.ts#bar".into(), ty: "number".into() },
            &ts_struct,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("b.ts")).unwrap(), "function bar(): number {}\n");

        // Refused when a return type already exists (agent should replace_node target:return).
        let with_ret = |_f: &str| {
            vec![fn_with(
                "a.rs#foo",
                rng(1, 0, 1, 11),
                vec![sub("a.rs#foo:params", rng(1, 6, 1, 8)), sub("a.rs#foo:return", rng(1, 12, 1, 15))],
            )]
        };
        let err = apply_structural(
            &mut vfs,
            &EditOp::SetReturnType { node_id: "a.rs#foo".into(), ty: "u8".into() },
            &with_ret,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already has a return type"), "got: {err}");
    }

    #[test]
    fn replace_text_within_node_unique_guard() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export function add() {\n  return 1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_node("a.ts", "add", 1, 3)];
        apply_structural(
            &mut vfs,
            &EditOp::ReplaceText { node_id: "a.ts#add".into(), old_text: "return 1".into(), new_text: "return 42".into() },
            &structure_of,
        )
        .unwrap();
        assert!(vfs.read(Path::new("a.ts")).unwrap().contains("return 42"));
    }
}
