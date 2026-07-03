//! ci-lsp-index — build a SCIP index by SWEEPING a language server instead of running a
//! bespoke indexer: `documentSymbol` per file (definitions), `textDocument/references` per
//! symbol (the cross-reference occurrences). The output is a genuine SCIP `Index` protobuf,
//! so [`ci_scip::ScipIndex`]'s whole read path (structure, import graph, blast radius)
//! consumes it unchanged — one indexing pipeline for ANY language with a server, no
//! per-language scip-* tool.
//!
//! Server quirks this handles (verified against tsgo):
//! - ci-lsp advertises `hierarchicalDocumentSymbolSupport`, so capable servers return nested
//!   `DocumentSymbol[]` (real `selectionRange` for the name, real nesting for the moniker
//!   chain). Servers without it return FLAT `SymbolInformation[]` — one `containerName`
//!   level, `location.range` starting at the declaration KEYWORD — handled by re-locating
//!   the name in the source line.
//! - LSP positions are UTF-16 columns; SCIP/core ranges are byte columns. Emitted ranges are
//!   converted per line (identical for ASCII, correct for non-ASCII); positions SENT to the
//!   server stay UTF-16.
//! - `LspClient::request` returns the unwrapped JSON-RPC `result`.
use ci_core::{Error, Result};
use ci_lsp::LspClient;
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolRole};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// One named definition found by the sweep.
struct Def {
    /// Full SCIP symbol moniker.
    symbol: String,
    /// 0-based name position in LSP space (UTF-16 column) — for `references`/`definition`.
    line: u32,
    qchar: u32,
    /// The emitted name range in BYTE columns: [line, start, line, end].
    name_range: Vec<i32>,
    /// Full declaration span (SCIP `enclosing_range`), byte columns.
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

    // Only swept files become documents: a reference landing in a file the walk excluded
    // (generated code, .d.ts) must not invent a document — scip scopes to what it indexed.
    let file_lines: HashMap<&str, Vec<&str>> =
        files.iter().map(|(rel, content)| (rel.as_str(), content.lines().collect())).collect();

    let mut docs: HashMap<String, Vec<Occurrence>> = HashMap::new();
    let mut all_defs: Vec<(String, Def)> = Vec::new(); // (rel, def)

