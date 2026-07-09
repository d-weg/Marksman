//! lang-php — the PHP [`LanguageProvider`] at the GATED tier (rollout ladder Step 1,
//! assembled exactly like `lang-template`/`lang-java`): generic tree-sitter reads from
//! [`lang_fallback::FallbackProvider`] × a real checker gate, glued by [`Composed`].
//!
//! The write engine has two halves that never trade jobs:
//! - **verdict**: PHPStan (`phpstan analyse <paths> --error-format=json --level N
//!   --no-progress`, `src/gate.rs`). PHPStan is a batch analyser, not a server — the gate
//!   materializes the overlay buffers into a temp mirror and parses the per-file JSON. Without
//!   composer/vendor it degrades to unknown-symbol diagnostics, which the baseline diff excuses
//!   (§6): pre-existing state, never a false reject.
//! - **rename/willRename**: phpactor (real LSP fileOperations — source-verified), started
//!   lazily on the first rename/move only. Intelephense is EXCLUDED (its rename is
//!   premium-licensed); see `phpactor.rs` for the PHAR launcher.
//!
//! Reads, anchors, the PSR-4 import graph, and outlines all come from the shared fallback
//! grammar tables — this crate holds ONLY the PHP-specific engine (contract §7). PHP is
//! SUFFIX-typed (`function f(): int`), so `set_return_type` is NOT refused (unlike Java): the
//! registry's `return_type_suffix` marker is `true`, and the shared spine splices `": T"` after
//! the `)`.
use ci_core::{CommitResult, EditOp, EditOpts, Granularity, ImportGraph, LanguageProvider, Node, Result};
use ci_edit::{Composed, EngineFactory, GateEngine};
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

mod gate;
mod movefix;
mod phpactor;

/// The gated PHP provider. `gated()` is `true` because construction goes through the registry's
/// PHPStan check ([`gate_missing`]) — a missing php/phpstan disables the language with the
/// install hint (`ProviderBuild::Unavailable`), it never ships an ungated PHP silently.
pub struct PhpProvider {
    inner: Composed<FallbackProvider>,
}

impl PhpProvider {
    pub fn new(root: &Path) -> Self {
        Self::with_factory(root, engine_factory(root))
    }

    /// The assembly with an injected gate — what the conformance fast tier drives with a
    /// scripted checker, so the wiring is provable without php/phpstan on the machine.
    pub fn with_factory(root: &Path, factory: EngineFactory) -> Self {
        Self { inner: Composed::new(root, FallbackProvider::new(root, FbLang::Php), factory) }
    }
}

