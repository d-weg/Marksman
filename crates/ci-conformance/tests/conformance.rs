//! The conformance instances: every provider × its mini-fixture through the shared battery.
//! Adding a language = adding its fixtures here (see docs/provider-contract.md). The fast tier
//! runs in CI; `-- --ignored` adds the providers that shell out to real tools.

use ci_conformance::{run_edit_battery, run_read_battery, EditFixture, ReadFixture};
use ci_core::LanguageProvider;
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;

/// A provider constructor for one battery instance: repo root -> boxed provider.
type MkProvider = Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>>;

fn fallback(lang: FbLang) -> MkProvider {
    Box::new(move |root| Box::new(FallbackProvider::new(root, lang)))
}

// ---------------------------------------------------------------------------
// Fallback tier (ungated): read + edit batteries, fully in-process.
// ---------------------------------------------------------------------------

#[test]
fn conformance_python() {
    let mk = fallback(FbLang::Python);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/python",
            files: &[
                (
                    "m.py",
                    "from util import base\n\ndef probe(url):\n    \"\"\"Check the url.\"\"\"\n    return True\n\nclass Svc:\n    def run(self, x):\n        return probe(x) + base\n",
                ),
                ("util.py", "base = 1\n"),
            ],
            target: "m.py",
            want_ids: &["m.py#probe", "m.py#Svc", "m.py#Svc.run"],
            fn_symbol: "m.py#probe",
            expect_params: true,
            doc_symbol: Some("m.py#probe"),
            edge: Some(("m.py", "util.py")),
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/python",
            files: &[("m.py", "def probe(url):\n    \"\"\"Check the url.\"\"\"\n    return True\n")],
            target_symbol: "m.py#probe",
            clean: ("return True", "return False"),
            breaks: ("return True", "return ((True"),
        },
    );
}

#[test]
fn conformance_js() {
    let mk = fallback(FbLang::Js);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/js",
            files: &[
                (
                    "app.js",
                    "import { base } from \"./util.js\";\n\n// Formats a duration for display.\nfunction formatSpan(ms) {\n  return ms + base;\n}\n\nclass Panel {\n  render(rows) {\n    return rows.map(formatSpan);\n  }\n}\n",
                ),
                ("util.js", "export const base = 1;\n"),
            ],
            target: "app.js",
            want_ids: &["app.js#formatSpan", "app.js#Panel", "app.js#Panel.render"],
            fn_symbol: "app.js#formatSpan",
            expect_params: true,
            doc_symbol: Some("app.js#formatSpan"),
            edge: Some(("app.js", "util.js")),
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/js",
            files: &[("app.js", "// Formats a duration.\nfunction formatSpan(ms) {\n  return ms + 1;\n}\n")],
            target_symbol: "app.js#formatSpan",
            clean: ("return ms + 1;", "return ms + 2;"),
            breaks: ("return ms + 1;", "return (ms + 1;"),
        },
    );
}

#[test]
fn conformance_ts_fallback() {
    // The ablation read path (CI_TS_MODE=treesitter-gated reads through this) — NOT the
    // product TS provider; that one is `conformance_ts_scip` in the real-tool tier.
    let mk: MkProvider =
        Box::new(|root| Box::new(FallbackProvider::new(root, FbLang::Ts)));
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/ts",
            files: &[
                (
                    "src/rank.ts",
                    "import { clamp } from \"./util/math.js\";\nexport interface RankRow {\n  score: number;\n}\nexport class Ranker {\n  top(rows: RankRow[]): RankRow[] {\n    return rows;\n  }\n}\n// Ranks all rows.\nexport function rankAll(rows: RankRow[]): number {\n  return clamp(rows.length);\n}\n",
                ),
                ("src/util/math.ts", "export function clamp(x: number): number {\n  return x;\n}\n"),
            ],
            target: "src/rank.ts",
            want_ids: &["src/rank.ts#RankRow", "src/rank.ts#Ranker.top", "src/rank.ts#rankAll"],
            fn_symbol: "src/rank.ts#rankAll",
            expect_params: true,
            doc_symbol: Some("src/rank.ts#rankAll"),
            edge: Some(("src/rank.ts", "src/util/math.ts")),
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/ts",
            files: &[("src/rank.ts", "export function rankAll(n: number): number {\n  return n + 1;\n}\n")],
            target_symbol: "src/rank.ts#rankAll",
            clean: ("return n + 1;", "return n + 2;"),
            breaks: ("return n + 1;", "return (n + 1;"),
        },
    );
}

#[test]
fn conformance_go() {
    let mk = fallback(FbLang::Go);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/go",
            files: &[(
                "svc.go",
                "package svc\n\n// Latency bucket.\ntype Bucket struct {\n\tp99 float64\n}\n\nfunc Probe(url string) bool {\n\treturn true\n}\n\nfunc (b Bucket) Worst() float64 {\n\treturn b.p99\n}\n",
            )],
            target: "svc.go",
            want_ids: &["svc.go#Bucket", "svc.go#Probe", "svc.go#Worst"],
            fn_symbol: "svc.go#Probe",
            expect_params: true,
            doc_symbol: Some("svc.go#Bucket"),
            edge: None,
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/go",
            files: &[("svc.go", "package svc\n\nfunc Probe(url string) bool {\n\treturn true\n}\n")],
            target_symbol: "svc.go#Probe",
            clean: ("return true", "return false"),
            breaks: ("return true", "return (true"),
        },
    );
}

