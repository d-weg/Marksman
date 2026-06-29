use crate::lang::Lang;
use ci_core::{Config, Error, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A file selected for indexing, with its language and whether it's a doc.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub rel: PathBuf,
    pub abs: PathBuf,
    pub lang: Lang,
    pub is_doc: bool,
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

/// Walk `root` honoring `.gitignore`, applying config include/exclude/doc globs.
/// Returns code files (TS/TSX) and, if `index_docs`, markdown — sorted by path.
pub fn discover(root: &Path, config: &Config) -> Result<Vec<DiscoveredFile>> {
    let include = build_globset(&config.include)?;
    let exclude = build_globset(&config.exclude)?;
    let docs = build_globset(&config.doc_globs)?;

    let mut out = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        if !entry.file_type().map_or(false, |t| t.is_file()) {
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
        let is_code = lang.is_code() && include.is_match(rel_str.as_str());
        let is_doc = config.index_docs && docs.is_match(rel_str.as_str());
        if !is_code && !is_doc {
            continue;
        }
        out.push(DiscoveredFile { rel, abs, lang, is_doc });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_code_and_docs_skips_excluded() {
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
        assert!(rels.contains(&"README.md".to_string()));
        assert!(!rels.iter().any(|r| r.contains("node_modules")), "node_modules excluded");
        assert!(!rels.iter().any(|r| r.ends_with(".d.ts")), "declaration files skipped");
    }
}
