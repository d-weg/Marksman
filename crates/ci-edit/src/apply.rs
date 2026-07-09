//! Per-op apply handlers: each [`EditOp`] resolves its target against the structure
//! tree and stages the edit on the VFS overlay — nothing reaches disk until
//! `commit_edits` gates and commits the whole batch.
use ci_core::{EditOp, Error, Node, Range, Result};
use ci_vfs::Vfs;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use crate::{file_of, find, GateEngine};

/// Resolve a node id to its structure node (owned), or an `Anchor` error naming the id. The
/// structure tree is rebuilt per call (`structure_of`), so the returned `Node` is cloned out.
fn node_by_id(node_id: &str, structure_of: &impl Fn(&str) -> Vec<Node>) -> Result<Node> {
    let nodes = structure_of(file_of(node_id));
    find(&nodes, node_id).cloned().ok_or_else(|| Error::Anchor(node_id.to_string()))
}

/// The name declared by a snippet's LEADING declaration line — best-effort, cross-language
/// (declaration keywords share a shape: modifiers, keyword, name). Used by `add_symbol` for
/// the exists-check and by the MCP layer for the post-edit echo id; `None` just means "no
/// pre-check" (the gate still judges the result), so unsure is safe.
pub fn leading_symbol_name(snippet: &str) -> Option<String> {
    let line = snippet.lines().find(|l| {
        let t = l.trim();
        !(t.is_empty()
            || t.starts_with("//")
            || t.starts_with("/*")
            || t.starts_with('*')
            || t.starts_with("#[")   // Rust attribute (#[test], #[derive(..)])
            || t.starts_with('@'))   // TS/Java decorator, Python decorator
    })?;
    let mut toks = line.split_whitespace();
    let kw = loop {
        let t = toks.next()?;
        match t {
            "pub" | "export" | "default" | "async" | "unsafe" | "abstract" | "declare" | "static"
            | "final" | "public" | "private" | "protected" => continue,
            t if t.starts_with("pub(") => continue, // pub(crate) / pub(super)
            t => break t,
        }
    };
    match kw {
        "fn" | "function" | "struct" | "enum" | "trait" | "interface" | "type" | "class" | "const"
        | "let" | "var" | "def" | "mod" | "union" => {}
        _ => return None, // impl blocks, expressions, anything we can't name — no pre-check
    }
    let raw = toks.next()?;
    let name: String = raw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    (!name.is_empty()).then_some(name)
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
        EditOp::ReplaceInFile { path, old_text, new_text } => {
            // File-scoped: the escape hatch for text OUTSIDE every symbol anchor (imports,
            // `mod` declarations). Uniqueness in the whole file is the addressing; the gate
            // still verifies the result like any other op.
            let rel = path.to_string_lossy().replace('\\', "/");
            let content = vfs
                .read(Path::new(&rel))
                .ok_or_else(|| Error::Other(format!("replace_in_file: {rel} does not exist")))?;
            match content.matches(old_text.as_str()).count() {
                1 => {
                    vfs.write(Path::new(&rel), content.replacen(old_text.as_str(), new_text, 1));
                    Ok(())
                }
                0 if !new_text.is_empty() && content.contains(new_text.as_str()) => {
                    // The batch already produced this op's end state (a move's own rewrite
                    // covered it). Same-intent redundancy is SATISFIED, not an error — agents
                    // pair moves with helper edits, and rejecting the pair over our own
                    // automation cost whole bench runs (move-rust rounds 3 and 4).
                    Ok(())
                }
                0 => Err(Error::Other(format!(
                    "replace_in_file: oldText not found in {rel} — it must match the file's current text exactly"
                ))),
                n => Err(Error::Other(format!(
                    "replace_in_file: oldText occurs {n} times in {rel} — extend it until it is unique"
                ))),
            }
        }
        EditOp::ReplaceText { node_id, old_text, new_text } => {
            let file = file_of(node_id);
            let node = node_by_id(node_id, structure_of)?;
            let text = vfs.read_range(Path::new(file), &node.range).ok_or_else(|| {
                Error::Other(format!(
                    "cannot read the text of '{node_id}' (its file is missing or was deleted/moved                      earlier in this batch) — re-target the op at the file's NEW path, or drop it                      if a rename/move already covers this edit"
                ))
            })?;
            match text.matches(old_text.as_str()).count() {
                0 if !new_text.is_empty() && text.contains(new_text.as_str()) => {
                    // End state already present (a rename/move's own rewrite got here first) —
                    // satisfied, not a miss.
                    return Ok(());
                }
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
        // Append a NEW top-level symbol at the end of the file. Hygiene is server-side: one
        // blank line after the last item, trailing newline ensured, code trimmed (top-level ⇒
        // zero indent by definition). Already-present code is SATISFIED (idempotent, counted
        // as a redundant op); a same-named top-level symbol with DIFFERENT content refuses
        // with that symbol's current source — mechanically important for UNGATED languages,
        // where a silent redefinition would shadow instead of failing the gate.
        EditOp::AddSymbol { path, code } => {
            let rel = path.to_string_lossy().replace('\\', "/");
            let content = vfs.read(Path::new(&rel)).ok_or_else(|| {
                Error::Other(format!("ADD_SYMBOL: {rel} does not exist — use create_file for a new file"))
            })?;
            let snippet = code.trim();
            if snippet.is_empty() {
                return Err(Error::Other("ADD_SYMBOL: `value` is empty — send the complete new declaration".into()));
            }
            if content.contains(snippet) {
                return Ok(()); // end state already present — satisfied, not an error
            }
            if let Some(name) = leading_symbol_name(snippet) {
                let nodes = structure_of(&rel);
                if let Some(n) = nodes.iter().find(|n| n.name.as_deref() == Some(name.as_str())) {
                    let existing = vfs.read_range(Path::new(&rel), &n.range).unwrap_or_default();
                    return Err(Error::Other(format!(
                        "ADD_SYMBOL: `{name}` already exists in {rel} (L{}-{}) — use replace_node on `{}` to change it, or pick a different name. Its current source:\n{existing}",
                        n.range.start_line, n.range.end_line, n.id
                    )));
                }
            }
            let new_content = if content.trim().is_empty() {
                format!("{snippet}\n")
            } else {
                format!("{}\n\n{snippet}\n", content.trim_end())
            };
            vfs.write(Path::new(&rel), new_content);
            Ok(())
        }
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
            let text = vfs.read_range(Path::new(file), &node.range).ok_or_else(|| {
                Error::Other(format!(
                    "cannot read the text of '{node_id}' (its file is missing or was deleted/moved                      earlier in this batch) — re-target the op at the file's NEW path, or drop it                      if a rename/move already covers this edit"
                ))
            })?;
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
        // PREFIX-typed languages (Java/C/C++: the type sits BEFORE the name) have no legal
        // insertion point after `)` at all — this op refuses with the replace_text recipe
        // instead of splicing garbage (rollout spec, decision Q2; the registry carries the
        // per-language marker, so a new language never means editing this arm).
        EditOp::SetReturnType { node_id, ty } => {
            let file = file_of(node_id);
            if let Some(lang) = ci_build::prefix_return_language(Path::new(file)) {
                let name = node_id.rsplit(['#', '.', ':']).next().unwrap_or(node_id);
                return Err(Error::Other(format!(
                    "SET_RETURN_TYPE: {lang} declares the return type BEFORE the name, so this \
                     op (which inserts after `)`) cannot express it. Use replace_text on \
                     '{node_id}' instead: oldText = the signature fragment holding the current \
                     type (e.g. `int {name}(`), newText = the same fragment with the new type \
                     (e.g. `{ty} {name}(`)."
                )));
            }
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
            vfs.insert_before(Path::new(file), &at, &format!("{}{ty}", ci_build::return_delim(Path::new(file))))
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

/// Apply a rename through the gate engine at the symbol's name position, then apply the
/// returned WorkspaceEdit to the VFS (all references). Engine handles its own warmup.
pub(crate) fn apply_rename(
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
pub fn workspace_edit_is_empty(we: &Value) -> bool {
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
pub(crate) fn apply_move(vfs: &mut Vfs, from: &Path, to: &Path, root: &Path, engine: &mut dyn GateEngine) -> Result<()> {
    let from_rel = from.to_string_lossy().replace('\\', "/");
    let to_rel = to.to_string_lossy().replace('\\', "/");
    let t_wr = std::time::Instant::now();
    if let Ok(we) = engine.will_rename(&from_rel, &to_rel) {
        if std::env::var("CI_TIMING").is_ok() {
            eprintln!("[timing]   will_rename() {:?}", t_wr.elapsed());
        }
        if std::env::var("CI_LSP_DEBUG").is_ok() {
            eprintln!("willRename -> {we}");
        }
        let _ = apply_workspace_edit(vfs, root, &we);
    }
    vfs.move_file(from, to)
}

/// An import/use/mod/require line referencing module token `stem` — the shape those
/// statements share across languages. Best-effort by design: the gate (deleted-reference
/// diagnostics + the compiler over the radius) is the net behind this fast-path guard.
fn is_import_line_for(line: &str, stem: &str) -> bool {
    let t = line.trim();
    (t.starts_with("use ") || t.starts_with("pub use ") || t.starts_with("import ")
        || t.starts_with("from ") || t.starts_with("mod ") || t.starts_with("pub mod ")
        || t.contains("require("))
        && t.contains(stem)
}

/// Delete a file — refused (statically, via the SCIP reverse import graph) if
/// anything still imports it. The refusal is SELF-SUFFICIENT (the §5 law): each importer's
/// referencing line is shown with a ready-to-copy removal fix, so clearing the way is one
/// re-issued batch (fixes first, delete LAST) instead of a read-and-hunt per importer.
pub fn apply_delete(
    vfs: &mut Vfs,
    path: &Path,
    reverse_imports: &impl Fn(&str) -> Vec<String>,
) -> Result<()> {
    let rel = path.to_string_lossy().replace('\\', "/");
    let stem = Path::new(&rel).file_stem().and_then(|s| s.to_str()).unwrap_or(&rel).to_string();
    // The graph is PRE-BATCH truth; the refusal's own recipe is "fixes + delete LAST in one
    // batch", so an importer this batch already edited is judged by its STAGED content: no
    // referencing line left ⇒ cleared. Untouched importers stay blocking even when no line
    // matches (the stem heuristic is best-effort; unmatched means "inspect", not "clean").
    let importers: Vec<String> = reverse_imports(&rel)
        .into_iter()
        .filter(|imp| {
            if !vfs.is_staged(Path::new(imp)) {
                return true;
            }
            match vfs.read(Path::new(imp)) {
                Some(content) => content.lines().any(|l| is_import_line_for(l, &stem)),
                None => false, // importer itself deleted earlier in this batch
            }
        })
        .collect();
    if !importers.is_empty() {
        // The referencing lines, each with a ready-to-copy removal fix. An importer with no
        // matched line is still named (the agent inspects that one).
        let mut lines_out: Vec<String> = Vec::new();
        for imp in importers.iter().take(10) {
            let mut found = false;
            if let Some(content) = vfs.read(Path::new(imp)) {
                for (i, line) in content.lines().enumerate() {
                    if is_import_line_for(line, &stem) {
                        found = true;
                        lines_out.push(format!("  {imp}:{}: {}", i + 1, line.trim()));
                        lines_out.push(format!(
                            "    fix (ready to copy): {}",
                            serde_json::json!({"action":"replace_text","path":imp,"oldText":line.trim_end(),"newText":""})
                        ));
                    }
                }
            }
            if !found {
                lines_out.push(format!("  {imp}: (references it — no single import line matched; inspect this one)"));
            }
        }
        return Err(Error::Driver(format!(
            "DELETE_FILE refused: {rel} is still imported by {} file(s). Remove the references first — \
             re-issue ONE batch with each `fix` VERBATIM plus the delete_file LAST:\n{}",
            importers.len(),
            lines_out.join("\n")
        )));
    }
    vfs.delete(path);
    Ok(())
}

pub(crate) fn apply_workspace_edit(vfs: &mut Vfs, root: &Path, we: &Value) -> Result<()> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(dc) = we.get("documentChanges").and_then(Value::as_array) {
        for d in dc {
            // LSP resource operations: `documentChanges` may mix CreateFile ops with text
            // edits (ordered — a created file's content edit follows its create op).
            if d.get("kind").and_then(Value::as_str) == Some("create") {
                if let Some(uri) = d.get("uri").and_then(Value::as_str) {
                    let rel = uri_to_rel(uri, root)
                        .ok_or_else(|| Error::Other(format!("create uri outside root: {uri}")))?;
                    vfs.create(Path::new(&rel), String::new())?;
                }
                continue;
            }
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

    // A server may report the SAME file under two URI spellings (sourcekit-lsp emits both the
    // symlinked `/var/...` and the canonical `/private/var/...` for a macOS tempdir, each carrying
    // an IDENTICAL edit set). Both map to one rel — applying both would splice the edit twice and
    // corrupt the file. Deduplicate by rel, keeping the first occurrence (they are identical).
    let mut seen_rel: HashSet<String> = HashSet::new();
    for (uri, mut edits) in groups {
        let rel = uri_to_rel(&uri, root).ok_or_else(|| Error::Other(format!("uri outside root: {uri}")))?;
        if !seen_rel.insert(rel.clone()) {
            continue; // this file's edits already applied under its other URI spelling
        }
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
    if let Some(rel) = uri.strip_prefix(&prefix) {
        return Some(rel.to_string());
    }
    // A server may return the CANONICAL path (macOS resolves a `/var/...` tempdir to
    // `/private/var/...`; sourcekit-lsp does this) while `root` is the symlinked form — try the
    // canonicalized root before giving up, so a rename's edits aren't rejected as "outside root".
    let canon = std::fs::canonicalize(root).ok()?;
    let canon_prefix = format!("file://{}/", canon.to_string_lossy());
    uri.strip_prefix(&canon_prefix).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fn_node;
    use ci_core::{NodeKind, SymbolKind};
    use std::fs;

    // The delete refusal must be self-sufficient: each importer's referencing line + a
    // ready-to-copy removal fix, so clearing the way is ONE re-issued batch.
    #[test]
    fn delete_refusal_carries_copyable_removal_fixes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/gone.rs"), "pub fn g() {}\n").unwrap();
        std::fs::write(root.join("src/user.rs"), "use crate::gone::g;\npub fn u() { g() }\n").unwrap();
        let mut vfs = Vfs::new(root);
        let rev = |f: &str| if f == "src/gone.rs" { vec!["src/user.rs".to_string()] } else { vec![] };
        let err = apply_delete(&mut vfs, Path::new("src/gone.rs"), &rev).unwrap_err().to_string();
        assert!(err.contains("src/user.rs:1: use crate::gone::g;"), "line shown: {err}");
        assert!(err.contains("fix (ready to copy)") && err.contains("\"oldText\":\"use crate::gone::g;\""), "fix carried: {err}");
        assert!(err.contains("delete_file LAST"), "batch guidance: {err}");
    }

    // The refusal's recipe must actually work: an importer whose referencing line was removed
    // by an EARLIER op in the same batch is cleared (judged by staged content, not the
    // pre-batch graph) — while an importer the batch never touched stays blocking, even when
    // no line matches the stem heuristic.
    #[test]
    fn delete_clears_when_the_batch_already_removed_the_references() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/gone.rs"), "pub fn g() {}\n").unwrap();
        std::fs::write(root.join("src/user.rs"), "use crate::gone::g;\npub fn u() { g() }\n").unwrap();
        let rev = |f: &str| if f == "src/gone.rs" { vec!["src/user.rs".to_string()] } else { vec![] };

        // untouched importer -> still refused
        let mut vfs = Vfs::new(root);
        assert!(apply_delete(&mut vfs, Path::new("src/gone.rs"), &rev).is_err());

        // the same batch staged the fix -> delete allowed
        let mut vfs = Vfs::new(root);
        vfs.write(Path::new("src/user.rs"), "pub fn u() {}\n".into());
        assert!(apply_delete(&mut vfs, Path::new("src/gone.rs"), &rev).is_ok());

        // importer deleted earlier in the batch -> delete allowed
        let mut vfs = Vfs::new(root);
        vfs.delete(Path::new("src/user.rs"));
        assert!(apply_delete(&mut vfs, Path::new("src/gone.rs"), &rev).is_ok());

        // staged edit that KEEPS the reference -> still refused
        let mut vfs = Vfs::new(root);
        vfs.write(Path::new("src/user.rs"), "use crate::gone::g;\n".into());
        assert!(apply_delete(&mut vfs, Path::new("src/gone.rs"), &rev).is_err());
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
    fn add_symbol_appends_with_hygiene() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No trailing newline on the last item — hygiene must still yield one blank line + \n.
        fs::write(root.join("a.rs"), "fn foo() {\n    1;\n}").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_node("a.rs", "foo", 1, 3)];
        apply_structural(
            &mut vfs,
            &EditOp::AddSymbol { path: "a.rs".into(), code: "\nfn bar() {\n    2;\n}\n\n".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("a.rs")).unwrap(), "fn foo() {\n    1;\n}\n\nfn bar() {\n    2;\n}\n");

        // Empty file: no leading blank lines, just the symbol.
        fs::write(root.join("b.rs"), "\n\n").unwrap();
        apply_structural(
            &mut vfs,
            &EditOp::AddSymbol { path: "b.rs".into(), code: "fn solo() {}".into() },
            &structure_of,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("b.rs")).unwrap(), "fn solo() {}\n");

        // Missing file: soft error pointing at create_file.
        let err = apply_structural(
            &mut vfs,
            &EditOp::AddSymbol { path: "nope.rs".into(), code: "fn x() {}".into() },
            &structure_of,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("create_file"), "points at create_file for new files: {err}");
    }

    #[test]
    fn add_symbol_is_satisfied_when_code_already_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {\n    1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_node("a.rs", "foo", 1, 3)];
        apply_structural(
            &mut vfs,
            &EditOp::AddSymbol { path: "a.rs".into(), code: "fn foo() {\n    1;\n}".into() },
            &structure_of,
        )
        .unwrap();
        // Satisfied ⇒ nothing staged (commit_edits counts this as a redundant op).
        assert!(vfs.is_empty(), "identical code must be a no-op, not a duplicate append");
    }

    // A same-named top-level symbol with DIFFERENT content refuses with the existing source —
    // mechanically important for UNGATED languages, where appending a redefinition is legal
    // syntax and would silently shadow the original.
    #[test]
    fn add_symbol_refuses_same_name_different_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "pub fn foo() {\n    1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| vec![fn_node("a.rs", "foo", 1, 3)];
        let err = apply_structural(
            &mut vfs,
            &EditOp::AddSymbol { path: "a.rs".into(), code: "pub fn foo() {\n    2;\n}".into() },
            &structure_of,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already exists") && err.contains("replace_node"), "redirects to replace_node: {err}");
        assert!(err.contains("pub fn foo() {"), "shows the existing source: {err}");
        assert!(vfs.is_empty(), "refusal stages nothing");
    }

    #[test]
    fn leading_symbol_name_covers_the_declaration_shapes() {
        // Rust: modifiers, attributes above.
        assert_eq!(leading_symbol_name("pub fn parse(x: u8) -> u8 { x }").as_deref(), Some("parse"));
        assert_eq!(leading_symbol_name("#[test]\nfn roundtrip_holds() {\n}").as_deref(), Some("roundtrip_holds"));
        assert_eq!(leading_symbol_name("pub(crate) struct Widget {\n    n: i32,\n}").as_deref(), Some("Widget"));
        // TS/JS: export chains, generics glued to the name.
        assert_eq!(leading_symbol_name("export function slug(s: string): string { return s; }").as_deref(), Some("slug"));
        assert_eq!(leading_symbol_name("export default class Loader<T> {\n}").as_deref(), Some("Loader"));
        assert_eq!(leading_symbol_name("export const LIMIT = 10;").as_deref(), Some("LIMIT"));
        // Python: decorator above, name glued to `(`.
        assert_eq!(leading_symbol_name("@cached\ndef fetch_all(db):\n    pass").as_deref(), Some("fetch_all"));
        // Unnameable shapes -> None (no pre-check; the gate judges the result).
        assert_eq!(leading_symbol_name("impl Widget {\n    fn n(&self) {}\n}"), None);
        assert_eq!(leading_symbol_name("x + 1"), None);
        assert_eq!(leading_symbol_name(""), None);
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

        // Python rides the arrow family: `-> T` after `)` (delimiter resolved via the registry).
        fs::write(root.join("c.py"), "def baz():\n    pass\n").unwrap();
        let py_struct = |_f: &str| vec![fn_with("c.py#baz", rng(1, 0, 2, 8), vec![sub("c.py#baz:params", rng(1, 7, 1, 9))])];
        apply_structural(
            &mut vfs,
            &EditOp::SetReturnType { node_id: "c.py#baz".into(), ty: "int".into() },
            &py_struct,
        )
        .unwrap();
        assert_eq!(vfs.read(Path::new("c.py")).unwrap(), "def baz() -> int:\n    pass\n");

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

    // Q2 (rollout spec): prefix-typed languages refuse `set_return_type` with a SELF-SUFFICIENT
    // recipe — the error alone must let the agent re-issue the edit as replace_text (op named,
    // node named, example fragments carrying the requested type), with nothing written.
    #[test]
    fn set_return_type_refuses_with_recipe_on_prefix_typed_languages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Svc.java"), "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        let structure_of = |_f: &str| {
            vec![fn_with(
                "Svc.java#Svc.probe",
                rng(2, 2, 4, 3),
                vec![sub("Svc.java#Svc.probe:params", rng(2, 18, 2, 30))],
            )]
        };
        let err = apply_structural(
            &mut vfs,
            &EditOp::SetReturnType { node_id: "Svc.java#Svc.probe".into(), ty: "String".into() },
            &structure_of,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("java declares the return type BEFORE the name"), "names the language + why: {msg}");
        assert!(msg.contains("replace_text"), "steers to the op that works: {msg}");
        assert!(msg.contains("Svc.java#Svc.probe"), "recipe is anchored to the node: {msg}");
        assert!(msg.contains("`String probe(`"), "example carries the requested type + symbol name: {msg}");
        assert!(vfs.read(Path::new("Svc.java")).unwrap().contains("public int probe"), "nothing written");
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

    // A WorkspaceEdit whose `changes` names ONE file under two URI spellings (sourcekit-lsp emits
    // both the symlinked `/var/...` and the canonical `/private/var/...` for a macOS tempdir, each
    // with an IDENTICAL edit) must apply the edit ONCE — applying both splices twice and corrupts
    // the file. Also pins that the canonical-path spelling still resolves to the repo-relative key.
    #[test]
    fn workspace_edit_dedups_duplicate_uri_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Util.swift"), "func base() -> Int { return 1 }\n").unwrap();
        let mut vfs = Vfs::new(root);
        // Both spellings of the same file; one canonicalized (the `/private` prefix stripped so it
        // does NOT match `root` directly, exercising the canonical fallback in uri_to_rel).
        let raw = format!("file://{}/Util.swift", root.to_string_lossy());
        let canon = format!("file://{}/Util.swift", std::fs::canonicalize(root).unwrap().to_string_lossy());
        let edit = serde_json::json!({
            "range": {"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 9}},
            "newText": "fetchBase"
        });
        let we = serde_json::json!({ "changes": { raw: [edit.clone()], canon: [edit] } });
        apply_workspace_edit(&mut vfs, root, &we).unwrap();
        assert_eq!(
            vfs.read(Path::new("Util.swift")).unwrap(),
            "func fetchBase() -> Int { return 1 }\n",
            "the shared edit applied exactly once despite two URI spellings"
        );
    }
}
