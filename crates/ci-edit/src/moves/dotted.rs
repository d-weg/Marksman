//! The generic dotted-name move engine — one implementation for every language whose files
//! are referenced by a separator-joined qualified name derived from the directory layout
//! (Java `com.x.A` over source roots, PHP `App\Foo\Bar` over PSR-4). The java and php move
//! hooks were ~75% the same file with a config's worth of differences; contract §8's "extract
//! on the next consumer" rule lands here: the control flow (import scanning, longest-prefix
//! code mentions, string/comment masking, membership-declaration rewrites) lives ONCE, and a
//! language is a [`DottedSyntax`] plus the [`DottedLang`] hooks it genuinely owns (its
//! resolver, its declaration scanner, its masking grammar).
//!
//! A language's `MoveModel` impl delegates its four methods to [`ref_occurrences`] /
//! [`membership_edits`] / the syntax scalars — four one-liners. (A blanket
//! `impl<T: DottedLang> MoveModel for T` would be nicer but breaks coherence: lang-rust's own
//! `MoveModel` impl could no longer be proven disjoint from it.)
use super::{MembershipEdit, RefOccurrence};
use std::path::{Path, PathBuf};

/// The scalar differences between dotted-name languages. One `static` per language.
pub struct DottedSyntax {
    /// Name separator: `.` (java) / `\` (php).
    pub sep: char,
    /// Import statement keyword, trailing space included: `"import "` / `"use "`.
    pub import_kw: &'static str,
    /// Optional modifiers after the keyword (first match is stripped): `["static "]` /
    /// `["function ", "const "]`.
    pub import_modifiers: &'static [&'static str],
    /// Characters terminating the imported name: `[';']` / `[';', ',']`.
    pub import_stops: &'static [char],
    /// Alias separator when imports can alias (`" as "` in php); the name before it is kept.
    pub import_alias_kw: Option<&'static str>,
    /// Reject an import ENDING with this char — java's on-demand `import a.b.*;` names a
    /// package dir, not one file.
    pub reject_import_suffix: Option<char>,
    /// Reject an import CONTAINING this char — php's grouped `use A\B\{C, D};`.
    pub reject_import_containing: Option<char>,
    /// Whether `Name<sep>` still references `Name`. Java: yes — a trailing `.` is member
    /// access (`com.x.A.f()`) or a nested type. PHP: no — a trailing `\` is a DEEPER
    /// namespace (`App\Foo\Bar` is not a reference to `App\Foo`).
    pub trailing_sep_refs_target: bool,
    /// Whether a code-mention run may START with the separator (php's legal leading `\`:
    /// `\App\Foo::bar()`).
    pub run_may_start_with_sep: bool,
    /// Source-file extension, dot included: `".java"` / `".php"`.
    pub source_ext: &'static str,
}

/// What a dotted-name language genuinely owns. Masking is a hook — not shared code — because
/// the tree-sitter grammars live in lang-fallback, which depends on ci-edit (not vice versa).
pub trait DottedLang {
    fn syntax(&self) -> &'static DottedSyntax;
    /// The repo root (membership edits read the moved file's current content from disk).
    fn root(&self) -> &Path;
    /// Path → qualified name (`src/main/java/com/x/A.java` → `com.x.A`), inverting the same
    /// resolution the language's import graph uses — one resolver per language, per §7.
    fn path_to_name(&self, rel: &str) -> Option<String>;
    /// Qualified name → repo-relative source file, resolved from `from_rel`'s vantage.
    fn resolve_name(&self, from_rel: &str, name: &str) -> Option<PathBuf>;
    /// 0-based line index of the EXISTING membership declaration (`package p;` /
    /// `namespace N;`) in `content`, when present.
    fn decl_line(&self, content: &str) -> Option<usize>;
    /// Render a membership declaration for `ns` (no trailing newline): `package com.x;`.
    fn render_decl(&self, ns: &str) -> String;
    /// The line to INSERT a missing membership declaration at (java: past the license
    /// comment; php: after `<?php`).
    fn insert_line(&self, content: &str) -> usize;
    /// Byte extents of strings/comments in `content` — a qualified name inside one is never
    /// a rewrite target (a rewrite there still compiles, so the gate can't catch it).
    fn masked_spans(&self, content: &str) -> Vec<(usize, usize)>;
    /// The diagnostic for an import of a batch-deleted file (the anchored reject-recipe site).
    fn deletion_note(&self, name: &str, target: &str) -> String;
}

