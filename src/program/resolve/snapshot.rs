//! The snapshot types stored in a task directory at task-creation time.
//!
//! When a task is created from a `.cfe` program, the program's `select` statements
//! are evaluated against the registered workspaces/crates and the results are
//! serialized here.  This ensures that task execution is reproducible even if the
//! registered set of targets changes after the task was created.

use std::path::PathBuf;

/// The fully resolved form of a `.cfe` program, produced at task-creation time.
///
/// This snapshot captures which workspaces and standalone crates the program
/// will operate on and what their dependency relationships are.  The original
/// program AST is stored separately as the raw `.cfe` source text.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedProgram {
    /// Workspaces to iterate over, in inter-workspace dependency order.
    ///
    /// Each element corresponds to one iteration of a `for workspace { … }` block.
    pub workspace_executions: Vec<ResolvedWorkspaceExecution>,
    /// Standalone crates to iterate over, in dependency order.
    ///
    /// Each element corresponds to one iteration of a `for crate { … }` block.
    pub crate_executions: Vec<ResolvedCrateExecution>,
}

/// A single workspace that will be iterated over during task execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedWorkspaceExecution {
    /// Canonical path to the workspace root (directory containing `Cargo.toml`).
    pub manifest_dir: PathBuf,
    /// Other workspaces (by their canonical manifest dir) that must complete
    /// before this one may start.  An empty vec means no inter-workspace deps.
    pub dependencies: Vec<PathBuf>,
    /// Member crates of this workspace, in intra-workspace dependency order.
    ///
    /// Each element corresponds to one iteration of a `for crate in workspace { … }` block.
    pub member_crates: Vec<ResolvedCrateExecution>,
}

/// A single crate that will be iterated over during task execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCrateExecution {
    /// Canonical path to the crate's manifest directory.
    pub manifest_dir: PathBuf,
    /// Other crates (by their canonical manifest dir) in the same set that must
    /// complete before this one.  An empty vec means no tracked dependencies.
    pub dependencies: Vec<PathBuf>,
}
