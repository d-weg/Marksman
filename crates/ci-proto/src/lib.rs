//! ci-proto — the out-of-process language-provider wire protocol.
//!
//! A provider can run as a separate executable (a "sidecar") that the core spawns and talks to
//! over **stdio**, framed as **length-delimited protobuf** (`prost`). Protobuf over JSON because
//! `structure()` is called per-file thousands of times at indexing volume, where compact, typed,
//! cheap-to-decode messages matter; this speeds the provider RPC + the wall-clock axis (not the
//! agent token count, which is dominated by turns).
//!
//! - [`ProcessProvider`] — host side: spawns a provider binary and implements [`LanguageProvider`]
//!   by serializing each trait call to a [`Request`] and decoding the [`Response`].
//! - [`serve_stdio`] — sidecar side: a blocking loop that answers requests for a given provider.
//!
//! The wire mirrors the four trait methods (granularity / structure / import_graph / apply_edits)
//! plus `outline`. Complex enums are flattened (e.g. an [`EditOp`] becomes a tagged [`PbEditOp`])
//! so the schema stays a handful of plain messages with no nested oneofs.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node, NodeKind, Range,
    Result, SymbolKind,
};
use prost::Message;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

// method tags
const M_GRANULARITY: i32 = 0;
const M_STRUCTURE: i32 = 1;
const M_IMPORT_GRAPH: i32 = 2;
const M_OUTLINE: i32 = 3;
const M_APPLY_EDITS: i32 = 4;