/// Two occurrence kinds per line:
/// - import declarations: the qualified name is a span-rewritable reference resolving to a
///   file, noted for the deletion pass (the compiler reports the unresolved symbol too, but
///   the anchored reject-recipe wants the exact site).
/// - fully-qualified mentions in code (`new com.x.A()`, `App\Foo::bar()`): the LONGEST
///   dotted prefix resolving to a source file is a rewrite span (`note: None` — the gate
///   owns correctness; this keeps a move complete). Mentions inside strings/comments are
///   masked out (M1).
pub fn ref_occurrences<L: DottedLang + ?Sized>(lang: &L, rel: &str, content: &str) -> Vec<RefOccurrence> {
    let syn = lang.syntax();
    let mut out = Vec::new();
    let masked = lang.masked_spans(content);
    let line_starts = ci_core::text::line_start_offsets(content);
    let in_masked = |abs: usize| masked.iter().any(|&(s, e)| abs >= s && abs < e);
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(syn.import_kw) {
            let mut r = rest.trim_start();
            for m in syn.import_modifiers {
                if let Some(s) = r.strip_prefix(m) {
                    r = s.trim_start();
                    break;
                }
            }
            let name = r.split(syn.import_stops).next().unwrap_or("");
            let name = match syn.import_alias_kw {
                Some(alias) => name.split(alias).next().unwrap_or(name),
                None => name,
            };
            let name = name.trim().trim_start_matches(syn.sep);
            let rejected = name.is_empty()
                || syn.reject_import_suffix.is_some_and(|c| name.ends_with(c))
                || syn.reject_import_containing.is_some_and(|c| name.contains(c));
            if !rejected {
                if let Some(target) = lang.resolve_name(rel, name) {
                    let target = target.to_string_lossy().replace('\\', "/");
                    // An import line names exactly this name once — locate it DIRECTLY. The
                    // boundary-checker would read a legal leading separator (`use \App\Foo;`)
                    // as a longer-name boundary and yield zero spans, losing both the rewrite
                    // and the deletion diagnostic (M2).
                    if let Some(at) = line.find(name) {
                        out.push(RefOccurrence {
                            line: i,
                            span: Some((at, at + name.len())),
                            target: target.clone(),
                            note: Some(lang.deletion_note(name, &target)),
                        });
                    }
                }
            }
            continue; // an import line carries no other references worth scanning
        }
        // Fully-qualified references in code: walk separator-joined identifier runs, resolve
        // the longest prefix that lands on a source file.
        for (start, run) in sep_runs(line, syn) {
            if in_masked(line_starts[i] + start) {
                continue; // a name inside a string/comment is never a rewrite target (M1)
            }
            let segs: Vec<&str> = run.split(syn.sep).filter(|s| !s.is_empty()).collect();
            for n in (1..=segs.len()).rev() {
                let prefix = segs[..n].join(&syn.sep.to_string());
                if let Some(target) = lang.resolve_name(rel, &prefix) {
                    let target = target.to_string_lossy().replace('\\', "/");
                    // The rewrite span is the resolved prefix as it appears at `start`.
                    if let Some((s, e)) = name_spans(&line[start..], &prefix, syn).into_iter().next() {
                        out.push(RefOccurrence {
                            line: i,
                            span: Some((start + s, start + e)),
                            target,
                            note: None,
                        });
                    }
                    break; // longest resolving prefix wins; don't also emit its parents
                }
            }
        }
    }
    out
}

