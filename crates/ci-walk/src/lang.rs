use std::path::Path;

/// Coarse language tag used for provider dispatch. Marksman is code-only; anything that isn't a
/// source language a provider handles is `Other` (ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    Ts,
    Tsx,
    Rust,
    Python,
    Other,
}

impl Lang {
    pub fn of(path: &Path) -> Lang {
        let s = path.to_string_lossy();
        if s.ends_with(".d.ts") {
            return Lang::Other; // declaration files are not source
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some("ts") | Some("mts") | Some("cts") => Lang::Ts,
            Some("tsx") => Lang::Tsx,
            Some("rs") => Lang::Rust,
            Some("py") | Some("pyi") => Lang::Python,
            _ => Lang::Other,
        }
    }

    /// True for languages a provider handles (indexed as code, editable).
    pub fn is_code(self) -> bool {
        matches!(self, Lang::Ts | Lang::Tsx | Lang::Rust | Lang::Python)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_extensions() {
        assert_eq!(Lang::of(Path::new("src/a.ts")), Lang::Ts);
        assert_eq!(Lang::of(Path::new("src/a.tsx")), Lang::Tsx);
        assert_eq!(Lang::of(Path::new("src/a.d.ts")), Lang::Other);
        assert_eq!(Lang::of(Path::new("README.md")), Lang::Other);
        assert_eq!(Lang::of(Path::new("Cargo.toml")), Lang::Other);
        assert!(Lang::of(Path::new("x.ts")).is_code());
        assert!(!Lang::of(Path::new("x.md")).is_code());
    }
}