// ── messages ─────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbRange {
    #[prost(uint32, tag = "1")]
    pub start_line: u32,
    #[prost(uint32, tag = "2")]
    pub start_char: u32,
    #[prost(uint32, tag = "3")]
    pub end_line: u32,
    #[prost(uint32, tag = "4")]
    pub end_char: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbNode {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
    /// 0 = symbol (see `sym_kind`), 1 = syntax (see `syntax_kind`).
    #[prost(int32, tag = "3")]
    pub kind_tag: i32,
    #[prost(int32, tag = "4")]
    pub sym_kind: i32,
    #[prost(string, tag = "5")]
    pub syntax_kind: String,
    #[prost(message, optional, tag = "6")]
    pub range: Option<PbRange>,
    #[prost(message, optional, tag = "7")]
    pub name_range: Option<PbRange>,
    #[prost(message, repeated, tag = "8")]
    pub children: Vec<PbNode>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbEdge {
    #[prost(string, tag = "1")]
    pub from: String,
    #[prost(string, repeated, tag = "2")]
    pub tos: Vec<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbEditOp {
    #[prost(string, tag = "1")]
    pub op: String,
    #[prost(string, tag = "2")]
    pub node_id: String,
    /// code / body / new_name, depending on `op`.
    #[prost(string, tag = "3")]
    pub text: String,
    #[prost(string, tag = "4")]
    pub old_text: String,
    #[prost(string, tag = "5")]
    pub new_text: String,
    #[prost(string, tag = "6")]
    pub from: String,
    #[prost(string, tag = "7")]
    pub to: String,
    #[prost(string, tag = "8")]
    pub path: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbEditOpts {
    #[prost(bool, tag = "1")]
    pub write: bool,
    #[prost(bool, tag = "2")]
    pub dry_run: bool,
    #[prost(string, tag = "3")]
    pub tsconfig: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbCommitResult {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(uint64, tag = "2")]
    pub applied_ops: u64,
    #[prost(string, repeated, tag = "3")]
    pub changed_files: Vec<String>,
    #[prost(uint32, tag = "4")]
    pub repair_rounds: u32,
    #[prost(int64, tag = "5")]
    pub failed_op_index: i64,
    #[prost(string, tag = "6")]
    pub feedback: String,
    /// JSON-encoded `Vec<Diag>` of pre-existing radius errors excused by the baseline diff
    /// (empty = clean radius). JSON keeps the wire stable while the Diag shape evolves.
    #[prost(string, tag = "7")]
    pub preexisting_json: String,
    #[prost(uint64, tag = "8")]
    pub redundant_ops: u64,
    #[prost(string, tag = "9")]
    pub rewrite_summary: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Request {
    #[prost(int32, tag = "1")]
    pub method: i32,
    #[prost(string, tag = "2")]
    pub file: String,
    #[prost(string, tag = "3")]
    pub content: String,
    #[prost(message, repeated, tag = "4")]
    pub ops: Vec<PbEditOp>,
    #[prost(message, optional, tag = "5")]
    pub opts: Option<PbEditOpts>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Response {
    /// Non-empty on failure (the provider error); otherwise the payload fields apply.
    #[prost(string, tag = "1")]
    pub error: String,
    #[prost(int32, tag = "2")]
    pub granularity: i32,
    #[prost(message, repeated, tag = "3")]
    pub nodes: Vec<PbNode>,
    #[prost(message, repeated, tag = "4")]
    pub edges: Vec<PbEdge>,
    #[prost(string, tag = "5")]
    pub outline: String,
    #[prost(message, optional, tag = "6")]
    pub result: Option<PbCommitResult>,
}

// ── conversions ──────────────────────────────────────────────────────────────

fn sym_to_i32(k: SymbolKind) -> i32 {
    use SymbolKind::*;
    match k {
        Function => 0,
        Class => 1,
        Interface => 2,
        Enum => 3,
        TypeAlias => 4,
        Variable => 5,
        Method => 6,
        Struct => 7,
    }
}

fn i32_to_sym(i: i32) -> SymbolKind {
    use SymbolKind::*;
    match i {
        1 => Class,
        2 => Interface,
        3 => Enum,
        4 => TypeAlias,
        5 => Variable,
        6 => Method,
        7 => Struct,
        _ => Function,
    }
}

impl From<&Range> for PbRange {
    fn from(r: &Range) -> Self {
        PbRange { start_line: r.start_line, start_char: r.start_char, end_line: r.end_line, end_char: r.end_char }
    }
}
impl From<&PbRange> for Range {
    fn from(r: &PbRange) -> Self {
        Range { start_line: r.start_line, start_char: r.start_char, end_line: r.end_line, end_char: r.end_char }
    }
}

fn node_to_pb(n: &Node) -> PbNode {
    let (kind_tag, sym_kind, syntax_kind) = match &n.kind {
        NodeKind::Symbol(k) => (0, sym_to_i32(*k), String::new()),
        NodeKind::Syntax(s) => (1, 0, s.clone()),
    };
    PbNode {
        id: n.id.clone(),
        name: n.name.clone(),
        kind_tag,
        sym_kind,
        syntax_kind,
        range: Some((&n.range).into()),
        name_range: n.name_range.as_ref().map(Into::into),
        children: n.children.iter().map(node_to_pb).collect(),
    }
}

fn pb_to_node(p: &PbNode) -> Node {
    let kind = if p.kind_tag == 1 {
        NodeKind::Syntax(p.syntax_kind.clone())
    } else {
        NodeKind::Symbol(i32_to_sym(p.sym_kind))
    };
    Node {
        id: p.id.clone(),
        name: p.name.clone(),
        kind,
        range: p.range.as_ref().map(Into::into).unwrap_or(Range { start_line: 0, start_char: 0, end_line: 0, end_char: 0 }),
        name_range: p.name_range.as_ref().map(Into::into),
        children: p.children.iter().map(pb_to_node).collect(),
    }
}

fn op_to_pb(op: &EditOp) -> PbEditOp {
    let mut p = PbEditOp::default();
    match op {
        EditOp::SetBody { node_id, body } => {
            p.op = "set_body".into();
            p.node_id = node_id.clone();
            p.text = body.clone();
        }
        EditOp::ReplaceNode { node_id, code } => {
            p.op = "replace_node".into();
            p.node_id = node_id.clone();
            p.text = code.clone();
        }
        EditOp::ReplaceText { node_id, old_text, new_text } => {
            p.op = "replace_text".into();
            p.node_id = node_id.clone();
            p.old_text = old_text.clone();
            p.new_text = new_text.clone();
        }
        EditOp::InsertBefore { node_id, code } => {
            p.op = "insert_before".into();
            p.node_id = node_id.clone();
            p.text = code.clone();
        }
        EditOp::InsertInBody { node_id, code, after } => {
            p.op = "insert_in_body".into();
            p.node_id = node_id.clone();
            p.text = code.clone();
            p.old_text = after.clone().unwrap_or_default();
        }
        EditOp::DeleteInBody { node_id, text } => {
            p.op = "delete_in_body".into();
            p.node_id = node_id.clone();
            p.text = text.clone();
        }
        EditOp::InsertMember { node_id, code } => {
            p.op = "insert_member".into();
            p.node_id = node_id.clone();
            p.text = code.clone();
        }
        EditOp::AddParameter { node_id, param } => {
            p.op = "add_parameter".into();
            p.node_id = node_id.clone();
            p.text = param.clone();
        }
        EditOp::SetReturnType { node_id, ty } => {
            p.op = "set_return_type".into();
            p.node_id = node_id.clone();
            p.text = ty.clone();
        }
        EditOp::Rename { node_id, new_name } => {
            p.op = "rename".into();
            p.node_id = node_id.clone();
            p.text = new_name.clone();
        }
        EditOp::ReplaceInFile { path, old_text, new_text } => {
            p.op = "replace_in_file".into();
            p.path = path.to_string_lossy().into_owned();
            p.old_text = old_text.clone();
            p.new_text = new_text.clone();
        }
        EditOp::MoveFile { from, to } => {
            p.op = "move_file".into();
            p.from = from.to_string_lossy().into_owned();
            p.to = to.to_string_lossy().into_owned();
        }
        EditOp::AddSymbol { path, code } => {
            p.op = "add_symbol".into();
            p.path = path.to_string_lossy().into_owned();
            p.text = code.clone();
        }
        EditOp::CreateFile { path, code } => {
            p.op = "create_file".into();
            p.path = path.to_string_lossy().into_owned();
            p.text = code.clone();
        }
        EditOp::DeleteFile { path } => {
            p.op = "delete_file".into();
            p.path = path.to_string_lossy().into_owned();
        }
    }
    p
}

fn pb_to_op(p: &PbEditOp) -> Result<EditOp> {
    Ok(match p.op.as_str() {
        "set_body" => EditOp::SetBody { node_id: p.node_id.clone(), body: p.text.clone() },
        "replace_node" => EditOp::ReplaceNode { node_id: p.node_id.clone(), code: p.text.clone() },
        "replace_text" => EditOp::ReplaceText { node_id: p.node_id.clone(), old_text: p.old_text.clone(), new_text: p.new_text.clone() },
        "insert_before" => EditOp::InsertBefore { node_id: p.node_id.clone(), code: p.text.clone() },
        "insert_in_body" => EditOp::InsertInBody {
            node_id: p.node_id.clone(),
            code: p.text.clone(),
            after: (!p.old_text.is_empty()).then(|| p.old_text.clone()),
        },
        "delete_in_body" => EditOp::DeleteInBody { node_id: p.node_id.clone(), text: p.text.clone() },
        "insert_member" => EditOp::InsertMember { node_id: p.node_id.clone(), code: p.text.clone() },
        "add_parameter" => EditOp::AddParameter { node_id: p.node_id.clone(), param: p.text.clone() },
        "set_return_type" => EditOp::SetReturnType { node_id: p.node_id.clone(), ty: p.text.clone() },
        "rename" => EditOp::Rename { node_id: p.node_id.clone(), new_name: p.text.clone() },
        "replace_in_file" => EditOp::ReplaceInFile {
            path: PathBuf::from(&p.path),
            old_text: p.old_text.clone(),
            new_text: p.new_text.clone(),
        },
        "move_file" => EditOp::MoveFile { from: PathBuf::from(&p.from), to: PathBuf::from(&p.to) },
        "add_symbol" => EditOp::AddSymbol { path: PathBuf::from(&p.path), code: p.text.clone() },
        "create_file" => EditOp::CreateFile { path: PathBuf::from(&p.path), code: p.text.clone() },
        "delete_file" => EditOp::DeleteFile { path: PathBuf::from(&p.path) },
        other => return Err(Error::Driver(format!("unknown edit op over the wire: {other}"))),
    })
}

fn opts_to_pb(o: &EditOpts) -> PbEditOpts {
    PbEditOpts { write: o.write, dry_run: o.dry_run, tsconfig: o.tsconfig.clone().unwrap_or_default() }
}
fn pb_to_opts(p: &PbEditOpts) -> EditOpts {
    EditOpts {
        write: p.write,
        dry_run: p.dry_run,
        tsconfig: if p.tsconfig.is_empty() { None } else { Some(p.tsconfig.clone()) },
    }
}

fn commit_to_pb(c: &CommitResult) -> PbCommitResult {
    match c {
        CommitResult::Ok { applied_ops, changed_files, repair_rounds, preexisting_in_radius, redundant_ops, rewrite_summary } => PbCommitResult {
            ok: true,
            applied_ops: *applied_ops as u64,
            changed_files: changed_files.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            repair_rounds: *repair_rounds,
            failed_op_index: 0,
            feedback: String::new(),
            preexisting_json: if preexisting_in_radius.is_empty() {
                String::new()
            } else {
                serde_json::to_string(preexisting_in_radius).unwrap_or_default()
            },
            redundant_ops: *redundant_ops as u64,
            rewrite_summary: rewrite_summary.clone(),
        },
        CommitResult::Rejected { failed_op_index, feedback } => PbCommitResult {
            ok: false,
            applied_ops: 0,
            changed_files: vec![],
            repair_rounds: 0,
            failed_op_index: *failed_op_index,
            feedback: feedback.clone(),
            preexisting_json: String::new(),
            redundant_ops: 0,
            rewrite_summary: String::new(),
        },
    }
}
fn pb_to_commit(p: &PbCommitResult) -> CommitResult {
    if p.ok {
        CommitResult::Ok {
            applied_ops: p.applied_ops as usize,
            changed_files: p.changed_files.iter().map(PathBuf::from).collect(),
            repair_rounds: p.repair_rounds,
            preexisting_in_radius: serde_json::from_str(&p.preexisting_json).unwrap_or_default(),
            redundant_ops: p.redundant_ops as usize,
            rewrite_summary: p.rewrite_summary.clone(),
        }
    } else {
        CommitResult::Rejected { failed_op_index: p.failed_op_index, feedback: p.feedback.clone() }
    }
}

// ── framing ──────────────────────────────────────────────────────────────────

/// Write a length-delimited protobuf message (`[u32 big-endian len][bytes]`).
pub fn write_msg<W: Write, M: Message>(w: &mut W, m: &M) -> io::Result<()> {
    let buf = m.encode_to_vec();
    w.write_all(&(buf.len() as u32).to_be_bytes())?;
    w.write_all(&buf)?;
    w.flush()
}

/// Read one length-delimited message; `Ok(None)` at clean EOF.
pub fn read_msg<R: Read, M: Message + Default>(r: &mut R) -> io::Result<Option<M>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    M::decode(&buf[..]).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ── host: ProcessProvider ────────────────────────────────────────────────────

struct ChildIo {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// A [`LanguageProvider`] backed by a separate provider process spoken to over the protobuf wire.
/// Cheap to clone (the child + pipes are shared behind an `Arc<Mutex>`; calls are serialized).
#[derive(Clone)]
pub struct ProcessProvider {
    io: Arc<Mutex<ChildIo>>,
}

impl ProcessProvider {
    /// Spawn `command` (already configured with its args/root) as a provider sidecar.
    pub fn spawn(mut command: Command) -> Result<Self> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().map_err(|e| Error::Driver(format!("spawning provider sidecar: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Driver("sidecar stdin".into()))?;
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| Error::Driver("sidecar stdout".into()))?);
        Ok(ProcessProvider { io: Arc::new(Mutex::new(ChildIo { child, stdin, stdout })) })
    }

    fn call(&self, req: Request) -> Result<Response> {
        let mut io = self.io.lock().map_err(|_| Error::Driver("provider lock poisoned".into()))?;
        let io = &mut *io;
        write_msg(&mut io.stdin, &req).map_err(|e| Error::Driver(format!("provider write: {e}")))?;
        let resp: Response = read_msg(&mut io.stdout)
            .map_err(|e| Error::Driver(format!("provider read: {e}")))?
            .ok_or_else(|| Error::Driver("provider closed the connection".into()))?;
        if resp.error.is_empty() {
            Ok(resp)
        } else {
            Err(Error::Driver(resp.error))
        }
    }
}

impl Drop for ChildIo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LanguageProvider for ProcessProvider {
    fn granularity(&self) -> Granularity {
        match self.call(Request { method: M_GRANULARITY, ..Default::default() }) {
            Ok(r) if r.granularity == 1 => Granularity::Ast,
            _ => Granularity::Symbol,
        }
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let r = self.call(Request { method: M_STRUCTURE, file: file.to_string_lossy().into_owned(), ..Default::default() })?;
        Ok(r.nodes.iter().map(pb_to_node).collect())
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let r = self.call(Request { method: M_IMPORT_GRAPH, ..Default::default() })?;
        let mut g: ImportGraph = ImportGraph::new();
        for e in &r.edges {
            g.insert(PathBuf::from(&e.from), e.tos.iter().map(PathBuf::from).collect());
        }
        Ok(g)
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        let r = self.call(Request {
            method: M_APPLY_EDITS,
            ops: ops.iter().map(op_to_pb).collect(),
            opts: Some(opts_to_pb(opts)),
            ..Default::default()
        })?;
        Ok(r.result.as_ref().map(pb_to_commit).unwrap_or(CommitResult::Rejected {
            failed_op_index: -1,
            feedback: "provider returned no result".into(),
        }))
    }
}

impl ProcessProvider {
    /// `outline` is not on the trait (it's a free fn per provider) but the wire carries it.
    pub fn outline(&self, content: &str) -> Result<String> {
        let r = self.call(Request { method: M_OUTLINE, content: content.to_string(), ..Default::default() })?;
        Ok(r.outline)
    }
}

/// Resolve the sidecar executable for `lang` (`rust` / `ts` / …) and return a [`Command`] with
/// `--root` set, ready for [`ProcessProvider::spawn`]. Resolution order: `$CI_PROVIDER_<LANG>`
/// (an explicit/vendored path) → a sibling of the current exe (the dev/`cargo build` layout) →
/// the bare name on `PATH`. `None` when the first two miss and the caller wants to fall back to
/// in-process (the bare-`PATH` arm is only taken when `allow_path`).
pub fn sidecar_command(lang: &str, root: &Path, allow_path: bool) -> Option<Command> {
    sidecar_command_with(lang, root, allow_path, None)
}

/// Like [`sidecar_command`], but takes a `manifest_bin` — a vendored binary from the provider
/// manifest (`providers.<lang>.bin`), the highest-priority source in the resolution order (the
/// offline/air-gapped path). Resolution: `manifest_bin` → `$CI_PROVIDER_<LANG>` → exe sibling →
/// bare name on `PATH` (only when `allow_path`).
pub fn sidecar_command_with(
    lang: &str,
    root: &Path,
    allow_path: bool,
    manifest_bin: Option<&str>,
) -> Option<Command> {
    let bin = format!("peashooter-provider-{lang}");
    let from_manifest = manifest_bin.map(PathBuf::from).filter(|p| p.is_file());
    let from_env = std::env::var(format!("CI_PROVIDER_{}", lang.to_uppercase()))
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    let from_sibling = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(&bin)))
        .filter(|p| p.is_file());
    let mut cmd = match from_manifest.or(from_env).or(from_sibling) {
        Some(p) => Command::new(p),
        None if allow_path => Command::new(&bin),
        None => return None,
    };
    cmd.arg("--root").arg(root);
    Some(cmd)
}

// ── sidecar: serve ───────────────────────────────────────────────────────────

/// Answer one request against `provider`. `outline` is supplied separately since it isn't a trait
/// method. Errors are returned in `Response.error` (the host turns them into `Err`).
pub fn handle(provider: &dyn LanguageProvider, outline: &dyn Fn(&str) -> String, req: &Request) -> Response {
    let mut resp = Response::default();
    let r: Result<()> = (|| {
        match req.method {
            M_GRANULARITY => resp.granularity = if matches!(provider.granularity(), Granularity::Ast) { 1 } else { 0 },
            M_STRUCTURE => resp.nodes = provider.structure(Path::new(&req.file))?.iter().map(node_to_pb).collect(),
            M_IMPORT_GRAPH => {
                for (from, tos) in provider.import_graph()? {
                    resp.edges.push(PbEdge {
                        from: from.to_string_lossy().into_owned(),
                        tos: tos.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                    });
                }
            }
            M_OUTLINE => resp.outline = outline(&req.content),
            M_APPLY_EDITS => {
                let ops: Result<Vec<EditOp>> = req.ops.iter().map(pb_to_op).collect();
                let opts = req.opts.as_ref().map(pb_to_opts).unwrap_or_default();
                resp.result = Some(commit_to_pb(&provider.apply_edits(&ops?, &opts)?));
            }
            other => return Err(Error::Driver(format!("unknown method {other}"))),
        }
        Ok(())
    })();
    if let Err(e) = r {
        resp.error = e.to_string();
    }
    resp
}

/// Sidecar entry point: serve `provider` over stdin/stdout until the host closes the pipe.
pub fn serve_stdio(provider: impl LanguageProvider, outline: impl Fn(&str) -> String) -> io::Result<()> {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let stdout = io::stdout();
    let mut w = stdout.lock();
    while let Some(req) = read_msg::<_, Request>(&mut r)? {
        let resp = handle(&provider, &outline, &req);
        write_msg(&mut w, &resp)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_prefers_vendored_manifest_bin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("vendored-provider");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        // `zzlang` avoids colliding with any real `peashooter-provider-*` sibling of the test exe.
        let cmd = sidecar_command_with("zzlang", dir.path(), false, bin.to_str())
            .expect("a vendored manifest bin resolves the sidecar");
        assert_eq!(cmd.get_program(), bin.as_os_str(), "vendored binary used verbatim");
        // A missing vendored path with no env/sibling and no PATH fallback → nothing to spawn.
        assert!(sidecar_command_with("zzlang", dir.path(), false, Some("/no/such/bin")).is_none());
    }
}
