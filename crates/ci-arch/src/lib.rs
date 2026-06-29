//! ci-arch — zero-API folder/architecture map (port of folders.ts). Pure Rust,
//! language-blind: per-directory file-kind histograms, co-located docs, and detected
//! "module templates" (sibling dirs that repeat a file shape). Tells an agent where
//! a new module goes before a create.
use ci_core::{Error, Result};
use ignore::WalkBuilder;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const MAX_ARCH_FILES: usize = 20_000;
const DOC_NAMES: &[&str] = &["README.md", "AGENTS.md", "CLAUDE.md", "ARCHITECTURE.md"];

#[derive(Debug, Clone)]
pub struct ArchNode {
    /// Repo-relative directory ("" = root).
    pub dir: String,
    pub files: usize,
    /// filename-pattern histogram, e.g. {".service.ts": 3, "index": 1, ".tsx": 5}.
    pub suffixes: BTreeMap<String, usize>,
    pub doc: Option<String>,
    /// If this dir is a module container: the file shape its sub-modules repeat.
    pub template: Option<Vec<String>>,
    pub module_count: usize,
}

/// `foo.service.ts` → ".service.ts" · `index.ts` → "index" · `Bar.tsx` → ".tsx".
fn file_suffix(name: &str) -> String {
    if name == "index.ts" || name == "index.tsx" {
        return "index".to_string();
    }
    let ext = if name.ends_with(".tsx") {
        Some("tsx")
    } else if name.ends_with(".ts") {
        Some("ts")
    } else {
        None
    };
    if let Some(ext) = ext {
        let stem = &name[..name.len() - ext.len() - 1]; // strip ".ts"/".tsx"
        if let Some(dot) = stem.rfind('.') {
            let kind = &stem[dot + 1..];
            if !kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return format!(".{kind}.{ext}");
            }
        }
        return format!(".{ext}");
    }
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(e) => format!(".{e}"),
        None => name.to_string(),
    }
}

pub fn build_architecture(root: &Path) -> Result<Vec<ArchNode>> {
    let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut count = 0usize;
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        if !entry.file_type().map_or(false, |t| t.is_file()) {
            continue;
        }
        let abs = entry.path();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        let name = match rel.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let is_ts = (name.ends_with(".ts") || name.ends_with(".tsx")) && !name.ends_with(".d.ts");
        if !is_ts {
            continue;
        }
        if rel_s.contains("node_modules/")
            || rel_s.contains("/dist/")
            || rel_s.starts_with("dist/")
            || rel_s.contains("/build/")
            || rel_s.starts_with("build/")
            || rel_s.contains(".codeindex")
        {
            continue;
        }
        count += 1;
        if count > MAX_ARCH_FILES {
            return Err(Error::Other(format!(
                "architecture scan exceeded {MAX_ARCH_FILES} files under {} — pass a narrower path",
                root.display()
            )));
        }
        let dir = rel.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        by_dir.entry(dir).or_default().push(name.to_string());
    }

    let dirs: Vec<String> = by_dir.keys().cloned().collect();
    let suffix_set = |d: &str| -> BTreeSet<String> {
        by_dir.get(d).map(|names| names.iter().map(|n| file_suffix(n)).collect()).unwrap_or_default()
    };

    // Candidate dirs = dirs with files + every ancestor (so a pure container dir —
    // children but no direct files — can still be detected as a module template).
    let mut all: BTreeSet<String> = by_dir.keys().cloned().collect();
    for d in &dirs {
        let mut cur = Path::new(d.as_str()).parent();
        while let Some(p) = cur {
            let ps = p.to_string_lossy().replace('\\', "/");
            let stop = ps.is_empty();
            all.insert(ps);
            if stop {
                break;
            }
            cur = p.parent();
        }
    }

    let mut nodes = Vec::new();
    for d in &all {
        let names = by_dir.get(d).cloned().unwrap_or_default();
        let mut suffixes: BTreeMap<String, usize> = BTreeMap::new();
        for n in &names {
            *suffixes.entry(file_suffix(n)).or_insert(0) += 1;
        }
        let doc = DOC_NAMES.iter().find(|dn| root.join(d).join(dn).exists()).map(|s| s.to_string());

        // Module container: immediate child dirs that repeat a file shape.
        let child_dirs: Vec<&String> = dirs
            .iter()
            .filter(|cd| {
                if *cd == d {
                    return false;
                }
                let parent =
                    Path::new(cd.as_str()).parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                if d.is_empty() {
                    !cd.contains('/')
                } else {
                    parent == *d
                }
            })
            .collect();

        let mut template = None;
        let mut module_count = 0;
        if child_dirs.len() >= 2 {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for cd in &child_dirs {
                for s in suffix_set(cd) {
                    *counts.entry(s).or_insert(0) += 1;
                }
            }
            let threshold = child_dirs.len().div_ceil(2);
            let mut common: Vec<String> =
                counts.into_iter().filter(|(_, c)| *c >= threshold).map(|(s, _)| s).collect();
            common.sort();
            if common.len() >= 2 {
                template = Some(common);
                module_count = child_dirs.len();
            }
        }

        // Drop pure-container dirs that aren't an actual module template (noise).
        if names.is_empty() && template.is_none() {
            continue;
        }
        nodes.push(ArchNode { dir: d.clone(), files: names.len(), suffixes, doc, template, module_count });
    }
    nodes.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(nodes)
}

