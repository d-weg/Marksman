//! Package-aware relevance weighting (ARCHITECTURE.md §6.1) — faithful port of
//! src/package-weight.ts. Pure and language-blind: role inference from
//! deps/tsconfig/name + a query-conditioned layer boost, composed as a per-package
//! multiplier on the fused score. Used at index time (compute roles) and query time.
use crate::config::Config;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageRole {
    Backend,
    Frontend,
    Mobile,
    Shared,
    Docs,
    Unknown,
}

impl PackageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PackageRole::Backend => "backend",
            PackageRole::Frontend => "frontend",
            PackageRole::Mobile => "mobile",
            PackageRole::Shared => "shared",
            PackageRole::Docs => "docs",
            PackageRole::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<PackageRole> {
        Some(match s {
            "backend" => PackageRole::Backend,
            "frontend" => PackageRole::Frontend,
            "mobile" => PackageRole::Mobile,
            "shared" => PackageRole::Shared,
            "docs" => PackageRole::Docs,
            "unknown" => PackageRole::Unknown,
            _ => return None,
        })
    }
}

const KNOWN_ROLES: [PackageRole; 5] = [
    PackageRole::Backend,
    PackageRole::Frontend,
    PackageRole::Mobile,
    PackageRole::Shared,
    PackageRole::Docs,
];

/// Everything we can learn about a package to classify its role (gathered at index time).
#[derive(Debug, Clone, Default)]
pub struct RoleSignals {
    pub name: String,
    pub dir: String,
    pub deps: Vec<String>,
    pub ts_lib: Vec<String>,
    pub ts_types: Vec<String>,
}

/// Dependency fingerprints in priority order (mobile first — RN apps also pull react).
const DEP_FINGERPRINTS: &[(PackageRole, &[&str])] = &[
    (
        PackageRole::Mobile,
        &["react-native", "expo", "@expo/", "@react-navigation/", "@react-native", "nativewind"],
    ),
    (
        PackageRole::Frontend,
        &[
            "next", "nuxt", "vite", "@vitejs/", "vue", "svelte", "@sveltejs/", "@angular/",
            "react-dom", "react-router-dom", "gatsby", "@remix-run/", "astro",
        ],
    ),
    (
        PackageRole::Backend,
        &[
            "express", "fastify", "elysia", "hono", "koa", "@nestjs/", "@hapi/", "drizzle-orm",
            "drizzle-kit", "prisma", "@prisma/", "mongoose", "typeorm", "sequelize", "knex", "pg",
            "postgres", "mysql", "mysql2", "@aws-sdk/", "firebase-admin", "ioredis", "bullmq",
            "kafkajs", "@trpc/server", "apollo-server", "graphql-yoga",
        ],
    ),
];

fn dep_matches(deps: &[String], prefixes: &[&str]) -> bool {
    deps.iter().any(|d| {
        prefixes
            .iter()
            .any(|p| d == p || d.starts_with(p) || d.starts_with(&format!("{p}-")))
    })
}

fn infer_role_from_name_dir(name: &str, dir: &str) -> PackageRole {
    let hay = format!("{name} {dir}").to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| hay.contains(n));
    if has(&["mobile", "expo", "react-native", "native", "/app"]) {
        PackageRole::Mobile
    } else if has(&["backend", "server", "/api", "worker", "lambda", "functions"]) {
        PackageRole::Backend
    } else if has(&["frontend", "web", "client", "www", "dashboard", "admin", "site"]) {
        PackageRole::Frontend
    } else if has(&["docs", "documentation"]) {
        PackageRole::Docs
    } else if has(&["shared", "common", "core", "types", "schema", "lib", "util", "packages/"]) {
        PackageRole::Shared
    } else {
        PackageRole::Unknown
    }
}

/// Infer a package's role: deps fingerprints → tsconfig hints → name/dir fallback.
pub fn infer_role(s: &RoleSignals) -> PackageRole {
    if !s.deps.is_empty() {
        for (role, deps) in DEP_FINGERPRINTS {
            if dep_matches(&s.deps, deps) {
                return *role;
            }
        }
    }
    if s.ts_types.iter().any(|t| t == "bun-types" || t == "node") {
        return PackageRole::Backend;
    }
    if s.ts_lib.iter().any(|l| l.starts_with("dom")) {
        return PackageRole::Frontend;
    }
    infer_role_from_name_dir(&s.name, &s.dir)
}