#[test]
fn conformance_java() {
    // The UNGATED fallback instance: still valid — it is the CI_JAVA_MODE=treesitter
    // ablation arm's provider (the product java provider is the gated `conformance_java_*`
    // pair below).
    let mk = fallback(FbLang::Java);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/java",
            files: &[
                (
                    "src/main/java/app/Svc.java",
                    "package app;\n\nimport lib.Dep;\n\n// Probes the service.\npublic class Svc {\n  private int hits = 0;\n\n  public int probe(String url) {\n    return new Dep().value();\n  }\n}\n",
                ),
                (
                    "src/main/java/lib/Dep.java",
                    "package lib;\n\npublic class Dep {\n  public int value() {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/main/java/app/Svc.java",
            want_ids: &[
                "src/main/java/app/Svc.java#Svc",
                "src/main/java/app/Svc.java#Svc.probe",
                "src/main/java/app/Svc.java#Svc.hits",
            ],
            fn_symbol: "src/main/java/app/Svc.java#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/main/java/app/Svc.java#Svc"),
            edge: Some(("src/main/java/app/Svc.java", "src/main/java/lib/Dep.java")),
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/java",
            files: &[("Svc.java", "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.java#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return (1;"),
        },
    );
}

#[test]
fn conformance_php() {
    // The UNGATED fallback instance: still valid — it is the CI_PHP_MODE=treesitter ablation
    // arm's provider (the product php provider is the gated `conformance_php_*` pair below).
    let mk = fallback(FbLang::Php);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/php",
            files: &[
                (
                    "composer.json",
                    "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } } }\n",
                ),
                (
                    "src/App/Svc.php",
                    "<?php\nnamespace App;\n\nuse App\\Lib\\Dep;\n\n// Probes the service.\nclass Svc {\n  private int $hits = 0;\n\n  public function probe(string $url): int {\n    return (new Dep())->value();\n  }\n}\n",
                ),
                (
                    "src/App/Lib/Dep.php",
                    "<?php\nnamespace App\\Lib;\n\nclass Dep {\n  public function value(): int {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/App/Svc.php",
            want_ids: &[
                "src/App/Svc.php#Svc",
                "src/App/Svc.php#Svc.probe",
            ],
            fn_symbol: "src/App/Svc.php#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/App/Svc.php#Svc"),
            edge: Some(("src/App/Svc.php", "src/App/Lib/Dep.php")),
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/php",
            files: &[("Svc.php", "<?php\nclass Svc {\n  public function probe(string $url): int {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.php#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return (1;"),
        },
    );
}

#[test]
fn conformance_swift() {
    // The UNGATED fallback instance: still valid — it is the CI_SWIFT_MODE=treesitter ablation
    // arm's provider (the product swift provider is the gated `conformance_swift_*` pair below).
    // Swift imports are module-level, so the import graph is the honest EMPTY graph (`edge: None`).
    let mk = fallback(FbLang::Swift);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/swift",
            files: &[(
                "Svc.swift",
                "import Foundation\n\n/// Probes the service.\nclass Svc {\n  var hits: Int = 0\n\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\n",
            )],
            target: "Svc.swift",
            want_ids: &["Svc.swift#Svc", "Svc.swift#Svc.probe", "Svc.swift#Svc.hits"],
            fn_symbol: "Svc.swift#Svc.probe",
            expect_params: true,
            doc_symbol: Some("Svc.swift#Svc"),
            edge: None,
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/swift",
            files: &[("Svc.swift", "class Svc {\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\n")],
            target_symbol: "Svc.swift#Svc.probe",
            clean: ("return 1", "return 2"),
            breaks: ("return 1", "return (1"),
        },
    );
}

