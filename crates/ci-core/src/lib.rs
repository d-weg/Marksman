//! ci-core — language-blind types, the `LanguageProvider` seam, and config.
//!
//! Nothing in this crate knows about any specific language. All syntax/semantics
//! live behind [`LanguageProvider`]; the core deals only in [`Node`],
//! [`ImportGraph`], [`EditOp`], [`Manifest`], etc. A provider's `structure()` tree
//! can be shallow (SCIP, symbol-level) or deep (AST, syntax-level) — see
//! [`Granularity`] — without the core caring.
pub mod config;
pub mod driver;
pub mod error;
pub mod fingerprint;
pub mod outline;
pub mod paths;
pub mod sandbox;
pub mod text;
pub mod toolchain;
pub mod types;
pub mod weight;

pub use config::{Config, ProviderManifest};
pub use driver::{LanguageProvider, ReadIndex};
pub use error::{Error, Result};
pub use outline::{elide_bodies, elide_bodies_with};
pub use paths::rel_path;
pub use sandbox::{oci_runtime, resolve_sandbox, tool_command, HostSandbox, OciSandbox, Sandbox};
pub use text::byte_offset;
pub use toolchain::{
    discover_tool, gate_timeout, probe_tool, run_capped, run_gate_capped, silent_tool_failure_diag,
    CappedOutput, ToolStatus, ToolchainReport, GATE_OUTPUT_CAP,
};
pub use types::*;
