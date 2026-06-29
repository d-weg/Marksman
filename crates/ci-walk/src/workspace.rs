use ci_core::{Error, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A package = a directory with its own manifest (package.json in v1). Used for
/// monorepo-aware ranking; the owner of a file is the deepest enclosing package.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Repo-relative package directory ("" for the root package).
    pub dir: PathBuf,
    /// Repo-relative manifest path.
    pub manifest: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub is_monorepo: bool,
    pub packages: Vec<Package>,
}

impl Workspace {
    /// The package owning `rel` = the one whose dir is the longest path prefix.
    pub fn package_for(&self, rel: &Path) -> Option<&Package> {
        self.packages
            .iter()
            .filter(|p| rel.starts_with(&p.dir))
            .max_by_key(|p| p.dir.components().count())
    }
}

/// Find every `package.json` (gitignore-aware) and turn each into a Package.
/// Falls back to a single root package when none is found.
pub fn detect_workspace(root: &Path) -> Result<Workspace> {
    let mut packages = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        if entry.file_name() != "package.json" {
            continue;
        }
        let abs = entry.path();
        let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
        let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = read_pkg_name(abs).unwrap_or_else(|| dir_name(&dir));
        packages.push(Package { name, dir, manifest: rel });
    }
    if packages.is_empty() {
        packages.push(Package {
            name: "root".into(),
            dir: PathBuf::new(),
            manifest: PathBuf::from("package.json"),
        });
    }
    packages.sort_by(|a, b| a.dir.cmp(&b.dir));
    let is_monorepo = packages.len() > 1;
    Ok(Workspace { is_monorepo, packages })
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".into())
}

fn read_pkg_name(abs: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(abs).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("name")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_monorepo_and_owner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("apps/backend/src")).unwrap();
        fs::create_dir_all(root.join("apps/frontend")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"mono"}"#).unwrap();
        fs::write(root.join("apps/backend/package.json"), r#"{"name":"@app/backend"}"#).unwrap();
        fs::write(root.join("apps/frontend/package.json"), r#"{"name":"@app/frontend"}"#).unwrap();

        let ws = detect_workspace(root).unwrap();
        assert!(ws.is_monorepo);
        assert_eq!(ws.packages.len(), 3);

        let owner = ws.package_for(Path::new("apps/backend/src/x.ts")).unwrap();
        assert_eq!(owner.name, "@app/backend");
    }
}
