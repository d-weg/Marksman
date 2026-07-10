//! lang-java — the Java [`LanguageProvider`] at the GATED tier (rollout ladder Step 1,
//! assembled exactly like `lang-template`): generic tree-sitter reads from
//! [`lang_fallback::FallbackProvider`] × a real compiler gate, glued by [`Composed`].
//!
//! The write engine has two halves that never trade jobs:
//! - **verdict**: a RESIDENT `javax.tools.JavaCompiler` sidecar (`src/GateSidecar.java`,
//!   launched as `java GateSidecar.java` — JEP 330). javac ships no structured CLI
//!   diagnostics and Maven/Gradle have no per-edit-cheap compile, but javax.tools IS javac
//!   in-process: a `DiagnosticListener` yields kind/source/line/col/code with no text
//!   parsing, and the JVM stays warm across edits. Classpath policy (decision Q3) lives in
//!   [`gate::derive_paths`]: build-tool-derived when pom.xml/build.gradle AND the tool are
//!   present, flat source-root otherwise.
//! - **rename/willRename**: jdtls (the de-facto Java LSP), started lazily on the first
//!   rename/move only. jdtls is push-diagnostics-only, so it never serves the gate verdict;
//!   see `jdtls.rs` for the Java-21+ runtime selection its launcher needs.
//!
//! Reads, anchors, the (honestly empty) import graph, and outlines all come from the shared
//! fallback grammar tables — this crate holds ONLY the Java-specific engine (contract §7).
//! `set_return_type` on Java refuses with a recipe at the spine level (decision Q2, keyed by
//! the registry's `return_type_suffix` marker), not here.
use ci_core::{CommitResult, EditOp, EditOpts, Granularity, ImportGraph, LanguageProvider, Node, Result};
use ci_edit::{Composed, EngineFactory, GateEngine};
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

mod gate;
mod jdtls;
mod movefix;

/// The gated Java provider. `gated()` is `true` because construction goes through the
/// registry's javac check ([`gate_missing`]) — a missing JDK disables the language with the
/// install hint (`ProviderBuild::Unavailable`), it never ships an ungated Java silently.
pub struct JavaProvider {
    inner: Composed<FallbackProvider>,
}

impl JavaProvider {
    pub fn new(root: &Path) -> Self {
        Self::with_factory(root, engine_factory())
    }

    /// The assembly with an injected gate — what the conformance fast tier drives with a
    /// scripted checker, so the wiring is provable without a JDK on the machine.
    pub fn with_factory(root: &Path, factory: EngineFactory) -> Self {
        Self { inner: Composed::new(root, FallbackProvider::new(root, FbLang::Java), factory) }
    }
}