// The product PHP provider's ASSEMBLY through the fast battery: lang-php's Composed wiring with
// a scripted gate (lang-template's mock), so reads, anchors, atomicity, and gate-reject routing
// are conformant on machines with no php/phpstan. The `breaks` payload is the mock's marker —
// the reject under test is the GATE channel, not the parser.
#[test]
fn conformance_php_gated_mock() {
    let mk: MkProvider = Box::new(|root| {
        Box::new(lang_php::PhpProvider::with_factory(root, lang_template::mock::factory()))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "php/gated+mock",
            files: &[
                (
                    "composer.json",
                    "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } } }\n",
                ),
                (
                    "src/App/Svc.php",
                    "<?php\nnamespace App;\n\nuse App\\Lib\\Dep;\n\n// Probes the service.\nclass Svc {\n  private int $hits = 0;\n\n  public function probe(string $url): int {\n    return (new Dep())->value();\n  }\n}\n",
                ),
                (
                    "src/App/Lib/Dep.php",
                    "<?php\nnamespace App\\Lib;\n\nclass Dep {\n  public function value(): int {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/App/Svc.php",
            want_ids: &[
                "src/App/Svc.php#Svc",
                "src/App/Svc.php#Svc.probe",
            ],
            fn_symbol: "src/App/Svc.php#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/App/Svc.php#Svc"),
            edge: Some(("src/App/Svc.php", "src/App/Lib/Dep.php")),
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "php/gated+mock",
            files: &[("Svc.php", "<?php\nclass Svc {\n  public function probe(string $url): int {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.php#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return 1; // TEMPLATE_TYPE_ERROR"),
        },
    );
}

// Real-tool tier: the SAME product PHP provider with its REAL gate (PHPStan). php/composer/
// phpstan are ABSENT on this machine, so this SKIPS via the provider's own toolchain probe when
// invoked with `-- --ignored` — it lands green only where PHPStan exists. The `breaks` payload
// PARSES but breaks the types: the reject proves the verdict is the analyser, not the grammar.
#[test]
#[ignore]
fn conformance_php_phpstan() {
    if lang_php::gate_missing(std::path::Path::new(".")).is_some() {
        eprintln!("SKIP: php/phpstan not installed — `composer require --dev phpstan/phpstan`");
        return;
    }
    let mk: MkProvider =
        Box::new(|root| Box::new(lang_php::PhpProvider::new(root)));
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "php/phpstan",
            files: &[
                (
                    "composer.json",
                    "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } } }\n",
                ),
                (
                    "src/App/Svc.php",
                    "<?php\nnamespace App;\n\nuse App\\Lib\\Dep;\n\n// Probes the service.\nclass Svc {\n  private int $hits = 0;\n\n  public function probe(string $url): int {\n    return (new Dep())->value();\n  }\n}\n",
                ),
                (
                    "src/App/Lib/Dep.php",
                    "<?php\nnamespace App\\Lib;\n\nclass Dep {\n  public function value(): int {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/App/Svc.php",
            want_ids: &[
                "src/App/Svc.php#Svc",
                "src/App/Svc.php#Svc.probe",
            ],
            fn_symbol: "src/App/Svc.php#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/App/Svc.php#Svc"),
            edge: Some(("src/App/Svc.php", "src/App/Lib/Dep.php")),
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "php/phpstan",
            files: &[("Svc.php", "<?php\nclass Svc {\n  public function probe(string $url): int {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.php#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return \"broken\";"),
        },
    );
}

// Contract clauses 3 + 5 with the REAL PHPStan gate: a type-break in a DEPENDENCY rejects
// because its unchanged IMPORTER was pulled into the blast radius. The break is confined to
// `Dep::value()` (now returns string); `Dep.php` in isolation still analyses clean — the
// contradiction surfaces only in `Svc::probe()`'s `int` return over `(new Dep())->value()`,
// and only if that importer entered the radius via the PSR-4 syntactic graph served
// transitively (Composed, semantic_edges = false). This is the proof the PHP resolver's edges
// exist AND reach the gate: without them the importer never analyses and the break ships clean.
// php/composer/phpstan are ABSENT here — the test SKIPS loudly, naming its install command, so a
// machine with PHPStan exercises the real reverse-radius reject.
#[test]
#[ignore]
fn conformance_php_reverse_radius_reject() {
    use ci_core::{CommitResult, EditOp, EditOpts};

    if lang_php::gate_missing(std::path::Path::new(".")).is_some() {
        eprintln!("SKIP: php/phpstan not installed — `composer require --dev phpstan/phpstan`");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (rel, content) in [
        ("composer.json", "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } } }\n"),
        (
            "src/App/Svc.php",
            "<?php\nnamespace App;\n\nuse App\\Lib\\Dep;\n\nclass Svc {\n  public function probe(): int {\n    return (new Dep())->value();\n  }\n}\n",
        ),
        (
            "src/App/Lib/Dep.php",
            "<?php\nnamespace App\\Lib;\n\nclass Dep {\n  public function value(): int {\n    return 1;\n  }\n}\n",
        ),
    ] {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    }
    let p = lang_php::PhpProvider::new(root);
    let dep = "src/App/Lib/Dep.php";
    let before = std::fs::read_to_string(root.join(dep)).unwrap();
    // Edit ONLY the dependency: value() now returns a string. Dep.php alone still type-checks —
    // the break lives in the unchanged importer's `int probe() { return ...->value(); }`, and
    // only surfaces if that importer entered the radius.
    let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
    let res = p
        .apply_edits(
            &[EditOp::ReplaceNode {
                node_id: format!("{dep}#Dep.value"),
                code: "public function value(): string {\n    return \"x\";\n  }".into(),
            }],
            &opts,
        )
        .unwrap();
    match &res {
        CommitResult::Rejected { feedback, .. } => assert!(
            feedback.contains("Svc.php"),
            "the unchanged importer is named as the break site (it entered the radius): {feedback}"
        ),
        other => panic!(
            "a type break in a dependency must reject via its importer in the radius: {other:?}"
        ),
    }
    assert_eq!(std::fs::read_to_string(root.join(dep)).unwrap(), before, "reject leaves disk untouched");
}

#[test]
fn conformance_ruby() {
    let mk = fallback(FbLang::Ruby);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/ruby",
            files: &[(
                "svc.rb",
                "# Probes the service.\nclass Svc\n  def probe(url)\n    true\n  end\nend\n\ndef helper\n  1\nend\n",
            )],
            target: "svc.rb",
            want_ids: &["svc.rb#Svc", "svc.rb#Svc.probe", "svc.rb#helper"],
            fn_symbol: "svc.rb#Svc.probe",
            expect_params: true,
            doc_symbol: Some("svc.rb#Svc"),
            edge: None,
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/ruby",
            files: &[("svc.rb", "class Svc\n  def probe(url)\n    true\n  end\nend\n")],
            target_symbol: "svc.rb#Svc.probe",
            clean: ("true", "false"),
            breaks: ("true", "(true"),
        },
    );
}

#[test]
fn conformance_c() {
    let mk = fallback(FbLang::C);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/c",
            files: &[(
                "probe.c",
                "struct bucket {\n  double p99;\n};\n\n// Probe the url.\nstatic int probe(const char *url) {\n  return 1;\n}\n",
            )],
            target: "probe.c",
            want_ids: &["probe.c#bucket", "probe.c#probe"],
            fn_symbol: "probe.c#probe",
            expect_params: true,
            doc_symbol: Some("probe.c#probe"),
            edge: None,
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/c",
            files: &[("probe.c", "static int probe(const char *url) {\n  return 1;\n}\n")],
            target_symbol: "probe.c#probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return (1;"),
        },
    );
}

