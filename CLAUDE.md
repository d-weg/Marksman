# Working in this repo

**Use the Marksman MCP tools for code navigation and edits here.** This *is* the Marksman codebase
(a Rust workspace of `ci-*` spine crates + per-language providers under `crates/langs/`) — dogfood
it. When the `marksman` MCP server is registered, prefer its **two tools** over raw
`grep`/`Read`/`Edit`:

- **`inspect`** — read and locate code; one tool, `mode`-dispatched:
  - `search` — find code by concept/task ("where is the gate diff computed") instead of guessing
    which files to grep.
  - `symbol` — exact/substring name → self-locating node-id handles.
  - `file` — a file's anchors + its import/module lines.
  - `node` — one anchor's full source (or a `:body` / `:param.N` / `:return` sub-node).
  - `map` — a directory / architecture overview.
- **`apply_edits`** — every code edit, structural or surgical (`rename`, `move_file`,
  `replace_text`, `replace_node`, `add_parameter`, `add_symbol`, …), applied atomically and
  **type-checked over the blast radius before it lands** (Rust here is gated by `cargo check` +
  rust-analyzer). Address a symbol by name — no locate step first when the task already names it.
  For a wide change, make the anchor edit *alone* and let the reject enumerate every affected site
  with a ready-to-copy fix. Trust the gate — don't re-verify a committed edit by hand.

Handles/ids from `inspect` feed straight into `apply_edits`.

If those tools aren't present, the `marksman` MCP server isn't registered (or points at a stale
binary): build it — `cargo build --release`, then `claude mcp add marksman --
target/release/marksman-mcp` — or fall back to the standard tools.

## Non-negotiables (see CONTRIBUTING.md)

- Never serve stale reads; never silently degrade a gate.
- Keep the workspace **0-warning** and `cargo test --workspace` green.
- Real-tool integration tests are `#[ignore]` (they need the language toolchains); run the relevant
  ones before touching a provider or the gate.
