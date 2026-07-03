//! ci-lsp-index — build a SCIP index by SWEEPING a language server instead of running a
//! bespoke indexer: `documentSymbol` per file (definitions), `textDocument/references` per
//! symbol (the cross-reference occurrences). The output is a genuine SCIP `Index` protobuf,
//! so [`ci_scip::ScipIndex`]'s whole read path (structure, import graph, blast radius)
//! consumes it unchanged — one indexing pipeline for ANY language with a server, no
//! per-language scip-* tool.
//!
//! Server quirks this handles (verified against tsgo):
//! - Without hierarchical-symbol client caps, servers return FLAT `SymbolInformation[]`
//!   (nesting only via `containerName`), and `location.range` starts at the declaration
//!   KEYWORD, not the name — the name's own range is re-located in the source line.
//! - `LspClient::request` returns the unwrapped JSON-RPC `result`.
use ci_core::{Error, Result};
use ci_lsp::LspClient;
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolRole};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// One named definition found by the sweep (an LSP `SymbolInformation` we could anchor).
struct Def {
    /// Full SCIP symbol moniker.
    symbol: String,
    /// 0-based name position (where `references` is asked).
    line: u32,
    character: u32,
    name_len: u32,
    /// Full declaration span (SCIP `enclosing_range`), 0-based [sl, sc, el, ec].
    enclosing: Vec<i32>,
}

/// Sweep `files` (repo-relative path + content) through the server `cmd` and return the
/// serialized SCIP index. `scheme` names the emitter in symbol monikers (e.g. `"lspx-ts"`).
pub fn sweep_index(root: &Path, files: &[(String, String)], cmd: Command, scheme: &str) -> Result<Vec<u8>> {
    // CI_TIMING=1: per-phase wall times on stderr (matches ci-build's instrumentation).
    let timing = std::env::var("CI_TIMING").is_ok();
    let t = std::time::Instant::now();
    let mut client = LspClient::start(root, cmd)?;
    // Open every file (didOpen) so the server sees the whole project; the pull replies also
    // force the project load before the sweep starts.
    client.diagnostics(files)?;
    if timing {
        eprintln!("[timing] lsp-sweep open+load {:.3}s ({} files)", t.elapsed().as_secs_f64(), files.len());
    }

    let mut docs: HashMap<String, Vec<Occurrence>> = HashMap::new();
    let mut all_defs: Vec<(String, Def)> = Vec::new(); // (rel, def)

    let t = std::time::Instant::now();
    for (rel, content) in files {
        let uri = file_uri(root, rel);
        let resp = client.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri}}))?;
        let syms: Vec<Value> = resp.as_array().cloned().unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        // name -> kind, for resolving containerName chains within this file.
        let kind_of: HashMap<&str, i64> = syms
            .iter()
            .filter_map(|s| Some((s["name"].as_str()?, s["kind"].as_i64()?)))
            .collect();

        for s in &syms {
            let (Some(name), Some(kind)) = (s["name"].as_str(), s["kind"].as_i64()) else { continue };
            if suffix_for(kind).is_none() {
                continue; // not a named declaration we chunk (module/file/typeparam/…)
            }
            let Some(start) = s.pointer("/location/range/start") else { continue };
            let (Some(sl), Some(sc)) = (start["line"].as_u64(), start["character"].as_u64()) else { continue };
            // The flat-symbol range starts at the declaration keyword: locate the NAME.
            let Some((nline, ncol)) = find_name(&lines, sl as usize, sc as usize, name) else { continue };

            let chain = container_chain(s, &kind_of);
            // Match scip's shape: members of TERM/FUNCTION containers (object-literal
            // properties, function-local declarations) are references to their type's member,
            // not definitions — emitting them creates bodyless anchors scip never advertises.
            if chain.last().is_some_and(|(_, ck)| matches!(suffix_for(*ck), Some(".") | Some("()."))) {
                continue;
            }
            // Canonical-definition check: servers list IMPORT/RE-EXPORT bindings in
            // documentSymbol too, but `definition` at a binding resolves elsewhere, while a
            // true definition resolves to itself. scip's shape has only the true definitions.
            if !is_canonical_def(&mut client, &uri, nline as u32, ncol as u32)? {
                continue;
            }
            let symbol = moniker(scheme, rel, &chain, name, kind);
            let enclosing = range4(s.pointer("/location/range"));

            all_defs.push((
                rel.clone(),
                Def {
                    symbol,
                    line: nline as u32,
                    character: ncol as u32,
                    name_len: name.chars().count() as u32,
                    enclosing,
                },
            ));
        }
    }

    if timing {
        eprintln!("[timing] lsp-sweep documentSymbol+canonical-filter {:.3}s ({} defs)", t.elapsed().as_secs_f64(), all_defs.len());
    }

    // Definition occurrences.
    for (rel, d) in &all_defs {
        let mut occ = Occurrence::new();
        occ.symbol = d.symbol.clone();
        occ.symbol_roles = SymbolRole::Definition as i32;
        occ.range = vec![d.line as i32, d.character as i32, d.line as i32, (d.character + d.name_len) as i32];
        occ.enclosing_range = d.enclosing.clone();
        docs.entry(rel.clone()).or_default().push(occ);
    }

    // Reference occurrences: one `references` query per definition, results attributed to
    // the document they occur IN (SCIP's per-document occurrence model).
    let t = std::time::Instant::now();
    let root_prefix = format!("file://{}/", root.to_string_lossy());
    for (rel, d) in &all_defs {
        let uri = file_uri(root, rel);
        let resp = client.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": d.line, "character": d.character},
                "context": {"includeDeclaration": false},
            }),
        )?;
        for loc in resp.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(ref_uri) = loc["uri"].as_str() else { continue };
            let Some(ref_rel) = ref_uri.strip_prefix(&root_prefix) else { continue }; // outside root
            let mut occ = Occurrence::new();
            occ.symbol = d.symbol.clone();
            occ.symbol_roles = 0;
            occ.range = range4(loc.get("range"));
            docs.entry(ref_rel.to_string()).or_default().push(occ);
        }
    }

    if timing {
        eprintln!("[timing] lsp-sweep references {:.3}s", t.elapsed().as_secs_f64());
    }

    let mut documents: Vec<Document> = docs
        .into_iter()
        .map(|(rel, occurrences)| {
            let mut doc = Document::new();
            doc.relative_path = rel;
            doc.occurrences = occurrences;
            doc
        })
        .collect();
    documents.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    let mut index = Index::new();
    index.documents = documents;
    index.write_to_bytes().map_err(|e| Error::Other(format!("scip serialize: {e}")))
}