/// Declaration-line-only membership (these languages have no `mod.rs`: a file is a member of
/// its package/namespace by directory layout): a cross-scope move rewrites the moved file's
/// own declaration to the destination; a same-scope move is an EMPTY vec (handled — importers
/// still rewrite), and `None` only when either path isn't a resolvable source name (the
/// engine declines; the caller falls through to the language's native LSP).
pub fn membership_edits<L: DottedLang + ?Sized>(lang: &L, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
    let syn = lang.syntax();
    let parent = |name: &str| match name.rfind(syn.sep) {
        Some(i) => name[..i].to_string(),
        None => String::new(),
    };
    let from_ns = parent(&lang.path_to_name(from)?);
    let to_ns = parent(&lang.path_to_name(to)?);
    if from_ns == to_ns {
        return Some(Vec::new()); // same scope: importers rewrite, the declaration stays
    }
    // The edit rides on the moved file (`from`), which the engine renders against `from`'s
    // content BEFORE the file moves.
    let content = std::fs::read_to_string(lang.root().join(from)).ok()?;
    match lang.decl_line(&content) {
        Some(idx) => {
            // Moved into the default/global scope → drop the declaration (empty line).
            let new_text = if to_ns.is_empty() { String::new() } else { lang.render_decl(&to_ns) };
            Some(vec![MembershipEdit::ReplaceLine { file: from.to_string(), line: idx, new_text }])
        }
        None if to_ns.is_empty() => Some(Vec::new()), // default → default: nothing to declare
        None => {
            // No declaration today, moving INTO a scope: ADD it (a missing InsertAt used to
            // decline the whole move — M3).
            Some(vec![MembershipEdit::InsertAt {
                file: from.to_string(),
                line: lang.insert_line(&content),
                text: format!("{}\n", lang.render_decl(&to_ns)),
            }])
        }
    }
}

/// Byte spans of the exact token `name` in `line`, boundary-checked so `com.x.A` never matches
/// inside `com.x.Abc` or `zcom.x.A`. The rule is ASYMMETRIC: a LEADING identifier char or
/// separator means we sit inside a longer, more-qualified name — never this one. A TRAILING
/// identifier char means a longer simple name; a trailing SEPARATOR is language-dependent
/// ([`DottedSyntax::trailing_sep_refs_target`]): java's `com.x.A.f()` still references
/// `com.x.A`, php's `App\Foo\Bar` does NOT reference `App\Foo`.
pub fn name_spans(line: &str, name: &str, syn: &DottedSyntax) -> Vec<(usize, usize)> {
    let before_ext = |c: char| c.is_alphanumeric() || c == '_' || c == syn.sep;
    let after_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = line[from..].find(name) {
        let at = from + pos;
        let end = at + name.len();
        let before_ok = at == 0 || !line[..at].chars().next_back().is_some_and(before_ext);
        let after_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|c| !after_ident(c) && (syn.trailing_sep_refs_target || c != syn.sep));
        if before_ok && after_ok {
            out.push((at, end));
        }
        from = end;
    }
    out
}

/// Maximal `[ident|sep]` runs in `line` that contain the separator — the candidate qualified
/// references. Each is `(start byte of the trimmed run, text)`; leading/trailing separators
/// are trimmed off (php's legal leading `\` anchors the run on its first identifier).
fn sep_runs(line: &str, syn: &DottedSyntax) -> Vec<(usize, String)> {
    let sep = syn.sep;
    let ext = |c: char| c.is_alphanumeric() || c == '_' || c == sep;
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let c = line[i..].chars().next().expect("i is on a char boundary");
        let starts = c.is_alphabetic() || c == '_' || (syn.run_may_start_with_sep && c == sep);
        if starts {
            let start = i;
            while i < line.len() && line[i..].chars().next().is_some_and(ext) {
                i += line[i..].chars().next().expect("in-bounds").len_utf8();
            }
            let raw = &line[start..i];
            let run = raw.trim_matches(sep);
            if run.contains(sep) {
                let run_start = start + (raw.len() - raw.trim_start_matches(sep).len());
                out.push((run_start, run.to_string()));
            }
        } else {
            i += c.len_utf8();
        }
    }
    out
}