#[test]
fn conformance_cpp() {
    let mk = fallback(FbLang::Cpp);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/cpp",
            files: &[(
                "svc.cpp",
                "namespace net {\nclass Svc {\n public:\n  int probe();\n};\n}\n\n// Adds two.\nint add(int a, int b) {\n  return a + b;\n}\n",
            )],
            target: "svc.cpp",
            want_ids: &["svc.cpp#net", "svc.cpp#net.Svc", "svc.cpp#add"],
            fn_symbol: "svc.cpp#add",
            expect_params: true,
            doc_symbol: Some("svc.cpp#add"),
            edge: None,
            expect_gated: false,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "fallback/cpp",
            files: &[("svc.cpp", "int add(int a, int b) {\n  return a + b;\n}\n")],
            target_symbol: "svc.cpp#add",
            clean: ("return a + b;", "return a - b;"),
            breaks: ("return a + b;", "return (a + b;"),
        },
    );
}

// ---------------------------------------------------------------------------
// Gated tiers: Rust reads in-process (fast); writes need rust-analyzer and TS
// needs scip-typescript/Node — those run in the real-tool tier or live as
// gate-soundness e2es in their provider crates (see the contract doc).
// ---------------------------------------------------------------------------

#[test]
fn conformance_rust_reads() {
    let mk: MkProvider =
        Box::new(|root| Box::new(lang_rust::RustProvider::new(root)));
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "rust",
            files: &[
                (
                    "lib.rs",
                    "mod util;\n\n/// Adds two numbers.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub struct Foo {\n    pub n: i32,\n}\n\nimpl Foo {\n    pub fn bar(&self) -> i32 {\n        self.n\n    }\n}\n",
                ),
                ("util.rs", "pub fn base() -> i32 {\n    1\n}\n"),
            ],
            target: "lib.rs",
            want_ids: &["lib.rs#add", "lib.rs#Foo", "lib.rs#Foo.bar"],
            fn_symbol: "lib.rs#add",
            expect_params: true,
            doc_symbol: Some("lib.rs#add"),
            edge: Some(("lib.rs", "util.rs")),
            expect_gated: true,
        },
    );
}

// The Step-1 SKELETON (lang-template) through the same battery, gated by its mock checker:
// a copied crate starts conformant — reads, anchors, edit atomicity, and a gate that rejects
// what its checker flags — before the real checker is wired in.
#[test]
fn conformance_template() {
    let mk: MkProvider = Box::new(|root| {
        Box::new(lang_template::GatedTreeSitter::new(root, FbLang::Go, lang_template::mock::factory()))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "template/go+mock-gate",
            files: &[(
                "svc.go",
                "package svc\n\n// Latency bucket.\ntype Bucket struct {\n\tp99 float64\n}\n\nfunc Probe(url string) bool {\n\treturn true\n}\n",
            )],
            target: "svc.go",
            want_ids: &["svc.go#Bucket", "svc.go#Probe"],
            fn_symbol: "svc.go#Probe",
            expect_params: true,
            doc_symbol: Some("svc.go#Bucket"),
            edge: None,
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "template/go+mock-gate",
            files: &[("svc.go", "package svc\n\nfunc Probe(url string) bool {\n\treturn true\n}\n")],
            target_symbol: "svc.go#Probe",
            clean: ("return true", "return false"),
            // The mock checker flags this marker — the reject here is the GATE, not the parser.
            breaks: ("return true", "return true // TEMPLATE_TYPE_ERROR"),
        },
    );
}

