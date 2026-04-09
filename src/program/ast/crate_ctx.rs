//! AST node types for the crate execution context.

use super::common::{
    Branch, CommonCondition, IfBlock, ManualStepNode, RunStep, SnapshotMetadataNode,
    WaitForContinueNode, WithEnvFileBlock,
};

/// The type of a Rust crate, used as a filter in crate-context conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrateTypeFilter {
    /// A binary crate (produces an executable).
    Bin,
    /// A library crate.
    Lib,
    /// A procedural macro crate.
    ProcMacro,
}

/// A block that iterates over all selected crates (standalone or within a workspace)
/// in dependency order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForCrateBlock {
    /// The statements to execute for each crate.
    pub statements: Vec<CrateStatement>,
}

/// A single statement in the crate execution context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateStatement {
    /// Execute a command in the crate's manifest directory.
    Run(RunStep),
    /// Pause for a manual step with instructions for the user.
    ManualStep(ManualStepNode),
    /// Conditional branching using crate-level conditions.
    If(IfBlock<CrateCondition, Self>),
    /// Capture and store cargo metadata for the current workspace under a name.
    SnapshotMetadata(SnapshotMetadataNode),
    /// Execute nested statements with environment variables loaded from a file.
    WithEnvFile(WithEnvFileBlock<Self>),
    /// Pause execution until the user releases this barrier.
    WaitForContinue(WaitForContinueNode),
}

/// A boolean condition available in the crate execution context.
///
/// Extends [`CommonCondition`] with conditions that inspect crate-specific properties
/// such as crate type. These conditions are only meaningful when operating on an
/// individual crate and are therefore unavailable at the workspace level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateCondition {
    /// A condition from the common set available in all contexts.
    Common(CommonCondition),
    /// True if this crate matches the given type filter.
    CrateType(CrateTypeFilter),
    /// True if this crate lives in a standalone (single-crate) workspace.
    Standalone,
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And(Vec<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or(Vec<Self>),
}

/// A condition allowed inside `select crates where ...` filters.
///
/// This is a restricted subset of [`CrateCondition`] that can be evaluated
/// statically against the registered configuration at task-creation time.
/// The `ask_user` and `run` variants are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateSelectCondition {
    /// True if the crate lives in a standalone (single-crate) workspace.
    Standalone,
    /// True if the crate matches the given type filter.
    CrateType(CrateTypeFilter),
    /// True if the inner condition evaluates to false.
    Not(Box<Self>),
    /// True if all inner conditions evaluate to true (short-circuits on first false).
    And(Vec<Self>),
    /// True if at least one inner condition evaluates to true (short-circuits on first true).
    Or(Vec<Self>),
}

/// Converts a [`CrateSelectCondition`] into a [`CrateCondition`].
impl From<CrateSelectCondition> for CrateCondition {
    fn from(cond: CrateSelectCondition) -> Self {
        match cond {
            CrateSelectCondition::Standalone => Self::Standalone,
            CrateSelectCondition::CrateType(t) => Self::CrateType(t),
            CrateSelectCondition::Not(inner) => Self::Not(Box::new(Self::from(*inner))),
            CrateSelectCondition::And(conditions) => {
                Self::And(conditions.into_iter().map(Self::from).collect())
            }
            CrateSelectCondition::Or(conditions) => {
                Self::Or(conditions.into_iter().map(Self::from).collect())
            }
        }
    }
}

/// A filter applied to the set of crates selected by a `select crates` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateFilter {
    /// Optional condition; if `None`, all registered standalone crates are selected.
    pub condition: Option<CrateSelectCondition>,
}

/// Type alias for a crate if-branch used in the crate context.
pub type CrateBranch = Branch<CrateCondition, CrateStatement>;

/// Type alias for a crate if-block used in the crate context.
pub type CrateIfBlock = IfBlock<CrateCondition, CrateStatement>;