/// Map a single (lowercased) path segment to a role, or None if it carries no layer signal.
/// Conservative on purpose — only directory names that almost always denote a layer. Ambiguous
/// ones (`app`, `ui`, `components`, `src`) are deliberately omitted to avoid false boosts.
fn segment_role(seg: &str) -> Option<PackageRole> {
    Some(match seg {
        "server" | "backend" | "api" | "apis" | "routes" | "route" | "controllers"
        | "controller" | "services" | "service" | "db" | "database" | "migrations"
        | "migration" | "prisma" | "drizzle" | "worker" | "workers" | "jobs" | "queue"
        | "lambda" | "functions" | "handlers" | "middleware"
        // cross-language backend conventions (Django/DRF, DDD/hexagonal, Rust):
        | "serializers" | "repositories" | "repository" | "usecases" | "dao" | "entities" => {
            PackageRole::Backend
        }
        "web" | "www" | "frontend" | "client" | "pages" | "dashboard" | "admin" | "site" => {
            PackageRole::Frontend
        }
        "mobile" | "native" | "screens" | "navigation" => PackageRole::Mobile,
        "docs" | "documentation" => PackageRole::Docs,
        "shared" | "common" | "core" | "types" | "util" | "utils" | "helpers" | "lib"
        | "libs" | "dto" | "constants" => PackageRole::Shared,
        _ => return None,
    })
}

/// Infer a file's role from its **path segments**, so layer weighting works BELOW the package
/// boundary — a `backend/` (or `server/`, `db/`, …) directory anywhere, including inside a
/// single-package repo with no per-dir manifest, is treated like a declared backend package.
/// The **deepest** (closest-to-file) decisive directory wins (handles `apps/web/server/x.ts`);
/// `Unknown` when no directory segment is decisive, so the caller can fall back to the package.
pub fn infer_role_from_path(rel_path: &str) -> PackageRole {
    let parts: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return PackageRole::Unknown; // no directory component, just a filename
    }
    for seg in parts[..parts.len() - 1].iter().rev() {
        if let Some(r) = segment_role(&seg.to_lowercase()) {
            return r;
        }
    }
    PackageRole::Unknown
}

fn default_layer_terms(role: PackageRole) -> &'static [&'static str] {
    match role {
        PackageRole::Backend => &[
            "route", "router", "controller", "service", "endpoint", "api", "handler",
            "transaction", "atomic", "schema", "migration", "drizzle", "sql", "query", "db",
            "database", "table", "column", "index", "constraint", "unique", "presign", "bucket",
            "storage", "upload", "cdn", "webhook", "cron", "queue", "worker", "middleware",
            "auth", "token", "credit", "billing", "payment", "deduct", "balance", "ledger",
            "invoice", "server",
            // cross-language backend frameworks / ORMs (fire when a task names them):
            "axum", "actix", "tokio", "sqlx", "diesel", "tonic", "grpc", "django", "flask",
            "fastapi", "sqlalchemy", "alembic", "celery", "pydantic", "orm", "repository",
            "usecase", "serializer", "gorm",
        ],
        PackageRole::Frontend => &[
            "page", "css", "dom", "browser", "spa", "ssr", "hydrate", "vite", "webpack",
            "router", "route", "fetch", "form",
        ],
        PackageRole::Mobile => &[
            "screen", "layout", "component", "tap", "press", "gesture", "scroll", "navigation",
            "navigator", "navigate", "view", "render", "style", "stylesheet", "expo", "native",
            "tab", "modal", "sheet", "drawer", "safearea", "keyboard", "animation", "animated",
            "flatlist", "touchable", "pressable",
        ],
        PackageRole::Shared => {
            &["type", "interface", "enum", "constant", "shared", "util", "helper", "dto"]
        }
        PackageRole::Docs => &["readme", "documentation", "guide", "changelog"],
        PackageRole::Unknown => &[],
    }
}

