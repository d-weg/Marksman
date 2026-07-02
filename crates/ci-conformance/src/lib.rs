//! ci-conformance — the provider contract, executable.
//!
//! One battery of assertions that every [`LanguageProvider`] must pass, parameterized over a
//! per-language mini-fixture. The prose version of the contract lives in
//! `docs/provider-contract.md`; THIS crate is what enforces it — a new provider ships by adding
//! its fixture to `tests/conformance.rs`, not by being reviewed really carefully.
//!
//! Two tiers, mirroring the repo's test layout:
//! - the FAST tier (`cargo test -p ci-conformance`) covers everything in-process: the eight
//!   fallback languages (read + edit + syntax gate) and the Rust provider's read path;
//! - the REAL-TOOL tier (`#[ignore]`, run with `-- --ignored`) covers providers that shell out
//!   (TypeScript via scip-typescript). Gate-soundness e2es that need a live checker
//!   (ts-morph / rust-analyzer, incl. the barrel + monorepo radius cases) stay in their
//!   provider crates — they are instances of this contract, referenced from the doc.
//!
//! Failures name the provider label and the violated clause, so an audit run reads as a
//! conformance report.

use ci_core::{CommitResult, EditOp, EditOpts, LanguageProvider, Node};
use std::path::Path;

/// A read-path fixture: small sources + what the contract requires the provider to see in them.
pub struct ReadFixture {
    /// Provider label for failure messages ("fallback/go", "rust", …).
    pub label: &'static str,
    /// (repo-relative path, content) — parent dirs are created automatically.
    pub files: &'static [(&'static str, &'static str)],
    /// The file whose `structure()` the battery inspects.
    pub target: &'static str,
    /// Node ids that must exist (the id CONTRACT: `file#Name` / `file#Scope.Name`).
    pub want_ids: &'static [&'static str],
    /// A function symbol that must carry a `:body` sub-node…
    pub fn_symbol: &'static str,
    /// …and a `:params` sub-node (some grammars expose no parameter field; a provider may
    /// document that as a limitation rather than fake one).
    pub expect_params: bool,
    /// A symbol whose leading comment / docstring must surface as a `:doc` sub-node.
    pub doc_symbol: Option<&'static str>,
    /// An import edge (`importer` -> `imported`) `import_graph()` must contain; `None` for
    /// languages whose fallback has no resolver (their graph must be EMPTY, never an error).
    pub edge: Option<(&'static str, &'static str)>,
    /// What `gated()` must report — the honesty flag the MCP layer relays to the agent.
    pub expect_gated: bool,
}

/// An edit-path fixture (in-process tiers only): a symbol to edit and two `replace_text`
/// payloads — one that keeps the file parsing, one that must trip the syntax gate.
pub struct EditFixture {
    pub label: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    /// Node id whose text contains both `clean.0` and `breaks.0`.
    pub target_symbol: &'static str,
    /// (old, new) that still parses — must COMMIT.
    pub clean: (&'static str, &'static str),
    /// (old, new) that no longer parses — must REJECT, atomically.
    pub breaks: (&'static str, &'static str),
}

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let abs = root.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, content).unwrap();
    }
}

fn collect<'n>(nodes: &'n [Node], out: &mut Vec<&'n Node>) {
    for n in nodes {
        out.push(n);
        collect(&n.children, out);
    }
}

fn find<'n>(nodes: &'n [Node], id: &str) -> Option<&'n Node> {
    let mut all = Vec::new();
    collect(nodes, &mut all);
    all.into_iter().find(|n| n.id == id)
}