fn file_uri(root: &Path, rel: &str) -> String {
    format!("file://{}/{}", root.to_string_lossy(), rel)
}

/// True when `definition` at (`line`, `character`) resolves back to that same position —
/// i.e. this occurrence IS the canonical definition (an import/re-export binding resolves
/// to the original file instead). Empty/odd responses keep the symbol (conservative).
fn is_canonical_def(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Result<bool> {
    let resp = client.request(
        "textDocument/definition",
        json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}),
    )?;
    // Location[] | Location | LocationLink[] | null
    let locs: Vec<&Value> = match &resp {
        Value::Array(a) if !a.is_empty() => a.iter().collect(),
        Value::Object(_) => vec![&resp],
        _ => return Ok(true),
    };
    Ok(locs.iter().any(|l| {
        let target_uri = l["uri"].as_str().or_else(|| l["targetUri"].as_str());
        let start = l.pointer("/range/start").or_else(|| l.pointer("/targetSelectionRange/start"));
        target_uri == Some(uri)
            && start.is_some_and(|s| {
                s["line"].as_u64() == Some(line as u64)
                    && s["character"].as_u64() == Some(character as u64)
            })
    }))
}

/// SCIP descriptor suffix for an LSP `SymbolKind`, or None for kinds we don't chunk.
/// (`#` = type, `().` = function/method, `.` = term — what [`ci_scip`] maps back to kinds.)
fn suffix_for(kind: i64) -> Option<&'static str> {
    match kind {
        5 | 10 | 11 | 23 => Some("#"),        // Class, Enum, Interface, Struct
        6 | 9 | 12 => Some("()."),            // Method, Constructor, Function
        7 | 8 | 13 | 14 | 22 => Some("."),    // Property, Field, Variable, Constant, EnumMember
        _ => None,
    }
}

