//! ci-edit — atomic, gated edits. Resolves targets to SCIP node ranges, applies
//! them to a VFS overlay, then gates with an LSP **baseline-diff** (fail only on
//! NEWLY introduced diagnostics) before committing — else rolls back by dropping
//! the VFS. Rename goes through LSP (all references); structural edits use the
//! node's `enclosing_range`. No AST: `set_body` and the fine verbs are refused at
//! Symbol granularity (use `replace_node`).
use ci_core::{CommitResult, Diag, EditOp, EditOpts, Error, Node, Range, Result};
use ci_lsp::LspClient;
use ci_vfs::Vfs;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

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
}

/// The generic LSP engine: any language with a language server.
impl GateEngine for LspClient {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        LspClient::diagnostics(self, files)
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
                    let m = e.to_string().to_lowercase();
                    let transient = attempt < 7
                        && (m.contains("-32602")
                            || m.contains("-32801")
                            || m.contains("references")
                            || m.contains("content modified")
                            || m.contains("not ready")
                            || m.contains("loading")
                            || m.contains("waiting"));
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
        self.request("workspace/willRenameFiles", params)
    }
}

/// Structured rich action payload (the MCP/wrapper input shape): `{action, target, name, value}`.
#[derive(Debug, Clone)]
pub struct Action {
    pub path: String,
    pub action: String,
    pub target: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
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
    let value = || a.value.clone().ok_or_else(|| Error::Other(format!("{} needs a value", a.action)));
    Ok(match a.action.as_str() {
        "rename" => EditOp::Rename { node_id: node()?, new_name: value()? },
        "replace" | "replace_node" => EditOp::ReplaceNode { node_id: node()?, code: value()? },
        "insert_before" => EditOp::InsertBefore { node_id: node()?, code: value()? },
        "create_file" => EditOp::CreateFile { path: a.path.clone().into(), code: value()? },
        "move_file" => EditOp::MoveFile { from: a.path.clone().into(), to: value()?.into() },
        "delete_file" => EditOp::DeleteFile { path: a.path.clone().into() },
        other => return Err(Error::Driver(format!("unsupported action: {other}"))),
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

fn file_of(node_id: &str) -> &str {
    node_id.split('#').next().unwrap_or(node_id)
}

fn op_node_id(op: &EditOp) -> Option<&str> {
    match op {
        EditOp::ReplaceNode { node_id, .. }
        | EditOp::InsertBefore { node_id, .. }
        | EditOp::ReplaceText { node_id, .. }
        | EditOp::Rename { node_id, .. } => Some(node_id),
        _ => None,
    }
}

/// Attribute a diagnostic to the op that introduced it: the node-targeted op in the
/// same file whose node starts nearest above the diagnostic's line. Makes a
/// rejection actionable — the agent re-emits just that op (scoped repair) instead
/// of the whole batch.
fn anchor(diag: &Diag, ops: &[EditOp], structure_of: &impl Fn(&str) -> Vec<Node>) -> Option<(usize, String)> {
    let mut candidates: Vec<(usize, String, u32)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let Some(node_id) = op_node_id(op) else { continue };
        if file_of(node_id) != diag.file {
            continue;
        }
        let nodes = structure_of(file_of(node_id));
        if let Some(node) = find(&nodes, node_id) {
            if node.range.start_line <= diag.line {
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
            let file = file_of(node_id);
            let nodes = structure_of(file);
            let node = find(&nodes, node_id).ok_or_else(|| Error::Anchor(node_id.clone()))?;
            vfs.replace_range(Path::new(file), &node.range, code)
        }
        EditOp::InsertBefore { node_id, code } => {
            let file = file_of(node_id);
            let nodes = structure_of(file);
            let node = find(&nodes, node_id).ok_or_else(|| Error::Anchor(node_id.clone()))?;
            vfs.insert_before(Path::new(file), &node.range, &format!("{code}\n\n"))
        }
        EditOp::ReplaceText { node_id, old_text, new_text } => {
            let file = file_of(node_id);
            let nodes = structure_of(file);
            let node = find(&nodes, node_id).ok_or_else(|| Error::Anchor(node_id.clone()))?;
            let text = vfs
                .read_range(Path::new(file), &node.range)
                .ok_or_else(|| Error::Other("node text unavailable".into()))?;
            match text.matches(old_text.as_str()).count() {
                0 => return Err(Error::Other("REPLACE_TEXT: oldText not found in node".into())),
                1 => {}
                _ => return Err(Error::Other("REPLACE_TEXT: oldText not unique in node".into())),
            }
            vfs.replace_range(Path::new(file), &node.range, &text.replacen(old_text, new_text, 1))
        }
        EditOp::CreateFile { path, code } => vfs.create(path, code.clone()),
        EditOp::SetBody { .. } => {
            Err(Error::Driver("SET_BODY needs AST granularity; use replace_node".into()))
        }
        EditOp::MoveFile { .. } | EditOp::DeleteFile { .. } => {
            Err(Error::Driver("file ops (move/delete) land in P3".into()))
        }
        EditOp::Rename { .. } => Err(Error::Driver("rename must go through apply_rename".into())),
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
        edits.sort_by(|a, b| edit_start(b).cmp(&edit_start(a)));
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

    for (i, op) in ops.iter().enumerate() {
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

    // Baseline = disk state of every affected file (disk is untouched until commit).
    let baseline_files: Vec<(String, String)> = affected
        .iter()
        .filter_map(|rel| std::fs::read_to_string(root.join(rel)).ok().map(|c| (rel.clone(), c)))
        .collect();
    let baseline = engine.diagnostics(&baseline_files)?;
    let baseline_keys: HashSet<String> = baseline.iter().map(diag_key).collect();

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
    let new: Vec<&Diag> = after.iter().filter(|d| !baseline_keys.contains(&diag_key(d))).collect();

    if !new.is_empty() {
        // Anchor each new diagnostic to the op that introduced it (scoped repair).
        let mut anchored_op: i64 = -1;
        let feedback = new
            .iter()
            .map(|d| match anchor(d, ops, structure_of) {
                Some((i, node_id)) => {
                    if anchored_op < 0 {
                        anchored_op = i as i64;
                    }
                    format!("op #{i} ({node_id}) -> {}:{} TS{} {}", d.file, d.line, d.code, d.message)
                }
                None => format!("{}:{} TS{} {}", d.file, d.line, d.code, d.message),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(CommitResult::Rejected { failed_op_index: anchored_op, feedback });
    }

    if opts.write && !opts.dry_run {
        vfs.commit()?;
    }
    Ok(CommitResult::Ok { applied_ops: ops.len(), changed_files: changed, repair_rounds: 0 })
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

    #[test]
    fn action_maps_to_ops_and_refuses_set_body() {
        let resolve = |_p: &str, _t: Option<&str>, n: Option<&str>| n.map(|n| format!("a.ts#{n}"));
        let rn = action_to_op(
            &Action { path: "a.ts".into(), action: "rename".into(), target: Some("function".into()), name: Some("add".into()), value: Some("sum".into()) },
            &resolve,
        )
        .unwrap();
        assert!(matches!(rn, EditOp::Rename { .. }));

        let sb = action_to_op(
            &Action { path: "a.ts".into(), action: "set_body".into(), target: Some("function".into()), name: Some("add".into()), value: Some("x".into()) },
            &resolve,
        );
        assert!(sb.is_err(), "set_body refused at Symbol granularity");
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
            &Action { path: "a.ts".into(), action: "move_file".into(), target: None, name: None, value: Some("b/a.ts".into()) },
            &resolve,
        )
        .unwrap();
        assert!(matches!(mv, EditOp::MoveFile { .. }));
        let del = action_to_op(
            &Action { path: "a.ts".into(), action: "delete_file".into(), target: None, name: None, value: None },
            &resolve,
        )
        .unwrap();
        assert!(matches!(del, EditOp::DeleteFile { .. }));
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

    // Real move via LSP willRenameFiles. #[ignore]; run with `--ignored`.
    #[test]
    #[ignore]
    fn move_file_rewrites_importers() {
        use ci_lsp::LspClient;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#).unwrap();
        fs::create_dir_all(root.join("src/util")).unwrap();
        fs::write(root.join("src/math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n").unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from \"./math.js\";\nexport const r = add(1, 2);\n").unwrap();

        let mut cmd = std::process::Command::new("npx");
        cmd.args(["--yes", "-p", "typescript-language-server", "-p", "typescript", "typescript-language-server", "--stdio"])
            .env("npm_config_cache", std::env::var("CI_NPM_CACHE").unwrap_or_else(|_| "/tmp/ci-npm-cache".into()));
        let mut lsp = LspClient::start(root, cmd).expect("start tsls");
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
        use ci_lsp::LspClient;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let original = "export function add(): number {\n  return 1;\n}\n";
        fs::write(root.join("src/a.ts"), original).unwrap();

        let structure_of = |_f: &str| vec![fn_node("src/a.ts", "add", 1, 3)];
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let mut cmd = std::process::Command::new("npx");
        cmd.args(["--yes", "-p", "typescript-language-server", "-p", "typescript", "typescript-language-server", "--stdio"])
            .env("npm_config_cache", std::env::var("CI_NPM_CACHE").unwrap_or_else(|_| "/tmp/ci-npm-cache".into()));
        let mut lsp = LspClient::start(root, cmd).expect("start tsls");
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
