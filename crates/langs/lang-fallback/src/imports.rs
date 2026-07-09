//! Syntactic import resolution — edges exist where the syntax makes them cheap and reliable:
//! Python (dotted modules, relative levels), JS/TS (relative specifiers, TS's `.js`→`.ts`
//! convention), and Java (`import a.b.C` → `<source-root>/a/b/C.java`, package decls being
//! path-bound). Misses cost an edge, never invent one; other fallback languages honestly
//! report none. Plus the gitignore-aware source-file walk the whole crate keys off.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tree_sitter::Node as TsNode;

/// Collect resolvable import edges from a file's syntax tree.
pub(crate) fn collect_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    match node.kind() {
        "import_statement" => {
            // `import a.b.c [as x], d.e` — each dotted name is an absolute module.
            let mut c = node.walk();
            for ch in node.named_children(&mut c) {
                let dotted = match ch.kind() {
                    "dotted_name" => Some(ch),
                    "aliased_import" => ch.child_by_field_name("name"),
                    _ => None,
                };
                if let Some(d) = dotted {
                    let parts = dotted_parts(&d, bytes);
                    push_absolute(root, &parts, out);
                }
            }
        }
        "import_from_statement" => {
            if let Some(module) = node.child_by_field_name("module_name") {
                let (level, mod_parts) = module_spec(&module, bytes);
                if let Some(base) = base_dir(from_rel, level) {
                    // the module itself (`from a.b import …` → a/b.py or a/b/__init__.py)
                    if !mod_parts.is_empty() {
                        if let Some(p) = resolve(root, &base, &mod_parts) {
                            out.push(p);
                        }
                    }
                    // each imported name, in case it's a submodule (`from pkg import sub`)
                    for name in imported_names(&node, &module, bytes) {
                        let mut parts = mod_parts.clone();
                        parts.push(name);
                        if let Some(p) = resolve(root, &base, &parts) {
                            out.push(p);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    // Recurse: imports can be nested (inside functions / try blocks).
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_imports(ch, bytes, from_rel, root, out);
    }
}

/// JS/TS import edges from RELATIVE specifiers (`import … from './x'`, `export … from '../y'`).
/// Bare specifiers are packages (skipped); TS convention `./x.js` resolves to `./x.ts`. Resolution
/// tries the specifier as written, each source extension, then `index.<ext>` — misses cost an
/// edge, never invent one.
pub(crate) fn collect_js_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    if matches!(node.kind(), "import_statement" | "export_statement") {
        if let Some(src) = node.child_by_field_name("source") {
            let spec = src.utf8_text(bytes).unwrap_or("").trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
            if spec.starts_with("./") || spec.starts_with("../") {
                if let Some(p) = resolve_js_specifier(root, from_rel, &spec) {
                    out.push(p);
                }
            }
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_js_imports(ch, bytes, from_rel, root, out);
    }
}

/// Java import edges: `import a.b.C;` → `<source-root>/a/b/C.java`; on-demand `import a.b.*;`
/// → every `.java` in `<source-root>/a/b` if that directory exists. Resolution is CONSERVATIVE
/// per contract §3 — an import that lands on no source file (an external dependency like
/// `java.util.List`, a jar-only symbol) contributes NO edge, never a guessed one. Source roots
/// are the conventional Maven/Gradle dirs plus a root derived from this file's own
/// package-declaration-to-path offset (see [`java_source_roots`]).
pub(crate) fn collect_java_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    if node.kind() == "import_declaration" {
        // Grammar shape: `import [static] <scoped_identifier|identifier> [. *] ;`. The
        // asterisk child marks an on-demand (wildcard) import; the dotted name is the
        // scoped_identifier text with the trailing `.*` (if any) already excluded from it.
        let mut dotted: Option<String> = None;
        let mut wildcard = false;
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            match ch.kind() {
                "scoped_identifier" | "identifier" => {
                    dotted = Some(ch.utf8_text(bytes).unwrap_or("").to_string());
                }
                "asterisk" => wildcard = true,
                _ => {}
            }
        }
        // A `.*` unnamed child renders `asterisk`-less in some grammar builds — detect the
        // trailing token from the whole statement text as a backstop.
        if !wildcard && node.utf8_text(bytes).unwrap_or("").contains(".*") {
            wildcard = true;
        }
        if let Some(dotted) = dotted {
            // `import static a.b.C.member;` / `import static a.b.C.*;` reference the TYPE `a.b.C`,
            // not a package: a single static import's last segment is the member (drop it), and a
            // static import never dir-globs (a static `.*` globs a class's members, not files).
            let is_static = node.utf8_text(bytes).unwrap_or("").split_whitespace().nth(1) == Some("static");
            let mut wildcard = wildcard;
            let mut parts: Vec<String> =
                dotted.split('.').filter(|s| !s.is_empty() && *s != "*").map(str::to_string).collect();
            if is_static {
                if !wildcard {
                    parts.pop(); // drop the imported member → the enclosing type `a.b.C`
                }
                wildcard = false;
            }
            if parts.is_empty() {
                return;
            }
            // The package a resolved file MUST declare to be this import — the guard against a
            // path-coincidence edge (a file sitting at `a/b/C.java` that belongs to a different
            // package). `a.b.C` → `a.b`; a wildcard `a.b.*` → `a.b`.
            let expected_pkg =
                if wildcard { parts.join(".") } else { parts[..parts.len() - 1].join(".") };
            for src_root in java_source_roots(root, from_rel) {
                if wildcard {
                    // `a.b.*` → the package directory's own `.java` files that actually declare
                    // `a.b` (the dir must exist).
                    let mut dir = src_root.clone();
                    for p in &parts {
                        dir.push(p);
                    }
                    let abs_dir = root.join(&dir);
                    if abs_dir.is_dir() {
                        let mut found = false;
                        for entry in std::fs::read_dir(&abs_dir).into_iter().flatten().flatten() {
                            let p = entry.path();
                            if p.extension().and_then(|e| e.to_str()) == Some("java") && p.is_file() {
                                if let Ok(rel) = p.strip_prefix(root) {
                                    if java_declares_package(root, rel, &expected_pkg) {
                                        out.push(norm(rel));
                                        found = true;
                                    }
                                }
                            }
                        }
                        if found {
                            break; // the first source root owning the package wins
                        }
                    }
                } else {
                    // `a.b.C` → <root>/a/b/C.java, only when that file declares package `a.b`.
                    let mut file = src_root.clone();
                    for p in &parts {
                        file.push(p);
                    }
                    let file = file.with_extension("java");
                    if root.join(&file).is_file() && java_declares_package(root, &file, &expected_pkg) {
                        out.push(norm(&file));
                        break;
                    }
                }
            }
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_java_imports(ch, bytes, from_rel, root, out);
    }
}

// ── PHP: PSR-4 (composer.json autoload map) ──────────────────────────────────

/// PHP import edges: a `use App\Foo\Bar;` resolves to the file the composer.json PSR-4
/// autoload map places `App\Foo\Bar` at (`"App\\": "src/"` → `src/Foo/Bar.php`). CONSERVATIVE
/// per contract §3 — a `use` whose FQCN no PSR-4 prefix claims (a vendor/global class), or a
/// repo with NO composer.json, contributes NO edge (an invented edge is worse than none).
/// Grouped uses (`use App\{Foo, Bar};`) expand to one edge per member.
pub(crate) fn collect_php_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    if node.kind() == "namespace_use_declaration" {
        for fqcn in use_clause_fqcns(&node, bytes) {
            if let Some(p) = resolve_use(root, &fqcn) {
                out.push(p);
            }
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_php_imports(ch, bytes, from_rel, root, out);
    }
}

/// Every FQCN a `namespace_use_declaration` imports, leading `\` trimmed. A plain
/// `use A\B\C;` yields one clause; a grouped `use A\B\{C, D};` prefixes the group's
/// `namespace_name` onto each clause.
fn use_clause_fqcns(decl: &TsNode, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    // A grouped import carries a `namespace_name` prefix child + a `namespace_use_group` body.
    let mut prefix = String::new();
    let mut c = decl.walk();
    for ch in decl.named_children(&mut c) {
        if ch.kind() == "namespace_name" {
            prefix = ch.utf8_text(bytes).unwrap_or("").trim_matches('\\').to_string();
        }
    }
    let mut c = decl.walk();
    for ch in decl.named_children(&mut c) {
        match ch.kind() {
            "namespace_use_clause" => {
                if let Some(f) = clause_name(&ch, bytes) {
                    out.push(f.trim_matches('\\').to_string());
                }
            }
            "namespace_use_group" => {
                let mut gc = ch.walk();
                for clause in ch.named_children(&mut gc) {
                    if clause.kind() == "namespace_use_clause" {
                        if let Some(f) = clause_name(&clause, bytes) {
                            let f = f.trim_matches('\\');
                            let joined =
                                if prefix.is_empty() { f.to_string() } else { format!("{prefix}\\{f}") };
                            out.push(joined);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The FQCN text of a `namespace_use_clause` (its `qualified_name`/`name` child), before any
/// `as` alias.
fn clause_name(clause: &TsNode, bytes: &[u8]) -> Option<String> {
    let mut c = clause.walk();
    for ch in clause.named_children(&mut c) {
        if matches!(ch.kind(), "qualified_name" | "name") {
            return ch.utf8_text(bytes).ok().map(str::to_string);
        }
    }
    None
}

/// The PSR-4 autoload map from `root/composer.json`: `namespace-prefix (no trailing \\)` →
/// list of repo-relative directories. Empty when there is no composer.json or no `psr-4`
/// block — the honest empty graph (contract §3). Textual JSON via serde so a malformed
/// composer.json degrades to no map, never a panic.
pub fn psr4_map(root: &Path) -> Vec<(String, Vec<String>)> {
    let Ok(text) = std::fs::read_to_string(root.join("composer.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let mut out = Vec::new();
    for key in ["autoload", "autoload-dev"] {
        let Some(psr4) = v.get(key).and_then(|a| a.get("psr-4")).and_then(|p| p.as_object()) else {
            continue;
        };
        for (prefix, dirs) in psr4 {
            let prefix = prefix.trim_end_matches('\\').to_string();
            let dirs: Vec<String> = match dirs {
                serde_json::Value::String(s) => vec![norm_dir(s)],
                serde_json::Value::Array(a) => a.iter().filter_map(|d| d.as_str()).map(norm_dir).collect(),
                _ => continue,
            };
            out.push((prefix, dirs));
        }
    }
    // Longest prefix first: a more-specific mapping wins over a broader one.
    out.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    out
}

/// Trim a PSR-4 directory value to a clean repo-relative dir (no leading `./`, no trailing `/`).
fn norm_dir(d: &str) -> String {
    d.trim_start_matches("./").trim_end_matches('/').to_string()
}

/// Resolve a `use A\B\C` FQCN to a repo-relative `.php` file via the PSR-4 map, or `None`
/// (a vendor/global class, or no composer.json). The move model keys deletion diagnostics on
/// this — one resolver shared with the import graph.
pub fn resolve_use(root: &Path, fqcn: &str) -> Option<PathBuf> {
    let fqcn = fqcn.trim_matches('\\');
    for (prefix, dirs) in psr4_map(root) {
        // The remainder after the prefix is the sub-path under the mapped dir. An empty prefix
        // (`"": "src/"`) maps the whole namespace tree.
        let rest = if prefix.is_empty() {
            Some(fqcn)
        } else if fqcn == prefix {
            Some("")
        } else {
            fqcn.strip_prefix(&format!("{prefix}\\"))
        };
        let Some(rest) = rest else { continue };
        if rest.is_empty() {
            continue; // the prefix itself names no class file
        }
        let sub: PathBuf = rest.split('\\').collect();
        for dir in &dirs {
            let file = Path::new(dir).join(&sub).with_extension("php");
            if root.join(&file).is_file() {
                return Some(norm(&file));
            }
        }
        // This is the LONGEST matching prefix (the map is sorted longest-first), and PSR-4 is
        // EXCLUSIVE: this prefix owns the FQCN. If none of its dirs hold the file the class is
        // unresolved — do NOT fall through to a broader prefix and land on a coincidental shadow
        // file (an invented edge that inverts delete-safety, contract §3).
        return None;
    }
    None
}

/// The fully-qualified class name of `rel` (`src/Foo/Bar.php` → `App\Foo\Bar` under
/// `"App\\": "src/"`), inverting the PSR-4 map: the class is the file stem, the namespace is
/// the dir tail below the mapped directory, prefixed by the map's namespace. `None` for
/// non-`.php` paths or a file under no mapped directory. Reused by the PHP move model.
pub fn file_to_fqcn(root: &Path, rel: &str) -> Option<String> {
    if !rel.ends_with(".php") {
        return None;
    }
    let stem = Path::new(rel).file_stem()?.to_str()?;
    let parent = Path::new(rel).parent().unwrap_or(Path::new(""));
    let parent_norm = parent.to_string_lossy().replace('\\', "/");
    // The most-specific (longest dir) mapping whose directory is a prefix of this file's dir.
    let mut best: Option<(usize, String)> = None; // (dir length, FQCN)
    for (prefix, dirs) in psr4_map(root) {
        for dir in &dirs {
            let dir_norm = dir.trim_end_matches('/');
            let under = if dir_norm.is_empty() {
                Some(parent_norm.as_str())
            } else if parent_norm == dir_norm {
                Some("")
            } else {
                parent_norm.strip_prefix(&format!("{dir_norm}/"))
            };
            let Some(tail) = under else { continue };
            let mut segs: Vec<String> = Vec::new();
            if !prefix.is_empty() {
                segs.push(prefix.clone());
            }
            segs.extend(tail.split('/').filter(|s| !s.is_empty()).map(str::to_string));
            segs.push(stem.to_string());
            let fqcn = segs.join("\\");
            if best.as_ref().is_none_or(|(len, _)| dir_norm.len() > *len) {
                best = Some((dir_norm.len(), fqcn));
            }
        }
    }
    best.map(|(_, f)| f)
}

/// The first `namespace A\B;` declaration as (line index, declared name) — the SINGLE PHP
/// namespace scanner, shared by the import resolver and the move model (§7: one scanner, no
/// divergent reimplementation). Handles the inline `<?php namespace App;` opener (a leading
/// `<?php` on the same line). `None` = the global namespace (no named declaration).
pub fn namespace_decl(content: &str) -> Option<(usize, String)> {
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        let t = t.strip_prefix("<?php").map(str::trim_start).unwrap_or(t);
        if let Some(rest) = t.strip_prefix("namespace ") {
            let name = rest.trim().trim_end_matches('{').trim().trim_end_matches(';').trim();
            if name.is_empty() {
                return None;
            }
            return Some((i, name.trim_matches('\\').to_string()));
        }
    }
    None
}

/// The source-root prefixes an `import a.b.C` is resolved against, most-specific first: the
/// root derived from THIS file's own `package` declaration (`src/main/java/com/x/A.java` with
/// `package com.x;` → `src/main/java`), then the conventional Maven/Gradle layout dirs, then
/// the repo root (flat layout). Deduped, order preserved.
pub fn java_source_roots(root: &Path, from_rel: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut push = |p: PathBuf| {
        let key = p.to_string_lossy().replace('\\', "/");
        if seen.insert(key) {
            out.push(p);
        }
    };
    if let Some(own) = package_source_root(root, from_rel) {
        push(own);
    }
    for conv in ["src/main/java", "src/test/java", "src"] {
        let p = PathBuf::from(conv);
        if root.join(&p).is_dir() {
            push(p);
        }
    }
    push(PathBuf::new()); // flat layout: imports resolve from the repo root
    out
}

/// The source root implied by `from_rel`'s own `package` declaration: for
/// `src/main/java/com/x/A.java` declaring `package com.x;`, strip the `com/x` suffix from the
/// file's parent dir to get `src/main/java`. `None` when the file has no package (default
/// package) or the directory tail doesn't match the package path — the conventional roots then
/// carry resolution.
fn package_source_root(root: &Path, from_rel: &str) -> Option<PathBuf> {
    let content = std::fs::read_to_string(root.join(from_rel)).ok()?;
    let pkg = package_of(&content)?;
    let pkg_parts: Vec<&str> = pkg.split('.').filter(|s| !s.is_empty()).collect();
    let parent = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let dir_parts: Vec<String> =
        parent.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if dir_parts.len() < pkg_parts.len() {
        return None;
    }
    let split = dir_parts.len() - pkg_parts.len();
    if dir_parts[split..] != pkg_parts[..] {
        return None; // the directory tail doesn't mirror the package: not a package-path file
    }
    Some(dir_parts[..split].iter().collect())
}

/// The fully-qualified name of the class in `rel` (`src/main/java/com/x/A.java` → `com.x.A`),
/// inverting the source-root resolution: the class is the file stem, the package is the dir
/// tail below whichever source root owns the file. `None` for non-`.java` paths. Reused by the
/// Java move model (path → reference) so move rewrites and the import graph speak one resolver.
pub fn file_to_fqn(root: &Path, rel: &str) -> Option<String> {
    let stem = Path::new(rel).file_stem()?.to_str()?;
    if !rel.ends_with(".java") {
        return None;
    }
    let parent = Path::new(rel).parent().unwrap_or(Path::new(""));
    let dir_parts: Vec<String> =
        parent.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    // The package prefix = the directory tail below the owning source root. Prefer the root
    // this file's own package declaration implies; else the longest conventional root that is a
    // prefix of the file's directory.
    let src_root = package_source_root(root, rel).or_else(|| {
        java_source_roots(root, rel).into_iter().find(|r| {
            let rp: Vec<String> =
                r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            dir_parts.len() >= rp.len() && dir_parts[..rp.len()] == rp[..]
        })
    })?;
    let rp: Vec<String> =
        src_root.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if dir_parts.len() < rp.len() || dir_parts[..rp.len()] != rp[..] {
        return None;
    }
    let mut fqn_parts = dir_parts[rp.len()..].to_vec();
    fqn_parts.push(stem.to_string());
    Some(fqn_parts.join("."))
}

/// True when the `.java` at `rel` declares exactly `expected` (empty = default package) — the
/// guard against a path-coincidence edge (a file sitting at the import's path but belonging to a
/// different package). An unreadable file fails CLOSED (no edge): an honest miss over an invented
/// edge (contract §3).
fn java_declares_package(root: &Path, rel: &Path, expected: &str) -> bool {
    match std::fs::read_to_string(root.join(rel)) {
        Ok(content) => package_of(&content).unwrap_or_default() == expected,
        Err(_) => false,
    }
}

/// Resolve an `import a.b.C` (dotted FQN, no `.*`) to a repo-relative `.java` file under
/// `from_rel`'s source roots, or `None` (external / unresolvable). The move model's occurrence
/// scanner keys deletion diagnostics on this — same resolver as the import graph. The resolved
/// file must DECLARE the import's package, or it's a path coincidence and no edge is emitted.
pub fn resolve_import(root: &Path, from_rel: &str, fqn: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = fqn.split('.').filter(|s| !s.is_empty() && *s != "*").collect();
    if parts.is_empty() {
        return None;
    }
    let expected_pkg = parts[..parts.len() - 1].join(".");
    for src_root in java_source_roots(root, from_rel) {
        let mut file = src_root.clone();
        for p in &parts {
            file.push(p);
        }
        let file = file.with_extension("java");
        if root.join(&file).is_file() && java_declares_package(root, &file, &expected_pkg) {
            return Some(norm(&file));
        }
    }
    None
}

/// The first `package a.b.c;` declaration as (line index, declared name) — the SINGLE Java
/// package scanner, shared by the import resolver and the move model (§7). `None` = the default
/// package.
pub fn package_decl(content: &str) -> Option<(usize, String)> {
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("package ") {
            let name = rest.trim().strip_suffix(';')?.trim().to_string();
            return Some((i, name));
        }
        if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        // A non-comment, non-package first statement means no package declaration follows.
        if t.starts_with("import ") || t.starts_with("public ") || t.starts_with("class ") {
            return None;
        }
    }
    None
}

/// The `package a.b.c;` name declared in a Java source, or `None` for the default package.
pub fn package_of(content: &str) -> Option<String> {
    package_decl(content).map(|(_, n)| n)
}

pub(crate) fn resolve_js_specifier(root: &Path, from_rel: &str, spec: &str) -> Option<PathBuf> {
    // Lexically normalize `./` and `../` so graph keys stay clean repo-relative paths.
    let joined = Path::new(from_rel).parent().unwrap_or(Path::new("")).join(spec);
    let mut base = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                base.pop();
            }
            std::path::Component::CurDir => {}
            other => base.push(other),
        }
    }
    // `./x.js` in TS source means `./x.ts` on disk — strip a source extension before probing.
    let stripped = ["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"]
        .iter()
        .find(|e| base.extension().and_then(|x| x.to_str()) == Some(**e))
        .map(|_| base.with_extension(""))
        .unwrap_or_else(|| base.clone());
    const EXTS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
    let mut candidates = vec![base.clone()];
    for e in EXTS {
        candidates.push(stripped.with_extension(e));
    }
    for e in EXTS {
        candidates.push(stripped.join(format!("index.{e}")));
    }
    candidates.into_iter().find(|c| root.join(c).is_file()).map(|c| norm(&c))
}

/// `import a.b.c` → try `a/b/c.py`, `a/b/c/__init__.py` from the repo root and `src/`.
fn push_absolute(root: &Path, parts: &[String], out: &mut Vec<PathBuf>) {
    for base in [PathBuf::new(), PathBuf::from("src")] {
        if let Some(p) = resolve(root, &base, parts) {
            out.push(p);
            return;
        }
    }
}

/// Split a `dotted_name` into its identifier parts.
fn dotted_parts(node: &TsNode, bytes: &[u8]) -> Vec<String> {
    node.utf8_text(bytes).unwrap_or("").split('.').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// `(level, parts)` for a `module_name`: a `dotted_name` is absolute (level 0); a
/// `relative_import` carries leading dots (level = dot count) and an optional dotted tail.
fn module_spec(node: &TsNode, bytes: &[u8]) -> (usize, Vec<String>) {
    match node.kind() {
        "dotted_name" => (0, dotted_parts(node, bytes)),
        "relative_import" => {
            let mut level = 0;
            let mut parts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                match ch.kind() {
                    "import_prefix" => level = ch.utf8_text(bytes).unwrap_or("").matches('.').count(),
                    "dotted_name" => parts = dotted_parts(&ch, bytes),
                    _ => {}
                }
            }
            (level.max(1), parts)
        }
        _ => (0, vec![]),
    }
}

/// The imported names of a `from … import a, b as c` (skips the module_name node + wildcard).
fn imported_names(stmt: &TsNode, module: &TsNode, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut c = stmt.walk();
    for ch in stmt.named_children(&mut c) {
        if ch.id() == module.id() {
            continue;
        }
        match ch.kind() {
            "dotted_name" => {
                if let Some(first) = dotted_parts(&ch, bytes).into_iter().next() {
                    names.push(first);
                }
            }
            "aliased_import" => {
                if let Some(n) = ch.child_by_field_name("name") {
                    if let Some(first) = dotted_parts(&n, bytes).into_iter().next() {
                        names.push(first);
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// The package directory a relative import is anchored at. Level 0 (absolute) → repo root;
/// level 1 → the file's own directory; each extra dot ascends one more.
fn base_dir(from_rel: &str, level: usize) -> Option<PathBuf> {
    if level == 0 {
        return Some(PathBuf::new());
    }
    let mut dir = Path::new(from_rel).parent()?.to_path_buf();
    for _ in 1..level {
        dir = dir.parent()?.to_path_buf();
    }
    Some(dir)
}

/// Resolve `base/parts…` to a repo-relative `.py` file or package `__init__.py`, if it exists.
fn resolve(root: &Path, base: &Path, parts: &[String]) -> Option<PathBuf> {
    if parts.is_empty() {
        return None;
    }
    let mut p = base.to_path_buf();
    for part in parts {
        p.push(part);
    }
    let as_file = p.with_extension("py");
    if root.join(&as_file).is_file() {
        return Some(norm(&as_file));
    }
    let as_init = p.join("__init__.py");
    if root.join(&as_init).is_file() {
        return Some(norm(&as_init));
    }
    None
}

fn norm(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().replace('\\', "/"))
}

/// Repo-relative source files with the given extension, gitignore-aware.
pub(crate) fn source_files(root: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out
}

pub(crate) fn has_ext(root: &Path, ext: &str) -> bool {
    !source_files(root, ext).is_empty()
}