/// Escape a descriptor name per the SCIP symbol grammar (backticks unless a simple identifier).
fn desc_name(s: &str) -> String {
    let simple = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '$' | '-'));
    if simple {
        s.to_string()
    } else {
        format!("`{}`", s.replace('`', "``"))
    }
}

/// Full SCIP moniker: `<scheme> lsp . . <path namespaces><container chain><leaf>`.
fn moniker(scheme: &str, rel: &str, chain: &[(String, i64)], name: &str, kind: i64) -> String {
    let mut descriptors = String::new();
    for seg in rel.split('/') {
        descriptors.push_str(&desc_name(seg));
        descriptors.push('/');
    }
    for (cname, ckind) in chain {
        descriptors.push_str(&desc_name(cname));
        descriptors.push_str(suffix_for(*ckind).unwrap_or("/"));
    }
    descriptors.push_str(&desc_name(name));
    descriptors.push_str(suffix_for(kind).unwrap_or("."));
    format!("{scheme} lsp . . {descriptors}")
}

/// Outer-to-inner container chain via `containerName` lookups (flat SymbolInformation).
fn container_chain(sym: &Value, kind_of: &HashMap<&str, i64>) -> Vec<(String, i64)> {
    let mut chain = Vec::new();
    let mut cur = sym["containerName"].as_str();
    while let Some(c) = cur {
        if c.is_empty() || chain.len() >= 8 {
            break;
        }
        chain.push((c.to_string(), kind_of.get(c).copied().unwrap_or(3)));
        cur = None; // SymbolInformation carries only ONE container level; deeper nesting unavailable
    }
    chain.reverse();
    chain
}

/// Locate `name` as a whole word at-or-after (`line`, `col`); flat symbol ranges start at the
/// declaration keyword, so scan the start line first, then a few lines below (decorators etc.).
fn find_name(lines: &[&str], line: usize, col: usize, name: &str) -> Option<(usize, usize)> {
    for (i, l) in lines.iter().enumerate().skip(line).take(4) {
        let from = if i == line { col.min(l.len()) } else { 0 };
        let hay = &l[from..];
        let mut offset = 0;
        while let Some(pos) = hay[offset..].find(name) {
            let abs = from + offset + pos;
            let before_ok = abs == 0
                || !l[..abs].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after = l[abs + name.len()..].chars().next();
            let after_ok = !after.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if before_ok && after_ok {
                return Some((i, abs));
            }
            offset += pos + name.len();
        }
    }
    None
}

/// LSP range value -> SCIP 4-element 0-based range (empty when absent).
fn range4(range: Option<&Value>) -> Vec<i32> {
    let Some(r) = range else { return vec![] };
    let g = |p: &str| r.pointer(p).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    vec![g("/start/line"), g("/start/character"), g("/end/line"), g("/end/character")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monikers_parse_under_the_scip_symbol_grammar() {
        let m = moniker("lspx-ts", "src/hub.ts", &[("Ranker".into(), 5)], "top", 6);
        let parsed = scip::symbol::parse_symbol(&m).expect("must parse");
        // path namespaces + class + method
        assert_eq!(parsed.descriptors.last().unwrap().name, "top");
        let m2 = moniker("lspx-ts", "src/hub.ts", &[], "compute", 12);
        assert!(scip::symbol::parse_symbol(&m2).is_ok());
    }

    #[test]
    fn find_name_skips_keyword_and_matches_whole_words() {
        let lines = vec!["export function computeAll(a: compute): number {", "  return compute(a);"];
        // whole-word: must not land inside `computeAll`
        assert_eq!(find_name(&lines, 0, 0, "compute"), Some((0, 30)));
        assert_eq!(find_name(&lines, 0, 0, "computeAll"), Some((0, 16)));
    }

    #[test]
    fn desc_name_escapes_non_identifiers() {
        assert_eq!(desc_name("hub"), "hub");
        assert_eq!(desc_name("hub.ts"), "`hub.ts`");
    }
}