    let t = std::time::Instant::now();
    for (rel, content) in files {
        let uri = file_uri(root, rel);
        let resp = request_retry(
            &mut client,
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        )?;
        let syms: Vec<Value> = resp.as_array().cloned().unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();

        // Hierarchical DocumentSymbol[] (has selectionRange) vs flat SymbolInformation[].
        if syms.first().is_some_and(|s| s.get("selectionRange").is_some()) {
            let mut chain = Vec::new();
            for s in &syms {
                collect_hier(&mut client, &uri, rel, scheme, &lines, s, &mut chain, &mut all_defs)?;
            }
        } else {
            collect_flat(&mut client, &uri, rel, scheme, &lines, &syms, &mut all_defs)?;
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
        occ.range = d.name_range.clone();
        occ.enclosing_range = d.enclosing.clone();
        docs.entry(rel.clone()).or_default().push(occ);
    }

    // Reference occurrences: one `references` query per definition, results attributed to
    // the document they occur IN (SCIP's per-document occurrence model). A symbol whose
    // query keeps failing is SKIPPED (its refs are missing, everything else survives) —
    // one transient server error must not abort a 20k-symbol sweep.
    let t = std::time::Instant::now();
    let root_prefix = format!("file://{}/", root.to_string_lossy());
    let mut skipped = 0usize;
    for (rel, d) in &all_defs {
        let uri = file_uri(root, rel);
        let resp = match request_retry(
            &mut client,
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": d.line, "character": d.qchar},
                "context": {"includeDeclaration": false},
            }),
        ) {
            Ok(r) => r,
            Err(e) => {
                skipped += 1;
                eprintln!("[ci-lsp-index] references failed for {} ({e}); its refs are missing from the index", d.symbol);
                continue;
            }
        };
        for loc in resp.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(ref_uri) = loc["uri"].as_str() else { continue };
            let Some(ref_rel) = ref_uri.strip_prefix(&root_prefix) else { continue }; // outside root
            let Some(ref_lines) = file_lines.get(ref_rel) else { continue }; // outside the swept set
            let mut occ = Occurrence::new();
            occ.symbol = d.symbol.clone();
            occ.symbol_roles = 0;
            occ.range = range4_bytes(loc.get("range"), ref_lines);
            docs.entry(ref_rel.to_string()).or_default().push(occ);
        }
    }
    if skipped > 0 {
        eprintln!("[ci-lsp-index] {skipped} symbol(s) skipped after retries");
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

/// Recursive collector for hierarchical `DocumentSymbol[]`: real `selectionRange` (no name
/// re-location) and the FULL nesting chain (namespace → class → member monikers match scip,
/// which the one-`containerName`-level flat shape cannot).
#[allow(clippy::too_many_arguments)]
fn collect_hier(
    client: &mut LspClient,
    uri: &str,
    rel: &str,
    scheme: &str,
    lines: &[&str],
    sym: &Value,
    chain: &mut Vec<(String, i64)>,
    out: &mut Vec<(String, Def)>,
) -> Result<()> {
    let (Some(name), Some(kind)) = (sym["name"].as_str(), sym["kind"].as_i64()) else { return Ok(()) };

    let eligible = suffix_for(kind).is_some()
        // scip's shape: members of TERM/FUNCTION containers (object-literal properties,
        // function-local declarations) are references to their type's member, not definitions.
        && !chain.last().is_some_and(|(_, ck)| matches!(suffix_for(*ck), Some(".") | Some("().")));

    if eligible {
        if let (Some(sl), Some(sc16)) = (
            sym.pointer("/selectionRange/start/line").and_then(|v| v.as_u64()),
            sym.pointer("/selectionRange/start/character").and_then(|v| v.as_u64()),
        ) {
            // Import/re-export bindings appear in documentSymbol too; only a symbol whose
            // `definition` resolves to itself is the canonical definition.
            if is_canonical_def(client, uri, sl as u32, sc16 as u32)? {
                out.push((
                    rel.to_string(),
                    Def {
                        symbol: moniker(scheme, rel, chain, name, kind),
                        line: sl as u32,
                        qchar: sc16 as u32,
                        name_range: range4_bytes(sym.get("selectionRange"), lines),
                        enclosing: range4_bytes(sym.get("range"), lines),
                    },
                ));
            }
        }
    }

    if let Some(children) = sym.get("children").and_then(|c| c.as_array()) {
        chain.push((name.to_string(), kind));
        for c in children {
            collect_hier(client, uri, rel, scheme, lines, c, chain, out)?;
        }
        chain.pop();
    }
    Ok(())
}

/// Collector for flat `SymbolInformation[]` (servers without hierarchical support): one
/// `containerName` level, range starts at the declaration keyword → re-locate the name.
fn collect_flat(
    client: &mut LspClient,
    uri: &str,
    rel: &str,
    scheme: &str,
    lines: &[&str],
    syms: &[Value],
    out: &mut Vec<(String, Def)>,
) -> Result<()> {
    // name -> kind, for resolving containerName chains within this file.
    let kind_of: HashMap<&str, i64> =
        syms.iter().filter_map(|s| Some((s["name"].as_str()?, s["kind"].as_i64()?))).collect();

    for s in syms {
        let (Some(name), Some(kind)) = (s["name"].as_str(), s["kind"].as_i64()) else { continue };
        if suffix_for(kind).is_none() {
            continue; // not a named declaration we chunk (module/file/typeparam/…)
        }
        let Some(start) = s.pointer("/location/range/start") else { continue };
        let (Some(sl), Some(sc16)) = (start["line"].as_u64(), start["character"].as_u64()) else { continue };
        let byte_col = lines.get(sl as usize).map(|l| u16_col_to_byte(l, sc16)).unwrap_or(0);
        let Some((nline, nbyte)) = find_name(lines, sl as usize, byte_col, name) else { continue };
        let qchar = byte_col_to_u16(lines[nline], nbyte);

        let chain = container_chain(s, &kind_of);
        if chain.last().is_some_and(|(_, ck)| matches!(suffix_for(*ck), Some(".") | Some("()."))) {
            continue;
        }
        if !is_canonical_def(client, uri, nline as u32, qchar as u32)? {
            continue;
        }
        out.push((
            rel.to_string(),
            Def {
                symbol: moniker(scheme, rel, &chain, name, kind),
                line: nline as u32,
                qchar: qchar as u32,
                name_range: vec![nline as i32, nbyte as i32, nline as i32, (nbyte + name.len()) as i32],
                enclosing: range4_bytes(s.pointer("/location").and_then(|l| l.get("range")), lines),
            },
        ));
    }
    Ok(())
}

fn file_uri(root: &Path, rel: &str) -> String {
    format!("file://{}/{}", root.to_string_lossy(), rel)
}

/// `request` with backoff on transient server states ("content modified", still-loading) —
/// a 20k-query sweep must ride out a hiccup, not abort on it.
fn request_retry(client: &mut LspClient, method: &str, params: Value) -> Result<Value> {
    let mut last = None;
    for attempt in 0..4 {
        match client.request(method, params.clone()) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let m = e.to_string().to_lowercase();
                let transient = m.contains("content modified")
                    || m.contains("-32801")
                    || m.contains("-32802") // ServerCancelled: the server asks for a retry
                    || m.contains("server cancelled")
                    || m.contains("-32602")
                    || m.contains("loading")
                    || m.contains("not ready");
                if !transient || attempt == 3 {
                    return Err(e);
                }
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Other("request retry exhausted".into())))
}

