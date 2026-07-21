//! lang-swift — the Swift [`LanguageProvider`] at the GATED tier (rollout ladder Step 1,
//! assembled exactly like `lang-template`/`lang-java`/`lang-php`): generic tree-sitter reads
//! from [`lang_fallback::FallbackProvider`] × a real compiler gate, glued by [`Composed`].
//!
//! The write engine has two halves that never trade jobs:
//! - **verdict**: `swift build` (`src/gate.rs`). NOT `swiftc -typecheck` — the Swift core team
//!   refutes typecheck-only as a gate because it misses SIL-phase diagnostics
//!   (definite-initialization, some exhaustiveness), i.e. it is UNSOUND: the exact false-clean
//!   failure the contract exists to prevent. `swift build` compiles the SwiftPM package, and its
//!   diagnostics are GCC-style text on stderr (no JSON) — regex-parsed to file:line:col.
//! - **rename**: sourcekit-lsp (ships with the toolchain), started lazily on the first rename
//!   only. Rename is production-quality since Swift 6 (cross-file, index-backed by IndexStoreDB,
//!   background indexing on by default since 6.1). sourcekit-lsp is SwiftPM-only (no Xcode
//!   projects); `willRenameFiles` is REFUTED (no handler), so moves fall to the §8 hooks.
//!
//! Reads, anchors, the (honestly empty) import graph, and outlines all come from the shared
//! fallback grammar tables — this crate holds ONLY the Swift-specific engine (contract §7). Swift
//! is SUFFIX-typed (`func f() -> Int`), so `set_return_type` is NOT refused (unlike Java): the
//! registry's `return_type_suffix` marker is `true`, and the shared spine splices ` -> T` after
//! the `)`.
//!
//! The module model is the structurally novel part (rollout spec, Swift section): imports are
//! MODULE-level and SwiftPM targets glob directories, so there is no within-target file→file
//! edge to extract (the honest empty graph, §3) and the §8 move hooks are near-no-ops — only
//! cross-target moves touch `Package.swift`. Swift is the degenerate-case validator that proves
//! the hooks may legally be no-ops.
use ci_core::{CommitResult, EditOp, EditOpts, Granularity, ImportGraph, LanguageProvider, Node, Result};
use ci_edit::{Composed, EngineFactory, GateEngine};
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

mod gate;
mod movefix;
mod sourcekit;

/// What a bare `move_file` covers for Swift — composed into the `apply_edits` description by
/// ci-mcp, so the completeness claim the agent reads lives NEXT TO the code that makes it true
/// (movefix + the `swift build` gate). Keep it one sentence fragment.
pub const MOVE_COVERAGE: &str = "within a target no reference rewrites are needed (imports are module-level); a cross-target move updates Package.swift membership, and the `swift build` gate judges the result";

/// The gated Swift provider. `gated()` is `true` because construction goes through the registry's
/// `swift` check ([`gate_missing`]) — a missing Swift toolchain disables the language with the
/// install hint (`ProviderBuild::Unavailable`), it never ships an ungated Swift silently.
pub struct SwiftProvider {
    inner: Composed<FallbackProvider>,
}

impl SwiftProvider {
    pub fn new(root: &Path) -> Self {
        Self::with_factory(root, engine_factory())
    }

    /// The assembly with an injected gate — what the conformance fast tier drives with a
    /// scripted checker, so the wiring is provable without a Swift toolchain on the machine.
    pub fn with_factory(root: &Path, factory: EngineFactory) -> Self {
        Self { inner: Composed::new(root, FallbackProvider::new(root, FbLang::Swift), factory) }
    }
}

impl LanguageProvider for SwiftProvider {
    fn granularity(&self) -> Granularity {
        self.inner.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.inner.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.inner.import_graph()
    }

    fn gated(&self) -> bool {
        true
    }

    fn prewarm(&self) {
        self.inner.prewarm()
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        self.inner.apply_edits(ops, opts)
    }
}

/// Builds the write engine (lazily in `apply_edits`, or off-thread via prewarm). When the
/// toolchain itself is the problem, say THAT with the install hint instead of a raw spawn error —
/// reads worked fine, so this is the user's first signal the WRITE path is missing a dependency.
fn engine_factory() -> EngineFactory {
    Arc::new(|root: &Path| {
        if let Some(missing) = gate_missing() {
            return Err(ci_core::Error::Driver(format!("swift edit engine unavailable.\n{missing}")));
        }
        Ok(Box::new(gate::SwiftEngine::new(root, ci_core::resolve_sandbox(root, "peashooter-swift")))
            as Box<dyn GateEngine + Send>)
    })
}