impl LanguageProvider for PhpProvider {
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
/// toolchain itself is the problem, say THAT with the install hint instead of a raw error —
/// reads worked fine, so this is the user's first signal the WRITE path is missing a dependency.
fn engine_factory(root: &Path) -> EngineFactory {
    let root = root.to_path_buf();
    Arc::new(move |engine_root: &Path| {
        let Some(phpstan) = gate::phpstan_binary(&root) else {
            let hint = gate_missing(&root).unwrap_or_else(|| "php/phpstan required for the gate".into());
            return Err(ci_core::Error::Driver(format!("php edit engine unavailable.\n{hint}")));
        };
        Ok(Box::new(gate::PhpEngine {
            root: engine_root.to_path_buf(),
            phpstan,
            lsp: None,
            sandbox: ci_core::resolve_sandbox(engine_root, "marksman-php"),
        }) as Box<dyn GateEngine + Send>)
    })
}

/// What the PHP provider needs from the machine, honestly scoped per tool: `php` + PHPStan are
/// the GATE (the provider is off without them — see [`gate_missing`]); phpactor is rename/move
/// only and optional (reads and the type-check gate work without it, a rename explains itself).
pub fn toolchain(root: &Path) -> ci_core::ToolchainReport {
    ci_core::ToolchainReport {
        lang: "php",
        tools: vec![
            php_status(),
            phpstan_status(root),
            ci_core::ToolStatus {
                tool: "phpactor",
                needed_for: "cross-file rename / move rewrites (reads and the phpstan gate work without it)",
                install: phpactor::INSTALL_HINT,
                found: phpactor::phpactor_phar().map(|p| p.to_string_lossy().into_owned()),
            },
        ],
    }
}

fn php_status() -> ci_core::ToolStatus {
    ci_core::ToolStatus {
        tool: "php (8.1+)",
        needed_for: "the runtime PHPStan and phpactor both run on — the php provider is disabled without it",
        install: "a PHP 8.1+ runtime, e.g. `brew install php` or https://www.php.net/downloads",
        found: ci_core::probe_tool(Command::new("php").arg("--version")),
    }
}

fn phpstan_status(root: &Path) -> ci_core::ToolStatus {
    ci_core::ToolStatus {
        tool: "phpstan",
        needed_for: "the type-check gate (batch analyse over the blast radius) — the php provider is disabled without it",
        install: "`composer require --dev phpstan/phpstan` (or the phpstan.phar from https://github.com/phpstan/phpstan/releases)",
        // Probe the SAME resolver the engine runs (`gate::phpstan_binary`): it honors
        // `$CI_PHPSTAN` and a Composer-vendored `vendor/bin/phpstan`, not just PATH — else the
        // probe reports "disabled" on a repo the engine would actually gate (§6 honesty inverted).
        found: gate::phpstan_binary(root).map(|p| p.to_string_lossy().into_owned()),
    }
}

/// The REQUIRED half of [`toolchain`] — php + phpstan — for the registry builders: this is what
/// turns into `ProviderBuild::Unavailable` (contract §6). phpactor stays out of it so a machine
/// without the LSP still gets gated PHP edits. Takes `root` because PHPStan may be repo-vendored
/// (`vendor/bin/phpstan`) — the one probe that must match its resolver exactly.
pub fn gate_missing(root: &Path) -> Option<String> {
    ci_core::ToolchainReport { lang: "php", tools: vec![php_status(), phpstan_status(root)] }.describe_missing()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CLASS: &str =
        "<?php\nclass Svc {\n  public function probe(string $url): int {\n    return 1;\n  }\n}\n";
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

    // ── fast tier: the assembly, provable without php/phpstan ────────────────────────────

    fn mock_provider(root: &Path) -> PhpProvider {
        PhpProvider::with_factory(root, lang_template::mock::factory())
    }

    // The template promise, kept by the copy: tree-sitter reads with PHP's qualified ids, a
    // gate-flagged edit rejects ATOMICALLY, a clean edit commits.
    #[test]
    fn assembly_reads_gate_and_commit() {
        let dir = write_repo(&[("Svc.php", CLASS)]);
        let root = dir.path();
        let p = mock_provider(root);
        assert!(p.gated(), "the gated tier reports itself");
        assert!(p
            .structure(Path::new("Svc.php"))
            .unwrap()
            .iter()
            .any(|n| n.id == "Svc.php#Svc"));

        let before = fs::read_to_string(root.join("Svc.php")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.php#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: format!("return 1; // {}", lang_template::mock::MARKER),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "checker-flagged edit rejects: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("Svc.php")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.php#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return 2;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(fs::read_to_string(root.join("Svc.php")).unwrap().contains("return 2;"));
    }

    // Q2 through the provider: PHP is SUFFIX-typed, so `set_return_type` is NOT refused — the
    // handler splices `": T"` after the `)` (the registry's `return_type_suffix = true`). This
    // is the OPPOSITE of Java's refuse-with-recipe; the fixture pins the correct output.
    #[test]
    fn set_return_type_appends_suffix_type() {
        let dir = write_repo(&[(
            "Svc.php",
            "<?php\nclass Svc {\n  public function probe(string $url) {\n    return 1;\n  }\n}\n",
        )]);
        let p = mock_provider(dir.path());
        let res = p
            .apply_edits(
                &[EditOp::SetReturnType { node_id: "Svc.php#Svc.probe".into(), ty: "int".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "suffix-typed set_return_type commits: {res:?}");
        let after = fs::read_to_string(dir.path().join("Svc.php")).unwrap();
        assert!(
            after.contains("public function probe(string $url): int"),
            "return type appended as `: int` after the params: {after}"
        );
    }

    // The §8 move path through the provider: a MoveFile commits, the moved file's namespace is
    // rewritten to the destination, and an importer's `use` retargets to the new FQCN — the
    // movefix hooks (phpactor absent) wired through the shared move engine + the gate. The gate
    // here is a movefix-delegating engine with a no-op verdict, so the wiring is provable
    // WITHOUT php/phpstan (the real PhpEngine's `will_rename` is byte-identical to this).
    #[test]
    fn move_rewrites_use_and_namespace_via_hooks() {
        use ci_core::Diag;
        use serde_json::{json, Value};

        // A gate that answers `will_rename` from the SAME movefix engine the real PhpEngine uses
        // (its diagnostics never gate — the move rewrite is what's under test).
        struct MoveEngine(std::path::PathBuf);
        impl GateEngine for MoveEngine {
            fn diagnostics(&mut self, _files: &[(String, String)]) -> Result<Vec<Diag>> {
                Ok(Vec::new())
            }
            fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
                Ok(json!({}))
            }
            fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
                Ok(crate::movefix::move_workspace_edit(&self.0, from, to).unwrap_or_else(|| json!({})))
            }
        }

        let dir = write_repo(&[
            ("composer.json", "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/\" } } }\n"),
            ("src/Helper.php", "<?php\nnamespace App;\nclass Helper {}\n"),
            ("src/Consumer.php", "<?php\nnamespace App;\nuse App\\Helper;\nclass Consumer {\n  private Helper $h;\n}\n"),
        ]);
        let root = dir.path();
        let root_buf = root.to_path_buf();
        let p = PhpProvider::with_factory(
            root,
            Arc::new(move |_r: &Path| Ok(Box::new(MoveEngine(root_buf.clone())) as Box<dyn GateEngine + Send>)),
        );
        let res = p
            .apply_edits(
                &[EditOp::MoveFile { from: "src/Helper.php".into(), to: "src/Sub/Helper.php".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "php move commits through the gate: {res:?}");
        assert!(root.join("src/Sub/Helper.php").exists() && !root.join("src/Helper.php").exists());
        assert!(
            fs::read_to_string(root.join("src/Sub/Helper.php")).unwrap().contains("namespace App\\Sub;"),
            "moved file's namespace rewritten to the destination"
        );
        assert!(
            fs::read_to_string(root.join("src/Consumer.php")).unwrap().contains("use App\\Sub\\Helper;"),
            "importer's use retargeted to the new FQCN"
        );
    }

    // §8 deletion soundness (T3): deleting a class whose importer still `use`s it yields a
    // diagnostic through the REAL PHP hooks (PSR-4 resolver + `use` scanner) — the gap-fill the
    // engine wires into `diagnostics()`. Pure hooks, no toolchain needed.
    #[test]
    fn deleted_class_still_used_is_flagged() {
        let dir = write_repo(&[
            ("composer.json", "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/\" } } }\n"),
            ("src/Helper.php", "<?php\nnamespace App;\nclass Helper {}\n"),
            ("src/Consumer.php", "<?php\nnamespace App;\nuse App\\Helper;\nclass Consumer {}\n"),
        ]);
        let root = dir.path();
        let consumer = fs::read_to_string(root.join("src/Consumer.php")).unwrap();
        // Helper deleted (empty-content buffer = the spine's deletion stand-in); Consumer survives.
        let files = vec![
            ("src/Helper.php".to_string(), String::new()),
            ("src/Consumer.php".to_string(), consumer),
        ];
        let diags = ci_edit::moves::deleted_reference_diags(&crate::movefix::PhpMoveModel(root), &files);
        assert!(
            diags.iter().any(|d| d.file == "src/Consumer.php" && d.message.contains("unresolved use")),
            "the surviving `use` of the deleted class is flagged: {diags:?}"
        );
    }

    // The toolchain surface the registry/doctor consume: php + phpstan are the REQUIRED tools
    // (gate_missing), phpactor is reported but optional.
    #[test]
    fn toolchain_scopes_php_phpstan_required_phpactor_optional() {
        let dir = tempfile::tempdir().unwrap();
        let report = toolchain(dir.path());
        assert_eq!(report.lang, "php");
        let tools: Vec<&str> = report.tools.iter().map(|t| t.tool).collect();
        assert!(tools.iter().any(|t| t.contains("php ")), "php probed: {tools:?}");
        assert!(tools.contains(&"phpstan"), "phpstan probed: {tools:?}");
        assert!(tools.contains(&"phpactor"), "phpactor probed: {tools:?}");
        let phpactor_tool = report.tools.iter().find(|t| t.tool == "phpactor").unwrap();
        assert!(phpactor_tool.install.contains("phpactor"), "actionable hint: {}", phpactor_tool.install);
        // php/phpstan are absent on this machine, so the gate requirement is unmet and reported
        // with an install hint; phpactor's absence must NOT be what trips gate_missing.
        match gate_missing(dir.path()) {
            Some(hint) => assert!(hint.contains("Install"), "actionable when php/phpstan missing: {hint}"),
            None => {
                // On a machine WITH php+phpstan, gate_missing is None regardless of phpactor.
            }
        }
    }

    // ── real-tool tier (#[ignore]): PHPStan / phpactor, blocked here ─────────────────────
    // php/composer/phpstan/phpactor are ALL ABSENT on this machine — each real-tool e2e SKIPS
    // loudly, naming its install command, so a machine with the toolchain exercises them.

    // Requires phpstan (`composer require --dev phpstan/phpstan` or the phpstan.phar). NOT
    // installed here — SKIPS loudly.
    #[test]
    #[ignore]
    fn phpstan_gate_rejects_type_error_and_accepts_clean() {
        let dir = write_repo(&[("Svc.php", CLASS)]);
        let root = dir.path();
        if gate::phpstan_binary(root).is_none() {
            eprintln!("SKIP: phpstan not installed — `composer require --dev phpstan/phpstan`");
            return;
        }
        let p = PhpProvider::new(root);

        let before = fs::read_to_string(root.join("Svc.php")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.php#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return \"broken\";".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type-breaking edit must reject: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("Svc.php")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.php#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return 2;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
    }

    // Batch atomicity under the REAL analyser (T1): one type-breaking op sinks the whole batch —
    // the clean op must not land either. Requires phpstan; SKIPS loudly otherwise.
    #[test]
    #[ignore]
    fn phpstan_gate_batch_is_atomic() {
        let dir = write_repo(&[(
            "Svc.php",
            "<?php\nclass Svc {\n  public function probe(string $url): int {\n    return 1;\n  }\n  public function total(): int {\n    return 0;\n  }\n}\n",
        )]);
        let root = dir.path();
        if gate::phpstan_binary(root).is_none() {
            eprintln!("SKIP: phpstan not installed — `composer require --dev phpstan/phpstan`");
            return;
        }
        let p = PhpProvider::new(root);
        let before = fs::read_to_string(root.join("Svc.php")).unwrap();
        let res = p
            .apply_edits(
                &[
                    EditOp::ReplaceText {
                        node_id: "Svc.php#Svc.total".into(),
                        old_text: "return 0;".into(),
                        new_text: "return 10;".into(),
                    },
                    EditOp::ReplaceText {
                        node_id: "Svc.php#Svc.probe".into(),
                        old_text: "return 1;".into(),
                        new_text: "return \"broken\";".into(),
                    },
                ],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Rejected { .. }), "a batch with a breaking op rejects: {res:?}");
        assert_eq!(
            fs::read_to_string(root.join("Svc.php")).unwrap(),
            before,
            "NOTHING from a rejected batch lands (the clean op included)"
        );
    }

    // Contract clause 5 with the real analyser (T1): PRE-EXISTING breakage (a type error in a
    // method the batch never touches) is baseline — it never blocks an unrelated clean edit, and
    // the commit result CARRIES it rather than claiming a clean radius. Kept INTRA-FILE so it does
    // not depend on PHPStan's cross-file resolution in the isolated overlay. Requires phpstan.
    #[test]
    #[ignore]
    fn phpstan_gate_baseline_excuses_preexisting_breakage() {
        let dir = write_repo(&[(
            "Svc.php",
            // `broken()` is already type-broken BEFORE the batch (returns string from an int method).
            "<?php\nclass Svc {\n  public function probe(): int {\n    return 1;\n  }\n  public function broken(): int {\n    return \"oops\";\n  }\n}\n",
        )]);
        let root = dir.path();
        if gate::phpstan_binary(root).is_none() {
            eprintln!("SKIP: phpstan not installed — `composer require --dev phpstan/phpstan`");
            return;
        }
        let p = PhpProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "Svc.php#Svc.probe".into(),
                    old_text: "return 1;".into(),
                    new_text: "return 2;".into(),
                }],
                &OPTS,
            )
            .unwrap();
        match &res {
            CommitResult::Ok { preexisting_in_radius, .. } => assert!(
                !preexisting_in_radius.is_empty(),
                "the excused pre-existing breakage is CARRIED, not hidden: {preexisting_in_radius:?}"
            ),
            other => panic!("pre-existing breakage must not block an unrelated edit: {other:?}"),
        }
        assert!(fs::read_to_string(root.join("Svc.php")).unwrap().contains("return 2;"));
    }

    // Cross-file resolution (T1, the schema-field regression): a batch that touches ONLY a
    // consumer must still be judged against a sibling class that lives in ANOTHER file on disk.
    // The gate mirrors the whole project into its overlay and feeds the siblings to PHPStan via
    // `scanDirectories`, so `new DocEntry(...)` resolves — a valid call commits (no false reject,
    // the bug that blew a bench rust arm to 1M tokens) and a wrong-arity call rejects (no false
    // accept — PHPStan can only check the arg count once the class resolves). Requires phpstan.
    #[test]
    #[ignore]
    fn phpstan_gate_resolves_sibling_class_across_files() {
        let dir = write_repo(&[
            (
                "src/DocEntry.php",
                "<?php\nnamespace Corpus;\nclass DocEntry {\n  public function __construct(public string $name, public string $path, public string $lower) {}\n}\n",
            ),
            (
                "src/Store.php",
                "<?php\nnamespace Corpus;\nclass Store {\n  public function make(string $n): DocEntry {\n    return new DocEntry($n, $n, $n);\n  }\n}\n",
            ),
        ]);
        let root = dir.path();
        if gate::phpstan_binary(root).is_none() {
            eprintln!("SKIP: phpstan not installed — `composer require --dev phpstan/phpstan`");
            return;
        }
        let p = PhpProvider::new(root);

        // Clean: still a valid 3-arg call to the sibling — commits (resolution ⇒ no false reject).
        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "src/Store.php#Store.make".into(),
                    old_text: "new DocEntry($n, $n, $n)".into(),
                    new_text: "new DocEntry($n, $n, strtolower($n))".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "valid cross-file ctor call commits: {ok:?}");

        // Break: drop an argument — DocEntry needs 3. Only detectable once the sibling resolves;
        // the pre-fix isolated overlay left DocEntry unknown and FALSE-ACCEPTED this.
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "src/Store.php#Store.make".into(),
                    old_text: "new DocEntry($n, $n, strtolower($n))".into(),
                    new_text: "new DocEntry($n, $n)".into(),
                }],
                &OPTS,
            )
            .unwrap();
        assert!(
            matches!(bad, CommitResult::Rejected { .. }),
            "a wrong-arity call to a sibling class must reject, not slip through as an unknown class: {bad:?}"
        );
    }

    // Requires phpactor (the PHAR from GitHub releases; $CI_PHPACTOR). NOT installed here —
    // SKIPS loudly, so a machine with phpactor exercises the real cross-file rename.
    #[test]
    #[ignore]
    fn phpactor_rename_lands_cross_file() {
        if phpactor::phpactor_phar().is_none() {
            eprintln!("SKIP: phpactor not installed — {}", phpactor::INSTALL_HINT);
            return;
        }
        let dir = write_repo(&[
            (
                "composer.json",
                "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/\" } } }\n",
            ),
            ("src/Util.php", "<?php\nnamespace App;\nclass Util {\n  public static function base(): int {\n    return 1;\n  }\n}\n"),
            ("src/App.php", "<?php\nnamespace App;\nclass App {\n  public function run(): int {\n    return Util::base();\n  }\n}\n"),
        ]);
        let root = dir.path();
        let p = PhpProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "src/Util.php#Util.base".into(), new_name: "fetchBase".into() }],
                &OPTS,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the gate: {res:?}");
        assert!(
            fs::read_to_string(root.join("src/Util.php")).unwrap().contains("fetchBase"),
            "definition renamed"
        );
        assert!(
            fs::read_to_string(root.join("src/App.php")).unwrap().contains("Util::fetchBase()"),
            "REFERENCE rewritten cross-file — the reason phpactor exists here"
        );
    }
}
