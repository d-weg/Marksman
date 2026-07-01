//! Structured non-code edits: change a value in a TOML file by dotted key, or a Markdown section
//! by heading path — **format-preserving** (only the addressed value/section changes; everything
//! else stays byte-for-byte) and **ungated** (these files aren't type-checked). Returning the whole
//! new file content keeps the caller trivial: the VFS just overwrites, and the atomic-batch /
//! rollback machinery in [`commit_edits`](crate::commit_edits) treats it like any other edit — so a
//! `Cargo.toml` dep can land in the same transaction as the `use` that needs it.
//!
//! JSON keeps its formatting via a small span scanner (no dependency; `serde_json` validates the
//! result so a splice can never write corrupt JSON). YAML is edited line-by-line and handles block
//! mappings only (the common config shape) — flow style / anchors are out of scope, best-effort.
use ci_core::{Error, Result};

/// A structured file format editable by structural path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Markdown,
    Json,
    Yaml,
}

/// The editable structured format for a path by extension, or `None` for anything else (including
/// code — those go through a `LanguageProvider`, not here).
pub fn format_of(path: &str) -> Option<Format> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".toml") {
        Some(Format::Toml)
    } else if p.ends_with(".md") || p.ends_with(".markdown") {
        Some(Format::Markdown)
    } else if p.ends_with(".json") {
        Some(Format::Json)
    } else if p.ends_with(".yaml") || p.ends_with(".yml") {
        Some(Format::Yaml)
    } else {
        None
    }
}

/// True for a file this module edits — used by the gate to skip type-checking it (ungated).
pub fn is_structured(path: &str) -> bool {
    format_of(path).is_some()
}

/// Set `key` to `value` in `content`, returning the whole new file (format-preserving). For TOML,
/// `key` is a dotted path (`dependencies.serde`) and `value` a TOML value expression (`"1.0"`,
/// `{ version = "1", features = ["derive"] }`); intermediate tables are created as needed. For
/// Markdown, `key` is a `/`-separated heading path and `value` the new section body.
pub fn set_key(content: &str, fmt: Format, key: &str, value: &str) -> Result<String> {
    match fmt {
        Format::Toml => toml_set(content, key, value),
        Format::Markdown => md_set_section(content, key, value),
        Format::Json => json_set(content, key, value),
        Format::Yaml => yaml_set(content, key, value),
    }
}

/// Delete `key` (a dotted TOML path or a Markdown heading path) from `content`, returning the whole
/// new file (format-preserving).
pub fn delete_key(content: &str, fmt: Format, key: &str) -> Result<String> {
    match fmt {
        Format::Toml => toml_delete(content, key),
        Format::Markdown => md_delete_section(content, key),
        Format::Json => json_delete(content, key),
        Format::Yaml => yaml_delete(content, key),
    }
}

// ── TOML (format-preserving via toml_edit) ──────────────────────────────────

fn toml_set(content: &str, key: &str, value: &str) -> Result<String> {
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Other(format!("SET_KEY: TOML parse failed: {e}")))?;
    let val: toml_edit::Value = value
        .parse()
        .map_err(|e| Error::Other(format!("SET_KEY: {value:?} is not a valid TOML value: {e}")))?;
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(Error::Other("SET_KEY: empty key".into()));
    }
    let mut table = doc.as_table_mut();
    for p in &parts[..parts.len() - 1] {
        let entry = table.entry(p).or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| Error::Other(format!("SET_KEY: {p:?} in {key:?} is not a table")))?;
    }
    let last = parts[parts.len() - 1];
    // Preserve the existing value's decor (its surrounding whitespace + trailing comment) when
    // updating in place, so `serde = "1.0"  # pinned` keeps its comment.
    let mut val = val;
    if let Some(existing) = table.get(last).and_then(|i| i.as_value()) {
        *val.decor_mut() = existing.decor().clone();
    }
    table.insert(last, toml_edit::Item::Value(val));
    Ok(doc.to_string())
}

fn toml_delete(content: &str, key: &str) -> Result<String> {
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Other(format!("DELETE_KEY: TOML parse failed: {e}")))?;
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(Error::Other("DELETE_KEY: empty key".into()));
    }
    let mut table = doc.as_table_mut();
    for p in &parts[..parts.len() - 1] {
        table = table
            .get_mut(p)
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| Error::Other(format!("DELETE_KEY: {key:?} not found (no table {p:?})")))?;
    }
    if table.remove(parts[parts.len() - 1]).is_none() {
        return Err(Error::Other(format!("DELETE_KEY: {key:?} not found")));
    }
    Ok(doc.to_string())
}

