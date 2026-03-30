//! The `.cfe` (cargo-for-each) program language: AST, parser, and execution support.
//!
//! A program is a text file that describes what operations to perform on which
//! Rust workspaces and crates.  It is parsed from a `.cfe` file on disk at task
//! creation time, resolved against the registered targets, and then executed
//! step-by-step by the task runner.

pub mod ast;
pub mod cursor;
pub mod evaluate;
pub mod parser;
pub mod resolve;

pub use ast::crate_ctx::{CrateFilter, ForCrateBlock};
pub use ast::workspace_ctx::{ForWorkspaceBlock, WorkspaceFilter};

/// A parsed `.cfe` program.
///
/// A program is a sequence of top-level statements that together describe which
/// workspaces and crates to operate on and what to do with each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The top-level statements of the program, in the order they appear in the source.
    pub statements: Vec<GlobalStatement>,
}

/// A single top-level statement in a `.cfe` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalStatement {
    /// Selects workspaces to operate on (`select workspaces [where <condition>];`).
    ///
    /// Multiple `SelectWorkspaces` statements accumulate: the union of all selected
    /// workspaces is used when a `for workspace` block is executed.
    SelectWorkspaces(WorkspaceFilter),
    /// Selects standalone crates to operate on (`select crates [where <condition>];`).
    ///
    /// Only standalone crates are reachable via this statement. Member crates of
    /// multi-crate workspaces are accessed through
    /// `for workspace { for crate in workspace { ... } }`.
    SelectCrates(CrateFilter),
    /// Iterates over all selected workspaces in inter-workspace dependency order.
    ForWorkspace(ForWorkspaceBlock),
    /// Iterates over all selected standalone crates in dependency order.
    ForCrate(ForCrateBlock),
}
