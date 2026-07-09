//! The Java gate: `JavaEngine` = the resident javax.tools sidecar for the VERDICT plus jdtls
//! (started lazily) for rename/willRename. The two never trade jobs: jdtls is push-diagnostics
//! only through v1.60 — waiting on its publish silence could mistake a slow server for a clean
//! file — while javax.tools IS javac, answering request/response with structured diagnostics.
use ci_core::{Diag, Error, Result};
use ci_edit::GateEngine;
use ci_lsp::LspClient;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::jdtls;

/// The single-file javax.tools wrapper (JEP 330: any JDK 17+ runs it straight from source).
const GATE_SIDECAR_SRC: &str = include_str!("GateSidecar.java");

/// The resident compiler process: one `java GateSidecar.java`, JSON-line request/response.
/// Classpath/sourcepath are derived once at start (the process is per-engine, engines are
/// per-provider) — see [`derive_paths`] for the Q3 policy.
pub(crate) struct JavacSidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    root: PathBuf,
    classpath: String,
    sourcepath: String,
    /// Holds the materialized GateSidecar.java for the child's lifetime.
    _src_dir: tempfile::TempDir,
}

impl Drop for JavacSidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl JavacSidecar {
    pub(crate) fn start(root: &Path) -> Result<Self> {
        let src_dir = tempfile::tempdir()
            .map_err(|e| Error::Driver(format!("materialize java gate sidecar: {e}")))?;
        let src = src_dir.path().join("GateSidecar.java");
        std::fs::write(&src, GATE_SIDECAR_SRC)
            .map_err(|e| Error::Driver(format!("materialize java gate sidecar: {e}")))?;
        let mut child = Command::new("java")
            .arg(&src)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Driver(format!("spawn java gate sidecar: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Driver("no sidecar stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Driver("no sidecar stdout".into()))?;
        let (classpath, sourcepath) = derive_paths(root);
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            root: root.to_path_buf(),
            classpath,
            sourcepath,
            _src_dir: src_dir,
        })
    }

    /// One request/response round-trip: overlay buffers in, ERROR diagnostics out. Buffer
    /// conventions from the shared spine hold — an empty buffer is a deletion stand-in the
    /// sidecar keeps as an (empty, valid) unit, so consumers of the deleted class fail.
    pub(crate) fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let req = json!({
            "files": files.iter().map(|(p, c)| json!({"path": p, "content": c})).collect::<Vec<_>>(),
            "classpath": self.classpath,
            "sourcepath": self.sourcepath,
        });
        writeln!(self.stdin, "{req}")
            .map_err(|e| Error::Driver(format!("java gate sidecar write: {e}")))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|e| Error::Driver(format!("java gate sidecar read: {e}")))?;
        if line.trim().is_empty() {
            return Err(Error::Driver("java gate sidecar exited (EOF) — restart the edit to respawn it".into()));
        }
        let v: Value = serde_json::from_str(&line)
            .map_err(|e| Error::Driver(format!("java gate sidecar reply unparsable: {e}")))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(Error::Driver(format!("java gate: {err}")));
        }
        // Only ERRORs gate (warnings/notes never reject); implicit sourcepath units report with
        // absolute paths — relativize so the baseline diff keys and the reject sites line up
        // with the repo-relative paths the spine speaks.
        let root_prefix = format!("{}/", self.root.to_string_lossy().trim_end_matches('/'));
        let diags = v["diagnostics"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d["kind"] == "ERROR")
            .map(|d| {
                let mut file = d["source"].as_str().unwrap_or("").replace('\\', "/");
                if let Some(rel) = file.strip_prefix(&root_prefix) {
                    file = rel.to_string();
                }
                Diag {
                    file,
                    // javac codes are string keys ("compiler.err.…"), not numbers; the human
                    // message already carries the substance, so code stays 0 (no "TS…" noise).
                    code: 0,
                    message: d["message"].as_str().unwrap_or("").to_string(),
                    line: d["line"].as_u64().unwrap_or(0) as u32,
                }
            })
            .collect();
        Ok(diags)
    }
}