// The product Java provider's ASSEMBLY through the fast battery: lang-java's Composed
// wiring with a scripted gate (lang-template's mock), so reads, anchors, atomicity, and
// gate-reject routing are conformant on machines with no JDK. The `breaks` payload is the
// mock's marker — the reject under test is the GATE channel, not the parser.
#[test]
fn conformance_java_gated_mock() {
    let mk: MkProvider = Box::new(|root| {
        Box::new(lang_java::JavaProvider::with_factory(root, lang_template::mock::factory()))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "java/gated+mock",
            files: &[
                (
                    "src/main/java/app/Svc.java",
                    "package app;\n\nimport lib.Dep;\n\n// Probes the service.\npublic class Svc {\n  private int hits = 0;\n\n  public int probe(String url) {\n    return new Dep().value();\n  }\n}\n",
                ),
                (
                    "src/main/java/lib/Dep.java",
                    "package lib;\n\npublic class Dep {\n  public int value() {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/main/java/app/Svc.java",
            want_ids: &[
                "src/main/java/app/Svc.java#Svc",
                "src/main/java/app/Svc.java#Svc.probe",
                "src/main/java/app/Svc.java#Svc.hits",
            ],
            fn_symbol: "src/main/java/app/Svc.java#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/main/java/app/Svc.java#Svc"),
            edge: Some(("src/main/java/app/Svc.java", "src/main/java/lib/Dep.java")),
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "java/gated+mock",
            files: &[("Svc.java", "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.java#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return 1; // TEMPLATE_TYPE_ERROR"),
        },
    );
}

// Real-tool tier: the SAME product Java provider with its REAL gate (the resident
// javax.tools sidecar — needs a JDK 17+, present on the dev machine). The `breaks` payload
// PARSES but breaks the types: the reject proves the verdict is the compiler, not the
// grammar. `cargo test -p ci-conformance -- --ignored`
#[test]
#[ignore]
fn conformance_java_javac() {
    let mk: MkProvider =
        Box::new(|root| Box::new(lang_java::JavaProvider::new(root)));
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "java/javac",
            files: &[
                (
                    "src/main/java/app/Svc.java",
                    "package app;\n\nimport lib.Dep;\n\n// Probes the service.\npublic class Svc {\n  private int hits = 0;\n\n  public int probe(String url) {\n    return new Dep().value();\n  }\n}\n",
                ),
                (
                    "src/main/java/lib/Dep.java",
                    "package lib;\n\npublic class Dep {\n  public int value() {\n    return 1;\n  }\n}\n",
                ),
            ],
            target: "src/main/java/app/Svc.java",
            want_ids: &[
                "src/main/java/app/Svc.java#Svc",
                "src/main/java/app/Svc.java#Svc.probe",
                "src/main/java/app/Svc.java#Svc.hits",
            ],
            fn_symbol: "src/main/java/app/Svc.java#Svc.probe",
            expect_params: true,
            doc_symbol: Some("src/main/java/app/Svc.java#Svc"),
            edge: Some(("src/main/java/app/Svc.java", "src/main/java/lib/Dep.java")),
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "java/javac",
            files: &[("Svc.java", "public class Svc {\n  public int probe(String url) {\n    return 1;\n  }\n}\n")],
            target_symbol: "Svc.java#Svc.probe",
            clean: ("return 1;", "return 2;"),
            breaks: ("return 1;", "return \"broken\";"),
        },
    );
}

// The product Swift provider's ASSEMBLY through the fast battery: lang-swift's Composed wiring
// with a scripted gate (lang-template's mock), so reads, anchors, atomicity, and gate-reject
// routing are conformant on machines with no Swift toolchain. The `breaks` payload is the mock's
// marker — the reject under test is the GATE channel, not the parser. Swift's import graph is the
// honest EMPTY graph (module-level imports), so `edge: None`.
#[test]
fn conformance_swift_gated_mock() {
    let mk: MkProvider = Box::new(|root| {
        Box::new(lang_swift::SwiftProvider::with_factory(root, lang_template::mock::factory()))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "swift/gated+mock",
            files: &[(
                "Svc.swift",
                "import Foundation\n\n/// Probes the service.\nclass Svc {\n  var hits: Int = 0\n\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\n",
            )],
            target: "Svc.swift",
            want_ids: &["Svc.swift#Svc", "Svc.swift#Svc.probe", "Svc.swift#Svc.hits"],
            fn_symbol: "Svc.swift#Svc.probe",
            expect_params: true,
            doc_symbol: Some("Svc.swift#Svc"),
            edge: None,
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "swift/gated+mock",
            files: &[("Svc.swift", "class Svc {\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\n")],
            target_symbol: "Svc.swift#Svc.probe",
            clean: ("return 1", "return 2"),
            breaks: ("return 1", "return 1 // TEMPLATE_TYPE_ERROR"),
        },
    );
}

