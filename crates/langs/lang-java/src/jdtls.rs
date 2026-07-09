//! jdtls resolution + launch — the rename/willRename half of the Java engine.
//!
//! Two deployment facts drive this module (both spec-verified):
//! - jdtls requires a **Java 21+ runtime** even though the gate's JDK floor is 17, so the
//!   launcher selects a 21+ home EXPLICITLY (`JAVA_HOME` is often unset while a new-enough
//!   JDK sits under /Library/Java/JavaVirtualMachines) instead of trusting the PATH `java`.
//! - jdtls is push-diagnostics-only through v1.60 — it must never serve the gate verdict
//!   (that's the javax.tools sidecar's job); this client exists for rename/willRename alone.
use ci_core::Result;
use ci_lsp::LspClient;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const INSTALL_HINT: &str =
    "`brew install jdtls` (jdtls itself runs on Java 21+ — e.g. `brew install openjdk@21`)";

/// The jdtls launcher: `$CI_JDTLS`, else PATH, else the Homebrew prefixes.
pub(crate) fn jdtls_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CI_JDTLS") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("jdtls");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    ["/opt/homebrew/bin/jdtls", "/usr/local/bin/jdtls"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// A Java 21+ home for jdtls's own runtime: `$JAVA_HOME` when new enough, else the newest
/// 21+ JDK under the macOS JavaVirtualMachines directory. `None` leaves the launcher's own
/// lookup (PATH `java`) — jdtls reports its floor loudly if that one is too old.
pub(crate) fn java21_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("JAVA_HOME") {
        let home = PathBuf::from(h);
        if java_major(&home).is_some_and(|v| v >= 21) {
            return Some(home);
        }
    }
    let mut best: Option<(u32, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir("/Library/Java/JavaVirtualMachines") {
        for entry in rd.flatten() {
            let home = entry.path().join("Contents/Home");
            if let Some(v) = java_major(&home) {
                if v >= 21 && best.as_ref().is_none_or(|(b, _)| v > *b) {
                    best = Some((v, home));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Major version of the JDK at `home`, read from its `release` file
/// (`JAVA_VERSION="24.0.1"`) — no process spawn, so probing every installed VM stays cheap.
fn java_major(home: &Path) -> Option<u32> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let ver = release.lines().find(|l| l.starts_with("JAVA_VERSION="))?.split('"').nth(1)?;
    let first: u32 = ver.split(['.', '-', '+']).next()?.parse().ok()?;
    match first {
        // Legacy `1.8.0_x` scheme: the major is the second component.
        1 => ver.split('.').nth(1)?.parse().ok(),
        v => Some(v),
    }
}

/// Start jdtls for `root`. The eclipse workspace persists per repo
/// (`.marksman/jdtls-workspace`): first import of a real Maven/Gradle repo can take minutes,
/// warm restarts don't — persisting it is load-bearing, not an optimization.
pub(crate) fn start(root: &Path, sandbox: &dyn ci_core::Sandbox) -> Result<LspClient> {
    let Some(bin) = jdtls_binary() else {
        return Err(ci_core::Error::Driver(format!(
            "java rename/move needs jdtls to rewrite references safely — Install: {INSTALL_HINT}. \
             Without it, reissue a SYMBOL rename as `replace_text` edits over the definition and \
             each reference in one batch — the javac gate type-checks the result, so a missed or \
             wrong site rejects rather than lands."
        )));
    };
    let mut cmd = Command::new(bin);
    cmd.arg("-data").arg(root.join(".marksman").join("jdtls-workspace"));
    if let Some(home) = java21_home() {
        // The brew launcher resolves its runtime through JAVA_HOME first — pin the 21+ one.
        cmd.env("JAVA_HOME", &home);
    }
    LspClient::start_in(root, cmd, sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The release-file version parse covers the modern (`24.0.1`) and legacy (`1.8.0_392`)
    // schemes — the discriminator deciding which installed JDK may run jdtls.
    #[test]
    fn java_major_reads_release_file_schemes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for (raw, want) in [
            ("JAVA_VERSION=\"24.0.1\"\nOS_ARCH=\"aarch64\"\n", Some(24)),
            ("IMPLEMENTOR=\"Azul\"\nJAVA_VERSION=\"17.0.10\"\n", Some(17)),
            ("JAVA_VERSION=\"21\"\n", Some(21)),
            ("JAVA_VERSION=\"1.8.0_392\"\n", Some(8)),
            ("NOTHING_USEFUL=1\n", None),
        ] {
            std::fs::write(home.join("release"), raw).unwrap();
            assert_eq!(java_major(home), want, "release: {raw:?}");
        }
        std::fs::remove_file(home.join("release")).unwrap();
        assert_eq!(java_major(home), None, "no release file = unknown, never a guess");
    }
}
