//! ci-lsp — a minimal LSP client over stdio, just enough to drive a language
//! server for the type-check gate: open files with in-memory (VFS) buffers, push
//! `didChange`, and collect the in-memory diagnostics the server publishes. Also
//! exposes a generic `request()` so the edit layer can call `textDocument/rename`.
//!
//! Synchronous: a background thread frames+parses server→client messages onto a
//! channel; the client thread correlates responses by id and drains
//! `publishDiagnostics`. Server→client requests are auto-answered so the server
//! never blocks.
use ci_core::{Diag, Error, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

fn file_uri(abs: &Path) -> String {
    format!("file://{}", abs.to_string_lossy())
}

pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    root: PathBuf,
    next_id: i64,
    /// uri -> current document version (for didOpen vs didChange).
    open: HashMap<String, i64>,
    /// Server supports LSP 3.17 pull diagnostics (`textDocument/diagnostic`). Preferred over
    /// the publish path: request/response semantics, so the gate can never mistake a slow
    /// server for a clean file (the publish path settles on silence — a race under load).
    pull_diagnostics: bool,
    /// Server sends `experimental/serverStatus` (rust-analyzer, when we advertise the client
    /// capability). Until it reports `quiescent`, a pull can return legitimately-EMPTY results
    /// (the file belongs to no loaded crate yet), so the first pull must wait for quiescence.
    saw_server_status: bool,
    quiescent: bool,
    /// One-time grace window granted, so the first pull can't race the server's FIRST
    /// serverStatus notification (sent moments after `initialized`) and skip the wait.
    status_grace_done: bool,
}

impl LspClient {
    /// Spawn a language server (`cmd`, supplied by the language provider — this crate
    /// is language-agnostic and Rust-only) and run the LSP handshake. `root` is the
    /// workspace root used for `rootUri` and document URIs.
    pub fn start(root: &Path, mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Driver(format!("spawn language server: {e}")))?;

