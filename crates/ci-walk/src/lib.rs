//! ci-walk — file discovery (gitignore-aware), language detection, and
//! workspace/package detection. Language-blind beyond a coarse extension tag.
pub mod discover;
pub mod lang;
pub mod workspace;

pub use discover::{discover, present_langs, DiscoveredFile};
pub use lang::Lang;
pub use workspace::{detect_workspace, Package, Workspace};