// ── Markdown (heading-path section replace, line-based) ──────────────────────

/// The ATX heading level (`#` count, 1–6) and trimmed title of a line, if it is a heading.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        if rest.starts_with([' ', '\t']) {
            return Some((hashes, rest.trim()));
        }
    }
    None
}

/// Locate the heading line for a `/`-separated path, descending into deeper headings within each
/// matched section. Returns `(heading_index, level)`.
fn find_heading(lines: &[&str], key: &str) -> Result<(usize, usize)> {
    let segments: Vec<&str> = key.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(Error::Other("Markdown key (heading path) is empty".into()));
    }
    let mut lo = 0usize;
    let mut hi = lines.len();
    let mut parent_level = 0usize;
    let mut found = (0usize, 0usize);
    for seg in &segments {
        let mut hit = None;
        let mut i = lo;
        while i < hi {
            if let Some((lvl, title)) = heading(lines[i]) {
                // Leaving the parent section (a heading at/above the parent) ends the search scope.
                if lvl <= parent_level {
                    break;
                }
                if title == *seg {
                    hit = Some((i, lvl));
                    break;
                }
            }
            i += 1;
        }
        let (idx, lvl) = hit.ok_or_else(|| Error::Other(format!("heading {seg:?} not found (in path {key:?})")))?;
        // Narrow the scope to this heading's own section for the next segment.
        lo = idx + 1;
        hi = section_end(lines, idx, lvl);
        parent_level = lvl;
        found = (idx, lvl);
    }
    Ok(found)
}

/// The index one past the last line of the section headed at `idx` (level `lvl`): the next heading
/// at level ≤ `lvl`, or end of file.
fn section_end(lines: &[&str], idx: usize, lvl: usize) -> usize {
    for (j, line) in lines.iter().enumerate().skip(idx + 1) {
        if let Some((l, _)) = heading(line) {
            if l <= lvl {
                return j;
            }
        }
    }
    lines.len()
}

fn md_set_section(content: &str, key: &str, body: &str) -> Result<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let (idx, lvl) = find_heading(&lines, key)?;
    let end = section_end(&lines, idx, lvl);
    // Replace the body (everything after the heading line up to the next same/higher heading),
    // keeping the heading line itself and the rest of the document untouched.
    let mut out: Vec<String> = lines[..=idx].iter().map(|s| s.to_string()).collect();
    for l in body.split('\n') {
        out.push(l.to_string());
    }
    for l in &lines[end..] {
        out.push(l.to_string());
    }
    Ok(out.join("\n"))
}

fn md_delete_section(content: &str, key: &str) -> Result<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let (idx, lvl) = find_heading(&lines, key)?;
    let end = section_end(&lines, idx, lvl);
    // Drop the heading line and its whole section.
    let mut out: Vec<String> = lines[..idx].iter().map(|s| s.to_string()).collect();
    out.extend(lines[end..].iter().map(|s| s.to_string()));
    Ok(out.join("\n"))
}

// ── JSON (span-based, format-preserving; hand-rolled so we add no dependency) ─

/// One `"key": value` member located inside an object, with byte spans into the source.
struct Member {
    key: String,
    key_start: usize,
    val_start: usize,
    val_end: usize,
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// From the opening quote at `i`, the index just past the closing quote (honoring `\` escapes).
fn scan_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// From the start of a JSON value at `i`, the index just past it.
fn scan_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => scan_string(b, i),
        b'{' => scan_bracketed(b, i, b'{', b'}'),
        b'[' => scan_bracketed(b, i, b'[', b']'),
        _ => {
            // number / true / false / null — up to the next structural delimiter.
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// From the opening bracket at `i`, the index just past the matching close (nested-aware, skipping
/// strings so a bracket inside a string doesn't miscount).
fn scan_bracketed(b: &[u8], i: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        let c = b[j];
        if c == b'"' {
            j = scan_string(b, j)?;
        } else if c == open {
            depth += 1;
            j += 1;
        } else if c == close {
            depth -= 1;
            j += 1;
            if depth == 0 {
                return Some(j);
            }
        } else {
            j += 1;
        }
    }
    None
}

/// The members of the object whose `{` is at `obj_open`, in source order.
fn json_members(b: &[u8], obj_open: usize) -> Vec<Member> {
    let mut out = Vec::new();
    let mut i = obj_open + 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) != Some(&b'"') {
            break; // `}` or malformed
        }
        let key_start = i;
        let Some(key_end) = scan_string(b, i) else { break };
        let key = String::from_utf8_lossy(&b[key_start + 1..key_end - 1]).into_owned();
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            break;
        }
        i = skip_ws(b, i + 1);
        let val_start = i;
        let Some(val_end) = scan_value(b, val_start) else { break };
        out.push(Member { key, key_start, val_start, val_end });
        i = skip_ws(b, val_end);
        if b.get(i) == Some(&b',') {
            i += 1;
        } else {
            break;
        }
    }
    out
}