/// Java source roots for `-sourcepath`: the Maven/Gradle convention dirs when they exist,
/// else the repo root (flat layouts). Absolute, so implicit-unit diagnostics relativize
/// cleanly against the root.
fn source_roots(root: &Path) -> Vec<PathBuf> {
    let conventional = ["src/main/java", "src/test/java"];
    let mut roots: Vec<PathBuf> =
        conventional.iter().map(|r| root.join(r)).filter(|p| p.is_dir()).collect();
    if roots.is_empty() {
        roots.push(root.to_path_buf());
    }
    roots
}

fn path_sep() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

/// Classpath + sourcepath for the sidecar — decision Q3: the DEPENDENCY classpath derives
/// through the build tool when both the build file and the tool are present (`mvn
/// dependency:build-classpath` / a Gradle init-script task); otherwise the flat source-root
/// classpath. A failed derivation degrades HONESTLY: it warns, and dependency-typed code then
/// carries unresolved-symbol errors in baseline and after alike — excused by the diff, never
/// a false reject; the gate still catches everything the overlay itself breaks.
pub(crate) fn derive_paths(root: &Path) -> (String, String) {
    let sourcepath = source_roots(root)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(path_sep());
    if root.join("pom.xml").is_file() && tool_present("mvn") {
        match maven_classpath(root) {
            Some(cp) => return (cp, sourcepath),
            None => eprintln!(
                "[lang-java] mvn dependency:build-classpath failed — gating with the flat \
                 source-root classpath (dependency types resolve as baseline errors, not rejects)"
            ),
        }
    }
    if (root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file())
        && tool_present("gradle")
    {
        match gradle_classpath(root) {
            Some(cp) => return (cp, sourcepath),
            None => eprintln!(
                "[lang-java] gradle classpath derivation failed — gating with the flat \
                 source-root classpath (dependency types resolve as baseline errors, not rejects)"
            ),
        }
    }
    (sourcepath.clone(), sourcepath)
}

fn tool_present(bin: &str) -> bool {
    ci_core::probe_tool(Command::new(bin).arg("--version")).is_some()
}

/// `mvn dependency:build-classpath` writes the resolved dependency jars to a file — the one
/// documented single-command derivation Maven has.
pub(crate) fn maven_classpath(root: &Path) -> Option<String> {
    let out = tempfile::NamedTempFile::new().ok()?;
    let status = Command::new("mvn")
        .args(["-q", "-B", "dependency:build-classpath"])
        .arg(format!("-Dmdep.outputFile={}", out.path().display()))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let cp = std::fs::read_to_string(out.path()).ok()?.trim().to_string();
    Some(cp)
}