/// Structural invariants every node in every provider's tree must hold.
fn check_node_invariants(label: &str, file: &str, content: &str, node: &Node, parent: Option<&Node>) {
    let id = &node.id;
    assert!(
        id.starts_with(&format!("{file}#")),
        "[{label}] id contract: '{id}' must start with '{file}#'"
    );
    assert!(
        node.range.start_line >= 1 && node.range.end_line >= node.range.start_line,
        "[{label}] range contract: '{id}' has an invalid line range {:?} (1-based, end >= start)",
        node.range
    );
    if let Some(p) = parent {
        let rest = id.strip_prefix(p.id.as_str());
        assert!(
            rest.is_some_and(|r| r.starts_with('.') || r.starts_with(':')),
            "[{label}] nesting contract: child '{id}' must extend parent '{}' with '.' (scope) or ':' (sub-node)",
            p.id
        );
        // Sub-nodes that address INSIDE the symbol must lie within it (`:doc` is the exception —
        // a leading comment sits above the declaration by definition).
        if rest.is_some_and(|r| r.starts_with(':')) && !id.ends_with(":doc") {
            assert!(
                node.range.start_line >= p.range.start_line && node.range.end_line <= p.range.end_line,
                "[{label}] containment contract: '{id}' {:?} escapes its symbol '{}' {:?}",
                node.range,
                p.id,
                p.range
            );
        }
    }
    if let (Some(name), Some(nr)) = (&node.name, &node.name_range) {
        let s = ci_core::text::byte_offset(content, nr.start_line, nr.start_char);
        let e = ci_core::text::byte_offset(content, nr.end_line, nr.end_char);
        let sliced = s.zip(e).and_then(|(s, e)| content.get(s..e));
        assert_eq!(
            sliced,
            Some(name.as_str()),
            "[{label}] name_range contract: '{}' must slice to the symbol's name {name:?} (got {sliced:?})",
            node.id
        );
    }
    for child in &node.children {
        check_node_invariants(label, file, content, child, Some(node));
    }
}

/// The read-path battery. Panics (test-style) on the first violated clause.
pub fn run_read_battery(mk: &dyn Fn(&Path) -> Box<dyn LanguageProvider>, fx: &ReadFixture) {
    let label = fx.label;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root, fx.files);
    let provider = mk(root);

    // gated() is the honesty flag: it must match the tier, not aspiration.
    assert_eq!(provider.gated(), fx.expect_gated, "[{label}] gated() must report the tier truthfully");

    // structure(): required ids, invariants, determinism.
    let nodes = provider.structure(Path::new(fx.target)).unwrap_or_else(|e| panic!("[{label}] structure({}) failed: {e}", fx.target));
    let content = std::fs::read_to_string(root.join(fx.target)).unwrap();
    let mut all = Vec::new();
    collect(&nodes, &mut all);
    let ids: Vec<&str> = all.iter().map(|n| n.id.as_str()).collect();
    for want in fx.want_ids {
        assert!(ids.contains(want), "[{label}] symbol contract: expected id '{want}' in {ids:?}");
    }
    for n in &nodes {
        check_node_invariants(label, fx.target, &content, n, None);
    }
    let again = provider.structure(Path::new(fx.target)).unwrap();
    assert_eq!(format!("{nodes:?}"), format!("{again:?}"), "[{label}] structure() must be deterministic");

    // Anchor contract: the named function carries the sub-nodes surgical edits target.
    let f = find(&nodes, fx.fn_symbol).unwrap_or_else(|| panic!("[{label}] fn symbol '{}' missing", fx.fn_symbol));
    assert!(
        f.children.iter().any(|c| c.id == format!("{}:body", fx.fn_symbol)),
        "[{label}] anchor contract: '{}' must expose a :body sub-node (set_body/insert_in_body target): {:?}",
        fx.fn_symbol,
        f.children.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
    if fx.expect_params {
        assert!(
            f.children.iter().any(|c| c.id == format!("{}:params", fx.fn_symbol)),
            "[{label}] anchor contract: '{}' must expose a :params sub-node (add_parameter target)",
            fx.fn_symbol
        );
    }
    if let Some(doc_sym) = fx.doc_symbol {
        let d = find(&nodes, doc_sym).unwrap_or_else(|| panic!("[{label}] doc symbol '{doc_sym}' missing"));
        assert!(
            d.children.iter().any(|c| c.id == format!("{doc_sym}:doc")),
            "[{label}] doc contract: '{doc_sym}' must expose its leading comment/docstring as :doc"
        );
    }

    // structure() of an unknown file must fail SOFT (empty or a clean error), never panic.
    let _ = provider.structure(Path::new("definitely_not_a_file.zz"));

    // import_graph(): deterministic; expected edge present, or honestly empty.
    let g = provider.import_graph().unwrap_or_else(|e| panic!("[{label}] import_graph() failed: {e}"));
    let g2 = provider.import_graph().unwrap();
    assert_eq!(format!("{g:?}"), format!("{g2:?}"), "[{label}] import_graph() must be deterministic");
    for from in g.keys() {
        assert!(
            from.is_relative(),
            "[{label}] graph contract: keys are repo-relative paths, got {from:?}"
        );
    }
    match fx.edge {
        Some((importer, imported)) => {
            let edges = g
                .get(Path::new(importer))
                .unwrap_or_else(|| panic!("[{label}] graph contract: no edges for '{importer}': {g:?}"));
            assert!(
                edges.iter().any(|e| e == Path::new(imported)),
                "[{label}] graph contract: expected edge {importer} -> {imported}, got {edges:?}"
            );
        }
        None => {
            assert!(
                g.is_empty(),
                "[{label}] graph contract: a language with no resolver must return an EMPTY graph \
                 (an invented edge is worse than none), got {g:?}"
            );
        }
    }
}