/// Descend `prefix` object keys from the root, returning the `{` position of the container object
/// that should hold the final key.
fn json_container(b: &[u8], key: &str, prefix: &[&str]) -> Result<usize> {
    let root = skip_ws(b, 0);
    if b.get(root) != Some(&b'{') {
        return Err(Error::Other("SET_KEY/DELETE_KEY: JSON root is not an object".into()));
    }
    let mut obj = root;
    for p in prefix {
        let m = json_members(b, obj)
            .into_iter()
            .find(|m| m.key == *p)
            .ok_or_else(|| Error::Other(format!("{p:?} not found in {key:?}")))?;
        if b.get(m.val_start) != Some(&b'{') {
            return Err(Error::Other(format!("{p:?} in {key:?} is not an object")));
        }
        obj = m.val_start;
    }
    Ok(obj)
}

/// The leading whitespace of the line containing byte `pos`.
fn line_indent_at(content: &str, pos: usize) -> String {
    let start = content[..pos].rfind('\n').map(|n| n + 1).unwrap_or(0);
    content[start..pos].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

fn split_path(key: &str) -> Result<(Vec<&str>, &str)> {
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    match parts.split_last() {
        Some((last, prefix)) => Ok((prefix.to_vec(), last)),
        None => Err(Error::Other("empty key".into())),
    }
}

fn json_set(content: &str, key: &str, value: &str) -> Result<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .map_err(|e| Error::Other(format!("SET_KEY: {value:?} is not a valid JSON value: {e}")))?;
    let b = content.as_bytes();
    let (prefix, last) = split_path(key)?;
    let obj = json_container(b, key, &prefix)?;
    let out = match json_members(b, obj).into_iter().find(|m| m.key == last) {
        Some(m) => format!("{}{}{}", &content[..m.val_start], value, &content[m.val_end..]),
        None => json_insert(content, b, obj, last, value)?,
    };
    // Safety net: never write corrupt JSON (a splice bug rolls back as an error, not a broken file).
    serde_json::from_str::<serde_json::Value>(&out)
        .map_err(|e| Error::Other(format!("SET_KEY produced invalid JSON ({e}); nothing written")))?;
    Ok(out)
}

/// Insert a brand-new `"key": value` member into the object at `obj_open`, matching the existing
/// members' indentation.
fn json_insert(content: &str, b: &[u8], obj_open: usize, key: &str, value: &str) -> Result<String> {
    let entry = format!("\"{key}\": {value}");
    match json_members(b, obj_open).last() {
        Some(last) => {
            let indent = line_indent_at(content, last.key_start);
            Ok(format!("{},\n{indent}{entry}{}", &content[..last.val_end], &content[last.val_end..]))
        }
        None => {
            // Empty object `{}` / `{ }`: open it up onto its own indented line.
            let close = scan_bracketed(b, obj_open, b'{', b'}')
                .ok_or_else(|| Error::Other("SET_KEY: unterminated JSON object".into()))?
                - 1;
            let obj_indent = line_indent_at(content, obj_open);
            Ok(format!("{}\n{obj_indent}  {entry}\n{obj_indent}{}", &content[..close], &content[close..]))
        }
    }
}

