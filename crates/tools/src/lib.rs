//! otto in-process tools (behind `otto_engine_core::Tool`) and the default permission gate.

pub mod fs;
pub mod gate;

pub use fs::{FsListTool, FsReadTool, FsWriteTool};
pub use gate::DefaultPermissionGate;
