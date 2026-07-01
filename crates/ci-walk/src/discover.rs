use crate::lang::Lang;
use ci_core::{Config, Error, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A code file selected for indexing, with its language.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub rel: PathBuf,
    pub abs: PathBuf,
    pub lang: Lang,
}

/// Build a GlobSet from config patterns. Each `**/x` pattern also gets a bare `x`
/// variant so root-level files match (globset's `**/` wants ≥1 leading segment).
fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|e| Error::Config(e.to_string()))?);
        if let Some(rest) = p.strip_prefix("**/") {
            b.add(Glob::new(rest).map_err(|e| Error::Config(e.to_string()))?);
        }
    }
    b.build().map_err(|e| Error::Config(e.to_string()))
}

/// Walk `root` honoring `.gitignore`, applying config include/exclude globs.
/// Returns the code files a provider handles — sorted by path.
pub fn discover(root: &Path, config: &Config) -> Result<Vec<DiscoveredFile>> {
    let include = build_globset(&config.include)?;
    let exclude = build_globset(&config.exclude)?;

    let mut out = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if exclude.is_match(rel_str.as_str()) {
            continue;
        }
        let lang = Lang::of(&rel);
        if !lang.is_code() || !include.is_match(rel_str.as_str()) {
            continue;
        }
        out.push(DiscoveredFile { rel, abs, lang });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// The set of code languages that actually have source files under `root` (gitignore- and
/// exclude-aware, but NOT gated by `config.include` — this is what *decides* which providers to
/// build, so it must see every language present). The provider registry uses this so a language's
/// tooling is only spun up when the repo contains its files (e.g. Node only for a `.ts*` repo).
pub fn present_langs(root: &Path, config: &Config) -> Result<BTreeSet<Lang>> {
    let exclude = build_globset(&config.exclude)?;
    let mut langs = BTreeSet::new();
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if exclude.is_match(rel_str.as_str()) {
            continue;
        }
        let lang = Lang::of(rel);
        if lang.is_code() {
            langs.insert(lang);
        }
    }
    Ok(langs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_code_skips_docs_and_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/x")).unwrap();
        fs::write(root.join("src/a.ts"), "export const x = 1;").unwrap();
        fs::write(root.join("src/b.tsx"), "export const y = 2;").unwrap();
        fs::write(root.join("src/c.d.ts"), "declare const z: number;").unwrap();
        fs::write(root.join("node_modules/x/dep.ts"), "export const d = 3;").unwrap();
        fs::write(root.join("README.md"), "# hi").unwrap();

        let files = discover(root, &Config::default()).unwrap();
        let rels: Vec<String> = files.iter().map(|f| f.rel.to_string_lossy().into()).collect();

        assert!(rels.contains(&"src/a.ts".to_string()));
        assert!(rels.contains(&"src/b.tsx".to_string()));
        assert!(!rels.contains(&"README.md".to_string()), "docs are not indexed (code-only)");
        assert!(!rels.iter().any(|r| r.contains("node_modules")), "node_modules excluded");
        assert!(!rels.iter().any(|r| r.ends_with(".d.ts")), "declaration files skipped");
    }

    #[test]
    fn present_langs_finds_every_source_language() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/x")).unwrap();
        fs::write(root.join("src/a.ts"), "export const x = 1;").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn y() {}").unwrap();
        fs::write(root.join("src/c.py"), "def z(): ...").unwrap();
        fs::write(root.join("node_modules/x/dep.ts"), "export const d = 3;").unwrap();
        fs::write(root.join("README.md"), "# hi").unwrap();

        let langs = present_langs(root, &Config::default()).unwrap();
        // Every code language present is detected; excluded dirs and non-code (md) don't count.
        assert!(langs.contains(&Lang::Ts));
        assert!(langs.contains(&Lang::Rust));
        assert!(langs.contains(&Lang::Python));
        assert!(!langs.contains(&Lang::Other), "non-code files (md) don't count");
        // A repo with no source of a language doesn't report it.
        let empty = tempfile::tempdir().unwrap();
        assert!(present_langs(empty.path(), &Config::default()).unwrap().is_empty());
    }
}