fn json_delete(content: &str, key: &str) -> Result<String> {
    let b = content.as_bytes();
    let (prefix, last) = split_path(key)?;
    let obj = json_container(b, key, &prefix)?;
    let m = json_members(b, obj)
        .into_iter()
        .find(|m| m.key == last)
        .ok_or_else(|| Error::Other(format!("DELETE_KEY: {key:?} not found")))?;
    let after = skip_ws(b, m.val_end);
    let out = if b.get(after) == Some(&b',') {
        // A trailing comma: drop the member's own line (or, if compact, just the member) + comma.
        let start = content[..m.key_start].rfind('\n').filter(|&n| n > obj).unwrap_or(obj + 1);
        format!("{}{}", &content[..start], &content[after + 1..])
    } else {
        // Last member: drop the previous member's trailing comma through this value.
        let start = content[obj + 1..m.key_start]
            .rfind(',')
            .map(|r| r + obj + 1)
            .or_else(|| content[..m.key_start].rfind('\n').filter(|&n| n > obj))
            .unwrap_or(obj + 1);
        format!("{}{}", &content[..start], &content[m.val_end..])
    };
    serde_json::from_str::<serde_json::Value>(&out)
        .map_err(|e| Error::Other(format!("DELETE_KEY produced invalid JSON ({e}); nothing written")))?;
    Ok(out)
}

// ── YAML (line-based; block mappings only — the common config shape) ─────────

fn yaml_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn yaml_is_content(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && !t.starts_with('#')
}

/// The mapping key on a block-mapping line (`key:` / `key: value`), else `None` (comment, list
/// item, or a `key:value` with no space — e.g. a bare URL).
fn yaml_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('-') {
        return None;
    }
    let (k, rest) = t.split_once(':')?;
    (rest.is_empty() || rest.starts_with([' ', '\t'])).then(|| k.trim())
}

/// The end (exclusive) of the block owned by the mapping key at `idx` (indent `indent`): the next
/// content line at indent ≤ `indent`, else end of file.
fn yaml_block_end(lines: &[String], idx: usize, indent: usize) -> usize {
    (idx + 1..lines.len())
        .find(|&i| yaml_is_content(&lines[i]) && yaml_indent(&lines[i]) <= indent)
        .unwrap_or(lines.len())
}

struct YamlHit {
    line: usize,
    indent: usize,
}

/// Locate a dotted key path, descending block by block; each segment must be an immediate child
/// (at the block's child indent) of the previous.
fn yaml_find(lines: &[String], key: &str) -> Result<YamlHit> {
    let segs: Vec<&str> = key.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Err(Error::Other("YAML key is empty".into()));
    }
    let (mut lo, mut hi) = (0usize, lines.len());
    let mut hit = YamlHit { line: 0, indent: 0 };
    for seg in &segs {
        let child_indent = (lo..hi)
            .find(|&i| yaml_is_content(&lines[i]))
            .map(|i| yaml_indent(&lines[i]))
            .ok_or_else(|| Error::Other(format!("YAML key {seg:?} not found in {key:?}")))?;
        let idx = (lo..hi)
            .find(|&i| {
                yaml_is_content(&lines[i])
                    && yaml_indent(&lines[i]) == child_indent
                    && yaml_key(&lines[i]) == Some(*seg)
            })
            .ok_or_else(|| Error::Other(format!("YAML key {seg:?} not found in {key:?}")))?;
        lo = idx + 1;
        hi = yaml_block_end(lines, idx, child_indent);
        hit = YamlHit { line: idx, indent: child_indent };
    }
    Ok(hit)
}

/// The `(lo, hi, child_indent)` block scope a new child of `parent` should be inserted into.
fn yaml_parent_scope(lines: &[String], parent: &[&str]) -> Result<(usize, usize, usize)> {
    if parent.is_empty() {
        let ci = lines.iter().find(|l| yaml_is_content(l)).map(|l| yaml_indent(l)).unwrap_or(0);
        return Ok((0, lines.len(), ci));
    }
    let pk = yaml_find(lines, &parent.join("."))?;
    let lo = pk.line + 1;
    let hi = yaml_block_end(lines, pk.line, pk.indent);
    let ci = (lo..hi)
        .find(|&i| yaml_is_content(&lines[i]))
        .map(|i| yaml_indent(&lines[i]))
        .unwrap_or(pk.indent + 2);
    Ok((lo, hi, ci))
}

/// A trailing `# comment` after the value on a line, preserved across a value change.
fn yaml_trailing_comment(line: &str) -> String {
    let after_colon = line.split_once(':').map(|(_, r)| r).unwrap_or(line);
    after_colon.find(" #").map(|p| after_colon[p..].to_string()).unwrap_or_default()
}