/// What the Swift provider needs from the machine, honestly scoped per tool: `swift` is the GATE
/// (the provider is off without it — see [`gate_missing`]); sourcekit-lsp is rename only and
/// optional (reads and the `swift build` gate work without it, a rename explains itself). Both
/// ship together in a Swift toolchain, but they are probed separately so the scoping stays honest.
pub fn toolchain() -> ci_core::ToolchainReport {
    ci_core::ToolchainReport {
        lang: "swift",
        tools: vec![
            swift_status(),
            ci_core::ToolStatus {
                tool: "sourcekit-lsp",
                needed_for: "cross-file rename (reads and the `swift build` gate work without it); SwiftPM projects only",
                install: sourcekit::INSTALL_HINT,
                found: sourcekit::sourcekit_binary().map(|p| p.to_string_lossy().into_owned()),
            },
        ],
    }
}

fn swift_status() -> ci_core::ToolStatus {
    ci_core::ToolStatus {
        tool: "swift (6.0+)",
        needed_for: "the type-check gate (`swift build` over the SwiftPM package) — the swift provider is disabled without it",
        install: "a Swift 6+ toolchain, e.g. Xcode / the Swift.org toolchain, or `swiftly install` (https://www.swift.org/install)",
        found: ci_core::probe_tool(Command::new("swift").arg("--version")),
    }
}