/// The edit-path battery (in-process tiers). Each clause runs on a FRESH fixture copy so a
/// committed edit can't mask a later atomicity check.
pub fn run_edit_battery(mk: &dyn Fn(&Path) -> Box<dyn LanguageProvider>, fx: &EditFixture) {
    let label = fx.label;
    let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
    let target_file = fx.target_symbol.split('#').next().unwrap().to_string();

    // 1. A clean replace_text COMMITS and lands on disk.
    {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), fx.files);
        let p = mk(dir.path());
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: fx.target_symbol.into(),
                    old_text: fx.clean.0.into(),
                    new_text: fx.clean.1.into(),
                }],
                &opts,
            )
            .unwrap_or_else(|e| panic!("[{label}] clean replace_text errored: {e}"));
        assert!(matches!(res, CommitResult::Ok { .. }), "[{label}] clean edit must commit: {res:?}");
        let after = std::fs::read_to_string(dir.path().join(&target_file)).unwrap();
        assert!(after.contains(fx.clean.1), "[{label}] committed edit must be on disk");
    }

    // 2. dry_run: the same edit reports Ok but disk is UNTOUCHED.
    {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), fx.files);
        let p = mk(dir.path());
        let before = std::fs::read_to_string(dir.path().join(&target_file)).unwrap();
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: fx.target_symbol.into(),
                    old_text: fx.clean.0.into(),
                    new_text: fx.clean.1.into(),
                }],
                &EditOpts { write: true, dry_run: true, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "[{label}] dry_run of a clean edit reports Ok: {res:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&target_file)).unwrap(),
            before,
            "[{label}] dry_run must never write"
        );
    }

    // 3. A parse-breaking edit REJECTS and disk is untouched (the syntax gate).
    {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), fx.files);
        let p = mk(dir.path());
        let before = std::fs::read_to_string(dir.path().join(&target_file)).unwrap();
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: fx.target_symbol.into(),
                    old_text: fx.breaks.0.into(),
                    new_text: fx.breaks.1.into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(
            matches!(res, CommitResult::Rejected { .. }),
            "[{label}] gate contract: an edit that no longer parses must reject: {res:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&target_file)).unwrap(),
            before,
            "[{label}] atomicity: a rejected edit must leave disk untouched"
        );
    }

    // 4. Batch atomicity: [clean, breaking] rejects as a WHOLE — the clean op must not land.
    {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), fx.files);
        let p = mk(dir.path());
        let before = std::fs::read_to_string(dir.path().join(&target_file)).unwrap();
        let res = p
            .apply_edits(
                &[
                    EditOp::ReplaceText {
                        node_id: fx.target_symbol.into(),
                        old_text: fx.clean.0.into(),
                        new_text: fx.clean.1.into(),
                    },
                    EditOp::ReplaceText {
                        node_id: fx.target_symbol.into(),
                        old_text: fx.breaks.0.into(),
                        new_text: fx.breaks.1.into(),
                    },
                ],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Rejected { .. }), "[{label}] a batch with a breaking op rejects: {res:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&target_file)).unwrap(),
            before,
            "[{label}] batch atomicity: NOTHING from a rejected batch lands (the clean op included)"
        );
    }

    // 5. A missing anchor fails soft: precise error naming the node, no partial write.
    {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), fx.files);
        let p = mk(dir.path());
        let res = p.apply_edits(
            &[EditOp::ReplaceText {
                node_id: format!("{target_file}#no_such_symbol"),
                old_text: "x".into(),
                new_text: "y".into(),
            }],
            &opts,
        );
        let ok_shape = match res {
            Ok(CommitResult::Rejected { .. }) | Err(_) => true,
            other => panic!("[{label}] a missing anchor must reject or error, got {other:?}"),
        };
        assert!(ok_shape);
    }
}