fn yaml_set(content: &str, key: &str, value: &str) -> Result<String> {
    let mut lines: Vec<String> = content.split('\n').map(String::from).collect();
    let (prefix, last) = split_path(key)?;
    if let Ok(h) = yaml_find(&lines, key) {
        // Replacing the scalar on an existing line — refuse if it's a nested mapping, not a scalar.
        let has_inline = lines[h.line].split_once(':').map(|(_, r)| !r.trim().is_empty()).unwrap_or(false);
        let end = yaml_block_end(&lines, h.line, h.indent);
        if !has_inline && (h.line + 1..end).any(|i| yaml_is_content(&lines[i])) {
            return Err(Error::Other(format!("SET_KEY: {key:?} is a nested mapping, not a scalar value")));
        }
        let comment = yaml_trailing_comment(&lines[h.line]);
        lines[h.line] = format!("{}{}: {}{}", " ".repeat(h.indent), last, value, comment);
        return Ok(lines.join("\n"));
    }
    // Not found: the parent path must exist; append a new child at the block's indent.
    let (lo, hi, child_indent) = yaml_parent_scope(&lines, &prefix)?;
    let at = (lo..hi).rev().find(|&i| yaml_is_content(&lines[i])).map(|i| i + 1).unwrap_or(hi);
    lines.insert(at, format!("{}{}: {}", " ".repeat(child_indent), last, value));
    Ok(lines.join("\n"))
}

