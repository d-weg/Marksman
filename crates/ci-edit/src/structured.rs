//! Structured non-code edits: change a value in a TOML file by dotted key, or a Markdown section
//! by heading path — **format-preserving** (only the addressed value/section changes; everything
//! else stays byte-for-byte) and **ungated** (these files aren't type-checked). Returning the whole
//! new file content keeps the caller trivial: the VFS just overwrites, and the atomic-batch /
//! rollback machinery in [`commit_edits`](crate::commit_edits) treats it like any other edit — so a
//! `Cargo.toml` dep can land in the same transaction as the `use` that needs it.
//!
//! JSON and YAML are intentionally not handled here yet: format-preserving JSON needs a
//! span-aware parser and YAML lacks a maintained format-preserving crate — both are follow-ups.
use ci_core::{Error, Result};

/// A structured file format editable by structural path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Markdown,
}

/// The editable structured format for a path by extension, or `None` for anything else (including
/// code — those go through a `LanguageProvider`, not here).
pub fn format_of(path: &str) -> Option<Format> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".toml") {
        Some(Format::Toml)
    } else if p.ends_with(".md") || p.ends_with(".markdown") {
        Some(Format::Markdown)
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
    }
}

/// Delete `key` (a dotted TOML path or a Markdown heading path) from `content`, returning the whole
/// new file (format-preserving).
pub fn delete_key(content: &str, fmt: Format, key: &str) -> Result<String> {
    match fmt {
        Format::Toml => toml_delete(content, key),
        Format::Markdown => md_delete_section(content, key),
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
    fn format_detection() {
        assert_eq!(format_of("Cargo.toml"), Some(Format::Toml));
        assert_eq!(format_of("docs/README.md"), Some(Format::Markdown));
        assert_eq!(format_of("src/main.rs"), None);
        assert!(is_structured("x.toml") && !is_structured("x.rs"));
    }
}