/// A query token "hits" a term when equal, or extends it as a prefix (term ≥ 4 chars).
fn term_hits(qset: &HashSet<&str>, term: &str) -> bool {
    qset.iter().any(|qt| *qt == term || (term.len() >= 4 && qt.starts_with(term)))
}

/// Per-role count of how many of that role's layer terms the query fires, plus the max across
/// roles (the normalizer). Empty/zero when layer weighting is disabled. Shared so the two public
/// entry points (`layer_multipliers`, `compute_package_weights`) score layers identically.
fn score_layers(qset: &HashSet<&str>, enabled: bool) -> (HashMap<&'static str, usize>, usize) {
    let mut scores: HashMap<&'static str, usize> = HashMap::new();
    let mut max_layer = 0usize;
    if enabled {
        for role in KNOWN_ROLES {
            let hits = default_layer_terms(role).iter().filter(|t| term_hits(qset, t)).count();
            scores.insert(role.as_str(), hits);
            max_layer = max_layer.max(hits);
        }
    }
    (scores, max_layer)
}

/// The layer multiplier for a role that fired `ls` of its terms: `1 + boost·(ls/max_layer)`, or
/// `1.0` (no boost) when weighting is off, no layer fired, or this role didn't fire.
fn layer_mult(ls: usize, max_layer: usize, boost: f64, enabled: bool) -> f64 {
    if enabled && max_layer > 0 && ls > 0 {
        1.0 + boost * (ls as f64 / max_layer as f64)
    } else {
        1.0
    }
}

/// A package as seen by the weighter (role precomputed at index time when available).
#[derive(Debug, Clone, Default)]
pub struct WeightedPackage {
    pub name: String,
    pub dir: String,
    pub role: Option<String>,
    pub deps: Vec<String>,
    pub ts_lib: Vec<String>,
    pub ts_types: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WeightDebug {
    pub roles: HashMap<String, PackageRole>,
    pub layer_scores: HashMap<&'static str, usize>,
    pub fired_layers: Vec<PackageRole>,
}

pub fn resolve_role(p: &WeightedPackage) -> PackageRole {
    if let Some(r) = p.role.as_deref().and_then(PackageRole::parse) {
        if r != PackageRole::Unknown {
            return r;
        }
    }
    infer_role(&RoleSignals {
        name: p.name.clone(),
        dir: p.dir.clone(),
        deps: p.deps.clone(),
        ts_lib: p.ts_lib.clone(),
        ts_types: p.ts_types.clone(),
    })
}

/// The query-conditioned layer multiplier **per role** (decoupled from any package), so the
/// caller can apply it against a file's *path*-derived role rather than only its package's role.
/// Each value is `1.0` (no boost) or `1 + boost·(roleHits/maxHits)` when the query fires that
/// layer. `"unknown"` is always `1.0`.
pub fn layer_multipliers(query_tokens: &[String], config: &Config) -> HashMap<&'static str, f64> {
    let qset: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
    let enabled = config.query_layer_weighting.enabled;
    let boost = config.query_layer_weighting.boost as f64;

    let (layer_scores, max_layer) = score_layers(&qset, enabled);

    let mut mult: HashMap<&'static str, f64> = HashMap::new();
    for role in KNOWN_ROLES {
        let ls = *layer_scores.get(role.as_str()).unwrap_or(&0);
        mult.insert(role.as_str(), layer_mult(ls, max_layer, boost, enabled));
    }
    mult.insert("unknown", 1.0);
    mult
}