fn yaml_delete(content: &str, key: &str) -> Result<String> {
    let lines: Vec<String> = content.split('\n').map(String::from).collect();
    let h = yaml_find(&lines, key)?;
    let end = yaml_block_end(&lines, h.line, h.indent);
    let mut out: Vec<String> = lines[..h.line].to_vec();
    out.extend_from_slice(&lines[end..]);
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_set_updates_value_preserving_format() {
        let src = "# a Cargo manifest\n[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1.0\"  # pinned\n";
        let out = set_key(src, Format::Toml, "dependencies.serde", "\"1.2\"").unwrap();
        assert!(out.contains("serde = \"1.2\"  # pinned"), "value changed, comment kept: {out}");
        assert!(out.contains("# a Cargo manifest"), "leading comment preserved");
        assert!(out.contains("name = \"x\""), "other keys untouched");
    }

    #[test]
    fn toml_set_creates_nested_table_and_adds_key() {
        let src = "[package]\nname = \"x\"\n";
        // A brand-new dependency: the [dependencies] table is created and re-parses.
        let out = set_key(src, Format::Toml, "dependencies.anyhow", "\"1\"").unwrap();
        assert!(out.parse::<toml_edit::DocumentMut>().is_ok(), "valid toml: {out}");
        assert!(out.contains("[dependencies]") && out.contains("anyhow = \"1\""), "dep added: {out}");
        // A structured value (inline table) is accepted verbatim.
        let out2 = set_key(&out, Format::Toml, "dependencies.serde", "{ version = \"1\", features = [\"derive\"] }").unwrap();
        assert!(out2.contains("serde = { version = \"1\", features = [\"derive\"] }"), "inline table set: {out2}");
    }

    #[test]
    fn toml_delete_removes_key() {
        let src = "[dependencies]\nserde = \"1\"\nanyhow = \"1\"\n";
        let out = delete_key(src, Format::Toml, "dependencies.serde").unwrap();
        assert!(!out.contains("serde"), "serde removed: {out}");
        assert!(out.contains("anyhow = \"1\""), "sibling kept");
        assert!(delete_key(src, Format::Toml, "dependencies.missing").is_err(), "missing key errors");
    }

    #[test]
    fn md_set_replaces_only_the_section_body() {
        let src = "# Title\nintro\n\n## Install\nold steps\nmore old\n\n## Usage\nkeep me\n";
        let out = set_key(src, Format::Markdown, "Install", "run `cargo add x`").unwrap();
        assert!(out.contains("## Install\nrun `cargo add x`\n"), "body replaced: {out}");
        assert!(!out.contains("old steps"), "old body gone");
        assert!(out.contains("## Usage\nkeep me"), "later section untouched");
        assert!(out.contains("# Title\nintro"), "earlier content untouched");
    }

    #[test]
    fn md_nested_heading_path_and_delete() {
        let src = "# API\n\n## Auth\n### Login\nlogin body\n### Logout\nlogout body\n\n## Other\nx\n";
        // A nested path targets the inner heading within its parent's scope.
        let out = set_key(src, Format::Markdown, "Auth/Logout", "new logout").unwrap();
        assert!(out.contains("### Logout\nnew logout"), "nested body set: {out}");
        assert!(out.contains("login body"), "sibling subsection kept");
        // Delete removes the heading and its whole section.
        let del = delete_key(&out, Format::Markdown, "Auth/Login").unwrap();
        assert!(!del.contains("### Login"), "login section removed: {del}");
        assert!(del.contains("### Logout"), "logout section kept");
        assert!(del.contains("## Other"), "unrelated section kept");
    }

    #[test]
    fn json_set_updates_and_adds_preserving_format() {
        let src = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"serde\": \"1.0\"\n  }\n}\n";
        // Bump an existing dependency — only that value changes.
        let out = set_key(src, Format::Json, "dependencies.serde", "\"1.2\"").unwrap();
        assert!(out.contains("\"serde\": \"1.2\""), "value updated: {out}");
        assert!(out.contains("\"name\": \"x\""), "siblings preserved");
        // Add a new dependency — inserted at the existing member indentation, with a comma.
        let out2 = set_key(&out, Format::Json, "dependencies.anyhow", "\"1\"").unwrap();
        assert!(out2.contains("\"serde\": \"1.2\",\n    \"anyhow\": \"1\""), "dep appended: {out2}");
        // Rejects an invalid JSON value rather than corrupting the file.
        assert!(set_key(src, Format::Json, "dependencies.serde", "not json").is_err());
    }

    #[test]
    fn json_set_into_empty_object_and_delete() {
        let src = "{\n  \"scripts\": {}\n}\n";
        let out = set_key(src, Format::Json, "scripts.build", "\"tsc\"").unwrap();
        assert!(out.contains("\"scripts\": {\n    \"build\": \"tsc\"\n  }"), "opened up empty object: {out}");
        // Delete the first of two members (comma handling), then the last.
        let two = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let del_a = delete_key(two, Format::Json, "a").unwrap();
        assert!(!del_a.contains("\"a\"") && del_a.contains("\"b\": 2"), "first member removed: {del_a}");
        let del_b = delete_key(two, Format::Json, "b").unwrap();
        assert!(!del_b.contains("\"b\"") && del_b.contains("\"a\": 1"), "last member removed: {del_b}");
    }

    #[test]
    fn yaml_set_updates_nested_scalar_and_adds_sibling() {
        let src = "package:\n  name: x\n  deps:\n    serde: 1.0\n";
        let out = set_key(src, Format::Yaml, "package.deps.serde", "1.2").unwrap();
        assert!(out.contains("    serde: 1.2"), "nested scalar updated: {out}");
        assert!(out.contains("  name: x"), "sibling preserved");
        // A new key joins the block at its children's indentation.
        let out2 = set_key(&out, Format::Yaml, "package.deps.anyhow", "1").unwrap();
        assert!(out2.contains("    serde: 1.2\n    anyhow: 1"), "sibling added at indent: {out2}");
        // A trailing comment survives a value change.
        let cmt = set_key("port: 8080 # default\n", Format::Yaml, "port", "9090").unwrap();
        assert_eq!(cmt, "port: 9090 # default\n");
    }

    #[test]
    fn yaml_delete_removes_key_and_its_block() {
        let src = "a:\n  b: 1\n  c: 2\nd: 3\n";
        let out = delete_key(src, Format::Yaml, "a").unwrap();
        assert!(!out.contains("b: 1") && !out.contains("c: 2"), "block removed: {out}");
        assert!(out.contains("d: 3"), "sibling kept");
        // Setting a scalar over a nested mapping is refused (would orphan its children).
        assert!(set_key(src, Format::Yaml, "a", "oops").is_err());
    }

    #[test]
    fn format_detection() {
        assert_eq!(format_of("Cargo.toml"), Some(Format::Toml));
        assert_eq!(format_of("docs/README.md"), Some(Format::Markdown));
        assert_eq!(format_of("package.json"), Some(Format::Json));
        assert_eq!(format_of("ci.yaml"), Some(Format::Yaml));
        assert_eq!(format_of(".github/workflows/x.yml"), Some(Format::Yaml));
        assert_eq!(format_of("src/main.rs"), None);
        assert!(is_structured("x.json") && is_structured("x.yaml") && !is_structured("x.rs"));
    }
}