/// Gradle has no single built-in equivalent; an injected init-script task printing the
/// resolved runtime classpath (test scope preferred, mirroring Maven's default) is the
/// standard one-command shape.
pub(crate) fn gradle_classpath(root: &Path) -> Option<String> {
    let init = tempfile::NamedTempFile::with_suffix(".gradle").ok()?;
    std::fs::write(
        init.path(),
        "allprojects { p ->\n  p.tasks.register('marksmanClasspath') {\n    doLast {\n      def c = p.configurations.findByName('testRuntimeClasspath') ?: p.configurations.findByName('runtimeClasspath') ?: p.configurations.findByName('compileClasspath')\n      if (c != null) { c.files.each { println it } }\n    }\n  }\n}\n",
    )
    .ok()?;
    let out = Command::new("gradle")
        .args(["-q", "--init-script"])
        .arg(init.path())
        .arg("marksmanClasspath")
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let jars: Vec<&str> = std::str::from_utf8(&out.stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    Some(jars.join(path_sep()))
}

/// The Java write engine behind `Composed`: javax.tools verdicts, jdtls rewrites.
pub(crate) struct JavaEngine {
    pub(crate) root: PathBuf,
    pub(crate) sidecar: JavacSidecar,
    /// jdtls, started on the FIRST rename/move only — diagnostics never wait on it, and a
    /// missing jdtls costs nothing until an op actually needs cross-file rewrites.
    pub(crate) lsp: Option<LspClient>,
}

impl JavaEngine {
    fn jdtls(&mut self) -> Result<&mut LspClient> {
        if self.lsp.is_none() {
            self.lsp = Some(jdtls::start(&self.root)?);
        }
        Ok(self.lsp.as_mut().expect("just set"))
    }
}

/// Diagnostics for references to files the CURRENT BATCH deletes (empty-content buffers, the
/// gate's deletion convention): `import a.b.C;` declarations resolving to a deleted path, via the
/// shared §8 engine over the Java move hooks. javac's own diagnostics DO report the resulting
/// unresolved symbol, but the reject-recipe contract (§5) wants the anchored, ready-to-copy site
/// too — so we produce it regardless, the same gap-fill shape lang-rust runs for rustc.
fn deleted_path_references(root: &Path, files: &[(String, String)]) -> Vec<Diag> {
    ci_edit::moves::deleted_reference_diags(&crate::movefix::JavaMoveModel(root), files)
}

impl GateEngine for JavaEngine {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let mut out = self.sidecar.diagnostics(files)?;
        out.extend(deleted_path_references(&self.root, files));
        Ok(out)
    }

    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        GateEngine::rename(self.jdtls()?, file, line, character, new_name)
    }

    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        // Engine-native FIRST (contract §8): jdtls's `willRenameFiles` genuinely rewrites the
        // package declaration AND every importer for a Java move — a complete, compiler-aware
        // rewrite the syntactic model only approximates. This is the OPPOSITE of lang-rust's
        // ordering, and for a principled reason: rust-analyzer's `willRenameFiles` returns
        // NOTHING for the submodule-move shape (engine-native simply doesn't exist there, so
        // movefix leads), whereas jdtls's handler DOES cover the Java move — so where it exists,
        // it wins. jdtls is absent on many machines and minutes-cold; when it's unavailable or
        // declines (empty edit), the movefix hooks are the runnable fallback, and the javac gate
        // judges whichever rewrite lands.
        if let Ok(lsp) = self.jdtls() {
            if let Ok(we) = GateEngine::will_rename(lsp, from, to) {
                if !ci_edit::workspace_edit_is_empty(&we) {
                    return Ok(we);
                }
            }
        }
        Ok(crate::movefix::move_workspace_edit(&self.root, from, to).unwrap_or_else(|| json!({})))
    }

    fn sync_disk(&mut self) -> Result<()> {
        // The sidecar holds no cross-call buffers (each request restates the full overlay);
        // only a started jdtls has state to resync.
        match self.lsp.as_mut() {
            Some(lsp) => lsp.sync_disk(),
            None => Ok(()),
        }
    }

    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        match self.lsp.as_mut() {
            Some(lsp) => lsp.fs_events(created, deleted),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Q3's fallback tier: no build file + no build tool -> the flat source-root classpath,
    // and the conventional Maven/Gradle roots are preferred over the repo root when present.
    #[test]
    fn flat_classpath_prefers_conventional_source_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (cp, sp) = derive_paths(root);
        assert_eq!(cp, root.to_string_lossy(), "flat layout: the repo root is the classpath");
        assert_eq!(cp, sp, "flat tier: classpath == sourcepath");

        std::fs::create_dir_all(root.join("src/main/java")).unwrap();
        std::fs::create_dir_all(root.join("src/test/java")).unwrap();
        let (_, sp) = derive_paths(root);
        assert_eq!(
            sp,
            format!(
                "{}{}{}",
                root.join("src/main/java").display(),
                path_sep(),
                root.join("src/test/java").display()
            ),
            "conventional roots replace the repo root once they exist"
        );
    }
}
