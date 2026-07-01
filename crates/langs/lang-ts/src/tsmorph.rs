//! ts-morph gate engine — a persistent Node sidecar holding the TS project in memory.
//! Lighter (in interface, not memory) than an LSP server for the TS write path: synchronous
//! diagnostics (no publish/settle race) and the raw TS LanguageService for precise rename /
//! move edits. The sidecar script is bundled in the binary and run with a managed ts-morph
//! install (no global dependency); `node` and `npm` are resolved from PATH like our other
//! TS tooling.
use ci_core::{Diag, Error, Result};
use ci_edit::GateEngine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIDECAR_SRC: &str = include_str!("sidecar.cjs");

/// Where the managed ts-morph install + sidecar live (cached across runs).
fn ts_morph_home() -> PathBuf {
    std::env::var("CI_TSMORPH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("ci-tsmorph"))
}

use crate::npm_cache;

/// Ensure ts-morph is installed in `home` (one-time `npm install --prefix`), then drop the
/// bundled sidecar next to its `node_modules` so `require('ts-morph')` resolves.
fn ensure_sidecar(home: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(home)?;
    let installed = home.join("node_modules/ts-morph/package.json");
    if !installed.exists() {
        // Serialize against concurrent MCP instances installing into the shared npm cache (same
        // race as scip-typescript). Re-check after the lock: another holder may have just installed.
        let _cache_lock = crate::NpxCacheLock::acquire();
        if !installed.exists() {
            let status = Command::new("npm")
                .args(["install", "--silent", "--no-audit", "--no-fund", "--prefix"])
                .arg(home)
                .arg("ts-morph")
                .env("npm_config_cache", npm_cache())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| Error::Driver(format!("npm install ts-morph failed to launch: {e}")))?;
            if !status.success() {
                return Err(Error::Driver(format!("npm install ts-morph failed ({status})")));
            }
        }
    }
    let sidecar = home.join("sidecar.cjs");
    std::fs::write(&sidecar, SIDECAR_SRC)?;
    Ok(sidecar)
}

/// A live ts-morph sidecar for one repo. stdout is pumped by a reader thread into a channel so
/// `call` can wait with a DEADLINE (a wedged sidecar must not hang the write path forever);
/// stderr is captured so a sidecar failure isn't invisible.
pub struct TsMorphClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    next_id: i64,
}

impl TsMorphClient {
    /// Start the sidecar for `root`. Loads the whole tsconfig program at startup (the warm
    /// cost), so callers should do this on a background thread (see TsProvider::prewarm).
    pub fn start(root: &Path) -> Result<Self> {
        let sidecar = ensure_sidecar(&ts_morph_home())?;
        let mut child = Command::new("node")
            .arg(&sidecar)
            .arg("--root")
            .arg(root)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Driver(format!("launching ts-morph sidecar (node) failed: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Driver("sidecar stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Driver("sidecar stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::Driver("sidecar stderr".into()))?;

        // Pump stdout lines into a channel so call() can use recv_timeout.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(std::mem::take(&mut line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Capture stderr (bounded) so a sidecar crash/throw is reportable, not silent.
        let errbuf = Arc::new(Mutex::new(String::new()));
        {
            let errbuf = errbuf.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(mut s) = errbuf.lock() {
                                s.push_str(&line);
                                if s.len() > 8192 {
                                    let cut = s.len() - 8192;
                                    s.drain(..cut);
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(Self { child, stdin, rx, stderr: errbuf, next_id: 1 })
    }

    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!(" — sidecar stderr: {}", s.trim()))
            .unwrap_or_default()
    }

    fn call(&mut self, op: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = op;
        req["id"] = id.into();
        writeln!(self.stdin, "{req}").map_err(|e| Error::Driver(format!("sidecar write: {e}")))?;
        self.stdin.flush().ok();

        // The first call pays the project load (a few seconds); later ops are sub-second. A
        // generous deadline keeps a wedged sidecar from hanging the engine (and its Mutex)
        // forever — recoverable as an error instead of a permanent hang.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Driver(format!("ts-morph sidecar timed out{}", self.stderr_tail())));
            }
            match self.rx.recv_timeout(remaining.min(Duration::from_secs(5))) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                    if v.get("id").and_then(Value::as_i64) == Some(id) {
                        if let Some(err) = v.get("error").and_then(Value::as_str) {
                            return Err(Error::Driver(format!("ts-morph: {err}")));
                        }
                        return Ok(v);
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue, // re-check deadline
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Driver(format!("ts-morph sidecar closed{}", self.stderr_tail())));
                }
            }
        }
    }
}

impl Drop for TsMorphClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl GateEngine for TsMorphClient {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let files_json: Vec<Value> =
            files.iter().map(|(p, c)| json!({ "path": p, "content": c })).collect();
        let res = self.call(json!({ "op": "diagnostics", "files": files_json }))?;
        let diags = res["diags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|d| Diag {
                        file: d["file"].as_str().unwrap_or_default().to_string(),
                        code: d["code"].as_i64().unwrap_or(0),
                        message: d["message"].as_str().unwrap_or_default().to_string(),
                        line: d["line"].as_u64().unwrap_or(1) as u32,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(diags)
    }

    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        let res = self.call(json!({
            "op": "rename", "file": file, "line": line, "character": character, "newName": new_name
        }))?;
        Ok(json!({ "changes": res.get("changes").cloned().unwrap_or_else(|| json!({})) }))
    }

    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        let res = self.call(json!({ "op": "willRename", "from": from, "to": to }))?;
        Ok(json!({ "changes": res.get("changes").cloned().unwrap_or_else(|| json!({})) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Real sidecar: npm-installs ts-morph + spawns node. #[ignore]; run with
    // `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn tsmorph_diagnostics_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n").unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from \"./math.js\";\nexport const r = add(1, 2);\n").unwrap();

        let mut c = TsMorphClient::start(root).expect("start ts-morph sidecar");

        // clean file -> no diagnostics
        let clean = c.diagnostics(&[("src/math.ts".into(), fs::read_to_string(root.join("src/math.ts")).unwrap())]).unwrap();
        assert!(clean.is_empty(), "clean file should have no errors: {clean:?}");

        // rename `add` at line 0, char 16 -> edits in BOTH math.ts (def) and app.ts (call site)
        let we = c.rename("src/math.ts", 0, 16, "sum").unwrap();
        let changes = we["changes"].as_object().expect("changes object");
        let files: Vec<String> =
            changes.keys().map(|u| u.rsplit('/').next().unwrap_or(u).to_string()).collect();
        assert!(files.iter().any(|f| f == "math.ts"), "rename touches definition: {files:?}");
        assert!(files.iter().any(|f| f == "app.ts"), "rename touches caller: {files:?}");

        // type error -> reported with the right code
        let bad = c.diagnostics(&[("src/math.ts".into(), "export const x: number = \"no\";\n".into())]).unwrap();
        assert!(bad.iter().any(|d| d.code == 2322), "type error must be reported: {bad:?}");
    }
}