pub fn format_architecture(nodes: &[ArchNode], subpath: Option<&str>) -> String {
    let filtered: Vec<&ArchNode> = match subpath {
        Some(sp) => nodes.iter().filter(|n| n.dir == sp || n.dir.starts_with(&format!("{sp}/"))).collect(),
        None => nodes.iter().collect(),
    };
    if filtered.is_empty() {
        return "(no source directories found)".to_string();
    }
    let mut lines = Vec::new();
    for n in filtered {
        let mut sufs: Vec<(&String, &usize)> = n.suffixes.iter().collect();
        sufs.sort_by(|a, b| b.1.cmp(a.1));
        let top: String =
            sufs.iter().take(5).map(|(s, c)| format!("{s}×{c}")).collect::<Vec<_>>().join(", ");
        let dir = if n.dir.is_empty() { "." } else { n.dir.as_str() };
        let mut line = format!("{dir}/  ({} files: {top})", n.files);
        if let Some(doc) = &n.doc {
            line.push_str(&format!("  [doc: {doc}]"));
        }
        if let Some(t) = &n.template {
            line.push_str(&format!(
                "\n    ↳ module container: {} sub-modules, each ~ {{ {} }}",
                n.module_count,
                t.join(", ")
            ));
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn file_suffix_classifies() {
        assert_eq!(file_suffix("foo.service.ts"), ".service.ts");
        assert_eq!(file_suffix("index.ts"), "index");
        assert_eq!(file_suffix("Bar.tsx"), ".tsx");
        assert_eq!(file_suffix("plain.ts"), ".ts");
    }

    #[test]
    fn detects_module_template() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // features/{auth,bids,users}/ each with .controller.ts + .service.ts + index.ts
        for m in ["auth", "bids", "users"] {
            let md = root.join("src/features").join(m);
            fs::create_dir_all(&md).unwrap();
            fs::write(md.join(format!("{m}.controller.ts")), "").unwrap();
            fs::write(md.join(format!("{m}.service.ts")), "").unwrap();
            fs::write(md.join("index.ts"), "").unwrap();
        }

        let nodes = build_architecture(root).unwrap();
        let feat = nodes.iter().find(|n| n.dir == "src/features").expect("features node");
        let t = feat.template.as_ref().expect("template detected");
        assert!(t.contains(&".controller.ts".to_string()));
        assert!(t.contains(&".service.ts".to_string()));
        assert!(t.contains(&"index".to_string()));
        assert_eq!(feat.module_count, 3);

        let out = format_architecture(&nodes, None);
        assert!(out.contains("module container"));
    }
}
