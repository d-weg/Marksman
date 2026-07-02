//! The conformance instances: every provider × its mini-fixture through the shared battery.
//! Adding a language = adding its fixtures here (see docs/provider-contract.md). The fast tier
//! runs in CI; `-- --ignored` adds the providers that shell out to real tools.

use ci_conformance::{run_edit_battery, run_read_battery, EditFixture, ReadFixture};
use ci_core::LanguageProvider;
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;

fn fallback(lang: FbLang) -> Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>> {
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
    let mk: Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>> =
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
    let mk = fallback(FbLang::Java);
    run_read_battery(
        &mk,
        &ReadFixture {
            label: "fallback/java",
            files: &[(
                "Svc.java",
                "// Probes the service.\npublic class Svc {\n  private int hits = 0;\n\n  public int probe(String url) {\n    return 1;\n  }\n}\n",
            )],
            target: "Svc.java",
            want_ids: &["Svc.java#Svc", "Svc.java#Svc.probe", "Svc.java#Svc.hits"],
            fn_symbol: "Svc.java#Svc.probe",
            expect_params: true,
            doc_symbol: Some("Svc.java#Svc"),
            edge: None,
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
    let mk: Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>> =
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
    let mk: Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>> = Box::new(|root| {
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

// Real-tool tier: the product TS provider (scip-typescript via npx).
// `cargo test -p ci-conformance -- --ignored`
#[test]
#[ignore]
fn conformance_ts_scip() {
    let mk: Box<dyn Fn(&Path) -> Box<dyn LanguageProvider>> = Box::new(|root| {
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