// Real-tool tier: the SAME product Swift provider with its REAL gate (`swift build` over a SwiftPM
// package — needs a Swift 6+ toolchain, present on the dev machine). The fixture IS a minimal
// package (Package.swift + an executable target); the `breaks` payload PARSES but breaks the types
// (returns String from an Int fn), so the reject proves the verdict is the compiler, not the
// grammar. `cargo test -p ci-conformance -- --ignored`
#[test]
#[ignore]
fn conformance_swift_swiftbuild() {
    if lang_swift::gate_missing().is_some() {
        eprintln!("SKIP: swift toolchain not installed — https://www.swift.org/install");
        return;
    }
    // A minimal SwiftPM package: the manifest + one executable target. `swift build` needs the
    // whole package, so the read + edit fixtures both live under `Sources/App/`.
    const MANIFEST: &str =
        "// swift-tools-version:5.9\nimport PackageDescription\n\nlet package = Package(\n  name: \"App\",\n  targets: [ .executableTarget(name: \"App\", path: \"Sources/App\") ]\n)\n";
    let mk: MkProvider =
        Box::new(|root| Box::new(lang_swift::SwiftProvider::new(root)));
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "swift/swiftbuild",
            files: &[
                ("Package.swift", MANIFEST),
                (
                    "Sources/App/main.swift",
                    "import Foundation\n\n/// Probes the service.\nclass Svc {\n  var hits: Int = 0\n\n  func probe(url: String) -> Int {\n    return 1\n  }\n}\nprint(Svc().probe(url: \"x\"))\n",
                ),
            ],
            target: "Sources/App/main.swift",
            want_ids: &[
                "Sources/App/main.swift#Svc",
                "Sources/App/main.swift#Svc.probe",
                "Sources/App/main.swift#Svc.hits",
            ],
            fn_symbol: "Sources/App/main.swift#Svc.probe",
            expect_params: true,
            doc_symbol: Some("Sources/App/main.swift#Svc"),
            edge: None,
            expect_gated: true,
        },
    );
    run_edit_battery(
        &mk,
        &EditFixture {
            label: "swift/swiftbuild",
            files: &[
                ("Package.swift", MANIFEST),
                (
                    "Sources/App/main.swift",
                    "func probe(url: String) -> Int {\n    return 1\n}\nprint(probe(url: \"x\"))\n",
                ),
            ],
            target_symbol: "Sources/App/main.swift#probe",
            clean: ("return 1", "return 2"),
            breaks: ("return 1", "return \"broken\""),
        },
    );
}

// Real-tool tier: the product TS provider (scip-typescript via npx).
// `cargo test -p ci-conformance -- --ignored`
#[test]
#[ignore]
fn conformance_ts_scip() {
    let mk: MkProvider = Box::new(|root| {
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true,"noEmit":true},"include":["src"]}"#,
        )
        .unwrap();
        Box::new(lang_ts::TsProvider::index(root).expect("scip-typescript indexing"))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "ts/scip",
            files: &[
                (
                    "src/rank.ts",
                    "import { clamp } from \"./util/math.js\";\nexport interface RankRow {\n  score: number;\n}\nexport class Ranker {\n  top(rows: RankRow[]): RankRow[] {\n    return rows;\n  }\n}\nexport function rankAll(rows: RankRow[]): number {\n  return clamp(rows.length);\n}\n",
                ),
                ("src/util/math.ts", "export function clamp(x: number): number {\n  return x;\n}\n"),
            ],
            target: "src/rank.ts",
            want_ids: &["src/rank.ts#RankRow", "src/rank.ts#Ranker.top", "src/rank.ts#rankAll"],
            fn_symbol: "src/rank.ts#rankAll",
            expect_params: true,
            doc_symbol: None,
            edge: Some(("src/rank.ts", "src/util/math.ts")),
            expect_gated: true,
        },
    );
}

// Real-tool tier: the SAME product TS provider, indexed by the tsgo LSP SWEEP (ci-lsp-index)
// instead of scip-typescript — the CI_TS_MODE=lsp comparison arm. Identical fixture and
// expectations as `conformance_ts_scip`: the two producers must be indistinguishable to the
// read path. `cargo test -p ci-conformance -- --ignored`
#[test]
#[ignore]
fn conformance_ts_lsp_sweep() {
    let mk: MkProvider = Box::new(|root| {
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true,"noEmit":true},"include":["src"]}"#,
        )
        .unwrap();
        Box::new(lang_ts::TsProvider::index_with_lsp_sweep(root).expect("tsgo LSP-sweep indexing"))
    });
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "ts/lsp-sweep",
            files: &[
                (
                    "src/rank.ts",
                    "import { clamp } from \"./util/math.js\";\nexport interface RankRow {\n  score: number;\n}\nexport class Ranker {\n  top(rows: RankRow[]): RankRow[] {\n    return rows;\n  }\n}\nexport function rankAll(rows: RankRow[]): number {\n  return clamp(rows.length);\n}\n",
                ),
                ("src/util/math.ts", "export function clamp(x: number): number {\n  return x;\n}\n"),
            ],
            target: "src/rank.ts",
            want_ids: &["src/rank.ts#RankRow", "src/rank.ts#Ranker.top", "src/rank.ts#rankAll"],
            fn_symbol: "src/rank.ts#rankAll",
            expect_params: true,
            doc_symbol: None,
            edge: Some(("src/rank.ts", "src/util/math.ts")),
            expect_gated: true,
        },
    );
}

// ---------------------------------------------------------------------------
// Contract §7 audit (docs/provider-contract.md, "Code consistency"): shared
// spine, no local re-implementations. The same-file-batch and byte-vs-char
// bugs both came from duplicated logic, so the spine helpers exist exactly
// once and provider crates must call them, not copy them. This scan is the
// executable form of that clause: a new language crate that re-grows a local
// fingerprint walk, path normalizer, outline elision, or prewarm thread
// fails here with the shared home to use instead.
//
// Scope: non-test code in `crates/langs/*/src/**/*.rs`. Comments and string
// literals are scrubbed and `#[cfg(test)]` mods blanked before matching, so
// doc prose mentioning a helper (or a fixture string that looks like code)
// can never trip the audit.
// ---------------------------------------------------------------------------

