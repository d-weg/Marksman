// ts-morph sidecar — a persistent Node process that holds the TS project in memory and
// answers the GateEngine operations (diagnostics / rename / willRename) for codeindex-rs.
// Protocol: newline-delimited JSON on stdin/stdout. One request -> one response, echoing `id`.
//
//   {"id":1,"op":"diagnostics","files":[{"path":"src/a.ts","content":"..."}]}
//     -> {"id":1,"diags":[{"file":"src/a.ts","line":12,"code":2554,"message":"..."}]}
//   {"id":2,"op":"rename","file":"src/a.ts","line":2,"character":16,"newName":"foo"}   (0-based)
//     -> {"id":2,"changes":{"file:///abs/src/a.ts":[{"range":{...},"newText":"foo"}]}}
//   {"id":3,"op":"willRename","from":"src/math.ts","to":"src/util/math.ts"}
//     -> {"id":3,"changes":{...}}
//
// Why ts-morph over an LSP server: synchronous, in-process diagnostics (no publish/settle
// race), and the raw TS LanguageService for precise rename/move edits. The project loads once
// at startup (the prewarm cost), then every op is cheap.
const fs = require("fs");
const path = require("path");
const readline = require("readline");
const { Project, ts } = require("ts-morph");

const rootIdx = process.argv.indexOf("--root");
const root = rootIdx >= 0 ? process.argv[rootIdx + 1] : process.cwd();
const abs = (rel) => path.resolve(root, rel);
const fileUri = (absPath) => "file://" + absPath;

// Load the whole tsconfig program once (this is the one-time "warm" cost).
let project;
try {
  project = new Project({ tsConfigFilePath: path.join(root, "tsconfig.json") });
} catch (e) {
  project = new Project({ compilerOptions: { allowJs: true, strict: true } });
  project.addSourceFilesAtPaths(path.join(root, "**/*.{ts,tsx}"));
}
const rawLs = project.getLanguageService().compilerObject; // ts.LanguageService

function compilerNode(absPath) {
  const sf = project.getSourceFile(absPath) || project.addSourceFileAtPathIfExists(absPath);
  return sf ? sf.compilerNode : undefined;
}

function rangeOf(cn, start, length) {
  const s = ts.getLineAndCharacterOfPosition(cn, start);
  const e = ts.getLineAndCharacterOfPosition(cn, start + length);
  return { start: { line: s.line, character: s.character }, end: { line: e.line, character: e.character } };
}

function setContent(absPath, content) {
  const sf = project.getSourceFile(absPath);
  if (sf) sf.replaceWithText(content);
  else project.createSourceFile(absPath, content, { overwrite: true });
}

// Files whose in-memory content has been overlaid and may differ from disk. Before each
// diagnostics request we restore any overlaid file NOT in the new request back to its on-disk
// content, so a REJECTED/aborted prior edit can't leave phantom source that corrupts a later
// gate pass. (A committed edit already wrote disk, so restoring == the new truth; a rejected
// edit restores the original.)
const dirtied = new Set();

function restoreDirtyExcept(keep) {
  for (const ap of [...dirtied]) {
    if (keep.has(ap)) continue;
    try {
      setContent(ap, fs.readFileSync(ap, "utf8"));
    } catch {
      const sf = project.getSourceFile(ap);
      if (sf) sf.delete(); // gone on disk
    }
    dirtied.delete(ap);
  }
}

function diagnostics(files) {
  restoreDirtyExcept(new Set(files.map((f) => abs(f.path))));
  for (const f of files) {
    setContent(abs(f.path), f.content);
    dirtied.add(abs(f.path));
  }
  const out = [];
  for (const f of files) {
    const sf = project.getSourceFile(abs(f.path));
    if (!sf) continue;
    for (const d of sf.getPreEmitDiagnostics()) {
      if (d.getCategory() !== ts.DiagnosticCategory.Error) continue;
      const mt = d.getMessageText();
      const message = typeof mt === "string" ? mt : ts.flattenDiagnosticMessageText(mt.compilerObject ? mt.compilerObject : mt, "\n");
      out.push({ file: f.path, code: d.getCode() || 0, message, line: d.getLineNumber() || 1 });
    }
  }
  return { diags: out };
}

function rename(file, line, character, newName) {
  const ap = abs(file);
  const cn = compilerNode(ap);
  if (!cn) return { changes: {} };
  const pos = ts.getPositionOfLineAndCharacter(cn, line, character);
  const locs = rawLs.findRenameLocations(ap, pos, false, false, true) || [];
  const changes = {};
  for (const loc of locs) {
    const lcn = compilerNode(loc.fileName);
    if (!lcn) continue;
    const uri = fileUri(loc.fileName);
    (changes[uri] = changes[uri] || []).push({ range: rangeOf(lcn, loc.textSpan.start, loc.textSpan.length), newText: newName });
  }
  return { changes };
}

function willRename(from, to) {
  const fmt = ts.getDefaultFormatCodeSettings ? ts.getDefaultFormatCodeSettings() : {};
  const fileChanges = rawLs.getEditsForFileRename(abs(from), abs(to), fmt, {}) || [];
  const changes = {};
  for (const fc of fileChanges) {
    const lcn = compilerNode(fc.fileName);
    if (!lcn) continue;
    const uri = fileUri(fc.fileName);
    const arr = (changes[uri] = changes[uri] || []);
    for (const tc of fc.textChanges) arr.push({ range: rangeOf(lcn, tc.span.start, tc.span.length), newText: tc.newText });
  }
  return { changes };
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let req;
  try { req = JSON.parse(line); } catch { return; }
  let res;
  try {
    if (req.op === "diagnostics") res = diagnostics(req.files || []);
    else if (req.op === "rename") res = rename(req.file, req.line, req.character, req.newName);
    else if (req.op === "willRename") res = willRename(req.from, req.to);
    else if (req.op === "ping") res = { ok: true };
    else res = { error: "unknown op: " + req.op };
  } catch (e) {
    res = { error: String((e && e.stack) || e) };
  }
  res.id = req.id;
  process.stdout.write(JSON.stringify(res) + "\n");
});
// Exit when the parent closes our stdin (process gone) so we don't orphan.
rl.on("close", () => process.exit(0));
process.stderr.write("[ts-morph-sidecar] ready for " + root + "\n");
