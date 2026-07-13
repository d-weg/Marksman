//! ci-providers — the ONE `make_provider`: language name → constructed provider.
//!
//! Both binaries (`marksman` and `marksman-mcp`) used to carry their own copy of this match,
//! and they drifted: the CLI silently lacked the php/swift gated arms, so those repos got
//! ungated fallback edits from one binary and gated providers from the other. This crate is
//! the single assembly point; a binary contributes only its log prefix and its registry
//! POLICY (what to do about `Failed` languages stays per-binary).
//!
//! This crate sits ABOVE the lang crates on purpose — `ci-build` deliberately has zero
//! lang-crate deps (`build_registry` takes this function as a closure precisely to invert
//! that dependency), so it must not host the assembly.
use ci_build::ProviderBuild;
use ci_core::Config;
use ci_proto::ProcessProvider;
use lang_fallback::{FallbackProvider, FbLang};
use lang_rust::RustProvider;
use lang_ts::TsProvider;
use std::path::Path;
use std::sync::Arc;

/// Construct the provider for one language, honoring the manifest's vendored binary and
/// `CI_PROVIDER=sidecar`. Called once per active language by `ci_build::build_registry`, so a
/// language's toolchain is never probed, fetched, or run unless the repo actually has its
/// files (a Rust-only repo never touches Node). Each language's TOOLCHAIN is checked before
/// any of it runs: a missing dependency becomes `Unavailable` with the install instructions
/// (permanent, carried on the registry), not a cryptic spawn error or a retry loop.
/// `prefix` is the binary's log tag (`"[marksman]"` / `"[marksman-mcp]"`).
pub fn make_provider(lang: &str, root: &Path, config: &Config, prefix: &str) -> ProviderBuild {
    if std::env::var("CI_PROVIDER").as_deref() == Ok("sidecar") {
        if let Some(cmd) = ci_proto::sidecar_command_with(lang, root, false, config.provider_bin(lang)) {
            eprintln!("{prefix} language: {lang} (sidecar process — protobuf wire)");
            match ProcessProvider::spawn(cmd) {
                Ok(p) => return ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("{prefix} sidecar {lang} failed to start ({e}); skipping");
                    return ProviderBuild::Failed(e.to_string());
                }
            }
        }
        eprintln!("{prefix} CI_PROVIDER=sidecar but no marksman-provider-{lang} found — using in-process");
    }
    match lang {
        "rust" => {
            // Reads are in-process tree-sitter (no external deps) — the provider always comes
            // up. rust-analyzer gates only WRITES: warn now if missing, and apply_edits repeats
            // the same install hint if actually invoked.
            if let Some(missing) = lang_rust::toolchain().describe_missing() {
                eprintln!("{prefix} warning: {missing}\n  (rust reads work; type-checked edits will fail until installed)");
            }
            eprintln!("{prefix} language: rust (tree-sitter reads + rust-analyzer scip graph; gate: cargo check, renames: rust-analyzer)");
            ProviderBuild::Ready(Arc::new(RustProvider::open(root, config.scip_enabled("rust"))))
        }
        "ts" => {
            // CI_TS_MODE ablation arms (docs/benchmarks.md): serve TS from tree-sitter instead
            // of SCIP — "treesitter" is the generic UNGATED provider (needs nothing external),
            // "treesitter-gated" keeps the warm ts-morph gate on a tree-sitter read path.
            match std::env::var("CI_TS_MODE").as_deref() {
                Ok("treesitter") => {
                    eprintln!("{prefix} language: typescript (ABLATION: generic tree-sitter, UNGATED — CI_TS_MODE=treesitter)");
                    return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Ts)));
                }
                Ok("treesitter-gated") => {
                    if let Some(missing) = lang_ts::toolchain().describe_missing() {
                        eprintln!("{prefix} typescript DISABLED (gated ablation still needs the gate's toolchain):\n{missing}");
                        return ProviderBuild::Unavailable(missing);
                    }
                    eprintln!("{prefix} language: typescript (ABLATION: tree-sitter read + ts-morph gate — CI_TS_MODE=treesitter-gated)");
                    return ProviderBuild::Ready(Arc::new(lang_ts::TsTreeGated::new(root)));
                }
                Ok("lsp") => {
                    // COMPARISON arm: index by sweeping the tsgo language server (ci-lsp-index)
                    // instead of scip-typescript; same SCIP read path, different producer.
                    if let Some(missing) = lang_ts::toolchain().describe_missing() {
                        eprintln!("{prefix} typescript DISABLED (the LSP sweep still needs Node for tsgo via npx):\n{missing}");
                        return ProviderBuild::Unavailable(missing);
                    }
                    eprintln!("{prefix} language: typescript (COMPARISON: tsgo LSP-sweep index — CI_TS_MODE=lsp)");
                    return match TsProvider::index_with_lsp_sweep(root) {
                        Ok(p) => ProviderBuild::Ready(Arc::new(p)),
                        Err(e) => {
                            eprintln!("{prefix} tsgo LSP-sweep indexing failed ({e}); skipping TS files");
                            ProviderBuild::Failed(e.to_string())
                        }
                    };
                }
                _ => {}
            }
            // TypeScript needs Node for BOTH paths (scip-typescript index + the gate). Missing
            // toolchain = the language is off, loudly and actionably — never a half-working
            // provider or an ungated fallback.
            if let Some(missing) = lang_ts::toolchain().describe_missing() {
                eprintln!("{prefix} typescript DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            // `open` loads the cached index.scip when the source fingerprint still matches
            // (ms), and re-runs scip-typescript only when it doesn't (~20s).
            eprintln!("{prefix} language: typescript — opening scip index for {} …", root.display());
            match TsProvider::open(root) {
                Ok(p) => ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("{prefix} typescript indexing failed ({e}); skipping TS files");
                    ProviderBuild::Failed(e.to_string())
                }
            }
        }
        "java" => {
            // The ungated ABLATION arm (mirrors CI_TS_MODE=treesitter): the generic fallback
            // provider, reachable for measurement — never the silent degradation path.
            if std::env::var("CI_JAVA_MODE").as_deref() == Ok("treesitter") {
                eprintln!("{prefix} language: java (ABLATION: generic tree-sitter, UNGATED — CI_JAVA_MODE=treesitter)");
                return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Java)));
            }
            // Gated tier: reads are in-process tree-sitter, the WRITE gate is the resident
            // javax.tools sidecar — so a missing JDK disables the language with the install
            // hint (contract §6), it never falls back to ungated edits silently.
            if let Some(missing) = lang_java::gate_missing() {
                eprintln!("{prefix} java DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            if let Some(t) = lang_java::toolchain().tools.iter().find(|t| t.tool == "jdtls" && t.found.is_none()) {
                eprintln!("{prefix} warning: java rename/move needs {} — Install: {}\n  (reads and the javac gate work without it)", t.tool, t.install);
            }
            eprintln!("{prefix} language: java (tree-sitter reads; gate: resident javax.tools sidecar, renames: jdtls)");
            ProviderBuild::Ready(Arc::new(lang_java::JavaProvider::new(root)))
        }
        "php" => {
            // The ungated ABLATION arm (mirrors CI_JAVA_MODE=treesitter): the generic fallback
            // provider, reachable for measurement — never the silent degradation path.
            if std::env::var("CI_PHP_MODE").as_deref() == Ok("treesitter") {
                eprintln!("{prefix} language: php (ABLATION: generic tree-sitter, UNGATED — CI_PHP_MODE=treesitter)");
                return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Php)));
            }
            // Gated tier: reads are in-process tree-sitter, the WRITE gate is PHPStan — so a
            // missing php/phpstan disables the language with the install hint (contract §6), it
            // never falls back to ungated edits silently.
            if let Some(missing) = lang_php::gate_missing(root) {
                eprintln!("{prefix} php DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            if let Some(t) = lang_php::toolchain(root).tools.iter().find(|t| t.tool == "phpactor" && t.found.is_none()) {
                eprintln!("{prefix} warning: php rename/move needs {} — Install: {}\n  (reads and the phpstan gate work without it)", t.tool, t.install);
            }
            eprintln!("{prefix} language: php (tree-sitter reads; gate: phpstan analyse, renames: phpactor)");
            ProviderBuild::Ready(Arc::new(lang_php::PhpProvider::new(root)))
        }
        "swift" => {
            // The ungated ABLATION arm (mirrors CI_JAVA_MODE=treesitter): the generic fallback
            // provider, reachable for measurement — never the silent degradation path.
            if std::env::var("CI_SWIFT_MODE").as_deref() == Ok("treesitter") {
                eprintln!("{prefix} language: swift (ABLATION: generic tree-sitter, UNGATED — CI_SWIFT_MODE=treesitter)");
                return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Swift)));
            }
            // Gated tier: reads are in-process tree-sitter, the WRITE gate is `swift build` — so a
            // missing Swift toolchain disables the language with the install hint (contract §6), it
            // never falls back to ungated edits silently.
            if let Some(missing) = lang_swift::gate_missing() {
                eprintln!("{prefix} swift DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            if let Some(t) = lang_swift::toolchain().tools.iter().find(|t| t.tool == "sourcekit-lsp" && t.found.is_none()) {
                eprintln!("{prefix} warning: swift rename needs {} — Install: {}\n  (reads and the `swift build` gate work without it)", t.tool, t.install);
            }
            eprintln!("{prefix} language: swift (tree-sitter reads; gate: `swift build`, renames: sourcekit-lsp)");
            ProviderBuild::Ready(Arc::new(lang_swift::SwiftProvider::new(root)))
        }
        // Every other supported language rides the generic tree-sitter fallback: full read
        // path, ungated edits, zero external dependencies.
        other => match FbLang::from_name(other) {
            Some(fb) => {
                eprintln!(
                    "{prefix} language: {} (generic tree-sitter fallback, in-process — edits are ungated)",
                    fb.label()
                );
                ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, fb)))
            }
            None => ProviderBuild::Failed(format!("unknown language '{other}'")),
        },
    }
}