/// Compute the per-package fused-score multiplier (static × query-conditioned).
/// Returns (weight-by-package-name, debug). Pure and deterministic.
pub fn compute_package_weights(
    packages: &[WeightedPackage],
    query_tokens: &[String],
    config: &Config,
) -> (HashMap<String, f64>, WeightDebug) {
    let qset: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();

    let mut roles: HashMap<String, PackageRole> = HashMap::new();
    for p in packages {
        roles.insert(p.name.clone(), resolve_role(p));
    }

    let enabled = config.query_layer_weighting.enabled;
    let boost = config.query_layer_weighting.boost as f64;

    let (layer_scores, max_layer) = score_layers(&qset, enabled);
    let fired_layers: Vec<PackageRole> = KNOWN_ROLES
        .into_iter()
        .filter(|r| *layer_scores.get(r.as_str()).unwrap_or(&0) > 0)
        .collect();

    let mut weight: HashMap<String, f64> = HashMap::new();
    for p in packages {
        let role = roles[&p.name];
        let static_w = config
            .package_weights
            .get(&p.name)
            .or_else(|| config.package_weights.get(role.as_str()))
            .map(|w| *w as f64)
            .unwrap_or(1.0);
        let ls = *layer_scores.get(role.as_str()).unwrap_or(&0);
        weight.insert(p.name.clone(), static_w * layer_mult(ls, max_layer, boost, enabled));
    }

    (weight, WeightDebug { roles, layer_scores, fired_layers })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, dir: &str, role: &str) -> WeightedPackage {
        WeightedPackage { name: name.into(), dir: dir.into(), role: Some(role.into()), ..Default::default() }
    }

    #[test]
    fn backend_query_boosts_backend_package() {
        let packages = vec![pkg("backend", "apps/backend", "backend"), pkg("mobile", "apps/mobile", "mobile")];
        let q: Vec<String> = "add a database migration and a schema constraint"
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let (w, dbg) = compute_package_weights(&packages, &q, &Config::default());
        assert!(w["backend"] > w["mobile"], "backend should outweigh mobile: {w:?}");
        assert!(dbg.fired_layers.contains(&PackageRole::Backend));
    }

    #[test]
    fn infers_role_from_path_segments() {
        // a `backend/` dir signals backend even with no manifest there.
        assert_eq!(infer_role_from_path("apps/backend/test.js"), PackageRole::Backend);
        assert_eq!(infer_role_from_path("src/db/migrate.ts"), PackageRole::Backend);
        assert_eq!(infer_role_from_path("packages/mobile/screens/Home.tsx"), PackageRole::Mobile);
        // deepest decisive segment wins: web (frontend) is shallower than server (backend).
        assert_eq!(infer_role_from_path("apps/web/server/handler.ts"), PackageRole::Backend);
        // no decisive segment -> Unknown (caller falls back to the package role).
        assert_eq!(infer_role_from_path("src/rrf.ts"), PackageRole::Unknown);
        // a bare filename has no directory component.
        assert_eq!(infer_role_from_path("index.ts"), PackageRole::Unknown);
    }

    #[test]
    fn layer_multiplier_boosts_fired_role_only() {
        let q: Vec<String> =
            "add a database migration".split_whitespace().map(str::to_string).collect();
        let m = layer_multipliers(&q, &Config::default());
        assert!(m["backend"] > 1.0, "backend should be boosted: {m:?}");
        assert_eq!(m["mobile"], 1.0, "mobile untouched by a backend query");
    }

    #[test]
    fn path_roles_cover_python_and_rust_conventions() {
        // Django/DRF + DDD directories now classify as backend, not Unknown.
        assert_eq!(infer_role_from_path("app/serializers/user.py"), PackageRole::Backend);
        assert_eq!(infer_role_from_path("src/repositories/order_repo.rs"), PackageRole::Backend);
        assert_eq!(infer_role_from_path("service/usecases/checkout.go"), PackageRole::Backend);
    }

    #[test]
    fn layer_query_boosts_backend_across_languages() {
        // A Rust (axum/sqlx) and a Python (django/sqlalchemy) query both fire the backend layer.
        for q in ["add an axum route backed by sqlx", "a django model with a sqlalchemy query"] {
            let toks: Vec<String> = q.split_whitespace().map(str::to_string).collect();
            let m = layer_multipliers(&toks, &Config::default());
            assert!(m["backend"] > 1.0, "backend should be boosted for {q:?}: {m:?}");
        }
    }

    #[test]
    fn infers_role_from_deps() {
        let s = RoleSignals { name: "x".into(), dir: "x".into(), deps: vec!["expo-router".into()], ..Default::default() };
        assert_eq!(infer_role(&s), PackageRole::Mobile);
    }
}