/// True when `definition` at (`line`, `character`) resolves back to that same position —
/// i.e. this occurrence IS the canonical definition (an import/re-export binding resolves
/// to the original file instead). Empty/odd responses keep the symbol (conservative).
fn is_canonical_def(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Result<bool> {
    let resp = request_retry(
        client,
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

/// Locate `name` as a whole word at-or-after (`line`, byte `col`); flat symbol ranges start
/// at the declaration keyword, so scan the start line first, then a few lines below
/// (decorators etc.). `col` must be a char boundary (callers convert via [`u16_col_to_byte`]).
fn find_name(lines: &[&str], line: usize, col: usize, name: &str) -> Option<(usize, usize)> {
    for (i, l) in lines.iter().enumerate().skip(line).take(4) {
        let mut from = if i == line { col.min(l.len()) } else { 0 };
        while from > 0 && !l.is_char_boundary(from) {
            from -= 1; // defensive: never slice mid-char even on a bad column
        }
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

/// UTF-16 column (LSP) -> byte column within `line`; clamps past end-of-line.
fn u16_col_to_byte(line: &str, col16: u64) -> usize {
    let mut u16s = 0u64;
    for (bi, ch) in line.char_indices() {
        if u16s >= col16 {
            return bi;
        }
        u16s += ch.len_utf16() as u64;
    }
    line.len()
}

/// Byte column -> UTF-16 column (for positions sent BACK to the server).
fn byte_col_to_u16(line: &str, byte_col: usize) -> u64 {
    line.char_indices()
        .take_while(|(bi, _)| *bi < byte_col)
        .map(|(_, ch)| ch.len_utf16() as u64)
        .sum()
}

/// LSP range (UTF-16 columns) -> SCIP 4-element 0-based BYTE-column range.
fn range4_bytes(range: Option<&Value>, lines: &[&str]) -> Vec<i32> {
    let Some(r) = range else { return vec![] };
    let g = |p: &str| r.pointer(p).and_then(|v| v.as_u64()).unwrap_or(0);
    let (sl, sc16, el, ec16) =
        (g("/start/line"), g("/start/character"), g("/end/line"), g("/end/character"));
    let to_byte =
        |line: u64, col16: u64| lines.get(line as usize).map(|l| u16_col_to_byte(l, col16)).unwrap_or(col16 as usize);
    vec![sl as i32, to_byte(sl, sc16) as i32, el as i32, to_byte(el, ec16) as i32]
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
    fn find_name_survives_non_ascii_prefixes() {
        // "número" puts multibyte chars before the name; a naive byte slice at the LSP
        // column would panic. 4-byte emoji ahead of the keyword shifts every boundary.
        let lines = vec!["const número = 1; // 🎯 marker", "export function alvo(x: number) {}"];
        assert_eq!(find_name(&lines, 1, 0, "alvo"), Some((1, 16)));
        // a column landing mid-emoji must not panic (defensive boundary walk)
        let l = lines[0];
        let mid_emoji = l.find('🎯').unwrap() + 1;
        assert!(find_name(&lines, 0, mid_emoji, "marker").is_some());
    }

    #[test]
    fn utf16_byte_column_round_trip() {
        let line = "const número = compute(1); // 🎯 alvo";
        // 'ú' is 1 UTF-16 unit but 2 bytes; '🎯' is 2 UTF-16 units and 4 bytes.
        let byte_col = line.find("compute").unwrap();
        let col16 = byte_col_to_u16(line, byte_col);
        assert_eq!(u16_col_to_byte(line, col16), byte_col);
        let alvo_byte = line.find("alvo").unwrap();
        let alvo16 = byte_col_to_u16(line, alvo_byte);
        assert!(alvo16 < alvo_byte as u64, "utf-16 col must be smaller past multibyte chars");
        assert_eq!(u16_col_to_byte(line, alvo16), alvo_byte);
    }

    #[test]
    fn desc_name_escapes_non_identifiers() {
        assert_eq!(desc_name("hub"), "hub");
        assert_eq!(desc_name("hub.ts"), "`hub.ts`");
    }
}