        let stdin = child.stdin.take().ok_or_else(|| Error::Driver("no lsp stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Driver("no lsp stdout".into()))?;
        let (tx, rx) = channel::<Value>();
        std::thread::spawn(move || reader_loop(stdout, tx));

        let mut client = LspClient {
            child,
            stdin,
            rx,
            root: root.to_path_buf(),
            next_id: 1,
            open: HashMap::new(),
            pull_diagnostics: false,
            saw_server_status: false,
            quiescent: false,
            status_grace_done: false,
        };

        let init = json!({
            "processId": null,
            "rootUri": file_uri(root),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {},
                    "synchronization": {},
                    "diagnostic": {},
                    // Hierarchical DocumentSymbol[] (selectionRange + children) instead of flat
                    // SymbolInformation[] — ci-lsp-index needs real name ranges and full nesting
                    // chains; servers without support keep returning the flat shape.
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                },
                "workspace": { "fileOperations": { "willRename": true } },
                "experimental": { "serverStatusNotification": true },
            },
            "workspaceFolders": [ { "uri": file_uri(root), "name": "root" } ],
        });
        let id = client.send_request("initialize", init)?;
        let resp = client.pump_until_response(id, Duration::from_secs(60))?;
        client.pull_diagnostics = resp
            .pointer("/result/capabilities/diagnosticProvider")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        client.send_notification("initialized", json!({}))?;
        Ok(client)
    }

    /// Repo root this server was started for (used to build `file://` URIs).
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Type-check the given files (repo-relative path + buffer content) and return
    /// the ERROR diagnostics. Buffers are in-memory overlays — disk is not read.
    pub fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let mut uri_to_rel: HashMap<String, String> = HashMap::new();
        for (rel, content) in files {
            let uri = file_uri(&self.root.join(rel));
            uri_to_rel.insert(uri.clone(), rel.clone());
            match self.open.get(&uri).copied() {
                None => {
                    self.open.insert(uri.clone(), 1);
                    self.send_notification(
                        "textDocument/didOpen",
                        json!({"textDocument": {"uri": uri, "languageId": "typescript", "version": 1, "text": content}}),
                    )?;
                }
                Some(v) => {
                    let nv = v + 1;
                    self.open.insert(uri.clone(), nv);
                    self.send_notification(
                        "textDocument/didChange",
                        json!({"textDocument": {"uri": uri, "version": nv}, "contentChanges": [{"text": content}]}),
                    )?;
                }
            }
        }

        if self.pull_diagnostics {
            return self.pull_diagnostics_for(&uri_to_rel);
        }

        let targets: HashSet<String> = uri_to_rel.keys().cloned().collect();
        let mut store: HashMap<String, Vec<Value>> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        // Poll cadence. Two ways to settle: every target re-published (fast), OR the server
        // has gone fully silent for `idle_quiet` — which is the ONLY signal for an all-clean
        // edit, because tsserver does NOT re-publish diagnostics for files that stay clean
        // (so `seen` would never reach `targets` and we'd burn the whole deadline). During a
        // cold project load tsserver streams progress/log messages, so a sustained silence
        // can't be mistaken for "still loading".
        let tick = Duration::from_millis(300);
        let idle_quiet = Duration::from_millis(1500);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut quiet_since = Instant::now();
        let timing = std::env::var("CI_TIMING").is_ok();
        let t_diag = Instant::now();
        let mut exit = "deadline";

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining.min(tick)) {
                Ok(msg) => {
                    self.observe(&msg);
                    quiet_since = Instant::now(); // any server activity resets the silence timer
                    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    if method == "textDocument/publishDiagnostics" {
                        if let Some(p) = msg.get("params") {
                            if let Some(uri) = p.get("uri").and_then(|u| u.as_str()) {
                                if targets.contains(uri) {
                                    let diags = p.get("diagnostics").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                                    store.insert(uri.to_string(), diags);
                                    seen.insert(uri.to_string());
                                }
                            }
                        }
                    } else if msg.get("id").is_some() && msg.get("method").is_some() {
                        self.reply_server_request(&msg)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // All targets re-published then a brief lull (one tick) -> settled fast.
                    if seen.len() >= targets.len() {
                        exit = "settled-all";
                        break;
                    }
                    // Sustained silence after activity (or from a warm start) -> the unreported
                    // files are simply clean. This is what rescues the all-clean case.
                    if quiet_since.elapsed() >= idle_quiet {
                        exit = "settled-quiet";
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if timing {
            eprintln!(
                "[timing]   diagnostics() {:?} via {exit} (seen {}/{} files)",
                t_diag.elapsed(), seen.len(), targets.len()
            );
        }

        let mut out = Vec::new();
        for (uri, rel) in &uri_to_rel {
            for d in store.get(uri).into_iter().flatten() {
                if let Some(diag) = error_diag(rel, d) {
                    out.push(diag);
                }
            }
        }
        Ok(out)
    }

    /// LSP 3.17 pull diagnostics: one `textDocument/diagnostic` request per target. The response
    /// is computed for the exact content we just pushed — no settle heuristics, so a slow server
    /// stalls the gate (recoverable timeout) instead of slipping a broken edit through. Transient
    /// "still loading / content modified" errors are retried until the deadline.
    fn pull_diagnostics_for(&mut self, uri_to_rel: &HashMap<String, String>) -> Result<Vec<Diag>> {
        let timing = std::env::var("CI_TIMING").is_ok();
        let t_diag = Instant::now();
        // Cold-server guard: before the workspace is loaded, a pull returns legitimately-EMPTY
        // diagnostics (the file belongs to no crate yet) — the one way a broken edit could still
        // slip through the pull path. Wait for quiescence first when the server reports status.
        self.wait_quiescent(Instant::now() + Duration::from_secs(120))?;
        let mut out = Vec::new();
        for (uri, rel) in uri_to_rel {
            // Generous: the FIRST pull can block on the cold project load (rust-analyzer queues
            // the request until the workspace is ready); warm pulls are sub-second.
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let id = self.send_request(
                    "textDocument/diagnostic",
                    json!({ "textDocument": { "uri": uri } }),
                )?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                let resp = self.pump_until_response(id, remaining.max(Duration::from_secs(1)))?;
                if let Some(err) = resp.get("error") {
                    let msg = err.to_string().to_lowercase();
                    let transient = msg.contains("content modified")
                        || msg.contains("-32801")
                        || msg.contains("-32802") // ServerCancelled + retriggerRequest: an explicit "retry me"
                        || msg.contains("server cancelled")
                        || msg.contains("loading")
                        || msg.contains("not ready");
                    if transient && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(150));
                        continue;
                    }
                    return Err(Error::Driver(format!("lsp textDocument/diagnostic error: {err}")));
                }
                for d in resp.pointer("/result/items").and_then(|i| i.as_array()).into_iter().flatten() {
                    if let Some(diag) = error_diag(rel, d) {
                        out.push(diag);
                    }
                }
                break;
            }
        }
        if timing {
            eprintln!("[timing]   diagnostics() {:?} via pull ({} files)", t_diag.elapsed(), uri_to_rel.len());
        }
        Ok(out)
    }

    /// Re-push the CURRENT on-disk content of every document this client has opened, restoring
    /// the server's buffers after a dry-run/rejected gate left overlay content in them. A file
    /// gone from disk is closed. (The edit layer calls this before computing rename edits —
    /// spans computed against phantom buffer state slice the wrong text on disk.)
    /// Staged file-system changes from the edit gate: `didClose` deleted buffers (an open
    /// buffer SHADOWS the file system in every LSP server) and push a
    /// `workspace/didChangeWatchedFiles` so servers running their own watcher
    /// (rust-analyzer) re-derive the project immediately instead of racing the OS notifier.
    pub fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        for rel in deleted {
            let uri = file_uri(&self.root.join(rel));
            if self.open.remove(&uri).is_some() {
                self.send_notification("textDocument/didClose", json!({"textDocument": {"uri": uri}}))?;
            }
        }
        let changes: Vec<Value> = created
            .iter()
            .map(|r| json!({"uri": file_uri(&self.root.join(r)), "type": 1})) // Created
            .chain(deleted.iter().map(|r| json!({"uri": file_uri(&self.root.join(r)), "type": 3}))) // Deleted
            .collect();
        if !changes.is_empty() {
            self.send_notification("workspace/didChangeWatchedFiles", json!({"changes": changes}))?;
            // The events invalidate the server's PROJECT view (crate graph / module tree); a
            // pull issued against the old view returns stale-clean — a false-clean gate on
            // moves/deletes (bench move-rust round 4: E0432s invisible to the gate). For
            // status-reporting servers (rust-analyzer), demand a FRESH quiescent: mark
            // non-quiescent and give the server a short window to signal. A busy signal keeps
            // the next pull waiting for real; total silence means no reload was triggered and
            // quiescence is restored.
            if self.saw_server_status {
                self.quiescent = false;
                let mut saw_status = false;
                let grace = Instant::now() + Duration::from_millis(2000);
                while !self.quiescent && Instant::now() < grace {
                    match self.rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(msg) => {
                            if msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus") {
                                saw_status = true;
                            }
                            self.observe(&msg);
                            if msg.get("id").is_some() && msg.get("method").is_some() {
                                self.reply_server_request(&msg)?;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(Error::Driver("lsp server disconnected".into()))
                        }
                    }
                }
                if !saw_status && !self.quiescent {
                    self.quiescent = true; // silence: the events triggered no reload
                }
            }
        }
        Ok(())
    }

    pub fn sync_disk(&mut self) -> Result<()> {
        let uris: Vec<String> = self.open.keys().cloned().collect();
        for uri in uris {
            let path = PathBuf::from(uri.strip_prefix("file://").unwrap_or(&uri));
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let v = self.open.get(&uri).copied().unwrap_or(1) + 1;
                    self.open.insert(uri.clone(), v);
                    self.send_notification(
                        "textDocument/didChange",
                        json!({"textDocument": {"uri": uri, "version": v}, "contentChanges": [{"text": content}]}),
                    )?;
                }
                Err(_) => {
                    self.open.remove(&uri);
                    self.send_notification("textDocument/didClose", json!({"textDocument": {"uri": uri}}))?;
                }
            }
        }
        Ok(())
    }

    /// Generic request (e.g. `textDocument/rename`) -> the response `result`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        let resp = self.pump_until_response(id, Duration::from_secs(30))?;
        if let Some(err) = resp.get("error") {
            return Err(Error::Driver(format!("lsp {method} error: {err}")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    // ── internals ──────────────────────────────────────────────────────────

    /// Track `experimental/serverStatus` notifications (rust-analyzer): `quiescent` flips true
    /// once initial analysis is done. Must be called on every message every receive loop sees,
    /// or a status update read by one loop is lost to the others.
    fn observe(&mut self, msg: &Value) {
        if msg.get("method").and_then(|m| m.as_str()) == Some("experimental/serverStatus") {
            self.saw_server_status = true;
            self.quiescent = msg.pointer("/params/quiescent").and_then(|q| q.as_bool()).unwrap_or(false);
        }
    }

    /// Block until a status-reporting server (rust-analyzer) is quiescent. No-op for servers
    /// that never sent `experimental/serverStatus` (tsls): their pulls are request-scoped and
    /// don't depend on a background workspace load.
    fn wait_quiescent(&mut self, deadline: Instant) -> Result<()> {
        if !self.saw_server_status && !self.status_grace_done {
            let grace = Instant::now() + Duration::from_secs(3);
            while !self.saw_server_status && Instant::now() < grace {
                match self.rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(msg) => {
                        self.observe(&msg);
                        if msg.get("id").is_some() && msg.get("method").is_some() {
                            self.reply_server_request(&msg)?;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(Error::Driver("lsp server disconnected".into()))
                    }
                }
            }
            self.status_grace_done = true;
        }
        while self.saw_server_status && !self.quiescent {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Driver("lsp server not quiescent before deadline".into()));
            }
            match self.rx.recv_timeout(remaining.min(Duration::from_millis(300))) {
                Ok(msg) => {
                    self.observe(&msg);
                    if msg.get("id").is_some() && msg.get("method").is_some() {
                        self.reply_server_request(&msg)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Driver("lsp server disconnected".into()))
                }
            }
        }
        Ok(())
    }

    fn pump_until_response(&mut self, id: i64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Driver(format!("lsp response {id} timed out")));
            }
            match self.rx.recv_timeout(remaining.min(Duration::from_secs(5))) {
                Ok(msg) => {
                    self.observe(&msg);
                    let is_resp = msg.get("id").map(|v| v == &json!(id)).unwrap_or(false)
                        && (msg.get("result").is_some() || msg.get("error").is_some());
                    if is_resp {
                        return Ok(msg);
                    }
                    if msg.get("id").is_some() && msg.get("method").is_some() {
                        self.reply_server_request(&msg)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Driver("lsp server disconnected".into()))
                }
            }
        }
    }

    /// Answer a server→client request so it never blocks. `workspace/configuration`
    /// needs an array result; everything else gets null.
    fn reply_server_request(&mut self, msg: &Value) -> Result<()> {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let result = if method == "workspace/configuration" {
            let n = msg
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(|i| i.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            Value::Array(vec![Value::Null; n])
        } else {
            Value::Null
        };
        self.write_msg(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_msg(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn write_msg(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_vec(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|_| self.stdin.write_all(&body))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| Error::Driver(format!("lsp write: {e}")))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.send_notification("exit", json!(null));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An LSP `Diagnostic` JSON value -> `Diag`, errors only (`None` for warnings/hints).
fn error_diag(rel: &str, d: &Value) -> Option<Diag> {
    if d.get("severity").and_then(|s| s.as_i64()).unwrap_or(1) != 1 {
        return None; // errors only
    }
    let code = d
        .get("code")
        .and_then(|c| c.as_i64().or_else(|| c.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0);
    let message = d.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let line = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|l| l.as_i64())
        .unwrap_or(0) as u32
        + 1;
    Some(Diag { file: rel.to_string(), code, message, line })
}

/// Read framed LSP messages (`Content-Length: N\r\n\r\n<json>`) onto the channel.
fn reader_loop(stdout: impl Read, tx: std::sync::mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut content_length = 0usize;
        // headers
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return, // EOF
                Ok(_) => {}
                Err(_) => return,
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse().unwrap_or(0);
            }
        }
        if content_length == 0 {
            continue;
        }
        let mut buf = vec![0u8; content_length];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        match serde_json::from_slice::<Value>(&buf) {
            Ok(v) => {
                if tx.send(v).is_err() {
                    return;
                }
            }
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Real end-to-end: spawns typescript-language-server via npx. Slow + network
    // on first run -> #[ignore]. Run with `cargo test -p ci-lsp -- --ignored`.
    #[test]
    #[ignore]
    fn gate_flags_type_error_and_passes_clean_code() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["src"]}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.ts"), "export const x: number = 1;\n").unwrap();

        // The provider supplies the server command; this crate stays generic.
        let mut cmd = Command::new("npx");
        cmd.args(["--yes", "-p", "typescript-language-server", "-p", "typescript", "typescript-language-server", "--stdio"])
            .env("npm_config_cache", std::env::var("CI_NPM_CACHE").unwrap_or_else(|_| "/tmp/ci-npm-cache".into()));
        let mut lsp = LspClient::start(root, cmd).expect("start tsls");

        // Clean buffer -> no error diagnostics.
        let clean = lsp.diagnostics(&[("src/a.ts".into(), "export const x: number = 1;\n".into())]).unwrap();
        assert!(clean.is_empty(), "clean code should have no errors, got {clean:?}");

        // Buffer with a type error -> at least one error diagnostic (TS2322).
        let bad = lsp
            .diagnostics(&[("src/a.ts".into(), "export const x: number = \"hi\";\n".into())])
            .unwrap();
        assert!(!bad.is_empty(), "type error should be flagged");
        assert!(bad.iter().any(|d| d.code == 2322), "expected TS2322, got {bad:?}");
    }
}