impl LanguageProvider for JavaProvider {
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
/// toolchain itself is the problem, say THAT with the install hint instead of a raw spawn
/// error — reads worked fine, so this is the user's first signal the WRITE path is missing
/// a dependency.
fn engine_factory() -> EngineFactory {
    Arc::new(|root: &Path| {
        let sandbox = ci_core::resolve_sandbox(root, "marksman-java");
        let sidecar = gate::JavacSidecar::start(root, &*sandbox).map_err(|e| match gate_missing() {
            Some(missing) => {
                ci_core::Error::Driver(format!("java edit engine failed to start ({e}).\n{missing}"))
            }
            None => e,
        })?;
        Ok(Box::new(gate::JavaEngine::new(root, sidecar, sandbox)) as Box<dyn GateEngine + Send>)
    })
}

/// What the Java provider needs from the machine, honestly scoped per tool: the JDK is the
/// GATE (the provider is off without it — see [`gate_missing`]); jdtls is rename/move only
/// and optional (reads and the type-check gate work without it, a rename explains itself).
pub fn toolchain() -> ci_core::ToolchainReport {
    ci_core::ToolchainReport {
        lang: "java",
        tools: vec![
            javac_status(),
            ci_core::ToolStatus {
                tool: "jdtls",
                needed_for: "cross-file rename / move rewrites (reads and the javac gate work without it)",
                install: jdtls::INSTALL_HINT,
                found: jdtls::jdtls_binary().map(|p| p.to_string_lossy().into_owned()),
            },
        ],
    }
}

fn javac_status() -> ci_core::ToolStatus {
    ci_core::ToolStatus {
        tool: "javac (JDK 17+)",
        needed_for: "the type-check gate (resident javax.tools sidecar) — the java provider is disabled without it",
        install: "a JDK 17 or newer, e.g. `brew install openjdk@21` or https://adoptium.net",
        found: ci_core::probe_tool(Command::new("javac").arg("-version")),
    }
}

/// The REQUIRED half of [`toolchain`] — javac alone — for the registry builders: this is
/// what turns into `ProviderBuild::Unavailable` (contract §6). jdtls stays out of it so a
/// machine without the LSP still gets gated Java edits.
pub fn gate_missing() -> Option<String> {
    ci_core::ToolchainReport { lang: "java", tools: vec![javac_status()] }.describe_missing()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CLASS: &str =
        "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n}\n";
    const OPTS: EditOpts = EditOpts { write: true, dry_run: false, tsconfig: None };

    fn write_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let abs = dir.path().join(rel);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, content).unwrap();
        }
        dir
    }

    // ── fast tier: the assembly, provable without a JDK ─────────────────────────────────

    /// The fast tier's gate is the SHARED template mock (flags any buffer containing `MARKER`) —
    /// no Java-specific reason for a local copy, so lang-java speaks the same scaffold as PHP/Swift
    /// and the conformance suite (§7).
    use lang_template::mock::MARKER;

    fn mock_provider(root: &Path) -> JavaProvider {
        JavaProvider::with_factory(root, lang_template::mock::factory())
    }

    // The template promise, kept by the copy: tree-sitter reads with Java's qualified ids,
    // a gate-flagged edit rejects ATOMICALLY, a clean edit commits.
    #[test]
    fn assembly_reads_gate_and_commit() {
        let dir = write_repo(&[("Svc.java", CLASS)]);
        let root = dir.path();
        let p = mock_provider(root);
        assert!(p.gated(), "the gated tier reports itself");
        assert!(p
            .structure(Path::new("Svc.java"))
            .unwrap()
            .iter()
            .any(|n| n.id == "Svc.java#Svc"));

        let before = fs::read_to_string(root.join("Svc.java")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.java#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: format!("return 1; // {MARKER}"),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "checker-flagged edit rejects: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("Svc.java")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.java#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return 2;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(fs::read_to_string(root.join("Svc.java")).unwrap().contains("return 2;"));
    }

    // Q2 through the provider: `set_return_type` on prefix-typed Java rejects with the
    // spine's replace_text recipe (the registry marker feeds ci-edit; this pins the
    // integration, the unit tests live in ci-build/ci-edit).
    #[test]
    fn set_return_type_rejects_with_replace_text_recipe() {
        let dir = write_repo(&[("Svc.java", CLASS)]);
        let p = mock_provider(dir.path());
        let res = p
            .apply_edits(
                &[EditOp::SetReturnType { node_id: "Svc.java#Svc.probe".into(), ty: "long".into() }],
                &OPTS,
            )
            .unwrap();
        match res {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("BEFORE the name"), "explains the refusal: {feedback}");
                assert!(feedback.contains("replace_text"), "carries the recipe: {feedback}");
            }
            other => panic!("prefix-typed set_return_type must reject: {other:?}"),
        }
        assert!(
            fs::read_to_string(dir.path().join("Svc.java")).unwrap().contains("public int probe"),
            "nothing written"
        );
    }

    // The toolchain surface the registry/doctor consume: javac is the only REQUIRED tool
    // (gate_missing), jdtls is reported but optional.
    #[test]
    fn toolchain_scopes_javac_required_jdtls_optional() {
        let report = toolchain();
        assert_eq!(report.lang, "java");
        let tools: Vec<&str> = report.tools.iter().map(|t| t.tool).collect();
        assert!(tools.iter().any(|t| t.contains("javac")), "javac probed: {tools:?}");
        assert!(tools.contains(&"jdtls"), "jdtls probed: {tools:?}");
        let jdtls_tool = report.tools.iter().find(|t| t.tool == "jdtls").unwrap();
        assert!(jdtls_tool.install.contains("brew install jdtls"), "actionable hint: {}", jdtls_tool.install);
        // On this machine javac exists, so the gate requirement is satisfied — and
        // gate_missing must NOT trip over the (absent) optional jdtls.
        if report.tools.iter().find(|t| t.tool.contains("javac")).unwrap().found.is_some() {
            assert_eq!(gate_missing(), None, "jdtls's absence must not disable the gate");
        } else {
            assert!(gate_missing().unwrap().contains("Install"), "actionable when javac is missing");
        }
    }

    // ── real-tool tier (#[ignore]): the javax.tools gate, run with `-- --ignored` ───────
    // These need `javac`/`java` on PATH (any JDK 17+; present on this machine).

    // Gate soundness in one arc: an edit that PARSES but breaks the types rejects (the
    // verdict is the compiler, not the grammar), disk stays byte-identical, and the
    // follow-up clean edit commits through the same resident sidecar.
    #[test]
    #[ignore]
    fn javac_gate_rejects_type_error_and_accepts_clean() {
        let dir = write_repo(&[("Svc.java", CLASS)]);
        let root = dir.path();
        let p = JavaProvider::new(root);

        let before = fs::read_to_string(root.join("Svc.java")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.java#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return \"broken\";".into(),
                }],
                &OPTS,
            )
            .unwrap();
        match &bad {
            CommitResult::Rejected { feedback, .. } => assert!(
                feedback.contains("incompatible types"),
                "the javac diagnostic reaches the reply: {feedback}"
            ),
            other => panic!("type-breaking (but parseable) edit must reject: {other:?}"),
        }
        assert_eq!(fs::read_to_string(root.join("Svc.java")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.java#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return 2;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(fs::read_to_string(root.join("Svc.java")).unwrap().contains("return 2;"));
    }

    // Batch atomicity under the REAL gate: one type-breaking op sinks the whole batch —
    // the clean op must not land either.
    #[test]
    #[ignore]
    fn javac_gate_batch_is_atomic() {
        let dir = write_repo(&[(
            "Svc.java",
            "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n\n  public int count() {\n    return 0;\n  }\n}\n",
        )]);
        let root = dir.path();
        let p = JavaProvider::new(root);
        let before = fs::read_to_string(root.join("Svc.java")).unwrap();
        let res = p
            .apply_edits(
                &[
                    EditOp::ReplaceText {
                        node_id: "Svc.java#Svc.count".into(),
                        old_text: "return 0;".into(),
                        new_text: "return 10;".into(),
                    },
                    EditOp::ReplaceText {
                        node_id: "Svc.java#Svc.probe".into(),
                        old_text: "return 1;".into(),
                        new_text: "return \"broken\";".into(),
                    },
                ],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Rejected { .. }), "batch with a breaking op rejects: {res:?}");
        assert_eq!(
            fs::read_to_string(root.join("Svc.java")).unwrap(),
            before,
            "NOTHING from a rejected batch lands (the clean op included)"
        );
    }

    // Contract clause 5 with the real compiler: PRE-EXISTING breakage (a type error in a
    // file the batch never touches, pulled in via -sourcepath) is baseline — it never
    // blocks an unrelated clean edit, and the commit result CARRIES it instead of
    // claiming a clean radius.
    #[test]
    #[ignore]
    fn javac_gate_baseline_excuses_preexisting_breakage() {
        let dir = write_repo(&[
            (
                "App.java",
                "public class App {\n  public int run() {\n    return Util.base();\n  }\n}\n",
            ),
            // Already broken BEFORE the batch: returns String from an int method.
            ("Util.java", "public class Util {\n  public static int base() {\n    return \"oops\";\n  }\n}\n"),
        ]);
        let root = dir.path();
        let p = JavaProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "App.java#App.run".into(),
                    old_text: "return Util.base();".into(),
                    new_text: "return Util.base() + 1;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        match &res {
            CommitResult::Ok { preexisting_in_radius, .. } => {
                assert!(
                    preexisting_in_radius.iter().any(|d| d.message.contains("incompatible types")),
                    "the excused breakage is CARRIED, not hidden: {preexisting_in_radius:?}"
                );
            }
            other => panic!("pre-existing breakage must not block an unrelated edit: {other:?}"),
        }
        assert!(fs::read_to_string(root.join("App.java")).unwrap().contains("Util.base() + 1"));
    }

    // §8 committed-move with the real compiler (T2): a cross-package `MoveFile` COMMITS and the
    // result COMPILES — the movefix fallback (jdtls absent) rewrites the moved file's `package`
    // line AND the importer's `import`, and the javac gate proves the rewrite is correct, not just
    // plausible. Java's move actually rewrites two things, so this is the load-bearing coverage.
    #[test]
    #[ignore]
    fn committed_cross_package_move_compiles() {
        let dir = write_repo(&[
            (
                "src/main/java/com/x/Helper.java",
                "package com.x;\npublic class Helper {\n  public static int base() { return 1; }\n}\n",
            ),
            (
                "src/main/java/com/x/App.java",
                "package com.x;\nimport com.x.Helper;\npublic class App {\n  public int run() { return Helper.base(); }\n}\n",
            ),
        ]);
        let root = dir.path();
        let p = JavaProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::MoveFile {
                    from: "src/main/java/com/x/Helper.java".into(),
                    to: "src/main/java/com/y/Helper.java".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "the moved+rewritten project compiles: {res:?}");
        assert!(!root.join("src/main/java/com/x/Helper.java").exists(), "old path gone");
        let moved = fs::read_to_string(root.join("src/main/java/com/y/Helper.java")).unwrap();
        assert!(moved.contains("package com.y;"), "moved file's package rewritten: {moved}");
        let app = fs::read_to_string(root.join("src/main/java/com/x/App.java")).unwrap();
        assert!(app.contains("import com.y.Helper;"), "importer retargeted to the new FQN: {app}");
    }

    // §8 deletion soundness (T3): deleting a class whose importer still references it yields a
    // diagnostic through the REAL Java hooks (resolver + ref scanner) — the gap-fill the engine
    // wires into `diagnostics()`. Pure hooks, no toolchain needed.
    #[test]
    fn deleted_class_still_imported_is_flagged() {
        let dir = write_repo(&[
            ("src/main/java/com/x/Helper.java", "package com.x;\npublic class Helper {}\n"),
            ("src/main/java/com/x/App.java", "package com.x;\nimport com.x.Helper;\npublic class App {}\n"),
        ]);
        let root = dir.path();
        // The batch DELETES Helper (empty-content buffer = the spine's deletion stand-in); App
        // survives still importing it. (Helper stays on disk — deletion lives in the buffer set.)
        let app = fs::read_to_string(root.join("src/main/java/com/x/App.java")).unwrap();
        let files = vec![
            ("src/main/java/com/x/Helper.java".to_string(), String::new()),
            ("src/main/java/com/x/App.java".to_string(), app),
        ];
        let diags = ci_edit::moves::deleted_reference_diags(&crate::movefix::JavaMoveModel(root), &files);
        assert!(
            diags.iter().any(|d| d.file == "src/main/java/com/x/App.java" && d.message.contains("unresolved import")),
            "the surviving importer of the deleted class is flagged: {diags:?}"
        );
    }

    // Contract clauses 3 + 5 with the real compiler: a type-breaking edit to a DEPENDENCY
    // rejects because its unchanged IMPORTER was pulled into the blast radius. A forward
    // reference (baseline test above) is caught by javac's own -sourcepath implicit
    // compilation; a REVERSE dependent is reachable only through the syntactic import graph
    // served transitively (Composed, semantic_edges = false). This is the proof that the
    // Java resolver's edges exist AND reach the gate — without them the importer never
    // compiles and the break ships clean.
    #[test]
    #[ignore]
    fn type_break_in_dependency_rejects_via_reverse_importer_in_radius() {
        let dir = write_repo(&[
            (
                "src/main/java/app/Svc.java",
                "package app;\n\nimport lib.Dep;\n\npublic class Svc {\n  public int probe() {\n    return new Dep().value();\n  }\n}\n",
            ),
            (
                "src/main/java/lib/Dep.java",
                "package lib;\n\npublic class Dep {\n  public int value() {\n    return 1;\n  }\n}\n",
            ),
        ]);
        let root = dir.path();
        let p = JavaProvider::new(root);

        let dep = "src/main/java/lib/Dep.java";
        let before = fs::read_to_string(root.join(dep)).unwrap();
        // Edit ONLY the dependency: value() now returns String. Dep.java in isolation still
        // compiles — the break surfaces only in the unchanged importer's `int probe() {
        // return ...value(); }`, and only if that importer entered the radius.
        let res = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: format!("{dep}#Dep.value"),
                    code: "public String value() {\n    return \"x\";\n  }".into(),
                }],
                &OPTS,
            )
            .unwrap();
        match &res {
            CommitResult::Rejected { feedback, .. } => {
                assert!(
                    feedback.contains("Svc.java"),
                    "the unchanged importer is named as the break site: {feedback}"
                );
                assert!(
                    feedback.contains("incompatible types"),
                    "the javac verdict (not a resolution failure) reaches the reply: {feedback}"
                );
            }
            other => panic!(
                "a type break in a dependency must reject via its importer in the radius: {other:?}"
            ),
        }
        assert_eq!(fs::read_to_string(root.join(dep)).unwrap(), before, "reject leaves disk untouched");
    }

    // Contract clause 2 through Composed: a committed edit is visible to structure() in the
    // same session with no manual reindex (the tree-sitter reader is live; this pins that
    // the GLUE keeps it that way for Java).
    #[test]
    #[ignore]
    fn committed_edit_refreshes_reads_in_session() {
        let dir = write_repo(&[("Svc.java", CLASS)]);
        let p = JavaProvider::new(dir.path());
        assert!(
            !p.structure(Path::new("Svc.java")).unwrap().iter().any(|n| n.id == "Svc.java#Svc.ping"),
            "ping does not exist yet"
        );
        let res = p
            .apply_edits(
                &[EditOp::InsertMember {
                    node_id: "Svc.java#Svc".into(),
                    code: "public int ping() {\n    return 2;\n  }".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "member insert commits: {res:?}");
        assert!(
            p.structure(Path::new("Svc.java")).unwrap().iter().any(|n| n
                .children
                .iter()
                .any(|c| c.id == "Svc.java#Svc.ping")
                || n.id == "Svc.java#Svc.ping"),
            "reads reflect the committed member without a reindex"
        );
    }

    // ── blocked-tool tier: written + honest about why it can't run here ─────────────────

    // Requires jdtls (`brew install jdtls`, itself needing a Java 21+ runtime). NOT
    // installed on this machine — the test SKIPS loudly instead of failing the whole
    // `--ignored` tier, so a machine with jdtls exercises the real rename path.
    #[test]
    #[ignore]
    fn jdtls_rename_lands_cross_file() {
        if jdtls::jdtls_binary().is_none() {
            eprintln!("SKIP: jdtls not installed — {}", jdtls::INSTALL_HINT);
            return;
        }
        let dir = write_repo(&[
            ("Util.java", "public class Util {\n  public static int base() {\n    return 1;\n  }\n}\n"),
            ("App.java", "public class App {\n  public int run() {\n    return Util.base();\n  }\n}\n"),
        ]);
        let root = dir.path();
        let p = JavaProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "Util.java#Util.base".into(), new_name: "fetchBase".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the gate: {res:?}");
        assert!(
            fs::read_to_string(root.join("Util.java")).unwrap().contains("fetchBase"),
            "definition renamed"
        );
        assert!(
            fs::read_to_string(root.join("App.java")).unwrap().contains("Util.fetchBase()"),
            "REFERENCE rewritten cross-file — the reason jdtls exists here"
        );
    }

    // An engine whose toolchain runs inside the `marksman-java` OCI container (bypassing the
    // registry's host probe, exactly as `CI_SANDBOX=oci` would at runtime).
    fn oci_java_factory() -> EngineFactory {
        Arc::new(|root: &Path| {
            let sandbox: Arc<dyn ci_core::Sandbox> = Arc::new(ci_core::OciSandbox::new(
                root.to_path_buf(),
                ci_core::oci_runtime().expect("an OCI runtime on PATH"),
                "marksman-java".into(),
            ));
            let sidecar = gate::JavacSidecar::start(root, &*sandbox)?;
            Ok(Box::new(gate::JavaEngine::new(root, sidecar, sandbox)) as Box<dyn GateEngine + Send>)
        })
    }

    // The M2.3 PAYOFF (docs/container-gate-spec.md §9b). Requires docker (or another OCI runtime)
    // up AND the java image:
    //   docker build -f docker/marksman-java.Dockerfile -t marksman-java docker/
    // Gate (javac sidecar) AND cross-file rename (jdtls) both run in the container from the image,
    // so this passes with NO host jdtls/javac — the exact bench finding (java rename fell back to
    // manual when host jdtls was absent), closed. jdtls does a real project import in a cold
    // container, so this is the slowest e2e here.
    #[test]
    #[ignore]
    fn oci_java_gate_and_rename_without_host_tools() {
        if ci_core::oci_runtime().is_none() {
            eprintln!("SKIP: no OCI runtime (docker/podman/nerdctl/container) on PATH");
            return;
        }
        let dir = write_repo(&[
            ("Util.java", "public class Util {\n  public static int base() {\n    return 1;\n  }\n}\n"),
            ("App.java", "public class App {\n  public int run() {\n    return Util.base();\n  }\n}\n"),
        ]);
        let root = dir.path();
        let p = JavaProvider::with_factory(root, oci_java_factory());
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "Util.java#Util.base".into(), new_name: "fetchBase".into() }],
                &OPTS,
            )
            .unwrap();
        // apply_edits returning (not erroring) proves the whole container path ran end to end: the
        // javac sidecar AND jdtls both launched inside the image, no host jdtls/javac consulted, and
        // the rename executed — jdtls renamed the definition in Util.java.
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the CONTAINER gate: {res:?}");
        assert!(
            fs::read_to_string(root.join("Util.java")).unwrap().contains("fetchBase"),
            "definition renamed inside the container"
        );
        assert!(
            fs::read_to_string(root.join("App.java")).unwrap().contains("Util.fetchBase()"),
            "cross-file reference rewritten by the CONTAINER's jdtls — no host jdtls consulted"
        );
    }

    // Requires mvn (`brew install maven`). NOT installed on this machine — SKIPS loudly.
    // Q3's build-tool derivation: a pom.xml + mvn present must route the classpath through
    // `dependency:build-classpath` (a dependency-less pom yields an empty-but-derived path,
    // which is still the build tool's answer, not the flat fallback).
    #[test]
    #[ignore]
    fn maven_classpath_derivation() {
        if ci_core::probe_tool(Command::new("mvn").arg("--version")).is_none() {
            eprintln!("SKIP: mvn not installed — `brew install maven`");
            return;
        }
        let dir = write_repo(&[(
            "pom.xml",
            "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n  <modelVersion>4.0.0</modelVersion>\n  <groupId>t</groupId>\n  <artifactId>t</artifactId>\n  <version>0</version>\n</project>\n",
        )]);
        assert!(
            gate::maven_classpath(dir.path()).is_some(),
            "mvn present + pom.xml => the build tool answers the classpath question"
        );
    }

    // Requires gradle (`brew install gradle`). NOT installed on this machine — SKIPS loudly.
    #[test]
    #[ignore]
    fn gradle_classpath_derivation() {
        if ci_core::probe_tool(Command::new("gradle").arg("--version")).is_none() {
            eprintln!("SKIP: gradle not installed — `brew install gradle`");
            return;
        }
        let dir = write_repo(&[("build.gradle", "plugins { id 'java' }\n")]);
        assert!(
            gate::gradle_classpath(dir.path()).is_some(),
            "gradle present + build.gradle => the init-script task answers the classpath question"
        );
    }
}