/// Ident-ish byte, for the word-boundary checks around scan matches.
fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `src` with comments and string/char literals blanked to spaces (newlines kept, so byte
/// offsets still map to line numbers). Handles line comments, nested block comments, plain
/// and raw (`r#"…"#`) and byte strings, and char literals vs lifetimes — after this pass,
/// every remaining brace and `fn` token is real code.
fn scrub(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for k in from..to.min(out.len()) {
            if out[k] != b'\n' {
                out[k] = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                let mut depth = 1usize;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                blank(&mut out, start, i);
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                blank(&mut out, start, i);
            }
            // Raw / byte string prefixes: r"…", r#"…"#, b"…", br#"…"#. Only when the
            // prefix starts a token (not the tail of an identifier) and a quote follows.
            b'r' | b'b' if i == 0 || !is_ident(b[i - 1]) => {
                let mut j = i + 1;
                if b[i] == b'b' && b.get(j) == Some(&b'r') {
                    j += 1;
                }
                let mut hashes = 0usize;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let start = i;
                    let close: Vec<u8> =
                        std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                    let mut k = j + 1;
                    while k < b.len() && !b[k..].starts_with(&close) {
                        k += 1;
                    }
                    i = (k + close.len()).min(b.len());
                    blank(&mut out, start, i);
                } else {
                    i += 1;
                }
            }
            // Char literal ('a', '\n') vs lifetime ('static): a lifetime never closes with
            // a quote one-or-two bytes in.
            b'\'' => {
                if b.get(i + 1) == Some(&b'\\') {
                    let start = i;
                    let mut k = i + 1;
                    while k < b.len() && b[k] != b'\'' {
                        k += if b[k] == b'\\' { 2 } else { 1 };
                    }
                    i = (k + 1).min(b.len());
                    blank(&mut out, start, i);
                } else if b.get(i + 2) == Some(&b'\'') {
                    blank(&mut out, i, i + 3);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("scrub only rewrites ASCII bytes to spaces")
}

/// Byte offset of the `}` closing the `{` at `open` (assumes scrubbed input, where every
/// brace is code).
fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, c) in s.bytes().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// Blank every `#[cfg(test)] mod …` block: test-local helpers may shadow spine names
/// (fixture builders and the like) without owning production behavior.
fn blank_test_mods(scrubbed: &mut String) {
    const ATTR: &str = "#[cfg(test)]";
    let mut from = 0;
    while let Some(off) = scrubbed[from..].find(ATTR) {
        let attr = from + off;
        let mut j = attr + ATTR.len();
        // Skip whitespace and any further attributes between the cfg and the item.
        loop {
            while scrubbed.as_bytes().get(j).is_some_and(|c| c.is_ascii_whitespace()) {
                j += 1;
            }
            if scrubbed[j..].starts_with("#[") {
                match scrubbed[j..].find(']') {
                    Some(e) => j += e + 1,
                    None => break,
                }
            } else {
                break;
            }
        }
        let item = &scrubbed[j..];
        let end = if item.starts_with("mod") || item.starts_with("pub mod") {
            item.find('{').and_then(|open| matching_brace(scrubbed, j + open))
        } else {
            None
        };
        // A cfg(test) on a non-mod item: blank just the attribute so the scan advances.
        let end = end.unwrap_or(attr + ATTR.len() - 1);
        let mut bytes = std::mem::take(scrubbed).into_bytes();
        for b in &mut bytes[attr..=end] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
        *scrubbed = String::from_utf8(bytes).expect("blanking rewrites ASCII bytes to spaces");
        from = end + 1;
    }
}

/// Every definition `fn <name>` in scrubbed source (word-boundary checked): byte offset of
/// the match plus the body span, `None` for bodiless declarations (`fn f();`).
fn fn_defs(scrubbed: &str, name: &str) -> Vec<(usize, Option<(usize, usize)>)> {
    let b = scrubbed.as_bytes();
    let pat = format!("fn {name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = scrubbed[from..].find(&pat) {
        let at = from + off;
        from = at + pat.len();
        let before_ok = at == 0 || !is_ident(b[at - 1]);
        let after_ok = b
            .get(at + pat.len())
            .is_none_or(|&c| c == b'(' || c == b'<' || c.is_ascii_whitespace());
        if !(before_ok && after_ok) {
            continue;
        }
        let sig = &scrubbed[at..];
        let body = match (sig.find('{'), sig.find(';')) {
            (Some(open), semi) if semi.is_none_or(|s| open < s) => {
                matching_brace(scrubbed, at + open).map(|close| (at + open, close + 1))
            }
            _ => None,
        };
        out.push((at, body));
    }
    out
}

fn line_of(s: &str, at: usize) -> usize {
    s[..at].bytes().filter(|&c| c == b'\n').count() + 1
}

/// Contract §7 made executable: no provider-local copy of a spine helper survives in
/// `crates/langs/*/src`. Each rule names the clause and the shared module to use, so a
/// failure is a pointer, not a puzzle. Must stay green on the consolidated tree and fire
/// on any re-grown local copy.
#[test]
fn contract_section7_reimplementation_audit() {
    let clause = "provider-contract.md \u{a7}7 (shared spine, no local re-implementations)";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let langs = root.join("crates/langs");

    // Every .rs under crates/langs/*/src (recursively — a copy hidden in a submodule is
    // still a copy). Integration tests outside src/ are not provider code.
    let mut files = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = std::fs::read_dir(&langs)
        .expect("crates/langs must exist")
        .map(|e| e.unwrap().path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    assert!(files.len() >= 10, "audit found too few lang sources — scan roots moved?");

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let mut src = scrub(&std::fs::read_to_string(path).unwrap());
        blank_test_mods(&mut src);

        // (a) Banned outright: consolidated wholesale, no per-language residue exists.
        for (name, shared) in
            [("fnv1a", "ci_core::fingerprint::fnv1a"), ("rel", "ci_core::rel_path")]
        {
            for (at, _) in fn_defs(&src, name) {
                violations.push(format!(
                    "{rel}:{}: local `fn {name}` — {clause}; use `{shared}`",
                    line_of(&src, at)
                ));
            }
        }

        // (b) Allowed only as thin delegators: the per-language part of a fingerprint is
        // the is_input predicate + path + version, so a wrapper is fine — a wrapper whose
        // body never reaches the shared module is a reimplementation.
        for name in ["source_fingerprint", "store_fingerprint"] {
            for (at, body) in fn_defs(&src, name) {
                let delegates =
                    body.is_some_and(|(s, e)| src[s..e].contains(&format!("fingerprint::{name}(")));
                if !delegates {
                    violations.push(format!(
                        "{rel}:{}: local `fn {name}` does not delegate to \
                         `ci_core::fingerprint::{name}` — {clause}; keep only the is_input \
                         predicate, fingerprint path, and version per provider",
                        line_of(&src, at)
                    ));
                }
            }
        }

        // (c) Outline shape: providers call `ci_treesitter::outline`, never hand-pair
        // body_ranges with an elision call. lang-fallback is the one sanctioned direct
        // caller: its per-language placeholder (`elide_bodies_with`, Python `...`) is
        // exactly the variant the shared fixed-placeholder helper does not express.
        if !rel.ends_with("lang-fallback/src/lib.rs") {
            let mut from = 0;
            while let Some(off) = src[from..].find("elide_bodies") {
                let at = from + off;
                from = at + "elide_bodies".len();
                if at > 0 && is_ident(src.as_bytes()[at - 1]) {
                    continue;
                }
                violations.push(format!(
                    "{rel}:{}: direct `elide_bodies` call — {clause}; build outlines via \
                     `ci_treesitter::outline` (grammar + def/body kinds are the only \
                     per-language inputs)",
                    line_of(&src, at)
                ));
            }
        }

        // (d) Prewarm discipline: the lock/wait/no-double-start behavior lives in
        // `ci_edit::spawn_prewarm`; any other spawn inside a provider's `prewarm` is the
        // hand-rolled thread pattern the consolidation removed. Spawns elsewhere (engine
        // IO pumps) are legitimate and out of scope.
        for (at, body) in fn_defs(&src, "prewarm") {
            let hand_rolled =
                body.is_some_and(|(s, e)| src[s..e].replace("spawn_prewarm", "").contains("spawn"));
            if hand_rolled {
                violations.push(format!(
                    "{rel}:{}: `fn prewarm` spawns its own thread — {clause}; use \
                     `ci_edit::spawn_prewarm` (pass the engine constructor + warming call)",
                    line_of(&src, at)
                ));
            }
        }
    }

    // Rule (a) also sweeps the SPINE crates: a helper consolidated into ci-core can be
    // re-grown inside ci-edit/ci-mcp just as easily as inside a provider (Composed carried
    // a verbatim `fn rel` copy through two verified gate runs before this scan existed).
    // Only the canonical homes are exempt.
    let canonical_homes = ["crates/ci-core/src/fingerprint.rs", "crates/ci-core/src/paths.rs"];
    let mut spine_files = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("crates"))
        .expect("crates/ must exist")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "langs"))
        .map(|p| p.join("src"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                spine_files.push(p);
            }
        }
    }
    spine_files.sort();
    assert!(spine_files.len() >= 15, "audit found too few spine sources — scan roots moved?");
    for path in &spine_files {
        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        if canonical_homes.contains(&rel.as_str()) {
            continue;
        }
        let mut src = scrub(&std::fs::read_to_string(path).unwrap());
        blank_test_mods(&mut src);
        for (name, shared) in
            [("fnv1a", "ci_core::fingerprint::fnv1a"), ("rel", "ci_core::rel_path")]
        {
            for (at, _) in fn_defs(&src, name) {
                violations.push(format!(
                    "{rel}:{}: local `fn {name}` in a spine crate — {clause}; use `{shared}`",
                    line_of(&src, at)
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "contract \u{a7}7 audit: provider-local reimplementations of spine helpers:\n{}",
        violations.join("\n")
    );
}
