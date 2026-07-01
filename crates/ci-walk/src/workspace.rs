use ci_core::{Error, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A package = a directory with its own manifest (`package.json` / `Cargo.toml` / `pyproject.toml`).
/// Used for monorepo-aware ranking; the owner of a file is the deepest enclosing package. `deps`
/// (dependency names, language-agnostic) feed role inference at index time.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Repo-relative package directory ("" for the root package).
    pub dir: PathBuf,
    /// Repo-relative manifest path.
    pub manifest: PathBuf,
    /// Dependency names declared by the manifest (npm / crate / PyPI), for `infer_role`.
    pub deps: Vec<String>,
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

/// Find every package manifest (`package.json` / `Cargo.toml` / `pyproject.toml`, gitignore-aware)
/// and turn each into a [`Package`] with its name + declared deps. A pure Cargo *workspace* root
/// (no `[package]`) is not itself a package. Falls back to a single root package when none is found.
pub fn detect_workspace(root: &Path) -> Result<Workspace> {
    let mut packages = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let entry = result.map_err(|e| Error::Other(e.to_string()))?;
        let fname = entry.file_name().to_string_lossy();
        if !matches!(fname.as_ref(), "package.json" | "Cargo.toml" | "pyproject.toml") {
            continue;
        }
        let abs = entry.path();
        let Some(m) = parse_manifest(abs) else { continue }; // e.g. a bare Cargo `[workspace]` root
        let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
        let dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = m.name.unwrap_or_else(|| dir_name(&dir));
        packages.push(Package { name, dir, manifest: rel, deps: m.deps });
    }
    if packages.is_empty() {
        packages.push(Package {
            name: "root".into(),
            dir: PathBuf::new(),
            manifest: PathBuf::from("package.json"),
            deps: vec![],
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

/// Name (if declared) + dependency names parsed from a manifest. `None` when the file isn't a real
/// package (a Cargo `[workspace]` root with no `[package]`, or an unparseable/nameless pyproject).
struct ManifestInfo {
    name: Option<String>,
    deps: Vec<String>,
}

fn parse_manifest(abs: &Path) -> Option<ManifestInfo> {
    let raw = std::fs::read_to_string(abs).ok()?;
    match abs.file_name()?.to_str()? {
        "package.json" => {
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            let name = v.get("name").and_then(|n| n.as_str()).map(str::to_string);
            let mut deps = Vec::new();
            for key in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(obj) = v.get(key).and_then(|d| d.as_object()) {
                    deps.extend(obj.keys().cloned());
                }
            }
            Some(ManifestInfo { name, deps })
        }
        "Cargo.toml" => {
            let v: toml::Value = toml::from_str(&raw).ok()?;
            // A pure `[workspace]` root (no `[package]`) is not a crate — skip it.
            let name = v.get("package")?.get("name")?.as_str()?.to_string();
            let deps = toml_table_keys(&v, "dependencies");
            Some(ManifestInfo { name: Some(name), deps })
        }
        "pyproject.toml" => {
            let v: toml::Value = toml::from_str(&raw).ok()?;
            let project = v.get("project");
            let poetry = v.get("tool").and_then(|t| t.get("poetry"));
            let name = project
                .and_then(|p| p.get("name"))
                .or_else(|| poetry.and_then(|p| p.get("name")))
                .and_then(|n| n.as_str())?
                .to_string();
            let mut deps = Vec::new();
            // PEP 621: `[project].dependencies = ["django>=4", ...]`.
            if let Some(arr) = project.and_then(|p| p.get("dependencies")).and_then(|d| d.as_array()) {
                deps.extend(arr.iter().filter_map(|d| d.as_str()).map(dep_name));
            }
            // Poetry: `[tool.poetry.dependencies]` table (skip the `python` pin).
            if let Some(t) = poetry.and_then(|p| p.get("dependencies")).and_then(|d| d.as_table()) {
                deps.extend(t.keys().filter(|k| *k != "python").cloned());
            }
            Some(ManifestInfo { name: Some(name), deps })
        }
        _ => None,
    }
}

/// Keys of a top-level TOML table (`[dependencies]`), empty when absent.
fn toml_table_keys(v: &toml::Value, table: &str) -> Vec<String> {
    v.get(table)
        .and_then(|d| d.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default()
}

/// The bare package name from a PEP 508 requirement string: `django>=4.0` → `django`,
/// `requests[security]` → `requests`.
fn dep_name(spec: &str) -> String {
    spec.trim()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .next()
        .unwrap_or("")
        .to_string()
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

    #[test]
    fn parses_cargo_and_pyproject_names_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A Rust crate.
        fs::create_dir_all(root.join("svc")).unwrap();
        fs::write(
            root.join("svc/Cargo.toml"),
            "[package]\nname = \"svc\"\n[dependencies]\naxum = \"0.7\"\nsqlx = { version = \"0.7\" }\n",
        )
        .unwrap();
        // A Python project (PEP 621).
        fs::create_dir_all(root.join("api")).unwrap();
        fs::write(
            root.join("api/pyproject.toml"),
            "[project]\nname = \"api\"\ndependencies = [\"django>=4.0\", \"celery\"]\n",
        )
        .unwrap();
        // A bare Cargo workspace root (no [package]) — must NOT become a package.
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"svc\"]\n").unwrap();

        let ws = detect_workspace(root).unwrap();
        let svc = ws.packages.iter().find(|p| p.name == "svc").expect("svc crate");
        assert!(svc.deps.contains(&"axum".to_string()), "cargo deps: {:?}", svc.deps);
        assert!(svc.deps.contains(&"sqlx".to_string()));
        let api = ws.packages.iter().find(|p| p.name == "api").expect("api project");
        assert!(api.deps.contains(&"django".to_string()), "pep621 deps: {:?}", api.deps);
        assert!(api.deps.contains(&"celery".to_string()));
        assert!(
            !ws.packages.iter().any(|p| p.manifest == Path::new("Cargo.toml")),
            "a bare [workspace] root is not a package"
        );
    }
}