/// The REQUIRED half of [`toolchain`] — `swift` alone — for the registry builders: this is what
/// turns into `ProviderBuild::Unavailable` (contract §6). sourcekit-lsp stays out of it so a
/// machine with a toolchain but no separate LSP still gets gated Swift edits (in practice the LSP
/// ships alongside `swift`, but the scoping mirrors Java/PHP: the gate is the hard requirement).
pub fn gate_missing() -> Option<String> {
    ci_core::ToolchainReport { lang: "swift", tools: vec![swift_status()] }.describe_missing()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CLASS: &str =
        "class Svc {\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\n";
    const OPTS: EditOpts = EditOpts { write: true, dry_run: false, tsconfig: None };

    /// A minimal SwiftPM package around `sources` (repo-relative `.swift` paths under
    /// `Sources/<target>/`), so `swift build` has a package to compile. The manifest names one
    /// executable target; `main.swift` gives it an entry point.
    fn write_package(target: &str, sources: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Package.swift"),
            format!(
                "// swift-tools-version:5.9\nimport PackageDescription\n\nlet package = Package(\n  name: \"{target}\",\n  targets: [ .executableTarget(name: \"{target}\", path: \"Sources/{target}\") ]\n)\n"
            ),
        )
        .unwrap();
        for (rel, content) in sources {
            let abs = root.join(rel);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, content).unwrap();
        }
        dir
    }

    fn write_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let abs = dir.path().join(rel);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, content).unwrap();
        }
        dir
    }

    // ── fast tier: the assembly, provable without a Swift toolchain ──────────────────────

    fn mock_provider(root: &Path) -> SwiftProvider {
        SwiftProvider::with_factory(root, lang_template::mock::factory())
    }

    // The template promise, kept by the copy: tree-sitter reads with Swift's qualified ids, a
    // gate-flagged edit rejects ATOMICALLY, a clean edit commits.
    #[test]
    fn assembly_reads_gate_and_commit() {
        let dir = write_repo(&[("Svc.swift", CLASS)]);
        let root = dir.path();
        let p = mock_provider(root);
        assert!(p.gated(), "the gated tier reports itself");
        assert!(p
            .structure(Path::new("Svc.swift"))
            .unwrap()
            .iter()
            .any(|n| n.id == "Svc.swift#Svc"));

        let before = fs::read_to_string(root.join("Svc.swift")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.swift#Svc.probe".into(),
                    old_text: "return 1".into(),
                    new_text: format!("return 1 // {}", lang_template::mock::MARKER),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "checker-flagged edit rejects: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("Svc.swift")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.swift#Svc.probe".into(),
                    old_text: "return 1".into(),
                    new_text: "return 2".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(fs::read_to_string(root.join("Svc.swift")).unwrap().contains("return 2"));
    }

    // Q2 through the provider: Swift is SUFFIX-typed, so `set_return_type` is NOT refused — the
    // handler splices ` -> T` after the `)` (the registry's `return_type_suffix = true`). This is
    // the OPPOSITE of Java's refuse-with-recipe; the fixture pins the correct output.
    #[test]
    fn set_return_type_appends_suffix_arrow_type() {
        let dir = write_repo(&[(
            "Svc.swift",
            "class Svc {\n  func probe(url: String) {\n    doWork()\n  }\n}\n",
        )]);
        let p = mock_provider(dir.path());
        let res = p
            .apply_edits(
                &[EditOp::SetReturnType { node_id: "Svc.swift#Svc.probe".into(), ty: "Int".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "suffix-typed set_return_type commits: {res:?}");
        let after = fs::read_to_string(dir.path().join("Svc.swift")).unwrap();
        assert!(
            after.contains("func probe(url: String) -> Int"),
            "return type appended as ` -> Int` after the params: {after}"
        );
    }

    // The toolchain surface the registry/doctor consume: `swift` is the only REQUIRED tool
    // (gate_missing), sourcekit-lsp is reported but optional.
    #[test]
    fn toolchain_scopes_swift_required_sourcekit_optional() {
        let report = toolchain();
        assert_eq!(report.lang, "swift");
        let tools: Vec<&str> = report.tools.iter().map(|t| t.tool).collect();
        assert!(tools.iter().any(|t| t.contains("swift ")), "swift probed: {tools:?}");
        assert!(tools.contains(&"sourcekit-lsp"), "sourcekit-lsp probed: {tools:?}");
        // On this machine swift exists, so the gate requirement is satisfied — and gate_missing
        // must NOT trip over the optional sourcekit-lsp.
        if report.tools.iter().find(|t| t.tool.contains("swift ")).unwrap().found.is_some() {
            assert_eq!(gate_missing(), None, "sourcekit-lsp's presence/absence must not disable the gate");
        } else {
            assert!(gate_missing().unwrap().contains("Install"), "actionable when swift is missing");
        }
    }

    // ── real-tool tier (#[ignore]): the `swift build` gate, run with `-- --ignored` ──────
    // These need a Swift toolchain (`swift`/`swiftc`/sourcekit-lsp; present on this machine).

    // Gate soundness in one arc: an edit that PARSES but breaks the types rejects (the verdict is
    // `swift build`, not the grammar), disk stays byte-identical, and the follow-up clean edit
    // commits. Needs a real SwiftPM package for `swift build` to have something to compile.
    #[test]
    #[ignore]
    fn swiftbuild_gate_rejects_type_error_and_accepts_clean() {
        let dir = write_package(
            "App",
            &[(
                "Sources/App/main.swift",
                "func probe(url: String) -> Int {\n    return 1\n}\nprint(probe(url: \"x\"))\n",
            )],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let target = "Sources/App/main.swift";

        let before = fs::read_to_string(root.join(target)).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: format!("{target}#probe"),
                    old_text: "return 1".into(),
                    new_text: "return \"broken\"".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type-breaking (but parseable) edit must reject: {bad:?}");
        assert_eq!(fs::read_to_string(root.join(target)).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: format!("{target}#probe"),
                    old_text: "return 1".into(),
                    new_text: "return 2".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(fs::read_to_string(root.join(target)).unwrap().contains("return 2"));
    }

    // G4 honesty: a `.swift` file OUTSIDE every SwiftPM target cannot be type-checked by
    // `swift build`, so an edit to it must be REFUSED — never committed under a false
    // "type-verified" claim (the gate would otherwise build clean, ignoring the file entirely).
    #[test]
    #[ignore]
    fn edit_to_file_outside_any_target_is_refused() {
        let dir = write_package(
            "App",
            &[
                ("Sources/App/main.swift", "print(\"hi\")\n"),
                // An orphan file NOT under Sources/App — SwiftPM never compiles it.
                ("Scratch/Orphan.swift", "func orphan() -> Int {\n    return 1\n}\n"),
            ],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let before = fs::read_to_string(root.join("Scratch/Orphan.swift")).unwrap();
        let res = p.apply_edits(
            &[EditOp::ReplaceText {
                node_id: "Scratch/Orphan.swift#orphan".into(),
                old_text: "return 1".into(),
                new_text: "return 2".into(),
            }],
            &OPTS,
        );
        assert!(res.is_err(), "an edit the gate cannot type-check must be refused, not committed: {res:?}");
        assert!(
            res.unwrap_err().to_string().contains("not in any SwiftPM target"),
            "the refusal names the reason so the agent can act"
        );
        assert_eq!(
            fs::read_to_string(root.join("Scratch/Orphan.swift")).unwrap(),
            before,
            "a refused edit leaves disk byte-identical"
        );
    }

    // Batch atomicity under the REAL gate: one type-breaking op sinks the whole batch — the clean
    // op must not land either.
    #[test]
    #[ignore]
    fn swiftbuild_gate_batch_is_atomic() {
        let dir = write_package(
            "App",
            &[(
                "Sources/App/main.swift",
                "func probe() -> Int {\n    return 1\n}\nfunc count() -> Int {\n    return 0\n}\nprint(probe() + count())\n",
            )],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let target = "Sources/App/main.swift";
        let before = fs::read_to_string(root.join(target)).unwrap();
        let res = p
            .apply_edits(
                &[
                    EditOp::ReplaceText {
                        node_id: format!("{target}#count"),
                        old_text: "return 0".into(),
                        new_text: "return 10".into(),
                    },
                    EditOp::ReplaceText {
                        node_id: format!("{target}#probe"),
                        old_text: "return 1".into(),
                        new_text: "return \"broken\"".into(),
                    },
                ],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Rejected { .. }), "batch with a breaking op rejects: {res:?}");
        assert_eq!(
            fs::read_to_string(root.join(target)).unwrap(),
            before,
            "NOTHING from a rejected batch lands (the clean op included)"
        );
    }

    // Contract clause 5 with the real compiler: PRE-EXISTING breakage (a type error already on
    // disk) is baseline — it never blocks an unrelated clean edit, and the commit result CARRIES
    // it instead of claiming a clean radius.
    #[test]
    #[ignore]
    fn swiftbuild_gate_baseline_excuses_preexisting_breakage() {
        let dir = write_package(
            "App",
            &[(
                "Sources/App/main.swift",
                // `broken` is already type-invalid BEFORE the batch (returns String from an Int fn).
                "func run() -> Int {\n    return 1\n}\nfunc broken() -> Int {\n    return \"oops\"\n}\nprint(run())\n",
            )],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let target = "Sources/App/main.swift";
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: format!("{target}#run"),
                    old_text: "return 1".into(),
                    new_text: "return 2".into(),
                }],
                &OPTS,
            )
            .unwrap();
        match &res {
            CommitResult::Ok { preexisting_in_radius, .. } => {
                assert!(
                    !preexisting_in_radius.is_empty(),
                    "the excused breakage is CARRIED, not hidden: {preexisting_in_radius:?}"
                );
            }
            other => panic!("pre-existing breakage must not block an unrelated edit: {other:?}"),
        }
        assert!(fs::read_to_string(root.join(target)).unwrap().contains("return 2"));
    }

    // Contract clause 2 through Composed: a committed edit is visible to structure() in the same
    // session with no manual reindex (the tree-sitter reader is live; this pins that the GLUE
    // keeps it that way for Swift).
    #[test]
    #[ignore]
    fn committed_edit_refreshes_reads_in_session() {
        let dir = write_package(
            "App",
            &[("Sources/App/main.swift", "struct Svc {\n  func probe() -> Int {\n    return 1\n  }\n}\nprint(Svc().probe())\n")],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let target = "Sources/App/main.swift";
        assert!(
            !p.structure(Path::new(target)).unwrap().iter().any(|n| n.id == format!("{target}#Svc.ping")),
            "ping does not exist yet"
        );
        let res = p
            .apply_edits(
                &[EditOp::InsertMember {
                    node_id: format!("{target}#Svc"),
                    code: "func ping() -> Int {\n    return 2\n  }".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "member insert commits: {res:?}");
        assert!(
            p.structure(Path::new(target)).unwrap().iter().any(|n| n
                .children
                .iter()
                .any(|c| c.id == format!("{target}#Svc.ping"))
                || n.id == format!("{target}#Svc.ping")),
            "reads reflect the committed member without a reindex"
        );
    }

    // Requires sourcekit-lsp (ships with the toolchain; present here). The rename must land
    // cross-file, index-backed, over a SwiftPM package. SKIPS loudly if the binary is absent so a
    // machine without the LSP still runs the rest of the `--ignored` tier.
    #[test]
    #[ignore]
    fn sourcekit_rename_lands_cross_file() {
        if sourcekit::sourcekit_binary().is_none() {
            eprintln!("SKIP: sourcekit-lsp not installed — {}", sourcekit::INSTALL_HINT);
            return;
        }
        let dir = write_package(
            "App",
            &[
                ("Sources/App/Util.swift", "struct Util {\n  static func base() -> Int {\n    return 1\n  }\n}\n"),
                ("Sources/App/main.swift", "func run() -> Int {\n    return Util.base()\n}\nprint(run())\n"),
            ],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "Sources/App/Util.swift#Util.base".into(), new_name: "fetchBase".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the gate: {res:?}");
        assert!(
            fs::read_to_string(root.join("Sources/App/Util.swift")).unwrap().contains("fetchBase"),
            "definition renamed"
        );
        assert!(
            fs::read_to_string(root.join("Sources/App/main.swift")).unwrap().contains("Util.fetchBase()"),
            "REFERENCE rewritten cross-file — the reason sourcekit-lsp exists here"
        );
    }

    // An engine whose toolchain runs inside the `peashooter-swift` OCI container.
    fn oci_swift_factory() -> EngineFactory {
        Arc::new(|root: &Path| {
            let sandbox: Arc<dyn ci_core::Sandbox> = Arc::new(ci_core::OciSandbox::new(
                root.to_path_buf(),
                ci_core::oci_runtime().expect("an OCI runtime on PATH"),
                "peashooter-swift".into(),
            ));
            Ok(Box::new(gate::SwiftEngine::new(root, sandbox))
                as Box<dyn GateEngine + Send>)
        })
    }

    // Requires docker (or another OCI runtime) up AND the swift image:
    //   docker build -f docker/peashooter-swift.Dockerfile -t peashooter-swift docker/
    // `swift build` gate AND the sourcekit-lsp rename both run in the container, so this passes
    // with NO host swift toolchain. Swift-on-Linux differs slightly from macOS; the rename must
    // still land cross-file.
    #[test]
    #[ignore]
    fn oci_swift_gate_and_rename_without_host_tools() {
        if ci_core::oci_runtime().is_none() {
            eprintln!("SKIP: no OCI runtime (docker/podman/nerdctl/container) on PATH");
            return;
        }
        let dir = write_package(
            "App",
            &[
                ("Sources/App/Util.swift", "struct Util {\n  static func base() -> Int {\n    return 1\n  }\n}\n"),
                ("Sources/App/main.swift", "func run() -> Int {\n    return Util.base()\n}\nprint(run())\n"),
            ],
        );
        let root = dir.path();
        let p = SwiftProvider::with_factory(root, oci_swift_factory());
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "Sources/App/Util.swift#Util.base".into(), new_name: "fetchBase".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the CONTAINER gate: {res:?}");
        assert!(fs::read_to_string(root.join("Sources/App/Util.swift")).unwrap().contains("fetchBase"), "definition renamed");
        assert!(
            fs::read_to_string(root.join("Sources/App/main.swift")).unwrap().contains("Util.fetchBase()"),
            "cross-file reference rewritten by the CONTAINER's sourcekit-lsp — no host swift consulted"
        );
    }

    // The degenerate §8 case PROVEN end-to-end (the JSON-shape unit test only pins the hooks;
    // this pins the contract): a committed within-target `MoveFile` leaves a COMPILING package.
    // Swift imports are module-level, so no reference rewrite is needed — the moved file stays
    // in the same target (SwiftPM globs the target dir recursively, so a subdirectory is still
    // in `App`), `main.swift` still resolves `Util` unchanged, and the real `swift build` gate
    // accepts the result. This is Swift's analogue of lang-rust's committed-move-compiles pin;
    // it is trivially clean here because the import graph is empty by design — the compiler,
    // not a rewrite, is the safety net.
    #[test]
    #[ignore]
    fn committed_within_target_move_compiles_with_no_rewrites() {
        let dir = write_package(
            "App",
            &[
                ("Sources/App/Util.swift", "struct Util {\n  static func base() -> Int {\n    return 1\n  }\n}\n"),
                ("Sources/App/main.swift", "func run() -> Int {\n    return Util.base()\n}\nprint(run())\n"),
            ],
        );
        let root = dir.path();
        let p = SwiftProvider::new(root);
        let main_before = fs::read_to_string(root.join("Sources/App/main.swift")).unwrap();
        let res = p
            .apply_edits(
                &[EditOp::MoveFile {
                    from: "Sources/App/Util.swift".into(),
                    to: "Sources/App/core/Util.swift".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(
            matches!(res, CommitResult::Ok { .. }),
            "within-target move commits through the real swift build gate: {res:?}"
        );
        assert!(!root.join("Sources/App/Util.swift").exists(), "moved off the old path");
        assert!(root.join("Sources/App/core/Util.swift").exists(), "landed at the new path");
        assert_eq!(
            fs::read_to_string(root.join("Sources/App/main.swift")).unwrap(),
            main_before,
            "no reference rewrite: module-level imports leave the importer byte-identical"
        );
    }
}
